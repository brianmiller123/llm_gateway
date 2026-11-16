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
    /// 请求开始时刻：记账时据此计算生成阶段耗时 gen_ms（流式扣除首 token
    /// 等待；流质终端/客户端断开两种收尾都能拿到真实生成长度）
    pub started: std::time::Instant,
    /// 管理员测试调用：跳过用量记账（不写 usage_logs，不污染状态页错误率）
    pub test_call: bool,
    /// M33：计价候选模型名（M33：客户端模型名 → 路由映射后的上游模型名，
    /// 依次查价；仅记账使用）
    pub pricing_models: Vec<String>,
    /// Coding Plan 计量（None = 用户无生效 Plan，跳过计数器写入）
    pub plan: Option<PlanBill>,
    /// 失败归因类别（连接加固/F3；仅内部候选失败行经 record_failure_row 有值，
    /// 成功行与终局行恒 None——状态页聚合以 `failure_class IS NULL` 过滤出
    /// 客户端可见行）：connect_error / header_timeout / body_timeout /
    /// body_read_error / upstream_status
    pub failure_class: Option<&'static str>,
    /// 上游真实 HTTP 状态（候选失败行 = 该候选响应状态；终局透传行 = 客户端
    /// 实际收到的上游错误状态；其余行 NULL）
    pub upstream_status: Option<i16>,
    /// 候选序号（0 = 主渠道，1.. = 降级链；候选失败行 = 该候选，终局透传行 =
    /// 产生所透传响应的候选；与 upstream_status 成对更新，见 proxy F4）
    pub attempt: Option<i16>,
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
    // 生成阶段耗时（ms；见 migrations/0032_usage_gen_ms.sql）
    gen_ms: i64,
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
            latency_ms, status, cost, client_ip, session_id, first_token_ms, gen_ms,
            failure_class, upstream_status, attempt)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::inet,$16,$17,$18,$19,$20,$21)",
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
    .bind(gen_ms)
    // 失败归因（连接加固/F3）：failure_class 仅候选失败行有值；upstream_status/
    // attempt 另带终局透传行归因（成对，见 proxy F4）；成功行三者恒 NULL
    .bind(meta.failure_class)
    .bind(meta.upstream_status)
    .bind(meta.attempt)
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

/// 失败归因行直写（连接加固/F6）：单条 INSERT，不开事务、不推进
/// user_monthly_usage / plan_usage_counters——归因行零 token 零成本，月度/
/// Plan 计数无账可记，此前的逐行事务 UPSERT 是纯写放大（上游故障风暴下
/// 每请求可产生多条候选归因行，全部挤在共享池上）。列集合与 record_usage
/// 的明细 INSERT 保持一致；成功/终局记账仍走 record_usage 全量事务。
pub async fn record_failure_log(
    pool: &PgPool,
    meta: &UsageMeta,
    status: u16,
    latency_ms: i64,
    gen_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO usage_logs
           (request_id, user_id, api_key_id, model, provider_id, endpoint, streamed,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            latency_ms, status, cost, client_ip, session_id, first_token_ms, gen_ms,
            failure_class, upstream_status, attempt)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::inet,$16,$17,$18,$19,$20,$21)",
    )
    .bind(meta.request_id)
    .bind(meta.user_id)
    .bind(meta.api_key_id)
    .bind(&meta.model)
    .bind(meta.provider_id)
    .bind(meta.endpoint)
    .bind(meta.streamed)
    // 归因行零 token 零成本（usage 恒 None，cost 恒 0）
    .bind(None::<i64>)
    .bind(None::<i64>)
    .bind(None::<i64>)
    .bind(None::<i64>)
    .bind(latency_ms as i32)
    .bind(status as i16)
    .bind(0.0_f64)
    .bind(meta.client_ip.map(|ip| ip.to_string()))
    .bind(meta.session_id.as_deref())
    .bind(meta.first_token_ms)
    .bind(gen_ms)
    .bind(meta.failure_class)
    .bind(meta.upstream_status)
    .bind(meta.attempt)
    .execute(pool)
    .await?;
    Ok(())
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

