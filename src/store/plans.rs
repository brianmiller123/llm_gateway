//! Coding Plan 存储：CRUD、用户→生效 Plan 运行时解析、周期计数器、用量查询。
//!
//! token 上限以精确 BIGINT 存储；API 层接受 "1.5G"/"500M"/"2T" 等单位串
//! （[`parse_token_limit`]）或纯数字。周期一律 UTC，与 usage_daily 聚合口径一致。

use std::collections::HashMap;

use chrono::{Datelike, DateTime, NaiveDate, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool};

pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";
pub const PERIOD_TOTAL: &str = "total";

pub const OVERAGE_BLOCK: &str = "block";
pub const OVERAGE_DOWNGRADE: &str = "downgrade";
pub const OVERAGE_LOG: &str = "log";

pub const VALID_PERIODS: [&str; 3] = [PERIOD_DAILY, PERIOD_MONTHLY, PERIOD_TOTAL];
pub const VALID_OVERAGES: [&str; 3] = [OVERAGE_BLOCK, OVERAGE_DOWNGRADE, OVERAGE_LOG];
pub const VALID_ALERT_CHANNELS: [&str; 3] = ["in_site", "email", "webhook"];

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
    pub overage_action: String,
    pub downgrade_model: Option<String>,
    pub alert_channels: Vec<String>,
    pub webhook_url: String,
}

impl PlanRuntime {
    /// 当前统计周期起点（UTC）：daily=当日 / monthly=当月 1 日 / total=常量纪元
    pub fn period_start(&self, now: DateTime<Utc>) -> NaiveDate {
        match self.period_type.as_str() {
            PERIOD_DAILY => now.date_naive(),
            PERIOD_MONTHLY => {
                NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
                    .expect("first of month is a valid date")
            }
            _ => NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch is a valid date"),
        }
    }

    /// 缓存/告警去重键：d:2026-09-01 / m:2026-09 / t
    pub fn period_key(&self, now: DateTime<Utc>) -> String {
        match self.period_type.as_str() {
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
    overage_action: String,
    downgrade_model: Option<String>,
    alert_channels: Value,
    webhook_url: String,
    priority: i32,
}

/// 全量用户 → 生效 Plan（enabled 限定；多分组取 priority 最高、同分取 plan_id 大）
pub async fn load_plan_runtimes(
    pool: &PgPool,
) -> Result<HashMap<i64, PlanRuntime>, sqlx::Error> {
    let rows = sqlx::query_as::<_, RuntimeRow>(
        "SELECT m.user_id, p.id AS plan_id, p.name AS plan_name, g.name AS group_name, \
                p.token_limit, p.period_type, p.overage_action, p.downgrade_model, \
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
                OR (p2.period_type = 'daily' AND pc.period_start = CURRENT_DATE) \
                OR (p2.period_type = 'monthly' \
                    AND pc.period_start = date_trunc('month', CURRENT_DATE)::date) \
             GROUP BY pc.plan_id \
         ) u ON u.plan_id = p.id \
         ORDER BY p.priority DESC, p.id",
    )
    .fetch_all(pool)
    .await
}

pub async fn find_plan(pool: &PgPool, id: i64) -> Result<Option<CodingPlan>, sqlx::Error> {
    sqlx::query_as::<_, CodingPlan>(
        "SELECT id, name, description, priority, token_limit, period_type, overage_action, \
                downgrade_model, alert_channels, webhook_url, enabled, created_at, updated_at \
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
    overage_action: &str,
    downgrade_model: Option<&str>,
    alert_channels: &Value,
    webhook_url: &str,
    enabled: bool,
) -> Result<CodingPlan, sqlx::Error> {
    sqlx::query_as::<_, CodingPlan>(
        "INSERT INTO coding_plans \
           (name, description, priority, token_limit, period_type, overage_action, \
            downgrade_model, alert_channels, webhook_url, enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
         RETURNING id, name, description, priority, token_limit, period_type, overage_action, \
                   downgrade_model, alert_channels, webhook_url, enabled, created_at, updated_at",
    )
    .bind(name)
    .bind(description)
    .bind(priority)
    .bind(token_limit)
    .bind(period_type)
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
           overage_action = COALESCE($7, overage_action), \
           downgrade_model = $8, \
           alert_channels = COALESCE($9, alert_channels), \
           webhook_url = COALESCE($10, webhook_url), \
           enabled = COALESCE($11, enabled), \
           updated_at = now() \
         WHERE id = $1 \
         RETURNING id, name, description, priority, token_limit, period_type, overage_action, \
                   downgrade_model, alert_channels, webhook_url, enabled, created_at, updated_at",
    )
    // $8 三态：None=不修改 / Some(None)=清空 / Some(Some(v))=设置
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(priority)
    .bind(token_limit)
    .bind(period_type)
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
    let (groups,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM user_groups WHERE plan_id = $1")
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

/// 各启用 Plan 的当期用量快照（缓存重载；只读不清理，与月度缓存同语义）
pub async fn load_usage_snapshots(pool: &PgPool) -> Result<Vec<PlanUsageSnapshot>, sqlx::Error> {
    sqlx::query_as::<_, PlanUsageSnapshot>(
        "SELECT pc.user_id, pc.plan_id, \
                CASE p.period_type \
                  WHEN 'daily' THEN 'd:' || to_char(pc.period_start, 'YYYY-MM-DD') \
                  WHEN 'monthly' THEN 'm:' || to_char(pc.period_start, 'YYYY-MM') \
                  ELSE 't' END AS period_key, \
                SUM(pc.tokens)::bigint AS used \
         FROM plan_usage_counters pc \
         JOIN coding_plans p ON p.id = pc.plan_id AND p.enabled = TRUE \
         WHERE (p.period_type = 'total') \
            OR (p.period_type = 'daily' AND pc.period_start = CURRENT_DATE) \
            OR (p.period_type = 'monthly' \
                AND pc.period_start = date_trunc('month', CURRENT_DATE)::date) \
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
    pub period_start: NaiveDate,
    pub tokens: i64,
    pub users: i64,
}

/// Plan 历史周期用量（计数器即历史快照：daily 逐日、monthly 逐月、total 单行）
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

    #[test]
    fn period_keys() {
        let rt = |pt: &str| PlanRuntime {
            plan_id: 1,
            plan_name: "p".into(),
            group_name: "g".into(),
            token_limit: 1,
            period_type: pt.into(),
            overage_action: OVERAGE_BLOCK.into(),
            downgrade_model: None,
            alert_channels: vec![],
            webhook_url: String::new(),
        };
        let now = Utc::now();
        assert_eq!(rt("total").period_key(now), "t");
        assert!(rt("daily").period_key(now).starts_with("d:"));
        assert_eq!(rt("monthly").period_key(now).len(), 9);
    }
}
