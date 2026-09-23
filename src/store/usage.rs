use chrono::{DateTime, Utc};
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
    /// L3：客户端会话关联（x-claude-code-session-id / session_id / x-grok-conv-id）
    pub session_id: Option<String>,
    /// L3：流式请求首个上游 chunk 到达耗时（ms；非流式 None）
    pub first_token_ms: Option<i64>,
    /// 管理员测试调用：跳过用量记账（不写 usage_logs，不污染状态页错误率）
    pub test_call: bool,
    /// 计价候选模型名（M33：客户端模型名 → 路由映射后的上游模型名，
    /// 依次查价；仅记账使用）
    pub pricing_models: Vec<String>,
    /// Coding Plan 计量（None = 用户无生效 Plan，跳过计数器写入）
    pub plan: Option<PlanBill>,
}

/// Coding Plan 计量载荷：生效 Plan 存在时随请求携带（proxy 期解析自 PlanRuntime）。
/// period_start 由 PlanRuntime.period_start(now) 按周期类型解析（UTC 时刻，hourly
/// 为小时桶起点）；period_key 用于内存缓存周期翻转判定。
#[derive(Debug, Clone)]
pub struct PlanBill {
    pub plan_id: i64,
    pub period_start: DateTime<Utc>,
    pub period_key: String,
}
/// 线上格式中间层：多上游 usage 字段名/嵌套差异在此归一（serde alias 只能
/// 扁平重命名，够不到 prompt_tokens_details 这类嵌套桶）。
#[derive(Debug, Default, serde::Deserialize)]
struct UsageWire {
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    /// P1-18：原生 Anthropic 字段名 cache_read_input_tokens
    #[serde(default, alias = "cache_read_input_tokens")]
    cache_read_tokens: Option<i64>,
    /// Anthropic 名 cache_creation_input_tokens
    #[serde(
        default,
        alias = "cache_creation_input_tokens",
        alias = "cache_write_input_tokens"
    )]
    cache_write_tokens: Option<i64>,
    /// DeepSeek：缓存命中为 input 的兄弟字段（非嵌套）
    #[serde(default, alias = "prompt_cache_hit_tokens")]
    deepseek_cache_hit: Option<i64>,
    /// OpenAI Chat：usage.prompt_tokens_details.cached_tokens
    #[serde(default)]
    prompt_tokens_details: Option<CachedTokensDetails>,
    /// Responses：usage.input_tokens_details.cached_tokens
    #[serde(default)]
    input_tokens_details: Option<CachedTokensDetails>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CachedTokensDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
}

