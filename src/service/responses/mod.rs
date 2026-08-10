//! Responses API ↔ Chat Completions 降级转换。
//!
//! 移植自 new-api（third/new-api/relaykit）：`/v1/responses` 客户端请求经
//! 本模块转换为 Chat Completions 请求转发给上游（convert_req），上游响应再转换回
//! Responses 形态（convert_resp 非流式 / stream 流式）。
//!
//! 决策门：provider `api_type = "openai-responses"` 时原生透传（不转换）；
//! 其余（默认 `openai` 等）一律走本模块降级转换。

pub mod convert_req;
pub mod convert_resp;
pub mod dto;
pub mod stream;

use serde_json::Value;

use crate::store::upstream::Provider;

/// 上游是否原生支持 /v1/responses（透传，不做降级转换）
pub fn provider_native_responses(p: &Provider) -> bool {
    p.api_type.eq_ignore_ascii_case("openai-responses")
}

/// 降级转换决策：/v1/responses 端点 + 非原生上游 → 转换
pub fn should_convert(endpoint_responses: bool, provider: &Provider) -> bool {
    endpoint_responses && !provider_native_responses(provider)
}

/// 请求转换入口：Responses 请求 JSON → Chat 请求 JSON
pub fn convert_request(req: &Value) -> Result<Value, String> {
    convert_req::responses_request_to_chat(req)
}

/// 非流式响应转换入口：Chat 响应 JSON → Responses 响应 JSON
pub fn convert_response(resp: &Value, id: &str, fallback_created: i64) -> Result<Value, String> {
    convert_resp::chat_response_to_responses(resp, id, fallback_created)
}