/// 用量明细保留期清理（连接加固/F7）：usage_logs 只增不减，查询随表龄退化。
/// 按 created_at 批量删除（单批 10000 行，避免长事务/大范围锁拖垮共享池）。
/// M-3：单次调用限幅——最多 [`PURGE_MAX_BATCHES`] 批（20 万行）、批间
/// [`PURGE_BATCH_PAUSE`] 间歇，积压表（数百万行）的首跑不再在单个 aggregator
/// tick 内打数千个背靠背 DELETE（索引/锁churn + WAL 尖峰 + 池连接被长期占住，
/// 拖延下一轮聚合）；剩余积压由 24h 节流的后续周期收敛。调用方以
/// `usage_retention_days > 0` 与 24h 节流控制节奏。
pub async fn purge_usage_logs(pool: &PgPool, retention_days: i64) -> Result<u64, sqlx::Error> {
    purge_usage_logs_capped(pool, retention_days, PURGE_MAX_BATCHES, PURGE_BATCH_PAUSE).await
}

/// M-3：单 tick 清理限幅——20 批 × 10000 行/批
const PURGE_MAX_BATCHES: u32 = 20;
/// M-3：批间间歇（给同池的记账/聚合写入留出喘息窗口）
const PURGE_BATCH_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// 清理实现（max_batches/pause 注入供测试；生产经 [`purge_usage_logs`]）。
/// 触及批数上限时 info 留痕（已删行数 + 积压未清提示），未触上限视为删空。
async fn purge_usage_logs_capped(
    pool: &PgPool,
    retention_days: i64,
    max_batches: u32,
    pause: std::time::Duration,
) -> Result<u64, sqlx::Error> {
    const BATCH: i64 = 10_000;
    let mut total = 0u64;
    let mut batches = 0u32;
    loop {
        let res = sqlx::query(
            "DELETE FROM usage_logs WHERE id IN (
                 SELECT id FROM usage_logs
                 WHERE created_at < now() - make_interval(days => $1::int)
                 LIMIT $2)",
        )
        .bind(retention_days)
        .bind(BATCH)
        .execute(pool)
        .await?;
        let deleted = res.rows_affected();
        total += deleted;
        if deleted == 0 {
            break;
        }
        batches += 1;
        if batches >= max_batches {
            tracing::info!(
                deleted = total,
                cap_rows = max_batches as u64 * BATCH as u64,
                "usage_logs purge hit per-tick batch cap; remaining backlog continues next cycle"
            );
            break;
        }
        tokio::time::sleep(pause).await;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{
        PlanBill, Usage, UsageMeta, purge_usage_logs, purge_usage_logs_capped,
        record_failure_log, record_usage,
    };
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

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

    // ---------- DB 集成测试（#[sqlx::test] 每测独立临时库，自动应用迁移） ----------

    /// 最小 UsageMeta 构造（user_id = None：跳过月度/Plan 计数器，仅验证明细行）
    fn meta(request_id: Uuid, failure_class: Option<&'static str>) -> UsageMeta {
        UsageMeta {
            request_id,
            user_id: None,
            api_key_id: None,
            model: "test-model".into(),
            provider_id: 1,
            endpoint: "openai",
            streamed: false,
            client_ip: None,
            session_id: None,
            first_token_ms: None,
            started: std::time::Instant::now(),
            test_call: false,
            pricing_models: Vec::new(),
            plan: None,
            failure_class,
            upstream_status: None,
            attempt: None,
        }
    }

    /// 0036 回归（F3 缺陷）：同一 request_id 的候选归因行（failure_class 非空）
    /// 与终局客户端可见行（NULL）必须共存——全表 UNIQUE 时代先落的归因行会把
    /// 后落的终局行挤掉（duplicate key），客户端实际收到的错误从状态页消失。
    #[sqlx::test(migrations = "./migrations")]
    async fn failure_row_and_client_visible_row_share_request_id(pool: sqlx::PgPool) {
        let rid = Uuid::new_v4();
        // 与 proxy 实际顺序一致：先落候选归因行，再落终局行
        record_usage(&pool, &meta(rid, Some("upstream_status")), None, 502, 10, 1, 0.0)
            .await
            .unwrap();
        record_usage(&pool, &meta(rid, None), None, 502, 10, 1, 0.0)
            .await
            .unwrap();

        let (total, visible): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COUNT(*) FILTER (WHERE failure_class IS NULL) \
             FROM usage_logs WHERE request_id = $1",
        )
        .bind(rid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(total, 2, "归因行与终局行都应落库");
        assert_eq!(visible, 1, "客户端可见行恰一条");
    }

    /// 0036 回归：终态行（failure_class NULL）每请求仍唯一——同 request_id 的
    /// 第二条 NULL 行必须被部分唯一索引拒绝（终局行只写一次的语义兜底）。
    #[sqlx::test(migrations = "./migrations")]
    async fn duplicate_client_visible_row_violates_partial_unique(pool: sqlx::PgPool) {
        let rid = Uuid::new_v4();
        record_usage(&pool, &meta(rid, None), None, 200, 10, 1, 0.0)
            .await
            .unwrap();
        let err = record_usage(&pool, &meta(rid, None), None, 200, 10, 1, 0.0)
            .await
            .unwrap_err();
        let sqlx::Error::Database(db) = err else {
            panic!("expected DatabaseError, got {err}");
        };
        assert!(db.is_unique_violation(), "expected 23505, got {db}");
        assert_eq!(
            db.constraint(),
            Some("usage_logs_request_id_client_visible_key"),
            "违例应来自部分唯一索引而非其它约束"
        );
    }

    // ---------- F6：失败归因行直写，不推进任何计数器 ----------

    /// 建最小用户（user_monthly_usage / plan_usage_counters 的 FK 归属）
    async fn seed_user(pool: &sqlx::PgPool) -> i64 {
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO users (username, source) VALUES ('f6-user', 'local') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        id
    }

    /// F6 契约：归因行走 record_failure_log 单条 INSERT——落库带归因列、
    /// 零 token 零成本，且 user_monthly_usage / plan_usage_counters 完全不动；
    /// 同一用户的 record_usage 终局路径仍正常推进计数（对照组）。
    #[sqlx::test(migrations = "./migrations")]
    async fn failure_row_insert_skips_monthly_and_plan_counters(pool: sqlx::PgPool) {
        let uid = seed_user(&pool).await;
        let mut m = meta(Uuid::new_v4(), Some("upstream_status"));
        m.user_id = Some(uid);
        m.upstream_status = Some(502);
        m.attempt = Some(0);

        record_failure_log(&pool, &m, 502, 10, 1).await.unwrap();

        let row: (Option<i64>, Option<i64>, f64, Option<String>, Option<i32>, Option<i16>) =
            sqlx::query_as(
                "SELECT input_tokens, output_tokens, cost::float8, failure_class, \
                        upstream_status, attempt FROM usage_logs WHERE request_id = $1",
            )
            .bind(m.request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, None, "归因行零输入 token");
        assert_eq!(row.1, None, "归因行零输出 token");
        assert_eq!(row.2, 0.0, "归因行零成本");
        assert_eq!(row.3.as_deref(), Some("upstream_status"), "归因列随行落库");
        assert_eq!(row.4, Some(502));
        assert_eq!(row.5, Some(0));

        let monthly: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM user_monthly_usage WHERE user_id = $1",
        )
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(monthly.0, 0, "归因行不得推进月度计数");
        let counters: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM plan_usage_counters WHERE user_id = $1",
        )
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(counters.0, 0, "归因行不得推进 Plan 计数");

        // 对照组：终局记账路径（record_usage）推进月度 + Plan 计数
        let mut terminal = meta(Uuid::new_v4(), None);
        terminal.user_id = Some(uid);
        terminal.plan = Some(PlanBill {
            plan_id: 7,
            period_start: Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(),
            period_key: "2026-11".into(),
        });
        let usage = Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(20),
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
        };
        record_usage(&pool, &terminal, Some(&usage), 200, 10, 1, 0.5)
            .await
            .unwrap();
        let monthly: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(tokens), 0)::bigint FROM user_monthly_usage WHERE user_id = $1",
        )
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(monthly, (1, 120), "终局记账推进月度计数（对照组）");
        let counters: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(tokens), 0)::bigint FROM plan_usage_counters WHERE user_id = $1",
        )
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(counters, (1, 120), "终局记账推进 Plan 计数（对照组）");
    }

    // ---------- F7：保留期批量清理 ----------

    /// 插入指定龄期的 usage_logs 行（days 用 i32——make_interval(days => int)）
    async fn insert_usage_at(pool: &sqlx::PgPool, age_days: i32) -> Uuid {
        let rid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO usage_logs (request_id, model, provider_id, created_at) \
             VALUES ($1, 'purge-test', 1, now() - make_interval(days => $2))",
        )
        .bind(rid)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
        rid
    }

    /// F7 契约：仅删除超过保留期的行，保留期内行不动
    #[sqlx::test(migrations = "./migrations")]
    async fn purge_usage_logs_removes_only_expired_rows(pool: sqlx::PgPool) {
        let old = insert_usage_at(&pool, 100).await;
        let fresh = insert_usage_at(&pool, 1).await;

        let removed = purge_usage_logs(&pool, 90).await.unwrap();
        assert_eq!(removed, 1, "仅清理超过保留期的行");
        assert_ne!(old, fresh);
        let remaining: (Uuid,) = sqlx::query_as("SELECT request_id FROM usage_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining.0, fresh, "保留期内行不动");
    }

    /// F7 契约：超过单批 10000 行时循环删空（10000 + 1 两批），
    /// 汇总删除总数
    #[sqlx::test(migrations = "./migrations")]
    async fn purge_usage_logs_batches_until_empty(pool: sqlx::PgPool) {
        sqlx::query(
            "INSERT INTO usage_logs (request_id, model, provider_id, created_at) \
             SELECT ('11111111-1111-1111-1111-' || lpad(g::text, 12, '0'))::uuid, \
                    'purge-bulk', 1, now() - interval '2 days' \
             FROM generate_series(1, 10001) AS g",
        )
        .execute(&pool)
        .await
        .unwrap();

        let removed = purge_usage_logs(&pool, 1).await.unwrap();
        assert_eq!(removed, 10001, "批量循环删空并汇总总数");
        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM usage_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining.0, 0);
    }

    /// M-3 契约：单次调用触及批数上限即止——注入 max_batches=1，10001 行积压
    /// 只清首批 10000 行，余量留给 24h 节流的下一周期
    #[sqlx::test(migrations = "./migrations")]
    async fn purge_usage_logs_stops_at_batch_cap(pool: sqlx::PgPool) {
        sqlx::query(
            "INSERT INTO usage_logs (request_id, model, provider_id, created_at) \
             SELECT ('22222222-2222-2222-2222-' || lpad(g::text, 12, '0'))::uuid, \
                    'purge-cap', 1, now() - interval '2 days' \
             FROM generate_series(1, 10001) AS g",
        )
        .execute(&pool)
        .await
        .unwrap();

        let removed =
            purge_usage_logs_capped(&pool, 1, 1, std::time::Duration::from_millis(1))
                .await
                .unwrap();
        assert_eq!(removed, 10_000, "触及批数上限即止，不越界清空");
        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM usage_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining.0, 1, "剩余积压留给下一周期");
    }
}