impl From<UsageWire> for Usage {
    fn from(w: UsageWire) -> Self {
        let mut u = Usage {
            prompt_tokens: w.prompt_tokens,
            completion_tokens: w.completion_tokens,
            input_tokens: w.input_tokens,
            output_tokens: w.output_tokens,
            cache_read_tokens: w.cache_read_tokens,
            cache_write_tokens: w.cache_write_tokens,
        };
        // 缓存读桶多源归一（缺失时回退）：显式/Anthropic 别名 > OpenAI Chat
        // prompt_tokens_details > Responses input_tokens_details > DeepSeek 兄弟字段
        if u.cache_read_tokens.is_none() {
            u.cache_read_tokens = w
                .prompt_tokens_details
                .and_then(|d| d.cached_tokens)
                .or_else(|| w.input_tokens_details.as_ref().and_then(|d| d.cached_tokens))
                .or(w.deepseek_cache_hit);
        }
        u
    }
}

 /// Responses API 用 input/output_tokens；缺失字段自动回退为 None（记账按 0 计）。
 /// 缓存桶（cache_read/cache_write）来自 prompt_tokens_details.cached_tokens 等多源
 /// 归一化（详见 responses::dto::Usage），用于按缓存费率计费与明细对账。
 #[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(from = "UsageWire")]
 pub struct Usage {
     pub prompt_tokens: Option<i64>,
     pub completion_tokens: Option<i64>,
     #[serde(default)]
     pub input_tokens: Option<i64>,
     #[serde(default)]
     pub output_tokens: Option<i64>,
     /// 缓存命中读 token（prompt_tokens 已包含，不重复计入 total）。
     pub cache_read_tokens: Option<i64>,
    /// 缓存写入 token（prompt_tokens 已包含）
     pub cache_write_tokens: Option<i64>,
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

    /// Anthropic 原生口径 → OpenAI 口径：Anthropic 的 input_tokens 不含缓存
    /// 桶（OpenAI 的 prompt_tokens 已含），混用同一结构体后 compute_cost 的
    /// fresh = input − cache_read − cache_write 与 total() 会把缓存桶丢账。
    /// 捕获侧（原生 anthropic 透传流/响应体）先折算，全链路按 OpenAI 口径一致。
    pub fn fold_anthropic_input(&mut self) {
        let buckets = self
            .cache_read_tokens
            .unwrap_or(0)
            .saturating_add(self.cache_write_tokens.unwrap_or(0));
        if buckets > 0 {
            let base = self.prompt_tokens.or(self.input_tokens).unwrap_or(0);
            self.prompt_tokens = None;
            self.input_tokens = Some(base.saturating_add(buckets));
        }
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
    let (cache_read, cache_write) = usage
        .map(|u| (u.cache_read_tokens, u.cache_write_tokens))
        .unwrap_or((None, None));
    // 上游 usage 为外部输入，饱和加法防 i64 回绕为负
    let tokens = input.unwrap_or(0).saturating_add(output.unwrap_or(0));
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO usage_logs
           (request_id, user_id, api_key_id, model, provider_id, endpoint, streamed,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            latency_ms, status, cost, client_ip, session_id, first_token_ms)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::inet,$16,$17)",
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
    .bind(cache_read)
    .bind(cache_write)
    .bind(latency_ms as i32)
    .bind(status as i16)
    .bind(cost)
    .bind(meta.client_ip.map(|ip| ip.to_string()))
    .bind(meta.session_id.as_deref())
    .bind(meta.first_token_ms)
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

    // Coding Plan 周期计数器：与明细写入同事务 UPSERT（原子一致）；
    // tokens 饱和加法已在上方计算，plan 计量与月度口径一致
    if let (Some(user_id), Some(bill)) = (meta.user_id, meta.plan.as_ref()) {
        sqlx::query(
            "INSERT INTO plan_usage_counters (user_id, plan_id, period_start, tokens)
             VALUES ($1,$2,$3,$4)
             ON CONFLICT (user_id, plan_id, period_start) DO UPDATE SET
               tokens = plan_usage_counters.tokens + EXCLUDED.tokens, updated_at = now()",
        )
        .bind(user_id)
        .bind(bill.plan_id)
        .bind(bill.period_start)
        .bind(tokens)
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
    let watermark: (i64,) =
        sqlx::query_as("SELECT watermark_id FROM aggregation_state WHERE id = 1 FOR UPDATE")
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
               -- 匿名请求（auth=none，user_id 为 NULL）无归属，跳过聚合防 NOT NULL 违例卡死水位
               AND usage_logs.user_id IS NOT NULL
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

#[cfg(test)]
mod tests {
    use super::Usage;

    #[test]
    fn openai_nested_cache_details_map_to_cache_read() {
        // OpenAI Chat：缓存命中藏在 prompt_tokens_details.cached_tokens
        let u: Usage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "prompt_tokens_details": { "cached_tokens": 80 }
        }))
        .unwrap();
        assert_eq!(u.cache_read_tokens, Some(80));
        assert_eq!(u.input(), 100, "prompt 已含缓存桶，不重复计入");

        // Responses：input_tokens_details.cached_tokens
        let u: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": 50,
            "output_tokens": 3,
            "input_tokens_details": { "cached_tokens": 20 }
        }))
        .unwrap();
        assert_eq!(u.cache_read_tokens, Some(20));
    }

    #[test]
    fn deepseek_cache_hit_sibling_maps_to_cache_read() {
        let u: Usage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 64,
            "completion_tokens": 2,
            "prompt_cache_hit_tokens": 60,
            "prompt_cache_miss_tokens": 4
        }))
        .unwrap();
        assert_eq!(u.cache_read_tokens, Some(60));
    }

    #[test]
    fn anthropic_aliases_win_over_nested_fallbacks() {
        // 显式/别名桶优先，不做嵌套回退（P1-18 行为保持）
        let u: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": 2000,
            "cache_read_input_tokens": 100000,
            "prompt_tokens_details": { "cached_tokens": 7 }
        }))
        .unwrap();
        assert_eq!(u.cache_read_tokens, Some(100000));
    }

    #[test]
    fn fold_anthropic_input_adopts_openai_semantics() {
        // Anthropic 原生：input 不含缓存桶 → 折叠后 compute_cost 的
        // fresh = input − cache_read − cache_write 恢复为真实新鲜输入
        let mut u: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": 2000,
            "output_tokens": 500,
            "cache_read_input_tokens": 100000,
            "cache_creation_input_tokens": 3000
        }))
        .unwrap();
        u.fold_anthropic_input();
        assert_eq!(u.input_tokens, Some(105000));
        assert_eq!(u.total(), 105500);
        // 无缓存桶 → 折叠为无操作（重复调用也不叠加）
        let mut u2: Usage = serde_json::from_value(serde_json::json!({
            "input_tokens": 10
        }))
        .unwrap();
        u2.fold_anthropic_input();
        u2.fold_anthropic_input();
        assert_eq!(u2.input_tokens, Some(10));
    }
}
