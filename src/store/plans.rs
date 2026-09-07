//! Coding Plan 存储：CRUD、用户→生效 Plan 运行时解析、周期计数器、用量查询、
//! 直连用户/加入分组成员管理。
//!
//! token 上限以精确 BIGINT 存储；API 层接受 "1.5G"/"500M"/"2T" 等单位串
//! （[`parse_token_limit`]）或纯数字。周期一律 UTC，与 usage_daily 聚合口径一致。

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool};

pub const PERIOD_HOURLY: &str = "hourly";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";
pub const PERIOD_TOTAL: &str = "total";

/// hourly 锚点：fixed=UTC 整点网格（锚点=epoch）；join=用户开通时间偏移
pub const ANCHOR_FIXED: &str = "fixed";
pub const ANCHOR_JOIN: &str = "join";

pub const OVERAGE_BLOCK: &str = "block";
pub const OVERAGE_DOWNGRADE: &str = "downgrade";
pub const OVERAGE_LOG: &str = "log";

pub const VALID_PERIODS: [&str; 4] = [PERIOD_HOURLY, PERIOD_DAILY, PERIOD_MONTHLY, PERIOD_TOTAL];
pub const VALID_ANCHOR_MODES: [&str; 2] = [ANCHOR_FIXED, ANCHOR_JOIN];
pub const VALID_OVERAGES: [&str; 3] = [OVERAGE_BLOCK, OVERAGE_DOWNGRADE, OVERAGE_LOG];
pub const VALID_ALERT_CHANNELS: [&str; 3] = ["in_site", "email", "webhook"];

/// hourly 窗口长度上限（一周）；下限 1 = 每小时
pub const MAX_PERIOD_HOURS: i32 = 168;

/// 周期配置校验（create/update 共用；纯函数可单测）。
/// period_hours 仅对 hourly 生效（其余类型任意合法值均被忽略）。
pub fn validate_period_config(
    period_type: &str,
    period_hours: i32,
    anchor_mode: &str,
) -> Result<(), String> {
    if !VALID_PERIODS.contains(&period_type) {
        return Err(format!("invalid period_type: {period_type}"));
    }
    if !VALID_ANCHOR_MODES.contains(&anchor_mode) {
        return Err(format!("invalid period_anchor_mode: {anchor_mode}"));
    }
    if period_type == PERIOD_HOURLY && !(1..=MAX_PERIOD_HOURS).contains(&period_hours) {
        return Err(format!(
            "period_hours must be within 1..={MAX_PERIOD_HOURS} for hourly plans"
        ));
    }
    Ok(())
}

/// "HH:MM"（或 "HH:MM:SS"）→ NaiveTime；API 层入参校验用
pub fn parse_active_time(raw: &str) -> Result<NaiveTime, String> {
    let t = raw.trim();
    NaiveTime::parse_from_str(t, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(t, "%H:%M:%S"))
        .map_err(|_| format!("invalid time '{raw}' (expected HH:MM)"))
}

/// 生效时段成对校验：必须成对出现；相等 = 零长度窗口无意义（全天请双空）
pub fn validate_active_window(
    start: Option<NaiveTime>,
    end: Option<NaiveTime>,
) -> Result<(), String> {
    if start.is_some() != end.is_some() {
        return Err("active window requires both start and end".into());
    }
    if let (Some(s), Some(e)) = (start, end) {
        if s == e {
            return Err("active window start == end (leave empty for full-day)".into());
        }
    }
    Ok(())
}

/// "1.5G"/"500M"/"2T"/"1024K"/纯数字 → 精确 token 数（十进制单位，大小写不敏感）
pub fn parse_token_limit(raw: &str) -> Result<i64, String> {
    let s = raw.trim();
    let Some(idx) = s.find(|c: char| !c.is_ascii_digit() && c != '.') else {
        return finish(s, "");
    };
    if s[..idx].is_empty() {
        return Err(format!("invalid token_limit: {raw:?}"));
    }
    finish(&s[..idx], &s[idx..])
}

fn finish(num: &str, unit: &str) -> Result<i64, String> {
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid token_limit: {num:?}{unit}"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("invalid token_limit: {num:?}{unit}"));
    }
    let mult: f64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" => 1.0,
        "K" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        other => return Err(format!("unknown token_limit unit {other:?} (use K/M/G/T)")),
    };
    let tokens = v * mult;
    if tokens >= i64::MAX as f64 {
        return Err("token_limit overflow".into());
    }
    Ok(tokens as i64)
}

/// 精确 token 数 → 人类可读（1500000000 → "1.5G"；500000000 → "500M"）
pub fn format_token_limit(tokens: i64) -> String {
    let t = tokens as f64;
    for (div, unit) in [(1e12, "T"), (1e9, "G"), (1e6, "M"), (1e3, "K")] {
        if t >= div {
            let s = format!("{:.3}", t / div);
            let s = s.trim_end_matches('0').trim_end_matches('.');
            return format!("{s}{unit}");
        }
    }
    tokens.to_string()
}

// ---------- 模型作用域（model_scope） ----------

/// 单条模型 pattern 长度上限；超长视为拼写事故而非合法标识符
pub const MAX_MODEL_PATTERN_LEN: usize = 128;
/// allow/deny 合计 pattern 条数上限；防误配超长列表拖慢热路径匹配
pub const MAX_MODEL_PATTERNS: usize = 32;

/// Plan 的模型作用域（`coding_plans.model_scope` JSONB，NULL = 对所有模型生效）。
/// - `allow`（白名单）：缺省/空 = 不限模型（仅受 deny 约束）
/// - `deny`（黑名单）：命中任一即排除，优先级高于 allow（冲突时黑名单赢）
/// pattern 语义与路由规则一致（精确匹配，或尾缀 `*` 前缀匹配覆盖模型家族，
/// 如 `claude-*`）；parse 时统一归一化为小写，匹配大小写不敏感。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelScope {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

impl ModelScope {
    /// 黑名单优先：命中任一 deny → false；白名单空 = 全放行（仅受 deny 约束）；
    /// 否则需命中任一 allow。
    pub fn matches(&self, model: &str) -> bool {
        if self.deny.iter().any(|p| model_matches_pattern(p, model)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|p| model_matches_pattern(p, model))
    }

    /// 作用域具体度（[`resolve_plan`] 择优排序键，越大越具体）：
    /// 2 = 全部条目均为精确匹配；1 = 含通配条目。
    /// 未配置作用域（None）由 [`scope_specificity`] 记 0（全模型 = 最不具体）。
    pub fn specificity(&self) -> u8 {
        if self
            .allow
            .iter()
            .chain(self.deny.iter())
            .all(|p| !p.ends_with('*'))
        {
            2
        } else {
            1
        }
    }
}

/// 择优排序键：未配置作用域 = 0（对所有模型生效，最不具体）。
fn scope_specificity(scope: Option<&ModelScope>) -> u8 {
    scope.map_or(0, ModelScope::specificity)
}

/// 模型 pattern 匹配（精确 或 尾缀 `*` 前缀；大小写不敏感，零分配）。
/// 与 `service::routing::matches_pattern` 同语义——作用域所见模型串 = 路由所见模型串。
pub fn model_matches_pattern(pattern: &str, model: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            model.len() >= prefix.len()
                && model.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        }
        None => pattern.eq_ignore_ascii_case(model),
    }
}

/// 单条 pattern 合法性（写入期明确报错，不静默忽略）：
/// 非空、≤[`MAX_MODEL_PATTERN_LEN`]、字符集 `alnum . _ + : - / *`；
/// 通配仅支持尾缀单个 `*`（中间 `*` / `**` 拒绝）；裸 `*` 拒绝——等价于不配置作用域，
/// 显式写出只会制造歧义配置。
pub fn validate_model_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("empty model pattern".into());
    }
    if pattern.len() > MAX_MODEL_PATTERN_LEN {
        return Err(format!(
            "model pattern too long (>{} chars)",
            MAX_MODEL_PATTERN_LEN
        ));
    }
    if !pattern.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'*' | b'.' | b'_' | b'+' | b':' | b'-' | b'/')
    }) {
        return Err(format!(
            "invalid character in model pattern '{pattern}' (allowed: letters digits . _ + : - / and trailing *)"
        ));
    }
    match pattern.matches('*').count() {
        0 => Ok(()),
        1 if pattern.ends_with('*') && pattern != "*" => Ok(()),
        1 if pattern == "*" => Err("bare '*' matches every model; omit model_scope instead".into()),
        1 => Err(format!(
            "model pattern '{pattern}' only supports trailing '*' (prefix match, same as routing rules)"
        )),
        n => Err(format!(
            "model pattern '{pattern}' must contain at most one '*' (got {n})"
        )),
    }
}

/// 解析 model_scope JSON（API 入参与 DB 行共用；归一化 pattern 为小写）：
/// - NULL / 缺省 → `Ok(None)`
/// - `{}` / 双空数组 → `Ok(None)`（等价不限制，归一化存 NULL）
/// - allow 与 deny 存在完全相同条目 → Err（黑名单优先语义下该条目永不命中，写入期报错）
/// - 结构错误（非对象/未知字段/非字符串条目/pattern 非法）→ Err，调用方 400 或加载期跳过该 Plan
pub fn parse_model_scope(v: Option<&Value>) -> Result<Option<ModelScope>, String> {
    let Some(v) = v else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let obj = v.as_object().ok_or_else(|| {
        format!("model_scope must be an object with optional 'allow'/'deny' arrays, got: {v}")
    })?;
    for key in obj.keys() {
        if key != "allow" && key != "deny" {
            return Err(format!(
                "unknown model_scope field '{key}' (expected 'allow'/'deny'; typo?)"
            ));
        }
    }
    let parse_list = |key: &str| -> Result<Vec<String>, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(vec![]),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    let s = it.as_str().ok_or_else(|| {
                        format!("model_scope.{key} entries must be strings, got: {it}")
                    })?;
                    validate_model_pattern(s).map_err(|e| format!("model_scope.{key}: {e}"))?;
                    out.push(s.to_ascii_lowercase());
                }
                Ok(out)
            }
            Some(other) => Err(format!(
                "model_scope.{key} must be an array of strings, got: {other}"
            )),
        }
    };
    let allow = parse_list("allow")?;
    let deny = parse_list("deny")?;
    if allow.len() + deny.len() > MAX_MODEL_PATTERNS {
        return Err(format!(
            "model_scope allows at most {MAX_MODEL_PATTERNS} patterns in total"
        ));
    }
    if allow.iter().any(|a| deny.contains(a)) {
        return Err(
            "model_scope: identical pattern in both 'allow' and 'deny' (deny wins, so the allow entry can never match)".into(),
        );
    }
    if allow.is_empty() && deny.is_empty() {
        return Ok(None);
    }
    Ok(Some(ModelScope { allow, deny }))
}

