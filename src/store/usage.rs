use sqlx::PgPool;
use std::net::IpAddr;
use uuid::Uuid;

/// 单次请求的用量元数据
#[derive(Debug, Clone)]
pub struct UsageMeta {
    pub request_id: Uuid,
    pub user_id: Option<i64>,
    pub api_key_id: Option<i64>,
    pub model: String,
    pub provider_id: i64,
    pub endpoint: &'static str,
    pub streamed: bool,
    /// 客户端来源 IP（X-Forwarded-For 优先，缺省取对端地址）
    pub client_ip: Option<IpAddr>,
}

/// 供应商返回的 usage（双形态）：Chat Completions 用 prompt/completion_tokens，
/// Responses API 用 input/output_tokens；缺失字段自动回退为 None（记账按 0 计）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    #[serde(default)]
    pub input_tokens: Option<i64>,
    #[serde(default)]
    pub output_tokens: Option<i64>,
}

impl Usage {
    /// 输入 token：Chat 形态优先，Responses 形态回退
    pub fn input(&self) -> i64 {
        self.prompt_tokens.or(self.input_tokens).unwrap_or(0)
    }

    /// 输出 token：Chat 形态优先，Responses 形态回退
    pub fn output(&self) -> i64 {
        self.completion_tokens.or(self.output_tokens).unwrap_or(0)
    }

    /// 全部 token（输入 + 输出），饱和加法防畸形上游数值回绕
    pub fn total(&self) -> i64 {
        self.input().saturating_add(self.output())
    }
}

/// 记账：单事务写入明细 + UPSERT 月度配额计数（原子一致）
pub async fn record_usage(
    pool: &PgPool,
    meta: &UsageMeta,
    usage: Option<&Usage>,
    status: u16,
    latency_ms: i64,
    cost: f64,
) -> Result<(), sqlx::Error> {
    let (input, output) = usage
        .map(|u| (Some(u.input()), Some(u.output())))
        .unwrap_or((None, None));
    // 上游 usage 为外部输入，饱和加法防 i64 回绕为负
    let tokens = input.unwrap_or(0).saturating_add(output.unwrap_or(0));

    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO usage_logs
           (request_id, user_id, api_key_id, model, provider_id, endpoint, streamed,
            input_tokens, output_tokens, latency_ms, status, cost, client_ip)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13::inet)",
    )
    .bind(meta.request_id)
    .bind(meta.user_id)
    .bind(meta.api_key_id)
    .bind(&meta.model)
    .bind(meta.provider_id)
    .bind(meta.endpoint)
    .bind(meta.streamed)
    .bind(input)
    .bind(output)
    .bind(latency_ms as i32)
    .bind(status as i16)
    .bind(cost)
    .bind(meta.client_ip.map(|ip| ip.to_string()))
    .execute(&mut *tx)
    .await?;

    if let Some(user_id) = meta.user_id {
        let month = chrono::Utc::now().format("%Y-%m").to_string();
        sqlx::query(
            "INSERT INTO user_monthly_usage (user_id, month, tokens, cost)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (user_id, month) DO UPDATE SET
               tokens = user_monthly_usage.tokens + EXCLUDED.tokens,
               cost   = user_monthly_usage.cost   + EXCLUDED.cost",
        )
        .bind(user_id)
        .bind(month)
        .bind(tokens)
        .bind(cost)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// 聚合任务：将增量 usage_logs 汇总进 usage_daily（水位线防重复）
///
/// 修复两个并发缺陷：
/// 1. 漏计（单实例）：原实现 INSERT..SELECT 与 MAX(id) 在不同快照执行，
///    期间新提交的行会被水位线跳过、永不聚合；改为单语句内取 MAX(id)（同一快照），
///    水位线不可能越过本次实际聚合的最大 id。
/// 2. 重复计（多实例）：水位行 FOR UPDATE 串行化并发聚合器。
pub async fn aggregate_daily(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    // 串行化并发聚合器：后到者阻塞至此事务提交，读到新水位
    let watermark: (i64,) = sqlx::query_as(
        "SELECT watermark_id FROM aggregation_state WHERE id = 1 FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await?;

    // 单语句完成聚合 + 水位推进：整条语句共享同一快照
    // 注意：不能用 `\` 续行拼接 SQL（会吞掉换行和缩进导致 token 粘连）
    sqlx::query(
        "WITH m AS (
             SELECT COALESCE(MAX(id), $1) AS max_id FROM usage_logs
         ),
         agg AS (
             INSERT INTO usage_daily (user_id, model, stat_date, call_count, input_tokens, output_tokens, cost)
             SELECT user_id, model, (created_at AT TIME ZONE 'UTC')::date,
                    COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(SUM(cost),0)
             FROM usage_logs, m
             WHERE usage_logs.id > $1 AND usage_logs.id <= m.max_id
             GROUP BY user_id, model, (created_at AT TIME ZONE 'UTC')::date
             ON CONFLICT (user_id, model, stat_date) DO UPDATE SET
               call_count    = usage_daily.call_count    + EXCLUDED.call_count,
               input_tokens  = usage_daily.input_tokens  + EXCLUDED.input_tokens,
               output_tokens = usage_daily.output_tokens + EXCLUDED.output_tokens,
               cost          = usage_daily.cost          + EXCLUDED.cost
         )
         UPDATE aggregation_state SET watermark_id = (SELECT max_id FROM m) WHERE id = 1",
    )
    .bind(watermark.0)
    .execute(&mut *tx)
    .await?;

    tx.commit().await
}
