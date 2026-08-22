use serde_json::Value;
use sqlx::{FromRow, PgPool};

/// 上游供应商（api_key_encrypted 为 AES-256-GCM 密文）
#[derive(Debug, Clone, FromRow)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    /// 上游方言：openai（默认，Chat Completions 兼容）/ openai-responses（原生 Responses API）等
    pub api_type: String,
    pub base_url: String,
    pub api_key_encrypted: String,
    pub timeout_ms: i32,
    /// 渠道级 extra_body：请求转发前深合并进上游请求体（{} = 未配置）
    pub extra_body: Value,
    /// 认证形态：bearer（默认，Authorization: Bearer）/ x-api-key（Anthropic 原生）
    pub auth_scheme: String,
    /// 渠道级静态附加请求头（{} = 未配置；覆盖同名透传头）
    pub extra_headers: Value,
    /// H6：渠道是否支持图像输入（FALSE → 发前主动降级图片 part，
    /// cc-switch media_sanitizer image_input_capability_from_settings 同款）
    pub supports_images: bool,
}

/// 模型路由规则
#[derive(Debug, Clone, FromRow)]
pub struct ModelRoute {
    pub model_pattern: String,
    pub provider_id: i64,
    pub fallback_ids: Vec<i64>,
    /// 上游实际模型名（映射）；NULL = 透传客户端模型名
    pub upstream_model: Option<String>,
    /// 模型级 extra_body：深合并时覆盖渠道级同名叶键（{} = 未配置）
    pub extra_body: Value,
    /// 模型级开关：Chat 请求 system 消息收拢到头部（MiniMax 类严格上游；
    /// 管理员按模型启用，默认关闭）
    pub strict_system_head: bool,
    /// H3：出站 reasoning_effort 值域钳制模式（NULL/passthrough = 原值透传；
    /// deepseek / low_high / openrouter 见 model_family::clamp_reasoning_effort）
    pub reasoning_effort_mode: Option<String>,
    /// H3：thinking 形态配置（NULL = 剥离；thinking_param / reasoning_split /
    /// enable_thinking 见 model_family::apply_thinking_form）
    pub thinking_form: Option<String>,
    /// H2：Responses 方言专属字段透传白名单（逗号分隔原文；NULL/空 = 全部剥离）。
    /// 可选项：store / safety_identifier / prompt_cache_retention / prompt_cache_key
    pub responses_passthrough_fields: Option<String>,
}

impl ModelRoute {
    /// 方言字段透传白名单（逗号分隔 → 去空白列表）
    pub fn passthrough_field_list(&self) -> Vec<String> {
        self.responses_passthrough_fields
            .as_deref()
            .map(|s| {
                s.split(',')
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub async fn load_providers(pool: &PgPool) -> Result<Vec<Provider>, sqlx::Error> {
    sqlx::query_as::<_, Provider>(
        "SELECT id, name, api_type, base_url, api_key_encrypted, timeout_ms, extra_body, auth_scheme, extra_headers, supports_images FROM providers WHERE enabled = TRUE",
    )
    .fetch_all(pool)
    .await
}

pub async fn load_routes(pool: &PgPool) -> Result<Vec<ModelRoute>, sqlx::Error> {
    sqlx::query_as::<_, ModelRoute>(
        "SELECT model_pattern, provider_id, fallback_ids, upstream_model, extra_body, strict_system_head, reasoning_effort_mode, thinking_form, responses_passthrough_fields
         FROM model_routes WHERE enabled = TRUE ORDER BY priority ASC, id ASC",
    )
    .fetch_all(pool)
    .await
}


