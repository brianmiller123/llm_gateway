//! Coding Plan 存储：CRUD、用户→生效 Plan 运行时解析、周期计数器、用量查询。
//!
//! token 上限以精确 BIGINT 存储；API 层接受 "1.5G"/"500M"/"2T" 等单位串
//! （[`parse_token_limit`]）或纯数字。周期一律 UTC，与 usage_daily 聚合口径一致。

use std::collections::HashMap;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
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
}

/// 全量用户 → 生效 Plan（enabled 限定；多分组取 priority 最高、同分取 plan_id 大）
pub async fn load_plan_runtimes(pool: &PgPool) -> Result<HashMap<i64, PlanRuntime>, sqlx::Error> {
    let rows = sqlx::query_as::<_, RuntimeRow>(
        "SELECT m.user_id, p.id AS plan_id, p.name AS plan_name, g.name AS group_name, \
                p.token_limit, p.period_type, p.period_hours, p.period_anchor_mode, \
                m.added_at AS member_since, p.overage_action, p.downgrade_model, \
                p.alert_channels, p.webhook_url, p.priority \
         FROM user_group_members m \
         JOIN user_groups g ON g.id = m.group_id \
         JOIN coding_plans p ON p.id = g.plan_id AND p.enabled = TRUE",
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<i64, (i32, i64, PlanRuntime)> = HashMap::new();
    for r in rows {
        let chan: Vec<String> = serde_json::from_value(r.alert_channels.clone())
            .ok()
            .unwrap_or_else(|| vec!["in_site".into()]);
        let rt = PlanRuntime {
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
        };
        // 入口优先级更高的覆盖；相同 (priority, plan_id) 后到者覆盖等价
        let key = (r.priority, r.plan_id);
        match map.get_mut(&r.user_id) {
            Some(entry) if entry.0 > key.0 => {}
            Some(entry) if entry.0 == key.0 && entry.1 > key.1 => {}
            _ => {
                map.insert(r.user_id, (key.0, key.1, rt));
            }
        }
    }
    Ok(map.into_iter().map(|(k, (_, _, v))| (k, v)).collect())
}

// ---------- CRUD ----------

pub async fn list_plan_summaries(pool: &PgPool) -> Result<Vec<PlanSummary>, sqlx::Error> {
    sqlx::query_as::<_, PlanSummary>(
        "SELECT p.id, p.name, p.description, p.priority, p.token_limit, p.period_type, \
                p.period_hours, p.period_anchor_mode, \
                p.overage_action, p.downgrade_model, p.alert_channels, p.webhook_url, \
                p.enabled, p.updated_at, \
                COALESCE(u.used_tokens, 0)::bigint AS used_tokens, \
                COALESCE(u.active_users, 0)::bigint AS active_users, \
                (SELECT COUNT(*) FROM user_groups g WHERE g.plan_id = p.id) AS group_count, \
                (SELECT COUNT(*) FROM user_group_members m \
                  JOIN user_groups g ON g.id = m.group_id WHERE g.plan_id = p.id) AS member_count \
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
                webhook_url, enabled, created_at, updated_at \
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
    enabled: bool,
) -> Result<CodingPlan, sqlx::Error> {
    sqlx::query_as::<_, CodingPlan>(
        "INSERT INTO coding_plans \
           (name, description, priority, token_limit, period_type, period_hours, \
            period_anchor_mode, overage_action, downgrade_model, alert_channels, \
            webhook_url, enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
         RETURNING id, name, description, priority, token_limit, period_type, period_hours, \
                   period_anchor_mode, overage_action, downgrade_model, alert_channels, \
                   webhook_url, enabled, created_at, updated_at",
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
    .fetch_one(pool)
    .await
}

/// 部分更新（None = 保留）。downgrade_model 支持显式清空 Some(None)。
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
    enabled: Option<bool>,
) -> Result<Option<CodingPlan>, sqlx::Error> {
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
           downgrade_model = $10, \
           alert_channels = COALESCE($11, alert_channels), \
           webhook_url = COALESCE($12, webhook_url), \
           enabled = COALESCE($13, enabled), \
           updated_at = now() \
         WHERE id = $1 \
         RETURNING id, name, description, priority, token_limit, period_type, period_hours, \
                   period_anchor_mode, overage_action, downgrade_model, alert_channels, \
                   webhook_url, enabled, created_at, updated_at",
    )
    // $10 三态：None=不修改 / Some(None)=清空 / Some(Some(v))=设置
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(priority)
    .bind(token_limit)
    .bind(period_type)
    .bind(period_hours)
    .bind(period_anchor_mode)
    .bind(overage_action)
    .bind(downgrade_model.map(|o| o.map(str::to_string)))
    .bind(alert_channels)
    .bind(webhook_url)
    .bind(enabled)
    .fetch_optional(pool)
    .await
}

/// 删除计划；返回解绑的分组数（plan_id FK ON DELETE SET NULL 自动解绑）。
/// 计数器/告警行无 FK，历史用量与告警保留可回溯。
pub async fn delete_plan(pool: &PgPool, id: i64) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM coding_plans WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if exists.is_none() {
        return Ok(None);
    }
    let (groups,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM user_groups WHERE plan_id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM coding_plans WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(groups))
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
             JOIN user_groups g ON g.id = m.group_id WHERE g.plan_id = $1) \
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
             JOIN user_groups g ON g.id = m.group_id WHERE g.plan_id = $1) \
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
             JOIN user_groups g ON g.id = m.group_id WHERE g.plan_id = $1)",
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
        }
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
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
}