/// 计划原始行（CRUD/审计快照）
#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct CodingPlan {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub priority: i32,
    pub token_limit: i64,
    pub period_type: String,
    /// hourly 窗口长度（小时，1..=168）；其余类型恒为 1，不参与计算
    pub period_hours: i32,
    /// hourly 锚点模式：fixed / join
    pub period_anchor_mode: String,
    pub overage_action: String,
    pub downgrade_model: Option<String>,
    pub alert_channels: Value,
    pub webhook_url: String,
    pub enabled: bool,
    /// 生效时段（服务器本地墙钟）；双 None = 全天；start > end = 跨零点
    pub active_start: Option<NaiveTime>,
    pub active_end: Option<NaiveTime>,
    /// 模型作用域（NULL = 对所有模型生效；结构见 [`parse_model_scope`]）
    pub model_scope: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 列表行：计划 + 当期用量聚合（看板首屏）
#[derive(Debug, FromRow, serde::Serialize)]
pub struct PlanSummary {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub priority: i32,
    pub token_limit: i64,
    pub period_type: String,
    pub period_hours: i32,
    pub period_anchor_mode: String,
    pub overage_action: String,
    pub downgrade_model: Option<String>,
    pub alert_channels: Value,
    pub webhook_url: String,
    pub active_start: Option<NaiveTime>,
    pub active_end: Option<NaiveTime>,
    pub model_scope: Option<Value>,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub used_tokens: i64,
    pub active_users: i64,
    pub group_count: i64,
    pub member_count: i64,
}

/// 用户生效 Plan 运行时（内存热路径；AppState::reload 刷新）
#[derive(Debug, Clone)]
pub struct PlanRuntime {
    pub plan_id: i64,
    pub plan_name: String,
    pub group_name: String,
    pub token_limit: i64,
    pub period_type: String,
    /// hourly 窗口长度（小时）；其余类型忽略
    pub period_hours: i32,
    /// hourly 锚点：fixed=UTC 整点网格；join=成员 added_at（开通时间）
    pub period_anchor_mode: String,
    /// join 锚点数据源；fixed 时不参与计算
    pub member_since: DateTime<Utc>,
    pub overage_action: String,
    pub downgrade_model: Option<String>,
    pub alert_channels: Vec<String>,
    pub webhook_url: String,
    /// 择优优先级（请求期 [`resolve_plan`] 用）
    pub priority: i32,
    /// 生效时段（服务器本地墙钟）；双 None = 全天；start > end = 跨零点
    pub active_start: Option<NaiveTime>,
    pub active_end: Option<NaiveTime>,
    /// 模型作用域（parse 后供热路径匹配；None = 对所有模型生效）
    pub model_scope: Option<ModelScope>,
}

impl PlanRuntime {
    /// 当前统计窗口起点时刻（UTC，纯时间函数）：daily=当日 00:00 / monthly=当月 1 日
    /// 00:00 / total=常量纪元 / hourly=按锚点与 period_hours 切分的当前桶起点。
    /// 宕机重启后按墙钟重算即可命中当前窗口——错过整段窗口即自然跳过，无需补偿任务。
    pub fn period_start(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        match self.period_type.as_str() {
            PERIOD_DAILY => now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc(),
            PERIOD_MONTHLY => NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
                .expect("first of month is a valid date")
                .and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc(),
            PERIOD_HOURLY => {
                // 锚点截断到整秒：保证与 DB 侧 extract(epoch)::bigint 桶键完全一致
                let anchor_ts = match self.period_anchor_mode.as_str() {
                    ANCHOR_JOIN => self.member_since.timestamp(),
                    _ => 0, // fixed：epoch 锚点 → UTC 整点网格
                };
                let step = (self.period_hours.max(1) as i64).saturating_mul(3600);
                // now < 锚点（时钟回拨/未来 added_at）钳为 0 → 桶起点 = 锚点
                let elapsed = (now.timestamp() - anchor_ts).max(0);
                DateTime::from_timestamp(anchor_ts + elapsed / step * step, 0)
                    .expect("bucket start is a valid instant")
            }
            _ => DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    /// 缓存/告警去重键：d:2026-09-01 / m:2026-09 / h:1788300000 / t。
    /// hourly 用桶起点 epoch 秒，与 plan_usage_counters.period_start 一一对应。
    pub fn period_key(&self, now: DateTime<Utc>) -> String {
        match self.period_type.as_str() {
            PERIOD_HOURLY => format!("h:{}", self.period_start(now).timestamp()),
            PERIOD_DAILY => format!("d:{}", now.date_naive()),
            PERIOD_MONTHLY => format!("m:{}-{:02}", now.year(), now.month()),
            _ => "t".to_string(),
        }
    }

    /// 生效时段判定：双 None = 全天；[start, end) 半开区间；start > end 视为
    /// 跨零点隔夜窗。`local_now` 传服务器本地墙钟（纯函数可单测）。
    pub fn active_at(&self, local_now: NaiveTime) -> bool {
        is_within_active_window(self.active_start, self.active_end, local_now)
    }
}

#[derive(FromRow)]
struct RuntimeRow {
    user_id: i64,
    plan_id: i64,
    plan_name: String,
    group_name: String,
    token_limit: i64,
    period_type: String,
    period_hours: i32,
    period_anchor_mode: String,
    member_since: DateTime<Utc>,
    overage_action: String,
    downgrade_model: Option<String>,
    alert_channels: Value,
    webhook_url: String,
    priority: i32,
    active_start: Option<NaiveTime>,
    active_end: Option<NaiveTime>,
    model_scope: Option<Value>,
}

/// 请求期解析用户生效 Plan（模型感知）：
/// 1. 过滤：当前处于生效时段，且模型作用域命中当前模型（`model=None` 用于无模型上下文
///    的路径——记账后告警/控制台展示——此时忽略作用域，与存量行为兼容）
/// 2. 择优：作用域更具体者优先（未配置=0 < 含通配=1 < 仅精确=2，见 [`ModelScope::specificity`]）；
///    同具体度取 (priority, plan_id) 最大（与存量「优先级最高、同分 plan_id 大」语义衔接）
/// 3. 冲突可追溯：≥2 个候选同时命中时 warn 一条（选中者 + 落选者及各自具体度），
///    供排查「这个模型为什么没走我配的 Plan」。
/// 全部候选都被过滤 → None（该用户此刻不受该批 Plan 限额）。
pub fn resolve_plan(
    plans: &HashMap<i64, Vec<PlanRuntime>>,
    user_id: i64,
    local_now: NaiveTime,
    model: Option<&str>,
) -> Option<PlanRuntime> {
    let candidates: Vec<&PlanRuntime> = plans
        .get(&user_id)?
        .iter()
        .filter(|p| p.active_at(local_now))
        .filter(|p| match (&p.model_scope, model) {
            (Some(scope), Some(m)) => scope.matches(m),
            _ => true,
        })
        .collect();
    let best = candidates.iter().copied().max_by_key(|p| match model {
        // 无模型上下文：完全沿用存量 (priority, plan_id) 语义，作用域不参与排序
        None => (0, p.priority, p.plan_id),
        Some(_) => (
            scope_specificity(p.model_scope.as_ref()),
            p.priority,
            p.plan_id,
        ),
    })?;
    if candidates.len() > 1 {
        tracing::warn!(
            user_id,
            model = model.unwrap_or(""),
            chosen_plan_id = best.plan_id,
            chosen_plan = %best.plan_name,
            chosen_specificity = scope_specificity(best.model_scope.as_ref()),
            overridden = ?candidates
                .iter()
                .filter(|c| c.plan_id != best.plan_id)
                .map(|c| {
                    (c.plan_id, c.plan_name.as_str(), scope_specificity(c.model_scope.as_ref()))
                })
                .collect::<Vec<_>>(),
            "multiple coding plans match this model; picked by (model_scope specificity, priority, plan_id)"
        );
    }
    Some(best.clone())
}

/// 双通道候选行源（分组加入 plan_groups ∪ 直连加入 plan_users）。
/// 全量加载与单用户回源查询共用同一份 SQL，避免两份语句漂移
/// （实例间 schema/语句 skew 曾导致旧实例 reload 永久失败、缓存冻结）。
const RUNTIME_SOURCE_SQL: &str = "\
           SELECT m.user_id, p.id AS plan_id, p.name AS plan_name, g.name AS group_name, \
                  p.token_limit, p.period_type, p.period_hours, p.period_anchor_mode, \
                  m.added_at AS member_since, p.overage_action, p.downgrade_model, \
                  p.alert_channels, p.webhook_url, p.priority, \
                  p.active_start, p.active_end, p.model_scope \
           FROM user_group_members m \
           JOIN user_groups g ON g.id = m.group_id \
           JOIN plan_groups pg ON pg.group_id = g.id \
           JOIN coding_plans p ON p.id = pg.plan_id AND p.enabled = TRUE \
           UNION ALL \
           SELECT pu.user_id, p.id, p.name, '' AS group_name, \
                  p.token_limit, p.period_type, p.period_hours, p.period_anchor_mode, \
                  pu.added_at AS member_since, p.overage_action, p.downgrade_model, \
                  p.alert_channels, p.webhook_url, p.priority, \
                  p.active_start, p.active_end, p.model_scope \
           FROM plan_users pu \
           JOIN coding_plans p ON p.id = pu.plan_id AND p.enabled = TRUE";

/// 全量用户 → 候选 Plan 列表（enabled 限定）。双通道入口：分组加入（plan_groups）∪
/// 直连加入（plan_users），每用户保留全部候选行——生效时段随时刻变化、模型作用域随
/// 请求模型变化，请求期用 [`resolve_plan`] 按「生效时段 + 作用域 + 具体度/priority」解析。
/// model_scope 解析失败的行跳过并 error 留痕（fail-closed，不猜测损坏配置的意图）。
pub async fn load_plan_runtimes(
    pool: &PgPool,
) -> Result<HashMap<i64, Vec<PlanRuntime>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, RuntimeRow>(&format!(
        "SELECT user_id, plan_id, plan_name, group_name, token_limit, period_type, \
                period_hours, period_anchor_mode, member_since, overage_action, \
                downgrade_model, alert_channels, webhook_url, priority, \
                active_start, active_end, model_scope \
         FROM ({RUNTIME_SOURCE_SQL}) r"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows_into_runtimes(rows))
}

/// 单用户回源：绕过内存缓存直接按 DB 解析该用户的候选 Plan。
/// 用于 `/api/me/plan` 缓存未命中时的兜底——reload 是全有或全无（任一节失败即整体
/// 不写入），多实例滚动部署/单节故障期间成员变更可能长时间不可见；用户可见的归属
/// 必须以 DB 为准，不能让缓存故障伪装成「未加入任何 Plan」。
pub async fn load_plan_runtimes_for_user(
    pool: &PgPool,
    user_id: i64,
) -> Result<HashMap<i64, Vec<PlanRuntime>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, RuntimeRow>(&format!(
        "SELECT user_id, plan_id, plan_name, group_name, token_limit, period_type, \
                period_hours, period_anchor_mode, member_since, overage_action, \
                downgrade_model, alert_channels, webhook_url, priority, \
                active_start, active_end, model_scope \
         FROM ({RUNTIME_SOURCE_SQL}) r WHERE r.user_id = $1"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows_into_runtimes(rows))
}

/// 运行时行 → 内存映射（model_scope 解析失败跳过并 error 留痕，fail-closed 同上）
fn rows_into_runtimes(rows: Vec<RuntimeRow>) -> HashMap<i64, Vec<PlanRuntime>> {
    let mut map: HashMap<i64, Vec<PlanRuntime>> = HashMap::new();
    for r in rows {
        // 模型作用域解析失败 = 存量配置损坏（手改 DB / 旧版本写入）：
        // 跳过该 Plan 并 error 留痕——宁可可修正地不限额，不可静默错误限额
        let model_scope = match parse_model_scope(r.model_scope.as_ref()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    plan_id = r.plan_id,
                    plan = %r.plan_name,
                    error = %e,
                    "invalid coding plan model_scope; plan excluded from runtime until fixed"
                );
                continue;
            }
        };
        let chan: Vec<String> = serde_json::from_value(r.alert_channels.clone())
            .ok()
            .unwrap_or_else(|| vec!["in_site".into()]);
        map.entry(r.user_id).or_default().push(PlanRuntime {
            plan_id: r.plan_id,
            plan_name: r.plan_name,
            group_name: r.group_name,
            token_limit: r.token_limit,
            period_type: r.period_type,
            period_hours: r.period_hours,
            period_anchor_mode: r.period_anchor_mode,
            member_since: r.member_since,
            overage_action: r.overage_action,
            downgrade_model: r.downgrade_model,
            alert_channels: chan,
            webhook_url: r.webhook_url,
            priority: r.priority,
            active_start: r.active_start,
            active_end: r.active_end,
            model_scope,
        });
    }
    map
}

/// 生效时段窗口判定（[`PlanRuntime::active_at`] 与用户归属归因共用）：
/// 双 None = 全天；[start, end) 半开区间；start > end 视为跨零点隔夜窗
pub fn is_within_active_window(
    start: Option<NaiveTime>,
    end: Option<NaiveTime>,
    local_now: NaiveTime,
) -> bool {
    match (start, end) {
        (Some(s), Some(e)) if s < e => local_now >= s && local_now < e,
        (Some(s), Some(e)) if s > e => local_now >= s || local_now < e, // 跨零点
        _ => true,
    }
}

/// 用户在双通道下的原始 Plan 归属（不去重启用态、不去重生效时段）。
/// 与 [`load_plan_runtimes`] 的区别：不过滤 enabled——用于「已加入但当前不生效」
/// 的诚实呈现（停用/时段窗外时用户不该被误报成「未加入任何 Plan」）。
#[derive(Debug, FromRow, serde::Serialize)]
pub struct UserPlanMembership {
    pub plan_id: i64,
    pub plan_name: String,
    pub enabled: bool,
    pub active_start: Option<NaiveTime>,
    pub active_end: Option<NaiveTime>,
    pub model_scope: Option<Value>,
}

pub async fn list_user_plan_memberships(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<UserPlanMembership>, sqlx::Error> {
    sqlx::query_as::<_, UserPlanMembership>(
        "SELECT p.id AS plan_id, p.name AS plan_name, p.enabled, \
                p.active_start, p.active_end, p.model_scope \
         FROM coding_plans p \
         WHERE p.id IN ( \
           SELECT pu.plan_id FROM plan_users pu WHERE pu.user_id = $1 \
           UNION \
           SELECT pg.plan_id FROM plan_groups pg \
             JOIN user_group_members m ON m.group_id = pg.group_id WHERE m.user_id = $1) \
         ORDER BY p.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

// ---------- CRUD ----------

pub async fn list_plan_summaries(pool: &PgPool) -> Result<Vec<PlanSummary>, sqlx::Error> {
    sqlx::query_as::<_, PlanSummary>(
        "SELECT p.id, p.name, p.description, p.priority, p.token_limit, p.period_type, \
                p.period_hours, p.period_anchor_mode, \
                p.overage_action, p.downgrade_model, p.alert_channels, p.webhook_url, \
                p.enabled, p.active_start, p.active_end, p.model_scope, p.updated_at, \
                COALESCE(u.used_tokens, 0)::bigint AS used_tokens, \
                COALESCE(u.active_users, 0)::bigint AS active_users, \
                (SELECT COUNT(*) FROM plan_groups pg WHERE pg.plan_id = p.id) AS group_count, \
                (SELECT COUNT(*) FROM ( \
                   SELECT m.user_id FROM user_group_members m \
                     JOIN plan_groups pg ON pg.group_id = m.group_id AND pg.plan_id = p.id \
                   UNION \
                   SELECT pu.user_id FROM plan_users pu WHERE pu.plan_id = p.id \
                 ) mm) AS member_count \
         FROM coding_plans p \
         LEFT JOIN ( \
             SELECT pc.plan_id, SUM(pc.tokens) AS used_tokens, COUNT(DISTINCT pc.user_id) AS active_users \
             FROM plan_usage_counters pc \
             JOIN coding_plans p2 ON p2.id = pc.plan_id \
             WHERE (p2.period_type = 'total') \
                OR (p2.period_type = 'daily' AND pc.period_start = \
                    date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') \
                OR (p2.period_type = 'monthly' AND pc.period_start = \
                    date_trunc('month', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') \
                OR (p2.period_type = 'hourly' \
                    AND pc.period_start > now() - make_interval(hours => p2.period_hours) \
                    AND pc.period_start <= now()) \
             GROUP BY pc.plan_id \
         ) u ON u.plan_id = p.id \
         ORDER BY p.priority DESC, p.id",
    )
    .fetch_all(pool)
    .await
}

pub async fn find_plan(pool: &PgPool, id: i64) -> Result<Option<CodingPlan>, sqlx::Error> {
    sqlx::query_as::<_, CodingPlan>(
        "SELECT id, name, description, priority, token_limit, period_type, period_hours, \
                period_anchor_mode, overage_action, downgrade_model, alert_channels, \
                webhook_url, enabled, active_start, active_end, model_scope, \
                created_at, updated_at \
         FROM coding_plans WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn create_plan(
    pool: &PgPool,
    name: &str,
    description: &str,
    priority: i32,
    token_limit: i64,
    period_type: &str,
    period_hours: i32,
    period_anchor_mode: &str,
    overage_action: &str,
    downgrade_model: Option<&str>,
    alert_channels: &Value,
    webhook_url: &str,
    active_start: Option<NaiveTime>,
    active_end: Option<NaiveTime>,
    model_scope: Option<&ModelScope>,
    enabled: bool,
) -> Result<CodingPlan, sqlx::Error> {
    let scope_json = model_scope.map(|s| serde_json::to_value(s).unwrap_or(Value::Null));
    sqlx::query_as::<_, CodingPlan>(
        "INSERT INTO coding_plans \
           (name, description, priority, token_limit, period_type, period_hours, \
            period_anchor_mode, overage_action, downgrade_model, alert_channels, \
            webhook_url, enabled, active_start, active_end, model_scope) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) \
         RETURNING id, name, description, priority, token_limit, period_type, period_hours, \
                   period_anchor_mode, overage_action, downgrade_model, alert_channels, \
                   webhook_url, enabled, active_start, active_end, model_scope, \
                   created_at, updated_at",
    )
    .bind(name)
    .bind(description)
    .bind(priority)
    .bind(token_limit)
    .bind(period_type)
    .bind(period_hours)
    .bind(period_anchor_mode)
    .bind(overage_action)
    .bind(downgrade_model)
    .bind(alert_channels)
    .bind(webhook_url)
    .bind(enabled)
    .bind(active_start)
    .bind(active_end)
    .bind(scope_json.as_ref())
    .fetch_one(pool)
    .await
}

/// 部分更新（字段未提供 = 保留现值）。
/// - downgrade_model 三态：absent=保留 / Some(None)=清空 / Some(Some(v))=设置
/// - active_window 三态（成对原子）：absent=保留 / Some(None)=全天 / Some(Some((s,e)))=设置
/// - model_scope 三态：absent=保留 / Some(None)=清空（全模型生效）/ Some(Some(v))=设置
#[allow(clippy::too_many_arguments)]
pub async fn update_plan(
    pool: &PgPool,
    id: i64,
    name: Option<&str>,
    description: Option<&str>,
    priority: Option<i32>,
    token_limit: Option<i64>,
    period_type: Option<&str>,
    period_hours: Option<i32>,
    period_anchor_mode: Option<&str>,
    overage_action: Option<&str>,
    downgrade_model: Option<Option<&str>>,
    alert_channels: Option<&Value>,
    webhook_url: Option<&str>,
    active_window: Option<Option<(NaiveTime, NaiveTime)>>,
    model_scope: Option<Option<ModelScope>>,
    enabled: Option<bool>,
) -> Result<Option<CodingPlan>, sqlx::Error> {
    // 三态展开：present 标志区分「未提供」（保留现值）与「提供 NULL」（清空）——
    // 直接绑 Option<Option<T>> 会被编码成同一 NULL，无法表达保留语义
    let (dm_present, dm_value) = (downgrade_model.is_some(), downgrade_model.and_then(|o| o));
    let (win_present, win_start, win_end) = match active_window {
        None => (false, None, None),
        Some(None) => (true, None, None),
        Some(Some((s, e))) => (true, Some(s), Some(e)),
    };
    let (scope_present, scope_json) = match &model_scope {
        None => (false, None),
        Some(None) => (true, None),
        Some(Some(s)) => (true, Some(serde_json::to_value(s).unwrap_or(Value::Null))),
    };
    sqlx::query_as::<_, CodingPlan>(
        "UPDATE coding_plans SET \
           name = COALESCE($2, name), \
           description = COALESCE($3, description), \
           priority = COALESCE($4, priority), \
           token_limit = COALESCE($5, token_limit), \
           period_type = COALESCE($6, period_type), \
           period_hours = COALESCE($7, period_hours), \
           period_anchor_mode = COALESCE($8, period_anchor_mode), \
           overage_action = COALESCE($9, overage_action), \
           downgrade_model = CASE WHEN $10 THEN $11 ELSE downgrade_model END, \
           alert_channels = COALESCE($12, alert_channels), \
           webhook_url = COALESCE($13, webhook_url), \
           enabled = COALESCE($14, enabled), \
           active_start = CASE WHEN $15 THEN $16 ELSE active_start END, \
           active_end = CASE WHEN $15 THEN $17 ELSE active_end END, \
           model_scope = CASE WHEN $18 THEN $19 ELSE model_scope END, \
           updated_at = now() \
         WHERE id = $1 \
         RETURNING id, name, description, priority, token_limit, period_type, period_hours, \
                   period_anchor_mode, overage_action, downgrade_model, alert_channels, \
                   webhook_url, enabled, active_start, active_end, model_scope, \
                   created_at, updated_at",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(priority)
    .bind(token_limit)
    .bind(period_type)
    .bind(period_hours)
    .bind(period_anchor_mode)
    .bind(overage_action)
    .bind(dm_present)
    .bind(dm_value)
    .bind(alert_channels)
    .bind(webhook_url)
    .bind(enabled)
    .bind(win_present)
    .bind(win_start)
    .bind(win_end)
    .bind(scope_present)
    .bind(scope_json.as_ref())
    .fetch_optional(pool)
    .await
}

/// 删除影响面：被解绑的分组数与直连用户数（plan_groups/plan_users 随 FK CASCADE 清除；
/// 计数器/告警行无 FK，历史用量与告警保留可回溯）
#[derive(Debug, serde::Serialize)]
pub struct PlanDeleteImpact {
    pub groups: i64,
    pub direct_users: i64,
}

pub async fn delete_plan(pool: &PgPool, id: i64) -> Result<Option<PlanDeleteImpact>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM coding_plans WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Ok(None);
    }
    let (groups,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plan_groups WHERE plan_id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let (direct_users,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM plan_users WHERE plan_id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("DELETE FROM coding_plans WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(PlanDeleteImpact {
        groups,
        direct_users,
    }))
}

// ---------- Plan 成员（直连用户 / 加入分组） ----------

#[derive(Debug, FromRow, serde::Serialize)]
pub struct PlanMemberUser {
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub status: i16,
    pub added_at: DateTime<Utc>,
}

/// 直连用户成员分页列表（q 模糊匹配用户名/显示名/邮箱）
pub async fn list_plan_users(
    pool: &PgPool,
    plan_id: i64,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PlanMemberUser>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, PlanMemberUser>(
        "SELECT u.id AS user_id, u.username, u.display_name, u.email, u.status, pu.added_at \
         FROM plan_users pu JOIN users u ON u.id = pu.user_id \
         WHERE pu.plan_id = $1 \
           AND ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%' \
                OR u.display_name ILIKE '%' || $2 || '%' \
                OR u.email ILIKE '%' || $2 || '%') \
         ORDER BY u.id LIMIT $3 OFFSET $4",
    )
    .bind(plan_id)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM plan_users pu JOIN users u ON u.id = pu.user_id \
         WHERE pu.plan_id = $1 \
           AND ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%' \
                OR u.display_name ILIKE '%' || $2 || '%' \
                OR u.email ILIKE '%' || $2 || '%')",
    )
    .bind(plan_id)
    .bind(q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

#[derive(Debug, FromRow, serde::Serialize)]
pub struct PlanMemberGroup {
    pub group_id: i64,
    pub name: String,
    pub description: String,
    pub ldap_sync: bool,
    pub member_count: i64,
    pub added_at: DateTime<Utc>,
}

/// 已加入分组的分页列表（q 模糊匹配组名/描述）
pub async fn list_plan_groups(
    pool: &PgPool,
    plan_id: i64,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PlanMemberGroup>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, PlanMemberGroup>(
        "SELECT g.id AS group_id, g.name, g.description, g.ldap_sync, \
                (SELECT COUNT(*) FROM user_group_members m WHERE m.group_id = g.id) AS member_count, \
                pg.added_at \
         FROM plan_groups pg JOIN user_groups g ON g.id = pg.group_id \
         WHERE pg.plan_id = $1 \
           AND ($2::text IS NULL OR g.name ILIKE '%' || $2 || '%' \
                OR g.description ILIKE '%' || $2 || '%') \
         ORDER BY g.id LIMIT $3 OFFSET $4",
    )
    .bind(plan_id)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM plan_groups pg JOIN user_groups g ON g.id = pg.group_id \
         WHERE pg.plan_id = $1 \
           AND ($2::text IS NULL OR g.name ILIKE '%' || $2 || '%' \
                OR g.description ILIKE '%' || $2 || '%')",
    )
    .bind(plan_id)
    .bind(q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

#[derive(Debug, FromRow, serde::Serialize)]
pub struct PlanUserPick {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub source: String,
    pub status: i16,
    pub is_member: bool,
}

/// 用户选择器：全用户分页检索 + Plan 直连标记（单查询渲染勾选态）
pub async fn search_users_for_plan(
    pool: &PgPool,
    plan_id: i64,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PlanUserPick>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, PlanUserPick>(
        "SELECT u.id, u.username, u.display_name, u.email, u.source, u.status, \
                (pu.user_id IS NOT NULL) AS is_member \
         FROM users u \
         LEFT JOIN plan_users pu ON pu.plan_id = $1 AND pu.user_id = u.id \
         WHERE ($2::text IS NULL OR u.username ILIKE '%' || $2 || '%' \
                OR u.display_name ILIKE '%' || $2 || '%' \
                OR u.email ILIKE '%' || $2 || '%') \
         ORDER BY u.id LIMIT $3 OFFSET $4",
    )
    .bind(plan_id)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM users u \
         WHERE ($1::text IS NULL OR u.username ILIKE '%' || $1 || '%' \
                OR u.display_name ILIKE '%' || $1 || '%' \
                OR u.email ILIKE '%' || $1 || '%')",
    )
    .bind(q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

#[derive(Debug, FromRow, serde::Serialize)]
pub struct PlanGroupPick {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub ldap_sync: bool,
    pub member_count: i64,
    pub is_member: bool,
}

/// 分组选择器：全分组分页检索 + Plan 加入标记（单查询渲染勾选态）
pub async fn search_groups_for_plan(
    pool: &PgPool,
    plan_id: i64,
    q: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PlanGroupPick>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, PlanGroupPick>(
        "SELECT g.id, g.name, g.description, g.ldap_sync, \
                (SELECT COUNT(*) FROM user_group_members m WHERE m.group_id = g.id) AS member_count, \
                (pg.group_id IS NOT NULL) AS is_member \
         FROM user_groups g \
         LEFT JOIN plan_groups pg ON pg.plan_id = $1 AND pg.group_id = g.id \
         WHERE ($2::text IS NULL OR g.name ILIKE '%' || $2 || '%' \
                OR g.description ILIKE '%' || $2 || '%') \
         ORDER BY g.id LIMIT $3 OFFSET $4",
    )
    .bind(plan_id)
    .bind(q)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM user_groups g \
         WHERE ($1::text IS NULL OR g.name ILIKE '%' || $1 || '%' \
                OR g.description ILIKE '%' || $1 || '%')",
    )
    .bind(q)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

/// 批量添加直连用户（幂等守卫 ON CONFLICT）。调用方先做重复预检以给出精确 409。
/// 事务内执行：与重复预检同事务，冲突窗口内后到者新增 0 行 → 上层回滚并报冲突。
pub async fn add_plan_users(
    conn: &mut sqlx::PgConnection,
    plan_id: i64,
    user_ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if user_ids.is_empty() {
        return Ok(0);
    }
    Ok(sqlx::query(
        "INSERT INTO plan_users (plan_id, user_id) \
         SELECT $1, x FROM unnest($2::bigint[]) AS x \
         ON CONFLICT (plan_id, user_id) DO NOTHING",
    )
    .bind(plan_id)
    .bind(user_ids)
    .execute(conn)
    .await?
    .rows_affected())
}

/// 批量加入分组（幂等守卫 ON CONFLICT）；事务语义同 [`add_plan_users`]。
pub async fn add_plan_groups(
    conn: &mut sqlx::PgConnection,
    plan_id: i64,
    group_ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if group_ids.is_empty() {
        return Ok(0);
    }
    Ok(sqlx::query(
        "INSERT INTO plan_groups (plan_id, group_id) \
         SELECT $1, x FROM unnest($2::bigint[]) AS x \
         ON CONFLICT (plan_id, group_id) DO NOTHING",
    )
    .bind(plan_id)
    .bind(group_ids)
    .execute(conn)
    .await?
    .rows_affected())
}

pub async fn remove_plan_users(
    pool: &PgPool,
    plan_id: i64,
    user_ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if user_ids.is_empty() {
        return Ok(0);
    }
    Ok(
        sqlx::query("DELETE FROM plan_users WHERE plan_id = $1 AND user_id = ANY($2::bigint[])")
            .bind(plan_id)
            .bind(user_ids)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

pub async fn remove_plan_groups(
    pool: &PgPool,
    plan_id: i64,
    group_ids: &[i64],
) -> Result<u64, sqlx::Error> {
    if group_ids.is_empty() {
        return Ok(0);
    }
    Ok(
        sqlx::query("DELETE FROM plan_groups WHERE plan_id = $1 AND group_id = ANY($2::bigint[])")
            .bind(plan_id)
            .bind(group_ids)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

/// 参数校验：请求 id 中不存在于 users 的部分（保持入参顺序）
pub async fn missing_user_ids(pool: &PgPool, ids: &[i64]) -> Result<Vec<i64>, sqlx::Error> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT id FROM users WHERE id = ANY($1::bigint[])")
        .bind(ids)
        .fetch_all(pool)
        .await?;
    let found: HashSet<i64> = rows.into_iter().map(|(id,)| id).collect();
    Ok(ids
        .iter()
        .copied()
        .filter(|id| !found.contains(id))
        .collect())
}

/// 参数校验：请求 id 中不存在于 user_groups 的部分（保持入参顺序）
pub async fn missing_group_ids(pool: &PgPool, ids: &[i64]) -> Result<Vec<i64>, sqlx::Error> {
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM user_groups WHERE id = ANY($1::bigint[])")
            .bind(ids)
            .fetch_all(pool)
            .await?;
    let found: HashSet<i64> = rows.into_iter().map(|(id,)| id).collect();
    Ok(ids
        .iter()
        .copied()
        .filter(|id| !found.contains(id))
        .collect())
}

/// 重复预检：这些用户中已直连该 Plan 的 id
pub async fn existing_plan_user_ids(
    conn: &mut sqlx::PgConnection,
    plan_id: i64,
    ids: &[i64],
) -> Result<Vec<i64>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT user_id FROM plan_users WHERE plan_id = $1 AND user_id = ANY($2::bigint[])",
    )
    .bind(plan_id)
    .bind(ids)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// 重复预检：这些分组中已加入该 Plan 的 id
pub async fn existing_plan_group_ids(
    conn: &mut sqlx::PgConnection,
    plan_id: i64,
    ids: &[i64],
) -> Result<Vec<i64>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT group_id FROM plan_groups WHERE plan_id = $1 AND group_id = ANY($2::bigint[])",
    )
    .bind(plan_id)
    .bind(ids)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// 冲突提示用：id → username（保持入参顺序）
pub async fn user_names(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, username FROM users WHERE id = ANY($1::bigint[])")
            .bind(ids)
            .fetch_all(pool)
            .await?;
    let by_id: HashMap<i64, String> = rows.into_iter().collect();
    Ok(ids
        .iter()
        .filter_map(|id| by_id.get(id).map(|n| (*id, n.clone())))
        .collect())
}

/// 冲突提示用：id → group name（保持入参顺序）
pub async fn group_names(pool: &PgPool, ids: &[i64]) -> Result<Vec<(i64, String)>, sqlx::Error> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM user_groups WHERE id = ANY($1::bigint[])")
            .bind(ids)
            .fetch_all(pool)
            .await?;
    let by_id: HashMap<i64, String> = rows.into_iter().collect();
    Ok(ids
        .iter()
        .filter_map(|id| by_id.get(id).map(|n| (*id, n.clone())))
        .collect())
}

/// 用量缓存引导行：(user, plan, period_key, 当期已用)
#[derive(FromRow)]
pub struct PlanUsageSnapshot {
    pub user_id: i64,
    pub plan_id: i64,
    pub period_key: String,
    pub used: i64,
}

/// 各启用 Plan 的当期用量快照（缓存重载；只读不清理，与月度缓存同语义）。
/// 日期/月份界显式按 UTC 计算（CURRENT_DATE/date_trunc 裸用会随会话时区漂移）；
/// hourly 以「(now - period_hours, now]」开区间窗命中当前桶（桶起点 ≤ now 恒成立，
/// 上一桶起点 ≤ now - period_hours 被严格大于排除）。
pub async fn load_usage_snapshots(pool: &PgPool) -> Result<Vec<PlanUsageSnapshot>, sqlx::Error> {
    sqlx::query_as::<_, PlanUsageSnapshot>(
        "SELECT pc.user_id, pc.plan_id, \
                CASE p.period_type \
                  WHEN 'daily' THEN 'd:' || to_char(pc.period_start AT TIME ZONE 'UTC', 'YYYY-MM-DD') \
                  WHEN 'monthly' THEN 'm:' || to_char(pc.period_start AT TIME ZONE 'UTC', 'YYYY-MM') \
                  WHEN 'hourly' THEN 'h:' || (extract(epoch from pc.period_start)::bigint)::text \
                  ELSE 't' END AS period_key, \
                SUM(pc.tokens)::bigint AS used \
         FROM plan_usage_counters pc \
         JOIN coding_plans p ON p.id = pc.plan_id AND p.enabled = TRUE \
         WHERE (p.period_type = 'total') \
            OR (p.period_type = 'daily' AND pc.period_start = \
                date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') \
            OR (p.period_type = 'monthly' AND pc.period_start = \
                date_trunc('month', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC') \
            OR (p.period_type = 'hourly' \
                AND pc.period_start > now() - make_interval(hours => p.period_hours) \
                AND pc.period_start <= now()) \
         GROUP BY pc.user_id, pc.plan_id, p.period_type, pc.period_start",
    )
    .fetch_all(pool)
    .await
}

// ---------- 用量查询（看板/回溯） ----------

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct DailyUsage {
    pub stat_date: NaiveDate,
    pub call_count: i64,
    pub tokens: i64,
}

/// Plan 成员逐日用量趋势（usage_daily 按成员聚合；历史回溯）
pub async fn plan_daily_trend(
    pool: &PgPool,
    plan_id: i64,
    days: i32,
) -> Result<Vec<DailyUsage>, sqlx::Error> {
    sqlx::query_as::<_, DailyUsage>(
        "SELECT d.stat_date, SUM(d.call_count)::bigint AS call_count, \
                SUM(d.input_tokens + d.output_tokens)::bigint AS tokens \
         FROM usage_daily d \
         WHERE d.user_id IN ( \
             SELECT m.user_id FROM user_group_members m \
             JOIN plan_groups pg ON pg.group_id = m.group_id WHERE pg.plan_id = $1 \
             UNION \
             SELECT pu.user_id FROM plan_users pu WHERE pu.plan_id = $1) \
           AND d.stat_date >= CURRENT_DATE - $2::int \
         GROUP BY d.stat_date ORDER BY d.stat_date",
    )
    .bind(plan_id)
    .bind(days)
    .fetch_all(pool)
    .await
}

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct PeriodUsage {
    pub period_start: DateTime<Utc>,
    pub tokens: i64,
    pub users: i64,
}

/// Plan 历史周期用量（计数器即历史快照：hourly 逐桶、daily 逐日、monthly 逐月、total 单行；
/// period_start 为窗口起点时刻 RFC3339 序列化）
pub async fn plan_period_history(
    pool: &PgPool,
    plan_id: i64,
    limit: i64,
) -> Result<Vec<PeriodUsage>, sqlx::Error> {
    sqlx::query_as::<_, PeriodUsage>(
        "SELECT period_start, SUM(tokens)::bigint AS tokens, COUNT(DISTINCT user_id)::bigint AS users \
         FROM plan_usage_counters WHERE plan_id = $1 \
         GROUP BY period_start ORDER BY period_start DESC LIMIT $2",
    )
    .bind(plan_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct PlanUserUsage {
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub call_count: i64,
    pub tokens: i64,
}

/// 时间范围内 Plan 成员按用户用量（回溯查询；分页）
pub async fn plan_user_usage(
    pool: &PgPool,
    plan_id: i64,
    from: NaiveDate,
    to: NaiveDate,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PlanUserUsage>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, PlanUserUsage>(
        "SELECT u.id AS user_id, u.username, u.display_name, \
                COALESCE(SUM(d.call_count), 0)::bigint AS call_count, \
                COALESCE(SUM(d.input_tokens + d.output_tokens), 0)::bigint AS tokens \
         FROM users u \
         JOIN usage_daily d ON d.user_id = u.id AND d.stat_date BETWEEN $2 AND $3 \
         WHERE u.id IN ( \
             SELECT m.user_id FROM user_group_members m \
             JOIN plan_groups pg ON pg.group_id = m.group_id WHERE pg.plan_id = $1 \
             UNION \
             SELECT pu.user_id FROM plan_users pu WHERE pu.plan_id = $1) \
         GROUP BY u.id, u.username, u.display_name \
         ORDER BY tokens DESC \
         LIMIT $4 OFFSET $5",
    )
    .bind(plan_id)
    .bind(from)
    .bind(to)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT d.user_id)::bigint \
         FROM usage_daily d \
         WHERE d.stat_date BETWEEN $2 AND $3 \
           AND d.user_id IN ( \
             SELECT m.user_id FROM user_group_members m \
             JOIN plan_groups pg ON pg.group_id = m.group_id WHERE pg.plan_id = $1 \
             UNION \
             SELECT pu.user_id FROM plan_users pu WHERE pu.plan_id = $1)",
    )
    .bind(plan_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    Ok((rows, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_units() {
        assert_eq!(parse_token_limit("500M").unwrap(), 500_000_000);
        assert_eq!(parse_token_limit("1.5G").unwrap(), 1_500_000_000);
        assert_eq!(parse_token_limit("2T").unwrap(), 2_000_000_000_000);
        assert_eq!(parse_token_limit("1024k").unwrap(), 1_024_000);
        assert_eq!(parse_token_limit("1234567").unwrap(), 1_234_567);
        assert_eq!(parse_token_limit(" 1.5g ").unwrap(), 1_500_000_000);
        assert!(parse_token_limit("1X").is_err());
        assert!(parse_token_limit("G").is_err());
        assert!(parse_token_limit("-5M").is_err());
    }

    #[test]
    fn formats_limits() {
        assert_eq!(format_token_limit(500_000_000), "500M");
        assert_eq!(format_token_limit(1_500_000_000), "1.5G");
        assert_eq!(format_token_limit(2_000_000_000_000), "2T");
        assert_eq!(format_token_limit(1_500_000), "1.5M");
        assert_eq!(format_token_limit(999), "999");
    }

    fn rt(pt: &str) -> PlanRuntime {
        rt_cfg(
            pt,
            1,
            ANCHOR_FIXED,
            Utc.timestamp_opt(0, 0).single().unwrap(),
        )
    }

    fn rt_cfg(pt: &str, hours: i32, anchor: &str, member_since: DateTime<Utc>) -> PlanRuntime {
        PlanRuntime {
            plan_id: 1,
            plan_name: "p".into(),
            group_name: "g".into(),
            token_limit: 1,
            period_type: pt.into(),
            period_hours: hours,
            period_anchor_mode: anchor.into(),
            member_since,
            overage_action: OVERAGE_BLOCK.into(),
            downgrade_model: None,
            alert_channels: vec![],
            webhook_url: String::new(),
            priority: 0,
            active_start: None,
            active_end: None,
            model_scope: None,
        }
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    /// 全天候选、无模型上下文的解析（DB 测试断言用；12:00 落在任何非跨零点窗内则视窗而定）
    fn resolve(map: &HashMap<i64, Vec<PlanRuntime>>, uid: i64) -> Option<PlanRuntime> {
        resolve_plan(map, uid, NaiveTime::from_hms_opt(12, 0, 0).unwrap(), None)
    }

    #[test]
    fn active_window_semantics() {
        let t = |h: u32, m: u32| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        let mut p = rt("daily");
        assert!(p.active_at(t(9, 0))); // 双 None = 全天
        p.active_start = Some(t(9, 0));
        p.active_end = Some(t(18, 0));
        assert!(p.active_at(t(9, 0))); // [start, end) 半开
        assert!(p.active_at(t(17, 59)));
        assert!(!p.active_at(t(18, 0)));
        assert!(!p.active_at(t(8, 59)));
        p.active_start = Some(t(22, 0));
        p.active_end = Some(t(6, 0));
        assert!(p.active_at(t(22, 0))); // 跨零点 22:00-06:00
        assert!(p.active_at(t(5, 59)));
        assert!(!p.active_at(t(6, 0)));
        assert!(!p.active_at(t(12, 0)));
    }

    #[test]
    fn resolve_plan_prefers_active_by_priority() {
        let t = |h: u32| NaiveTime::from_hms_opt(h, 0, 0).unwrap();
        // 高优先级仅在 9-18 生效；低优先级全天；第三候选仅在深夜
        let mut hi = rt("daily");
        hi.plan_id = 2;
        hi.priority = 10;
        hi.active_start = Some(t(9));
        hi.active_end = Some(t(18));
        let mut lo = rt("daily");
        lo.plan_id = 1;
        lo.priority = 1;
        let mut night = rt("daily");
        night.plan_id = 3;
        night.priority = 5;
        night.active_start = Some(t(23));
        night.active_end = Some(t(4));
        let map = [(7i64, vec![night.clone(), lo, hi.clone()])].into();
        assert_eq!(resolve_plan(&map, 7, t(12), None).unwrap().plan_id, 2); // 时段内取高优
        assert_eq!(resolve_plan(&map, 7, t(20), None).unwrap().plan_id, 1); // 时段外回退全天
        assert_eq!(resolve_plan(&map, 7, t(23), None).unwrap().plan_id, 3); // 23:00 hi 窗外 → night(5) 胜 lo(1)
        hi.active_start = Some(t(22));
        let map2 = [(7i64, vec![night.clone(), hi])].into();
        assert_eq!(resolve_plan(&map2, 7, t(23), None).unwrap().plan_id, 2); // 同分取 plan_id 大
        let map3 = [(7i64, vec![night])].into();
        assert!(resolve_plan(&map3, 7, t(12), None).is_none()); // 全部不在时段 → None
    }

    #[test]
    fn parses_active_time() {
        assert_eq!(
            parse_active_time("09:00").unwrap(),
            NaiveTime::from_hms_opt(9, 0, 0).unwrap()
        );
        assert_eq!(
            parse_active_time(" 18:30:00 ").unwrap(),
            NaiveTime::from_hms_opt(18, 30, 0).unwrap()
        );
        assert!(parse_active_time("24:00").is_err());
        assert!(parse_active_time("9点").is_err());
        let t = |h: u32, m: u32| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        let t9 = t(9, 0);
        assert!(validate_active_window(Some(t9), None).is_err());
        assert!(validate_active_window(Some(t9), Some(t9)).is_err());
        assert!(validate_active_window(None, None).is_ok());
        assert!(validate_active_window(Some(t9), Some(t(18, 0))).is_ok());
    }

    #[test]
    fn period_keys() {
        let now = utc(2026, 9, 2, 10, 30, 0);
        assert_eq!(rt("total").period_key(now), "t");
        assert_eq!(rt("daily").period_key(now), "d:2026-09-02");
        assert_eq!(rt("monthly").period_key(now), "m:2026-09");
        assert!(rt("hourly").period_key(now).starts_with("h:"));
    }

    #[test]
    fn legacy_period_starts() {
        let now = utc(2026, 9, 2, 10, 30, 5);
        assert_eq!(rt("daily").period_start(now), utc(2026, 9, 2, 0, 0, 0));
        assert_eq!(rt("monthly").period_start(now), utc(2026, 9, 1, 0, 0, 0));
        assert_eq!(
            rt("total").period_start(now),
            Utc.timestamp_opt(0, 0).single().unwrap()
        );
    }
    #[test]
    fn hourly_fixed_on_the_hour() {
        let p = rt_cfg("hourly", 1, ANCHOR_FIXED, utc(2026, 1, 1, 0, 0, 0));
        // 整点入桶；桶内任意时刻同桶起点
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 10, 0, 0)),
            utc(2026, 9, 2, 10, 0, 0)
        );
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 10, 59, 59)),
            utc(2026, 9, 2, 10, 0, 0)
        );
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 9, 59, 59)),
            utc(2026, 9, 2, 9, 0, 0)
        );
    }

    #[test]
    fn hourly_fixed_multi_hour_and_day_rollover() {
        let p = rt_cfg("hourly", 5, ANCHOR_FIXED, utc(2026, 1, 1, 0, 0, 0));
        // epoch 锚点 = 纯 UTC 网格：N 整除 24 时才与当日 00:00 对齐；
        // 2026-09-02 的 5h 网格相位为 03:00（86400 % 18000 = 14400s，跨日相位漂移）
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 4, 59, 59)),
            utc(2026, 9, 2, 3, 0, 0)
        );
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 8, 0, 0)),
            utc(2026, 9, 2, 8, 0, 0)
        );
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 7, 59, 59)),
            utc(2026, 9, 2, 3, 0, 0)
        );
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 2, 0, 0)),
            utc(2026, 9, 1, 22, 0, 0)
        );
        // 跨日网格连续：9/2 02:00 落在 [9/1 22:00, 9/2 03:00) 桶
        let t = utc(2026, 9, 2, 2, 0, 0);
        let s = p.period_start(t);
        assert!(s <= t && t < s + chrono::Duration::hours(5));
        assert_eq!(s.timestamp() % (5 * 3600), 0);
        let p7 = rt_cfg("hourly", 7, ANCHOR_FIXED, utc(2026, 1, 1, 0, 0, 0));
        // 7h 桶不整除 24h：桶起点全部落在 epoch 起的 7h 网格上
        let got = p7.period_start(utc(2026, 9, 2, 2, 0, 0));
        assert_eq!(got.timestamp() % (7 * 3600), 0);
        assert_eq!(
            p7.period_start(utc(2026, 9, 2, 2, 0, 0)),
            utc(2026, 9, 1, 20, 0, 0)
        );
        assert!(got + chrono::Duration::hours(7) > utc(2026, 9, 2, 2, 0, 0));
    }

    #[test]
    fn hourly_join_anchor_offset() {
        // 开通时间 14:37:05 → 桶起点 14:37:05 / 16:37:05 / ...
        let joined = utc(2026, 9, 1, 14, 37, 5);
        let p = rt_cfg("hourly", 2, ANCHOR_JOIN, joined);
        assert_eq!(p.period_start(utc(2026, 9, 1, 15, 0, 0)), joined);
        assert_eq!(p.period_start(utc(2026, 9, 1, 16, 37, 4)), joined);
        assert_eq!(
            p.period_start(utc(2026, 9, 1, 16, 37, 5)),
            utc(2026, 9, 1, 16, 37, 5)
        );
        assert_eq!(
            p.period_start(utc(2026, 9, 2, 8, 0, 0)),
            utc(2026, 9, 2, 6, 37, 5)
        );
    }

    #[test]
    fn hourly_future_anchor_clamps() {
        // 时钟早于锚点（回拨/预开通）：钳到锚点本身，不产生负槽位
        let joined = utc(2026, 9, 1, 14, 37, 5);
        let p = rt_cfg("hourly", 3, ANCHOR_JOIN, joined);
        assert_eq!(p.period_start(utc(2026, 9, 1, 10, 0, 0)), joined);
        assert_eq!(
            p.period_key(utc(2026, 9, 1, 10, 0, 0)),
            format!("h:{}", joined.timestamp())
        );
    }

    #[test]
    fn hourly_key_is_bucket_epoch() {
        let p = rt_cfg("hourly", 1, ANCHOR_FIXED, utc(2026, 1, 1, 0, 0, 0));
        let now = utc(2026, 9, 2, 10, 30, 0);
        let start = p.period_start(now);
        assert_eq!(p.period_key(now), format!("h:{}", start.timestamp()));
        // 相邻桶键不同（告警去重随周期翻转自动重挂）
        assert_ne!(
            p.period_key(start),
            p.period_key(start + chrono::Duration::hours(1))
        );
    }

    #[test]
    fn validates_period_config() {
        assert!(validate_period_config("hourly", 1, "fixed").is_ok());
        assert!(validate_period_config("hourly", 168, "join").is_ok());
        assert!(validate_period_config("hourly", 0, "fixed").is_err());
        assert!(validate_period_config("hourly", 169, "fixed").is_err());
        assert!(validate_period_config("monthly", 0, "fixed").is_ok());
        assert!(validate_period_config("daily", 1, "bogus").is_err());
        assert!(validate_period_config("weekly", 1, "fixed").is_err());
    }

    #[test]
    fn db_bucket_window_parity() {
        // 模拟 load_usage_snapshots 的 (now - N·h, now] 谓词：当前桶必命中，上一桶必排除
        let p = rt_cfg("hourly", 2, ANCHOR_JOIN, utc(2026, 9, 1, 14, 0, 0));
        let now = utc(2026, 9, 1, 17, 30, 0);
        let cur = p.period_start(now);
        let prev = cur - chrono::Duration::hours(2);
        let in_window = |t: DateTime<Utc>| t > now - chrono::Duration::hours(2) && t <= now;
        assert!(in_window(cur));
        assert!(!in_window(prev));
    }

    // ---------- 模型作用域（model_scope） ----------

    fn scope(allow: &[&str], deny: &[&str]) -> ModelScope {
        ModelScope {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn model_pattern_matching_across_vendors() {
        // 精确匹配：三家厂商命名规范（Anthropic / OpenAI / Google）
        assert!(model_matches_pattern(
            "claude-sonnet-4-5",
            "claude-sonnet-4-5"
        ));
        assert!(model_matches_pattern("gpt-4o", "gpt-4o"));
        assert!(model_matches_pattern("gemini-2.5-pro", "gemini-2.5-pro"));
        // 大小写不敏感（客户端大小写不规范时仍命中）
        assert!(model_matches_pattern("GPT-4o", "gpt-4O"));
        // 尾缀 * 前缀匹配覆盖整个模型家族（Anthropic）
        assert!(model_matches_pattern("claude-*", "claude-sonnet-4-5"));
        assert!(model_matches_pattern("claude-*", "claude-opus-4-1"));
        assert!(model_matches_pattern(
            "claude-*",
            "claude-3-5-sonnet-20241022"
        ));
        assert!(!model_matches_pattern("claude-*", "gpt-4o"));
        assert!(!model_matches_pattern("claude-*", "claude")); // 不含分隔符的前缀不命中
        // OpenAI 家族与带日期后缀变体
        assert!(model_matches_pattern("gpt-4*", "gpt-4o-2024-11-20"));
        assert!(model_matches_pattern("o*", "o3-mini"));
        // Google：models/ 前缀方言
        assert!(model_matches_pattern("models/*", "models/gemini-2.5-pro"));
        // DeepSeek（大小写混写）与 OpenRouter（provider/model 斜杠命名）
        assert!(model_matches_pattern("deepseek-chat", "DeepSeek-Chat"));
        assert!(model_matches_pattern(
            "anthropic/*",
            "anthropic/claude-sonnet-4-5"
        ));
        // 非尾缀通配不受支持（与路由规则同语义，写入期拒绝）
        assert!(!model_matches_pattern("*-latest", "gpt-4o-latest"));
    }

    #[test]
    fn validates_model_patterns() {
        assert!(validate_model_pattern("claude-sonnet-4-5").is_ok());
        assert!(validate_model_pattern("claude-*").is_ok());
        assert!(validate_model_pattern("models/gemini-2.5-pro").is_ok());
        assert!(validate_model_pattern("GPT-4O").is_ok()); // 大小写归一化在 parse
        // 拼写事故在写入期明确报错而非静默忽略
        assert!(validate_model_pattern("").is_err());
        assert!(validate_model_pattern("claude sonnet").is_err()); // 空格
        assert!(validate_model_pattern("claude-4·5").is_err()); // 非法字符
        assert!(validate_model_pattern("gpt-*-mini").is_err()); // 中间 *
        assert!(validate_model_pattern("**").is_err());
        assert!(validate_model_pattern("*").is_err()); // 裸 * 等价不配置
        assert!(validate_model_pattern("*-latest").is_err()); // 前缀 *
        let long = "a".repeat(MAX_MODEL_PATTERN_LEN + 1);
        assert!(validate_model_pattern(&long).is_err());
    }

    #[test]
    fn parses_model_scope() {
        assert!(parse_model_scope(None).unwrap().is_none());
        assert!(parse_model_scope(Some(&json!(null))).unwrap().is_none());
        assert!(parse_model_scope(Some(&json!({}))).unwrap().is_none()); // 归一化为 NULL
        assert!(
            parse_model_scope(Some(&json!({"allow": [], "deny": []})))
                .unwrap()
                .is_none()
        );

        let s = parse_model_scope(Some(&json!({"allow": ["Claude-*", "gpt-4o"]})))
            .unwrap()
            .unwrap();
        assert_eq!(s.allow, vec!["claude-*", "gpt-4o"]); // 归一化小写
        assert!(s.deny.is_empty());

        // deny-only：除黑名单外全放行
        let s = parse_model_scope(Some(&json!({"deny": ["gpt-*"]})))
            .unwrap()
            .unwrap();
        assert!(s.allow.is_empty() && s.deny == vec!["gpt-*"]);

        // 拼写/结构错误明确报错
        assert!(parse_model_scope(Some(&json!({"white": ["claude-*"]}))).is_err());
        assert!(parse_model_scope(Some(&json!({"allow": ["gpt-*-mini"]}))).is_err());
        assert!(parse_model_scope(Some(&json!({"allow": [""]}))).is_err());
        assert!(parse_model_scope(Some(&json!({"allow": [42]}))).is_err()); // 非字符串
        assert!(parse_model_scope(Some(&json!("claude-*"))).is_err()); // 非对象
        // 同一条目同时出现在黑白名单 → 永不命中，写入期报错
        assert!(
            parse_model_scope(Some(&json!(
                {"allow": ["gpt-4o", "claude-*"], "deny": ["gpt-4o"]}
            )))
            .is_err()
        );
        // 条数上限（合计 33 条 > 32）
        let many: Vec<String> = (0..=MAX_MODEL_PATTERNS).map(|i| format!("m{i}")).collect();
        assert!(parse_model_scope(Some(&json!({ "allow": many }))).is_err());
    }

    #[test]
    fn model_scope_blacklist_over_whitelist() {
        // 冲突时黑名单优先：命中 allow 的模型被 deny 覆盖
        let s = scope(&["claude-*"], &["claude-2*"]);
        assert!(s.matches("claude-sonnet-4-5"));
        assert!(s.matches("claude-3-5-haiku-latest"));
        assert!(!s.matches("claude-2-opus")); // 黑名单赢
        // 白名单缺省 = 全放行（仅受黑名单约束）
        let s = scope(&[], &["gpt-4o"]);
        assert!(s.matches("claude-sonnet-4-5"));
        assert!(s.matches("gemini-2.5-pro"));
        assert!(!s.matches("gpt-4o"));
        // 精确 deny 不覆盖日期后缀变体——要覆盖变体应写 gpt-4o*（前缀通配）
        assert!(s.matches("gpt-4o-2024-11-20"));
        assert!(!scope(&[], &["gpt-4o*"]).matches("gpt-4o-2024-11-20"));
        // 白名单命中才生效
        let s = scope(&["gpt-4o", "gemini-2.5-pro"], &[]);
        assert!(s.matches("gpt-4o"));
        assert!(s.matches("gemini-2.5-pro"));
        assert!(!s.matches("claude-sonnet-4-5"));
        // 具体度：仅精确=2；含通配=1
        assert_eq!(scope(&["gpt-4o"], &[]).specificity(), 2);
        assert_eq!(scope(&["gpt-*"], &[]).specificity(), 1);
        assert_eq!(scope(&["gpt-4o"], &["o*"]).specificity(), 1);
    }

    #[test]
    fn resolve_plan_model_scope_specificity() {
        let t = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        let mut unscoped = rt("monthly"); // plan_id 1, priority 100（高优但不限模型）
        unscoped.plan_id = 1;
        unscoped.priority = 100;
        let mut wildcard = rt("monthly"); // plan_id 2, claude-*
        wildcard.plan_id = 2;
        wildcard.priority = 1;
        wildcard.model_scope = Some(scope(&["claude-*"], &[]));
        let mut exact = rt("monthly"); // plan_id 3, claude-sonnet-4-5 精确
        exact.plan_id = 3;
        exact.priority = 1;
        exact.model_scope = Some(scope(&["claude-sonnet-4-5"], &[]));
        let map = [(9i64, vec![unscoped, wildcard.clone(), exact])].into();

        // 作用域更具体者优先（specificity 压过 priority）：精确 > 通配 > 不限
        assert_eq!(
            resolve_plan(&map, 9, t, Some("claude-sonnet-4-5"))
                .unwrap()
                .plan_id,
            3
        );
        assert_eq!(
            resolve_plan(&map, 9, t, Some("claude-opus-4-1"))
                .unwrap()
                .plan_id,
            2
        );
        assert_eq!(resolve_plan(&map, 9, t, Some("gpt-4o")).unwrap().plan_id, 1);
        assert_eq!(
            resolve_plan(&map, 9, t, Some("gemini-2.5-pro"))
                .unwrap()
                .plan_id,
            1
        );
        // 无模型上下文（记账后告警/控制台）：忽略作用域，回到 (priority, plan_id)
        assert_eq!(resolve_plan(&map, 9, t, None).unwrap().plan_id, 1);

        // 黑名单把在期高优 plan 排除 → 回退次优
        let mut deny_gpt = rt("monthly");
        deny_gpt.plan_id = 4;
        deny_gpt.priority = 100;
        deny_gpt.model_scope = Some(scope(&[], &["gpt-*"]));
        let mut unscoped_lo = rt("monthly");
        unscoped_lo.plan_id = 6;
        unscoped_lo.priority = 0;
        let map2 = [(9i64, vec![deny_gpt, wildcard.clone(), unscoped_lo])].into();
        assert_eq!(
            resolve_plan(&map2, 9, t, Some("gpt-4o")).unwrap().plan_id,
            6
        );
        // deny-only 对 claude 全放行且具体度 1 > 0 → 胜 unscoped_lo
        assert_eq!(
            resolve_plan(&map2, 9, t, Some("claude-sonnet-4-5"))
                .unwrap()
                .plan_id,
            4
        );

        // 全部作用域都不命中当前模型 → None（该模型下不受限额）
        let mut only_claude = rt("monthly");
        only_claude.plan_id = 5;
        only_claude.model_scope = Some(scope(&["claude-*"], &[]));
        let map3 = [(9i64, vec![only_claude])].into();
        assert!(resolve_plan(&map3, 9, t, Some("gpt-4o")).is_none());
        assert!(resolve_plan(&map3, 9, t, Some("claude-sonnet-4-5")).is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn model_scope_crud_runtime_and_resolution(pool: sqlx::PgPool) {
        let alice = seed_user(&pool, "alice").await;
        let unscoped = seed_plan(&pool, "all", 5, true).await;
        let scoped = seed_plan_scoped(
            &pool,
            "claude-only",
            1,
            &parse_model_scope(Some(&json!({"allow": ["claude-*"], "deny": ["claude-2*"]})))
                .unwrap()
                .unwrap(),
        )
        .await;
        let mut tx = pool.begin().await.unwrap();
        add_plan_users(&mut *tx, unscoped.id, &[alice])
            .await
            .unwrap();
        add_plan_users(&mut *tx, scoped.id, &[alice]).await.unwrap();
        tx.commit().await.unwrap();

        // CRUD 回读：原始行带 model_scope
        let raw = find_plan(&pool, scoped.id).await.unwrap().unwrap();
        assert_eq!(
            raw.model_scope,
            Some(json!({"allow": ["claude-*"], "deny": ["claude-2*"]}))
        );
        let raw_unscoped = find_plan(&pool, unscoped.id).await.unwrap().unwrap();
        assert_eq!(raw_unscoped.model_scope, None);

        // 运行时加载：parse 后的 ModelScope 随 PlanRuntime；解析按模型作用域择优
        let rt = load_plan_runtimes(&pool).await.unwrap();
        let t = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        // claude-sonnet-4-5：scoped（specificity 2 > 0）压过 unscoped 的 priority 5
        let hit = resolve_plan(&rt, alice, t, Some("claude-sonnet-4-5")).unwrap();
        assert_eq!(hit.plan_id, scoped.id);
        assert_eq!(hit.model_scope.as_ref().unwrap().allow, vec!["claude-*"]);
        // gpt-4o：scoped 白名单不命中 → unscoped
        assert_eq!(
            resolve_plan(&rt, alice, t, Some("gpt-4o")).unwrap().plan_id,
            unscoped.id
        );
        // claude-2-opus：黑名单命中 → scoped 排除 → unscoped
        assert_eq!(
            resolve_plan(&rt, alice, t, Some("claude-2-opus"))
                .unwrap()
                .plan_id,
            unscoped.id
        );

        // PATCH 三态：设置（Some(Some)）/ 清空（Some(None)）/ 保留（None）
        let updated = update_plan(
            &pool,
            scoped.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Some(
                parse_model_scope(Some(&json!({"allow": ["gpt-*"]})))
                    .unwrap()
                    .unwrap(),
            )),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.model_scope, Some(json!({"allow": ["gpt-*"]})));
        let cleared = update_plan(
            &pool,
            scoped.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(None),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(cleared.model_scope, None);
        let kept = update_plan(
            &pool,
            scoped.id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(false),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(kept.model_scope, None); // absent=保留现值

        // 损坏的存量 scope（绕过 API 手改 DB）→ 加载期跳过该 Plan（fail-closed），其余不受影响
        sqlx::query(
            "UPDATE coding_plans SET model_scope = '{\"white\": [\"claude-*\"]}'::jsonb \
             WHERE id = $1",
        )
        .bind(scoped.id)
        .execute(&pool)
        .await
        .unwrap();
        let rt2 = load_plan_runtimes(&pool).await.unwrap();
        assert!(
            rt2.get(&alice)
                .unwrap()
                .iter()
                .all(|p| p.plan_id == unscoped.id)
        );
    }
    // ---------- DB 集成测试（#[sqlx::test] 每测独立临时库，自动应用迁移） ----------

    use serde_json::json;

    async fn seed_plan(pool: &PgPool, name: &str, priority: i32, enabled: bool) -> CodingPlan {
        create_plan(
            pool,
            name,
            "",
            priority,
            1_000_000,
            PERIOD_MONTHLY,
            1,
            ANCHOR_FIXED,
            OVERAGE_BLOCK,
            None,
            &json!(["in_site"]),
            "",
            None,
            None,
            None,
            enabled,
        )
        .await
        .unwrap()
    }

    /// 带模型作用域的种子 Plan（enabled 恒 true）
    async fn seed_plan_scoped(
        pool: &PgPool,
        name: &str,
        priority: i32,
        scope: &ModelScope,
    ) -> CodingPlan {
        create_plan(
            pool,
            name,
            "",
            priority,
            1_000_000,
            PERIOD_MONTHLY,
            1,
            ANCHOR_FIXED,
            OVERAGE_BLOCK,
            None,
            &json!(["in_site"]),
            "",
            None,
            None,
            Some(scope),
            true,
        )
        .await
        .unwrap()
    }

    async fn seed_user(pool: &PgPool, username: &str) -> i64 {
        crate::store::users::create_local_user(pool, username, username, "x", false)
            .await
            .unwrap()
            .id
    }

    async fn seed_group_with_member(pool: &PgPool, name: &str, user_id: i64) -> i64 {
        let g = crate::store::groups::create_group(pool, name, "", false)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_group_members (group_id, user_id) VALUES ($1, $2)")
            .bind(g.id)
            .bind(user_id)
            .execute(pool)
            .await
            .unwrap();
        g.id
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn plan_member_crud_unique_guard_and_search(pool: sqlx::PgPool) {
        let plan = seed_plan(&pool, "pro", 0, true).await;
        let u1 = seed_user(&pool, "alice").await;
        let u2 = seed_user(&pool, "bob").await;
        let g1 = crate::store::groups::create_group(&pool, "dev", "研发", false)
            .await
            .unwrap()
            .id;

        // 正常添加（事务内插入，与 API 路径一致）
        let mut tx = pool.begin().await.unwrap();
        let added = add_plan_users(&mut *tx, plan.id, &[u1, u2]).await.unwrap();
        assert_eq!(added, 2);
        let added_g = add_plan_groups(&mut *tx, plan.id, &[g1]).await.unwrap();
        assert_eq!(added_g, 1);
        tx.commit().await.unwrap();

        // 列表 + 总数
        let (members, total) = list_plan_users(&pool, plan.id, None, 20, 0).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(members.len(), 2);

        // 模糊搜索：用户名命中；无命中返回空（搜索为空场景）
        let (_, hits) = list_plan_users(&pool, plan.id, Some("ali"), 20, 0)
            .await
            .unwrap();
        assert_eq!(hits, 1);
        let (rows, hits) = list_plan_users(&pool, plan.id, Some("不存在的名字"), 20, 0)
            .await
            .unwrap();
        assert!(rows.is_empty() && hits == 0);

        // 选择器：is_member 标记 + 搜索为空
        let (picks, _) = search_users_for_plan(&pool, plan.id, None, 20, 0)
            .await
            .unwrap();
        assert_eq!(picks.iter().filter(|p| p.is_member).count(), 2);
        let (empty, total_all) = search_users_for_plan(&pool, plan.id, Some("zzz"), 20, 0)
            .await
            .unwrap();
        assert!(empty.is_empty() && total_all == 0);
        let (gpicks, _) = search_groups_for_plan(&pool, plan.id, None, 20, 0)
            .await
            .unwrap();
        assert_eq!(gpicks.iter().filter(|p| p.is_member).count(), 1);

        // 重复预检命中 + 重复插入幂等（ON CONFLICT → rows_affected = 0）
        let mut tx = pool.begin().await.unwrap();
        let dup = existing_plan_user_ids(&mut *tx, plan.id, &[u1, u2])
            .await
            .unwrap();
        assert_eq!(dup, vec![u1, u2]);
        let added = add_plan_users(&mut *tx, plan.id, &[u2]).await.unwrap();
        assert_eq!(added, 0);
        let dup_g = existing_plan_group_ids(&mut *tx, plan.id, &[g1])
            .await
            .unwrap();
        assert_eq!(dup_g, vec![g1]);
        tx.commit().await.unwrap();

        // 参数校验：不存在的 id（保持入参顺序）
        let missing = missing_user_ids(&pool, &[u1, 424242]).await.unwrap();
        assert_eq!(missing, vec![424242]);
        let missing = missing_group_ids(&pool, &[g1, 424242]).await.unwrap();
        assert_eq!(missing, vec![424242]);

        // 移除
        let removed = remove_plan_users(&pool, plan.id, &[u1, u2]).await.unwrap();
        assert_eq!(removed, 2);
        let removed = remove_plan_groups(&pool, plan.id, &[g1]).await.unwrap();
        assert_eq!(removed, 1);
        let (_, total) = list_plan_users(&pool, plan.id, None, 20, 0).await.unwrap();
        assert_eq!(total, 0);
        let (_, total_g) = list_plan_groups(&pool, plan.id, None, 20, 0).await.unwrap();
        assert_eq!(total_g, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn plan_membership_runtime_dual_channel_priority(pool: sqlx::PgPool) {
        // 分组通道：alice 经分组进 low；直连通道：bob 直连 high；
        // carol 双通道并存 → priority 高者生效
        let low = seed_plan(&pool, "low", 1, true).await;
        let high = seed_plan(&pool, "high", 10, true).await;
        let alice = seed_user(&pool, "alice").await;
        let bob = seed_user(&pool, "bob").await;
        let carol = seed_user(&pool, "carol").await;
        let g = seed_group_with_member(&pool, "dev", alice).await;

        let mut tx = pool.begin().await.unwrap();
        add_plan_groups(&mut *tx, low.id, &[g]).await.unwrap();
        add_plan_users(&mut *tx, high.id, &[bob, carol])
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_group_members (group_id, user_id) VALUES ($1, $2)")
            .bind(g)
            .bind(carol)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rt = load_plan_runtimes(&pool).await.unwrap();
        assert_eq!(resolve(&rt, alice).unwrap().plan_id, low.id);
        assert_eq!(resolve(&rt, alice).unwrap().group_name, "dev");
        assert_eq!(resolve(&rt, bob).unwrap().plan_id, high.id);
        assert_eq!(resolve(&rt, bob).unwrap().group_name, ""); // 直连无分组名
        assert_eq!(resolve(&rt, carol).unwrap().plan_id, high.id); // priority 10 > 1

        // 单用户回源与全量加载一致（/api/me/plan 缓存未命中兜底路径）
        for uid in [alice, bob, carol] {
            let scoped = load_plan_runtimes_for_user(&pool, uid).await.unwrap();
            assert_eq!(resolve(&scoped, uid).map(|p| p.plan_id), resolve(&rt, uid).map(|p| p.plan_id));
        }
        // 无归属用户回源为空（不误报归属）
        let dave = seed_user(&pool, "dave").await;
        assert!(load_plan_runtimes_for_user(&pool, dave).await.unwrap().is_empty());

        // 停用 Plan 退出运行时：carol 回退分组通道，bob 无入口 → 不限额
        sqlx::query("UPDATE coding_plans SET enabled = FALSE WHERE id = $1")
            .bind(high.id)
            .execute(&pool)
            .await
            .unwrap();
        let rt = load_plan_runtimes(&pool).await.unwrap();
        assert_eq!(resolve(&rt, carol).unwrap().plan_id, low.id);
        assert!(resolve(&rt, bob).is_none());

        // 汇总计数：分组数 = plan_groups 行数；成员数 = 双通道用户去重并集
        let summaries = list_plan_summaries(&pool).await.unwrap();
        let low_row = summaries.iter().find(|p| p.id == low.id).unwrap();
        assert_eq!(low_row.group_count, 1);
        assert_eq!(low_row.member_count, 2); // alice + carol
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn user_membership_listing_reports_inactive_states(pool: sqlx::PgPool) {
        // 回归「管理员添加成员后用户仍显示未加入」：归属列表不滤 enabled/window，
        // 停用与时段窗外的成员关系必须可见（诚实呈现），回源运行时则不含它们
        let t = |h: u32| NaiveTime::from_hms_opt(h, 0, 0).unwrap();
        let active = seed_plan(&pool, "active", 0, true).await;
        let disabled = seed_plan(&pool, "disabled", 1, true).await;
        sqlx::query("UPDATE coding_plans SET enabled = FALSE WHERE id = $1")
            .bind(disabled.id)
            .execute(&pool)
            .await
            .unwrap();
        let night = seed_plan(&pool, "night", 2, true).await;
        sqlx::query("UPDATE coding_plans SET active_start = $2, active_end = $3 WHERE id = $1")
            .bind(night.id)
            .bind(t(22))
            .bind(t(6))
            .execute(&pool)
            .await
            .unwrap();
        let u = seed_user(&pool, "alice").await;
        let g = seed_group_with_member(&pool, "dev", u).await;

        let mut tx = pool.begin().await.unwrap();
        add_plan_users(&mut *tx, active.id, &[u]).await.unwrap();
        add_plan_users(&mut *tx, disabled.id, &[u]).await.unwrap();
        add_plan_groups(&mut *tx, night.id, &[g]).await.unwrap();
        tx.commit().await.unwrap();

        // 归属列表：直连 + 分组双通道全部可见（含停用/窗外），按 plan 去重
        let memberships = list_user_plan_memberships(&pool, u).await.unwrap();
        let names: Vec<&str> = memberships.iter().map(|m| m.plan_name.as_str()).collect();
        assert_eq!(names, vec!["active", "disabled", "night"]);
        let by_name = |n: &str| memberships.iter().find(|m| m.plan_name == n).unwrap();
        assert!(by_name("active").enabled);
        assert!(!by_name("disabled").enabled);
        assert!(by_name("night").enabled);

        // 白天 12:00：仅 active 生效；night 时段窗外、disabled 停用
        let rt = load_plan_runtimes_for_user(&pool, u).await.unwrap();
        let hit = resolve_plan(&rt, u, t(12), None).unwrap();
        assert_eq!(hit.plan_id, active.id);
        assert!(!is_within_active_window(
            by_name("night").active_start,
            by_name("night").active_end,
            t(12)
        ));
        assert!(is_within_active_window(
            by_name("night").active_start,
            by_name("night").active_end,
            t(23)
        ));
        // 跨零点窗内 23:00 → night 生效（active 无窗口同样生效，priority 2 > 0 择优）
        assert_eq!(
            resolve_plan(&rt, u, t(23), None).unwrap().plan_id,
            night.id
        );

        // 移除直连成员后归属消失（分组通道行不受影响）
        remove_plan_users(&pool, active.id, &[u]).await.unwrap();
        let memberships = list_user_plan_memberships(&pool, u).await.unwrap();
        assert!(memberships.iter().all(|m| m.plan_id != active.id));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_plan_reports_impact_and_cascades(pool: sqlx::PgPool) {
        let plan = seed_plan(&pool, "gone", 0, true).await;
        let u = seed_user(&pool, "dave").await;
        let g = seed_group_with_member(&pool, "ops", u).await;

        let mut tx = pool.begin().await.unwrap();
        add_plan_groups(&mut *tx, plan.id, &[g]).await.unwrap();
        add_plan_users(&mut *tx, plan.id, &[u]).await.unwrap();
        tx.commit().await.unwrap();

        let impact = delete_plan(&pool, plan.id).await.unwrap().unwrap();
        assert_eq!(impact.groups, 1);
        assert_eq!(impact.direct_users, 1);
        // 再删 → None（幂等）
        assert!(delete_plan(&pool, plan.id).await.unwrap().is_none());

        // 关联行随 FK CASCADE 清除，运行时为空
        let rt = load_plan_runtimes(&pool).await.unwrap();
        assert!(rt.is_empty());
        let (groups_left,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plan_groups")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(groups_left, 0);
        let (users_left,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plan_users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users_left, 0);
    }
}
