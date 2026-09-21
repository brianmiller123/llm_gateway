//! 配置管理 API：供应商 / 路由规则 / 限流 / 配额 / 单价 CRUD。
//! 全部写操作：入库 → `reload()` 即时生效 → audit 留痕。仅管理员可访问。

use std::error::Error as StdError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Extension, Path, Query, State};
use axum::http::HeaderMap;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::service::ldap::LdapSettings;
use crate::state::AppState;
use crate::store::{audit, config, users};

pub fn routes(state: AppState) -> Router<AppState> {
    let admin = middleware::from_fn_with_state(state.clone(), super::console::require_admin);
    Router::new()
        .route(
            "/api/admin/providers",
            get(list_providers)
                .post(create_provider)
                .layer(admin.clone()),
        )
        .route(
            "/api/admin/providers/{id}",
            patch(update_provider).layer(admin.clone()),
        )
        .route(
            "/api/admin/providers/test-connection",
            post(test_connection).layer(admin.clone()),
        )
        .route(
            "/api/admin/routes",
            get(list_routes).post(create_route).layer(admin.clone()),
        )
        .route(
            "/api/admin/routes/{id}",
            patch(update_route).layer(admin.clone()),
        )
        .route(
            "/api/admin/rate-limits",
            get(list_rate_limits)
                .post(create_rate_limit)
                .layer(admin.clone()),
        )
        .route(
            "/api/admin/rate-limits/{id}",
            patch(update_rate_limit)
                .delete(delete_rate_limit)
                .layer(admin.clone()),
        )
        .route("/api/admin/quotas", get(list_quotas).layer(admin.clone()))
        .route(
            "/api/admin/quotas/{user_id}",
            put(upsert_quota).layer(admin.clone()),
        )
        .route(
            "/api/admin/prices",
            get(list_prices).post(create_price).layer(admin.clone()),
        )
        .route(
            "/api/admin/prices/{id}",
            delete(delete_price).layer(admin.clone()),
        )
        .route("/api/admin/models", get(list_models).layer(admin.clone()))
        .route(
            "/api/admin/models/refresh",
            post(refresh_models).layer(admin.clone()),
        )
        .route(
            "/api/admin/models/test",
            post(test_models).layer(admin.clone()),
        )
        .route(
            "/api/admin/settings/ldap",
            get(get_ldap_settings)
                .put(put_ldap_settings)
                .layer(admin.clone()),
        )
        .route(
            "/api/admin/settings/extra-body",
            get(get_extra_body_settings)
                .put(put_extra_body_settings)
                .layer(admin.clone()),
        )
        .route(
            "/api/admin/settings/headers",
            get(get_header_settings)
                .put(put_header_settings)
                .layer(admin.clone()),
        )
        .route(
            "/api/admin/api-endpoints",
            get(get_api_endpoint_settings)
                .put(put_api_endpoint_settings)
                .layer(admin.clone()),
        )
        .route(
            "/api/admin/api-endpoints/test",
            post(test_api_endpoint).layer(admin.clone()),
        )
        .route(
            "/api/admin/api-endpoints/results",
            get(list_api_test_results).layer(admin.clone()),
        )
        .route(
            "/api/admin/models/{id}",
            patch(update_model)
                .delete(delete_model)
                .layer(admin.clone()),
        )
        .route(
            "/api/admin/settings/ldap/test",
            post(test_ldap_settings).layer(admin),
        )
}
/// 管理员身份（require_admin 注入）
type Admin = Extension<users::UserRow>;

/// 供应商 Key 脱敏：保留前 6 后 4
fn mask_key(enc: &str) -> String {
    let s = enc;
    if s.len() <= 12 {
        return "****".to_string();
    }
    format!("{}****{}", &s[..6], &s[s.len() - 4..])
}

/// 测试连接并拉取模型列表（用表单实时值，支持未保存的供应商）
#[derive(Deserialize)]
struct TestConnectionReq {
    base_url: String,
    api_key: Option<String>,
    /// 编辑已有供应商时传入：api_key 留空则回退使用已保存的 Key
    provider_id: Option<i64>,
}

/// 拉取上游 /models（CN 网络下偶发 TCP 失败，重试 3 次退避）；
/// 成功返回模型 id 列表；失败返回已展开错误链的字符串。
async fn fetch_upstream_models(
    client: &reqwest::Client,
    base: &str,
    key: Option<&str>,
) -> Result<Vec<String>, String> {
    let url = format!("{base}/models");
    let mut last_err: Option<reqwest::Error> = None;
    for attempt in 0..3 {
        let mut rb = client.get(&url).timeout(Duration::from_secs(15));
        if let Some(k) = key {
            rb = rb.header("Authorization", format!("Bearer {k}"));
        }
        match rb.send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Err(format!(
                        "upstream {status}: {}",
                        text.chars().take(300).collect::<String>()
                    ));
                }
                let models: Vec<String> = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("data").and_then(|d| d.as_array()).cloned())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| {
                                m.get("id").and_then(|i| i.as_str()).map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(models);
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(400 * (attempt as u64 + 1))).await;
            }
        }
    }
    // 失败时展开错误链（hyper 底层原因：dns / tcp / tls / timeout）
    let mut detail = last_err.as_ref().map(|e| e.to_string()).unwrap_or_default();
    if let Some(e) = last_err.as_ref() {
        let mut src = e.source();
        let mut hops = 0;
        while let Some(s) = src {
            if hops >= 4 {
                break;
            }
            detail.push_str(&format!(" -> {s}"));
            src = s.source();
            hops += 1;
        }
    }
    Err(format!("connection failed after 3 attempts: {detail}"))
}

async fn test_connection(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<TestConnectionReq>,
) -> Result<Response, AppError> {
    let base = req.base_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(AppError::BadRequest(
            "base_url must start with http(s)://".into(),
        ));
    }
    // 1) 表单显式给了 Key → 用之（可测试新 Key）；
    // 2) 未给但指定了已保存的供应商 → 用库里加密 Key 解密（编辑时不改 Key 也能测）
    let key: Option<String> = match (
        req.api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty()),
        req.provider_id,
    ) {
        (Some(k), _) => Some(k.to_string()),
        (None, Some(pid)) => match config::find_provider(&st.pool, pid)
            .await
            .map_err(AppError::internal)?
        {
            Some(p) => crate::crypto::decrypt(&p.api_key_encrypted, &st.cfg.master_key).ok(),
            None => None,
        },
        (None, None) => None,
    };
    let models = fetch_upstream_models(&st.client, base, key.as_deref())
        .await
        .map_err(AppError::BadRequest)?;
    // 测试的是已保存的供应商 → 把拉到的模型同步进模型库
    if let Some(pid) = req.provider_id {
        config::replace_provider_models(&st.pool, pid, &models)
            .await
            .map_err(AppError::internal)?;
        // 自动兜底路由立即生效（无需等 30s 周期重载）；失败必须暴露：
        // DB 已变更但内存未更新，静默会长期不一致
        st.reload().await.map_err(AppError::internal)?;
        audit::log(
            &st.pool,
            Some(admin.0.id),
            "provider.models.sync",
            Some("provider"),
            Some(pid),
            Some(json!({"models": models.len()})),
        )
        .await
        .map_err(AppError::internal)?;
    }
    Ok(Json(json!({ "models": models, "count": models.len() })).into_response())
}

// ---------- 模型库 ----------

async fn list_models(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let models = config::list_models(&st.pool)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({ "models": models })).into_response())
}

async fn delete_model(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if !config::delete_model(&st.pool, id)
        .await
        .map_err(AppError::internal)?
    {
        return Err(AppError::BadRequest("model not found".into()));
    }
    // 删除的可能是已禁用行：即时刷新禁用集合，避免残留条目误拦指向它的路由
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "model.delete",
        Some("model"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({ "deleted": true })).into_response())
}

#[derive(Deserialize)]
struct ModelPatch {
    enabled: bool,
}

/// 模型启停：禁用后该 (供应商, 模型) 不再作为路由候选、不在 /v1/models 列出；
/// 路由规则与价格配置不受影响。reload 即时生效。
async fn update_model(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
    Json(req): Json<ModelPatch>,
) -> Result<Response, AppError> {
    let model = config::set_model_enabled(&st.pool, id, req.enabled)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::BadRequest("model not found".into()))?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        if req.enabled {
            "model.enable"
        } else {
            "model.disable"
        },
        Some("model"),
        Some(id),
        Some(json!({ "provider_id": model.provider_id, "model_id": model.model_id })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({ "model": model })).into_response())
}

/// 对全部启用中的供应商并发拉取 /models 并写入模型库；
/// 单个失败不影响其余，汇总返回。
async fn refresh_models(State(st): State<AppState>, admin: Admin) -> Result<Response, AppError> {
    let providers = config::list_providers(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let enabled: Vec<config::AdminProvider> = providers.into_iter().filter(|p| p.enabled).collect();

    let mut updated = Vec::new();
    let mut failed = Vec::new();
    let tasks: Vec<_> = enabled
        .iter()
        .map(|p| {
            let st = st.clone();
            let p = p.clone();
            tokio::spawn(async move {
                let base = p.base_url.trim_end_matches('/').to_string();
                let key = crate::crypto::decrypt(&p.api_key_encrypted, &st.cfg.master_key).ok();
                let models = fetch_upstream_models(&st.client, &base, key.as_deref()).await;
                (p.id, p.name, models)
            })
        })
        .collect();
    for task in tasks {
        match task.await {
            Ok((pid, name, Ok(models))) => {
                let count = config::replace_provider_models(&st.pool, pid, &models)
                    .await
                    .map_err(AppError::internal)?;
                // 自动兜底路由立即生效（无需等 30s 周期重载）；失败必须暴露
                st.reload().await.map_err(AppError::internal)?;
                audit::log(
                    &st.pool,
                    Some(admin.0.id),
                    "provider.models.sync",
                    Some("provider"),
                    Some(pid),
                    Some(json!({"models": count})),
                )
                .await
                .map_err(AppError::internal)?;
                updated.push(json!({ "provider_id": pid, "provider_name": name, "count": count }));
            }
            Ok((pid, name, Err(err))) => {
                failed.push(json!({ "provider_id": pid, "provider_name": name, "error": err }));
            }
            Err(_) => {}
        }
    }
    Ok(Json(json!({ "updated": updated, "failed": failed })).into_response())
}

// ---------- 模型测试 ----------

/// 模型测试请求：model_id 缺省 = 测试全部
#[derive(Deserialize, Default)]
struct ModelTestReq {
    model_id: Option<i64>,
}

/// 单模型测试结果
#[derive(serde::Serialize)]
struct ModelTestResult {
    model_id: i64,
    model: String,
    provider_id: i64,
    provider_name: String,
    ok: bool,
    latency_ms: Option<i64>,
    error: Option<String>,
}

/// 对单个模型发最小 chat 请求验证可用性；超时取 min(供应商 timeout_ms, 20s)
async fn test_model_once(
    client: &reqwest::Client,
    provider: &config::AdminProvider,
    key: &str,
    model: &str,
) -> Result<i64, String> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = serde_json::to_vec(&serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    }))
    .unwrap_or_default();
    let started = std::time::Instant::now();
    let timeout = Duration::from_millis((provider.timeout_ms.max(1_000) as u64).min(20_000));
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let latency = started.elapsed().as_millis() as i64;
    if status.is_success() {
        Ok(latency)
    } else {
        Err(format!(
            "upstream {status}: {}",
            text.chars().take(300).collect::<String>()
        ))
    }
}

/// 测试模型可用性：单模型（body 带 model_id）或全部（并发上限 5，逐模型返回结果）
async fn test_models(
    State(st): State<AppState>,
    admin: Admin,
    payload: Option<Json<ModelTestReq>>,
) -> Result<Response, AppError> {
    let model_id = payload.and_then(|p| p.model_id);
    let models = config::list_models(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let providers = config::list_providers(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let targets: Vec<config::AdminModel> = match model_id {
        Some(id) => {
            let m = models
                .iter()
                .find(|m| m.id == id)
                .cloned()
                .ok_or_else(|| AppError::BadRequest("model not found".into()))?;
            vec![m]
        }
        // 测试全部跳过已禁用模型（管理员已明确停用，不再耗上游调用）；
        // 单模型测试不受限——显式指定即视为诊断意图
        None => models.into_iter().filter(|m| m.enabled).collect(),
    };

    let semaphore = Arc::new(tokio::sync::Semaphore::new(5));
    let mut handles = Vec::with_capacity(targets.len());
    for m in targets {
        let st = st.clone();
        let providers = providers.clone();
        let sem = semaphore.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await;
            let result =
                |ok: bool, latency_ms: Option<i64>, error: Option<String>| -> ModelTestResult {
                    ModelTestResult {
                        model_id: m.id,
                        model: m.model_id.clone(),
                        provider_id: m.provider_id,
                        provider_name: m.provider_name.clone(),
                        ok,
                        latency_ms,
                        error,
                    }
                };
            let Some(provider) = providers.iter().find(|p| p.id == m.provider_id) else {
                return result(false, None, Some("provider not found".into()));
            };
            if !provider.enabled {
                return result(false, None, Some("provider disabled".into()));
            }
            let key = match crate::crypto::decrypt(&provider.api_key_encrypted, &st.cfg.master_key)
            {
                Ok(k) => k,
                Err(e) => return result(false, None, Some(format!("key decrypt failed: {e}"))),
            };
            match test_model_once(&st.client, provider, &key, &m.model_id).await {
                Ok(latency) => result(true, Some(latency), None),
                Err(err) => result(false, None, Some(err)),
            }
        }));
    }
    let mut results = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(r) = h.await {
            results.push(r);
        }
    }
    results.sort_by(|a, b| {
        a.provider_name
            .cmp(&b.provider_name)
            .then_with(|| a.model.cmp(&b.model))
    });
    let ok = results.iter().filter(|r| r.ok).count();
    let failed = results.len() - ok;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "model.test",
        Some("model"),
        model_id,
        Some(json!({ "total": results.len(), "ok": ok, "failed": failed })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({ "results": results, "ok": ok, "failed": failed })).into_response())
}

// ---------- 供应商 ----------

#[derive(Deserialize)]
struct ProviderReq {
    name: String,
    #[serde(default = "default_api_type")]
    api_type: String,
    base_url: String,
    /// 明文 API Key（仅创建/更新时提交；不传则保留原值）
    api_key: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: i32,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "empty_object")]
    extra_body: serde_json::Value,
    /// 认证形态：bearer（默认）/ x-api-key
    #[serde(default = "default_auth_scheme")]
    auth_scheme: String,
    /// 渠道级静态附加请求头（覆盖同名透传头）
    #[serde(default = "empty_object")]
    extra_headers: serde_json::Value,
    /// H6：渠道是否支持图像输入（false = 发前主动降级图片）
    #[serde(default = "default_true")]
    supports_images: bool,
}

fn default_api_type() -> String {
    "openai".into()
}
fn default_timeout() -> i32 {
    120_000
}
fn default_true() -> bool {
    true
}
fn default_auth_scheme() -> String {
    "bearer".into()
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

/// extra_body 配置校验：必须是 JSON 对象、不含网关管理的顶层字段、≤ 8KB
fn validate_extra_body(v: &serde_json::Value) -> Result<(), AppError> {
    let Some(obj) = v.as_object() else {
        return Err(AppError::BadRequest(
            "extra_body must be a JSON object".into(),
        ));
    };
    for k in obj.keys() {
        if matches!(k.as_str(), "model" | "stream" | "stream_options") {
            return Err(AppError::BadRequest(format!(
                "extra_body top-level field '{k}' is managed by the gateway (model/stream/stream_options) and cannot be overridden"
            )));
        }
    }
    if serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX) > 8 * 1024 {
        return Err(AppError::BadRequest(
            "extra_body too large (max 8KB)".into(),
        ));
    }
    Ok(())
}

/// auth_scheme / extra_headers 配置校验：scheme 限枚举；headers 必须是
/// 值全为字符串的 JSON 对象（header 名合法字符）、不含网关管理的认证头、≤ 4KB
fn validate_extra_headers(scheme: &str, headers: &serde_json::Value) -> Result<(), AppError> {
    let scheme = scheme.trim();
    if !matches!(scheme, "bearer" | "x-api-key") {
        return Err(AppError::BadRequest(
            "auth_scheme must be 'bearer' or 'x-api-key'".into(),
        ));
    }
    let Some(obj) = headers.as_object() else {
        return Err(AppError::BadRequest(
            "extra_headers must be a JSON object".into(),
        ));
    };
    for (k, v) in obj {
        // 网关自管头：认证/内容类型/请求追踪不允许被渠道配置覆盖
        let lower = k.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization"
                | "x-api-key"
                | "content-type"
                | "x-request-id"
                | "content-length"
                | "host"
        ) {
            return Err(AppError::BadRequest(format!(
                "extra_headers field '{k}' is managed by the gateway and cannot be set"
            )));
        }
        if !v.is_string() {
            return Err(AppError::BadRequest(format!(
                "extra_headers field '{k}' must be a string value"
            )));
        }
    }
    if serde_json::to_vec(headers)
        .map(|b| b.len())
        .unwrap_or(usize::MAX)
        > 4 * 1024
    {
        return Err(AppError::BadRequest(
            "extra_headers too large (max 4KB)".into(),
        ));
    }
    Ok(())
}

async fn encrypt_key(st: &AppState, plain: Option<&str>) -> Result<Option<String>, AppError> {
    match plain {
        Some(p) if p.trim().is_empty() => Ok(None),
        Some(p) => crate::crypto::encrypt(p.trim().as_bytes(), &st.cfg.master_key)
            .map(Some)
            .map_err(AppError::internal),
        None => Ok(None),
    }
}

async fn list_providers(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let mut list = config::list_providers(&st.pool)
        .await
        .map_err(AppError::internal)?;
    // 脱敏后返回（明文仅创建/更新时短暂可见）
    for p in list.iter_mut() {
        p.api_key_encrypted = mask_key(&p.api_key_encrypted);
    }
    Ok(Json(json!({"providers": list})).into_response())
}

async fn create_provider(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<ProviderReq>,
) -> Result<Response, AppError> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::BadRequest(
            "provider name must be 1-64 chars".into(),
        ));
    }
    validate_extra_body(&req.extra_body)?;
    validate_extra_headers(&req.auth_scheme, &req.extra_headers)?;
    let api_key = req
        .api_key
        .as_deref()
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| AppError::BadRequest("api_key is required".into()))?;
    let enc = crate::crypto::encrypt(api_key.as_bytes(), &st.cfg.master_key)
        .map_err(AppError::internal)?;
    let provider = config::create_provider(
        &st.pool,
        name,
        req.api_type.trim(),
        req.base_url.trim(),
        &enc,
        req.timeout_ms,
        req.enabled,
        &req.extra_body,
        req.auth_scheme.trim(),
        &req.extra_headers,
        req.supports_images,
    )
    .await
    .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "provider.create",
        Some("provider"),
        Some(provider.id),
        Some(json!({"name": name, "extra_body": req.extra_body})),
    )
    .await
    .map_err(AppError::internal)?;
    let mut p = provider;
    p.api_key_encrypted = mask_key(&p.api_key_encrypted);
    Ok(Json(json!({"provider": p})).into_response())
}
#[derive(Deserialize)]
struct ProviderPatch {
    name: Option<String>,
    api_type: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    timeout_ms: Option<i32>,
    enabled: Option<bool>,
    /// None/缺省 = 不修改；空对象 = 清空
    #[serde(default)]
    extra_body: Option<serde_json::Value>,
    #[serde(default)]
    auth_scheme: Option<String>,
    /// None/缺省 = 不修改；空对象 = 清空
    #[serde(default)]
    extra_headers: Option<serde_json::Value>,
    /// H6：渠道是否支持图像输入
    #[serde(default)]
    supports_images: Option<bool>,
}

async fn update_provider(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
    Json(req): Json<ProviderPatch>,
) -> Result<Response, AppError> {
    if let Some(name) = req.name.as_deref() {
        if name.trim().is_empty() || name.trim().len() > 64 {
            return Err(AppError::BadRequest(
                "provider name must be 1-64 chars".into(),
            ));
        }
    }
    if let Some(v) = req.extra_body.as_ref() {
        validate_extra_body(v)?;
    }
    if let Some(scheme) = req.auth_scheme.as_deref() {
        validate_extra_headers(
            scheme,
            req.extra_headers.as_ref().unwrap_or(&serde_json::json!({})),
        )?;
    } else if let Some(v) = req.extra_headers.as_ref() {
        validate_extra_headers("bearer", v)?;
    }
    let enc = encrypt_key(&st, req.api_key.as_deref()).await?;
    let provider = config::update_provider(
        &st.pool,
        id,
        req.name.as_deref().map(str::trim),
        req.api_type.as_deref().map(str::trim),
        req.base_url.as_deref().map(str::trim),
        enc.as_deref(),
        req.timeout_ms,
        req.enabled,
        req.extra_body.as_ref(),
        req.auth_scheme.as_deref().map(str::trim),
        req.extra_headers.as_ref(),
        req.supports_images,
    )
    .await
    .map_err(AppError::internal)?
    .ok_or_else(|| AppError::BadRequest("provider not found".into()))?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "provider.update",
        Some("provider"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    let mut p = provider;
    p.api_key_encrypted = mask_key(&p.api_key_encrypted);
    Ok(Json(json!({"provider": p})).into_response())
}

// ---------- 路由规则 ----------

#[derive(Deserialize)]
struct RouteReq {
    model_pattern: String,
    provider_id: i64,
    #[serde(default = "default_priority")]
    priority: i32,
    #[serde(default)]
    fallback_ids: Vec<i64>,
    /// 上游实际模型名（映射）；空/NULL = 透传客户端模型名
    #[serde(default)]
    upstream_model: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "empty_object")]
    extra_body: serde_json::Value,
    /// 模型级开关：Chat 请求 system 消息收拢到头部（qwen3「system message
    /// must be at the beginning」/ MiniMax 类严格上游）
    #[serde(default)]
    strict_system_head: bool,
    /// 多条 system 收拢时是否合并为单条（true = 合并，MiniMax 类；
    /// false = 仅前移保持多条独立，qwen3 类；strict_system_head 关闭时无效果）
    #[serde(default = "default_true")]
    system_head_merge: bool,
    /// H3：reasoning_effort 值域钳制模式（缺省/空 = passthrough）
    #[serde(default)]
    reasoning_effort_mode: Option<String>,
    /// H3：thinking 形态（缺省/空 = 剥离；thinking_param / reasoning_split / enable_thinking）
    #[serde(default)]
    thinking_form: Option<String>,
    /// H2：Responses 方言字段透传白名单（逗号分隔；缺省/空 = 全部剥离）
    #[serde(default)]
    responses_passthrough_fields: Option<String>,
}

fn default_priority() -> i32 {
    100
}

/// 归一化 upstream_model：trim、空串 → None、超长报错
fn normalize_upstream_model(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(raw) = raw else { return Ok(None) };
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if s.len() > 128 {
        return Err(AppError::BadRequest(
            "upstream_model must be 1-128 chars".into(),
        ));
    }
    Ok(Some(s.to_string()))
}

/// H3：reasoning_effort_mode 归一化（trim、空串 → None、非法值报错）
fn normalize_effort_mode(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(raw) = raw else { return Ok(None) };
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match s {
        "passthrough" | "deepseek" | "low_high" | "openrouter" => Ok(Some(s.to_string())),
        _ => Err(AppError::BadRequest(format!(
            "reasoning_effort_mode must be one of passthrough/deepseek/low_high/openrouter, got '{s}'"
        ))),
    }
}

/// H3：thinking_form 归一化（trim、空串 → None、非法值报错）
fn normalize_thinking_form(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(raw) = raw else { return Ok(None) };
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match s {
        "thinking_param" | "reasoning_split" | "enable_thinking" => Ok(Some(s.to_string())),
        _ => Err(AppError::BadRequest(format!(
            "thinking_form must be one of thinking_param/reasoning_split/enable_thinking, got '{s}'"
        ))),
    }
}

/// H2：responses_passthrough_fields 归一化（trim、空串 → None、非法字段报错）
fn normalize_passthrough_fields(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(raw) = raw else { return Ok(None) };
    let mut fields: Vec<String> = Vec::new();
    for f in raw.split(',') {
        let f = f.trim();
        if f.is_empty() {
            continue;
        }
        if !matches!(
            f,
            "store" | "safety_identifier" | "prompt_cache_retention" | "prompt_cache_key"
        ) {
            return Err(AppError::BadRequest(format!(
                "responses_passthrough_fields contains unsupported field '{f}' \
                 (allowed: store, safety_identifier, prompt_cache_retention, prompt_cache_key)"
            )));
        }
        fields.push(f.to_string());
    }
    fields.sort();
    fields.dedup();
    if fields.is_empty() {
        Ok(None)
    } else {
        Ok(Some(fields.join(",")))
    }
}
async fn list_routes(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let routes = config::list_routes(&st.pool)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({"routes": routes})).into_response())
}

async fn create_route(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<RouteReq>,
) -> Result<Response, AppError> {
    let pattern = req.model_pattern.trim();
    if pattern.is_empty() || pattern.len() > 128 {
        return Err(AppError::BadRequest(
            "model_pattern must be 1-128 chars".into(),
        ));
    }
    validate_extra_body(&req.extra_body)?;
    // 主供应商与 fallback 必须存在（此前缺口：FK 违例直接 500）
    if config::find_provider(&st.pool, req.provider_id)
        .await
        .map_err(AppError::internal)?
        .is_none()
    {
        return Err(AppError::BadRequest(format!(
            "provider {} not found",
            req.provider_id
        )));
    }
    for pid in &req.fallback_ids {
        if config::find_provider(&st.pool, *pid)
            .await
            .map_err(AppError::internal)?
            .is_none()
        {
            return Err(AppError::BadRequest(format!(
                "fallback provider {pid} not found"
            )));
        }
    }
    let upstream_model = normalize_upstream_model(req.upstream_model.as_deref())?;
    let effort_mode = normalize_effort_mode(req.reasoning_effort_mode.as_deref())?;
    let thinking_form = normalize_thinking_form(req.thinking_form.as_deref())?;
    let passthrough = normalize_passthrough_fields(req.responses_passthrough_fields.as_deref())?;
    let route = config::create_route(
        &st.pool,
        pattern,
        req.provider_id,
        req.priority,
        &req.fallback_ids,
        upstream_model.as_deref(),
        req.enabled,
        &req.extra_body,
        req.strict_system_head,
        req.system_head_merge,
        effort_mode.as_deref(),
        thinking_form.as_deref(),
        passthrough.as_deref(),
    )
    .await
    .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "route.create",
        Some("route"),
        Some(route.id),
        Some(json!({"model_pattern": pattern})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"route": route})).into_response())
}

#[derive(Deserialize)]
struct RoutePatch {
    model_pattern: Option<String>,
    provider_id: Option<i64>,
    priority: Option<i32>,
    fallback_ids: Option<Vec<i64>>,
    /// 外层 None = 不改动；内层 None/null = 清空映射；Some = 设置
    #[serde(default)]
    upstream_model: Option<Option<String>>,
    enabled: Option<bool>,
    /// None/缺省 = 不修改；空对象 = 清空
    #[serde(default)]
    extra_body: Option<serde_json::Value>,
    /// None/缺省 = 不修改
    #[serde(default)]
    strict_system_head: Option<bool>,
    /// 多条 system 收拢时是否合并为单条（None/缺省 = 不修改；
    /// strict_system_head 关闭时无效果）
    #[serde(default)]
    system_head_merge: Option<bool>,
    /// H3：外层 None = 不改动；内层 None/null = 清空回 passthrough；Some = 设置
    #[serde(default)]
    reasoning_effort_mode: Option<Option<String>>,
    /// H3：thinking 形态（外层 None = 不改动；内层 None = 清空）
    #[serde(default)]
    thinking_form: Option<Option<String>>,
    /// H2：方言字段透传白名单（外层 None = 不改动；内层 None = 清空）
    #[serde(default)]
    responses_passthrough_fields: Option<Option<String>>,
}

async fn update_route(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
    Json(req): Json<RoutePatch>,
) -> Result<Response, AppError> {
    // 与 create 一致：pattern 长度/空值校验（此前缺口可写入空 pattern）
    if let Some(p) = req.model_pattern.as_deref().map(str::trim) {
        if p.is_empty() || p.len() > 128 {
            return Err(AppError::BadRequest(
                "model_pattern must be 1-128 chars".into(),
            ));
        }
    }
    if let Some(v) = req.extra_body.as_ref() {
        validate_extra_body(v)?;
    }
    if let Some(pid) = req.provider_id {
        if config::find_provider(&st.pool, pid)
            .await
            .map_err(AppError::internal)?
            .is_none()
        {
            return Err(AppError::BadRequest(format!("provider {pid} not found")));
        }
    }
    if let Some(ids) = req.fallback_ids.as_deref() {
        for pid in ids {
            if config::find_provider(&st.pool, *pid)
                .await
                .map_err(AppError::internal)?
                .is_none()
            {
                return Err(AppError::BadRequest(format!(
                    "fallback provider {pid} not found"
                )));
            }
        }
    }
    let upstream_model = req
        .upstream_model
        .map(|v| normalize_upstream_model(v.as_deref()))
        .transpose()?;
    let effort_mode = req
        .reasoning_effort_mode
        .map(|v| normalize_effort_mode(v.as_deref()))
        .transpose()?;
    let thinking_form = req
        .thinking_form
        .map(|v| normalize_thinking_form(v.as_deref()))
        .transpose()?;
    let passthrough = req
        .responses_passthrough_fields
        .map(|v| normalize_passthrough_fields(v.as_deref()))
        .transpose()?;
    let route = config::update_route(
        &st.pool,
        id,
        req.model_pattern.as_deref().map(str::trim),
        req.provider_id,
        req.priority,
        req.fallback_ids,
        upstream_model,
        req.enabled,
        req.extra_body.as_ref(),
        req.strict_system_head,
        req.system_head_merge,
        effort_mode,
        thinking_form,
        passthrough,
    )
    .await
    .map_err(AppError::internal)?
    .ok_or_else(|| AppError::BadRequest("route not found".into()))?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "route.update",
        Some("route"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"route": route})).into_response())
}

// ---------- 限流规则 ----------

#[derive(Deserialize)]
struct RateLimitReq {
    scope: String,
    scope_id: Option<i64>,
    /// 模型限定（空/缺省 = 所有模型；客户端模型名精确匹配）
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "default_rpm")]
    rpm: i32,
    #[serde(default = "default_burst")]
    burst: i32,
    /// 并发上限（在途请求数；0 = 不限）
    #[serde(default)]
    concurrency: i32,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// 归一化模型限定：trim、空串 → None；超长（>128，与表列宽一致）→ 400
fn normalize_rule_model(model: Option<&str>) -> Result<Option<String>, AppError> {
    let m = model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    if let Some(m) = &m {
        if m.len() > 128 {
            return Err(AppError::BadRequest("model must be <= 128 chars".into()));
        }
    }
    Ok(m)
}

fn default_rpm() -> i32 {
    60
}
fn default_burst() -> i32 {
    10
}

async fn list_rate_limits(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let rules = config::list_rate_rules(&st.pool)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({"rules": rules})).into_response())
}

/// 定向限制守卫：用户/Key 维度的限流·并发规则不得指向受保护管理员——
/// 否则绕过用户管理守卫，直接锁死唯一管理入口的代理访问（429）。
async fn ensure_rule_target_allowed(
    st: &AppState,
    scope: &str,
    scope_id: Option<i64>,
) -> Result<(), AppError> {
    match scope {
        "user" => {
            if let Some(sid) = scope_id {
                crate::api::console::ensure_not_protected_by_id(st, sid).await?;
            }
        }
        "api_key" => {
            if let Some(kid) = scope_id {
                if let Some(uid) = crate::store::keys::find_owner_user_id(&st.pool, kid)
                    .await
                    .map_err(AppError::internal)?
                {
                    crate::api::console::ensure_not_protected_by_id(st, uid).await?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn create_rate_limit(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<RateLimitReq>,
) -> Result<Response, AppError> {
    let scope = req.scope.trim().to_string();
    if !matches!(scope.as_str(), "global" | "user" | "api_key") {
        return Err(AppError::BadRequest(
            "scope must be global | user | api_key".into(),
        ));
    }
    ensure_rule_target_allowed(&st, &scope, req.scope_id).await?;
    if req.rpm <= 0 || req.burst <= 0 {
        return Err(AppError::BadRequest("rpm and burst must be > 0".into()));
    }
    if req.concurrency < 0 {
        return Err(AppError::BadRequest("concurrency must be >= 0 (0 = unlimited)".into()));
    }
    let model = normalize_rule_model(req.model.as_deref())?;
    let rule = config::create_rate_rule(
        &st.pool,
        &scope,
        req.scope_id,
        model.as_deref(),
        req.rpm,
        req.burst,
        req.concurrency,
        req.enabled,
    )
    .await
    .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "rate_limit.create",
        Some("rate_limit"),
        Some(rule.id),
        Some(json!({"scope": scope, "model": model})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"rule": rule})).into_response())
}

#[derive(Deserialize)]
struct RateLimitPatch {
    scope: Option<String>,
    /// Some(v) 更新 / Some(None) 清空 / None 不变
    scope_id: Option<Option<i64>>,
    /// Some(Some(v)) 设置 / Some(None) 或空串 清空 / None 不变
    model: Option<Option<String>>,
    rpm: Option<i32>,
    burst: Option<i32>,
    concurrency: Option<i32>,
    enabled: Option<bool>,
}

async fn update_rate_limit(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
    Json(req): Json<RateLimitPatch>,
) -> Result<Response, AppError> {
    // 与 create 一致：更新同样校验（此前缺口可导致 rpm/burst <= 0 → 限流器除零/常拒）
    if let Some(scope) = req.scope.as_deref().map(str::trim) {
        if !matches!(scope, "global" | "user" | "api_key") {
            return Err(AppError::BadRequest(
                "scope must be global | user | api_key".into(),
            ));
        }
    }
    if req.rpm.is_some_and(|v| v <= 0) || req.burst.is_some_and(|v| v <= 0) {
        return Err(AppError::BadRequest("rpm and burst must be > 0".into()));
    }
    let old = config::list_rate_rules(&st.pool)
        .await
        .map_err(AppError::internal)?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| AppError::BadRequest("rate limit rule not found".into()))?;
    let final_scope = req
        .scope
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| old.scope.clone());
    let final_scope_id = match req.scope_id {
        Some(v) => v,
        None => old.scope_id,
    };
    ensure_rule_target_allowed(&st, &final_scope, final_scope_id).await?;
    let model = match req.model {
        None => None,
        Some(m) => Some(normalize_rule_model(m.as_deref())?),
    };
    let rule = config::update_rate_rule(
        &st.pool,
        id,
        req.scope.as_deref().map(str::trim),
        req.scope_id,
        model.as_ref().map(|m| m.as_deref()),
        req.rpm,
        req.burst,
        req.concurrency,
        req.enabled,
    )
    .await
    .map_err(AppError::internal)?
    .ok_or_else(|| AppError::BadRequest("rate limit rule not found".into()))?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "rate_limit.update",
        Some("rate_limit"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"rule": rule})).into_response())
}

async fn delete_rate_limit(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if !config::delete_rate_rule(&st.pool, id)
        .await
        .map_err(AppError::internal)?
    {
        return Err(AppError::BadRequest("rate limit rule not found".into()));
    }
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "rate_limit.delete",
        Some("rate_limit"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})).into_response())
}

// ---------- 用户配额 ----------

async fn list_quotas(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let quotas = config::list_quotas(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let quotas: Vec<serde_json::Value> = quotas
        .into_iter()
        .map(|q| {
            let mut v = serde_json::to_value(&q).map_err(AppError::internal)?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "protected".into(),
                    json!(crate::api::console::is_protected_admin(&st.cfg, &q.username)),
                );
            }
            Ok(v)
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(Json(json!({"quotas": quotas})).into_response())
}

#[derive(Deserialize)]
struct QuotaReq {
    /// 缺省清空配额（None 保留原值；null 清空）
    #[serde(default)]
    monthly_token_quota: Option<Option<i64>>,
    #[serde(default)]
    monthly_cost_quota: Option<Option<f64>>,
    billing_day: Option<i32>,
    notify_percent: Option<i32>,
    enabled: Option<bool>,
}

async fn upsert_quota(
    State(st): State<AppState>,
    admin: Admin,
    Path(user_id): Path<i64>,
    Json(req): Json<QuotaReq>,
) -> Result<Response, AppError> {
    if let Some(Some(v)) = req.monthly_token_quota {
        if v < 0 {
            return Err(AppError::BadRequest("quota must be >= 0".into()));
        }
    }
    if let Some(Some(v)) = req.monthly_cost_quota {
        if v < 0.0 {
            return Err(AppError::BadRequest("quota must be >= 0".into()));
        }
    }
    if let Some(d) = req.billing_day {
        if !(1..=28).contains(&d) {
            return Err(AppError::BadRequest("billing_day must be 1-28".into()));
        }
    }
    crate::api::console::ensure_not_protected_by_id(&st, user_id).await?;
    let quota = config::upsert_quota(
        &st.pool,
        user_id,
        req.monthly_token_quota,
        req.monthly_cost_quota,
        req.billing_day,
        req.notify_percent,
        req.enabled,
    )
    .await
    .map_err(AppError::internal)?
    .ok_or_else(|| AppError::BadRequest("user not found".into()))?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "quota.update",
        Some("user"),
        Some(user_id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"quota": quota})).into_response())
}

// ---------- 模型价格 ----------

#[derive(Deserialize)]
struct PriceReq {
    model: String,
    input_price_per_m: Option<f64>,
    output_price_per_m: Option<f64>,
    #[serde(default = "default_currency")]
    currency: String,
    /// RFC3339 或 YYYY-MM-DD；缺省今天
    effective_from: Option<chrono::NaiveDate>,
}

fn default_currency() -> String {
    "CNY".into()
}

async fn list_prices(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let prices = config::list_prices(&st.pool)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({"prices": prices})).into_response())
}

async fn create_price(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<PriceReq>,
) -> Result<Response, AppError> {
    let model = req.model.trim();
    if model.is_empty() || model.len() > 128 {
        return Err(AppError::BadRequest("model must be 1-128 chars".into()));
    }
    let price = config::create_price(
        &st.pool,
        model,
        req.input_price_per_m,
        req.output_price_per_m,
        req.currency.trim(),
        req.effective_from
            .unwrap_or_else(|| chrono::Local::now().date_naive()),
    )
    .await
    .map_err(AppError::internal)?;
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "price.create",
        Some("price"),
        Some(price.id),
        Some(json!({"model": model})),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"price": price})).into_response())
}

async fn delete_price(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if !config::delete_price(&st.pool, id)
        .await
        .map_err(AppError::internal)?
    {
        return Err(AppError::BadRequest("price not found".into()));
    }
    st.reload().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "price.delete",
        Some("price"),
        Some(id),
        None,
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})).into_response())
}

// ---------- 系统设置（LDAP） ----------

/// GET 返回当前生效配置；bind 密码永不回显，仅暴露 has_password
#[derive(serde::Serialize)]
struct LdapSettingsResp {
    url: String,
    starttls: bool,
    bind_dn: Option<String>,
    base_dn: String,
    user_filter: String,
    admin_groups: Vec<String>,
    has_password: bool,
}

/// PUT / test 请求体；bind_password 传空串 = 不修改（test 时回退现有密码）
#[derive(Deserialize)]
struct LdapSettingsReq {
    url: String,
    starttls: bool,
    #[serde(default)]
    bind_dn: Option<String>,
    #[serde(default)]
    bind_password: String,
    base_dn: String,
    user_filter: String,
    #[serde(default)]
    admin_groups: Vec<String>,
}

/// 校验并整理 LDAP 配置；返回 (settings, 是否需要写入新密码)
fn validate_ldap_req(req: &LdapSettingsReq) -> Result<(LdapSettings, bool), AppError> {
    let url = req.url.trim().to_string();
    if !url.is_empty() && !(url.starts_with("ldap://") || url.starts_with("ldaps://")) {
        return Err(AppError::BadRequest(
            "LDAP URL 必须以 ldap:// 或 ldaps:// 开头".into(),
        ));
    }
    if url.len() > 255 {
        return Err(AppError::BadRequest("LDAP URL 过长".into()));
    }
    let user_filter = req.user_filter.trim().to_string();
    // 过滤器必须保留 {0} 占位符，否则所有用户搜索都失败且难排查
    if !url.is_empty() && !user_filter.contains("{0}") {
        return Err(AppError::BadRequest(
            "用户过滤器必须包含 {0} 占位符（登录名位置）".into(),
        ));
    }
    if user_filter.len() > 255 {
        return Err(AppError::BadRequest("用户过滤器过长".into()));
    }
    let admin_groups: Vec<String> = req
        .admin_groups
        .iter()
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    Ok((
        LdapSettings {
            url: url.clone(),
            starttls: req.starttls,
            bind_dn: req
                .bind_dn
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
            bind_password: (!req.bind_password.is_empty()).then(|| req.bind_password.clone()),
            base_dn: req.base_dn.trim().to_string(),
            user_filter,
            admin_groups,
        },
        !req.bind_password.is_empty(),
    ))
}

async fn get_ldap_settings(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let db = config::load_ldap_settings(&st.pool)
        .await
        .map_err(AppError::internal)?;
    let has_password = !db.ldap_bind_password_enc.is_empty()
        && crate::crypto::decrypt(&db.ldap_bind_password_enc, &st.cfg.master_key).is_ok();
    let current = st.ldap.read().clone();
    Ok(Json(json!(LdapSettingsResp {
        url: current.url,
        starttls: current.starttls,
        bind_dn: current.bind_dn,
        base_dn: current.base_dn,
        user_filter: current.user_filter,
        admin_groups: current.admin_groups,
        has_password,
    }))
    .into_response())
}

async fn put_ldap_settings(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<LdapSettingsReq>,
) -> Result<Response, AppError> {
    let (validated, new_password) = validate_ldap_req(&req)?;
    let enc = if new_password {
        Some(
            crate::crypto::encrypt(
                validated.bind_password.as_deref().unwrap_or("").as_bytes(),
                &st.cfg.master_key,
            )
            .map_err(AppError::internal)?,
        )
    } else {
        None
    };
    config::save_ldap_settings(
        &st.pool,
        &validated.url,
        validated.starttls,
        validated.bind_dn.as_deref().unwrap_or(""),
        enc.as_deref(),
        &validated.base_dn,
        &validated.user_filter,
        &validated.admin_groups,
    )
    .await
    .map_err(AppError::internal)?;
    st.reload_ldap().await.map_err(AppError::internal)?;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "settings.ldap.update",
        Some("settings"),
        None,
        Some(json!({
            "ldap_enabled": !validated.url.is_empty(),
            "admin_groups": validated.admin_groups.len()
        })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({"ok": true})).into_response())
}

/// 用表单实时值测试连接（未保存也可测）；密码留空回退已保存密码
async fn test_ldap_settings(
    State(st): State<AppState>,
    _a: Admin,
    Json(req): Json<LdapSettingsReq>,
) -> Result<Response, AppError> {
    let (mut validated, new_password) = validate_ldap_req(&req)?;
    if !new_password {
        let db = config::load_ldap_settings(&st.pool)
            .await
            .map_err(AppError::internal)?;
        if !db.ldap_bind_password_enc.is_empty() {
            validated.bind_password =
                crate::crypto::decrypt(&db.ldap_bind_password_enc, &st.cfg.master_key).ok();
        }
    }
    match crate::service::ldap::test_connection(&validated).await {
        Ok(msg) => Ok(Json(json!({"ok": true, "message": msg})).into_response()),
        Err(e) => Err(AppError::BadRequest(format!("LDAP 连接测试失败：{e}"))),
    }
}

// ---------- 系统设置（extra_body 全局开关） ----------

/// extra_body 合并全局开关：false = 保留所有配置但不合并进上游请求体
async fn get_extra_body_settings(
    State(st): State<AppState>,
    _a: Admin,
) -> Result<Response, AppError> {
    let enabled = *st.extra_body_enabled.read();
    Ok(Json(json!({ "enabled": enabled })).into_response())
}

#[derive(Deserialize)]
struct ExtraBodySettingsReq {
    enabled: bool,
}

async fn put_extra_body_settings(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<ExtraBodySettingsReq>,
) -> Result<Response, AppError> {
    config::save_extra_body_enabled(&st.pool, req.enabled)
        .await
        .map_err(AppError::internal)?;
    *st.extra_body_enabled.write() = req.enabled;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "settings.extra_body.update",
        Some("settings"),
        None,
        Some(json!({ "enabled": req.enabled })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({ "enabled": req.enabled })).into_response())
}

// ---------- 系统设置（自定义 Header） ----------

/// 上游请求头黑名单：网关代管的头（认证/方言协商/消息框架），全局设置
/// 覆盖会破坏代理语义 → 保存即 400。Connection 不在列（用户显式场景），
/// 仅对 HTTP/1.1 上游生效（HTTP/2 为连接级协议，无该头）
const UPSTREAM_HEADER_BLOCKLIST: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "content-type",
    "content-length",
    "transfer-encoding",
    "accept",
    "anthropic-version",
    "anthropic-beta",
];

/// 客户端响应头黑名单：消息框架/连接管理头由 HTTP 层（hyper）控制，
/// 手工注入会破坏响应解析
const RESPONSE_HEADER_BLOCKLIST: &[&str] = &[
    "host",
    "content-type",
    "content-length",
    "transfer-encoding",
    "connection",
];

/// 单组 header 配置上限
const HEADER_SETTINGS_MAX_ENTRIES: usize = 16;
/// 序列化上限（单组）
const HEADER_SETTINGS_MAX_BYTES: usize = 4 * 1024;

/// 校验并整理一组 header：JSON 对象 {name: value(字符串)}；
/// name/value 经 HeaderName/HeaderValue 解析校验，命中黑名单即 400。
/// 返回整理后的对象（name 去空白、空值项剔除）
fn validate_header_map(
    v: &serde_json::Value,
    blocklist: &[&str],
    label: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, AppError> {
    let Some(map) = v.as_object() else {
        return Err(AppError::BadRequest(format!(
            "{label} must be a JSON object"
        )));
    };
    if map.len() > HEADER_SETTINGS_MAX_ENTRIES {
        return Err(AppError::BadRequest(format!(
            "{label} 超过 {HEADER_SETTINGS_MAX_ENTRIES} 条上限"
        )));
    }
    let mut out = serde_json::Map::new();
    for (k, val) in map {
        let name = k.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let Some(value) = val.as_str() else {
            return Err(AppError::BadRequest(format!(
                "{label}['{k}'] 的值必须是字符串"
            )));
        };
        if blocklist.iter().any(|b| name.eq_ignore_ascii_case(b)) {
            return Err(AppError::BadRequest(format!(
                "header '{name}' 由网关管理，不能在 {label} 中设置"
            )));
        }
        name.parse::<axum::http::HeaderName>()
            .map_err(|e| AppError::BadRequest(format!("header 名 '{name}' 不合法：{e}")))?;
        axum::http::HeaderValue::from_str(value.trim())
            .map_err(|e| AppError::BadRequest(format!("header '{name}' 的值不合法：{e}")))?;
        out.insert(name, serde_json::Value::String(value.trim().to_string()));
    }
    if serde_json::to_vec(&out).unwrap_or_default().len() > HEADER_SETTINGS_MAX_BYTES {
        return Err(AppError::BadRequest(format!(
            "{label} 序列化后超过 {} 字节上限",
            HEADER_SETTINGS_MAX_BYTES
        )));
    }
    Ok(out)
}

async fn get_header_settings(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let current = st.custom_headers.read().clone();
    Ok(Json(serde_json::json!({
        "upstream_headers": current.upstream,
        "response_headers": current.response,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct HeaderSettingsReq {
    #[serde(default)]
    upstream_headers: serde_json::Value,
    #[serde(default)]
    response_headers: serde_json::Value,
}

/// 保存：校验 → 入库 → 运行时状态即时生效 → audit 留痕（周期 reload 兜底）。
/// null/缺省字段 = 清空该组
async fn put_header_settings(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<HeaderSettingsReq>,
) -> Result<Response, AppError> {
    // null/缺省字段 = 清空该组（非对象值仍 400）
    let upstream_v = serde_json::Value::Object(
        req.upstream_headers
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let response_v = serde_json::Value::Object(
        req.response_headers
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let upstream = validate_header_map(&upstream_v, UPSTREAM_HEADER_BLOCKLIST, "upstream_headers")?;
    let response = validate_header_map(&response_v, RESPONSE_HEADER_BLOCKLIST, "response_headers")?;
    let settings = config::HeaderSettings {
        upstream: upstream.clone(),
        response: response.clone(),
    };
    config::save_header_settings(
        &st.pool,
        &serde_json::Value::Object(upstream.clone()),
        &serde_json::Value::Object(response.clone()),
    )
    .await
    .map_err(AppError::internal)?;
    *st.custom_headers.write() = settings;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "settings.headers.update",
        Some("settings"),
        None,
        Some(json!({
            "upstream_count": upstream.len(),
            "response_count": response.len(),
        })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(json!({
        "upstream_headers": upstream,
        "response_headers": response,
    }))
    .into_response())
}

// ---------- API 端点管理（Response API × Anthropic Messages API） ----------

/// 单 API 端点视图（GET 设置 / PUT 后回显共用）
#[derive(serde::Serialize)]
struct ApiEndpointOut {
    path: &'static str,
    address: String,
    enabled: bool,
    visible: bool,
}

#[derive(serde::Serialize)]
struct ApiEndpointsResp {
    public_base: String,
    public_base_override: bool,
    responses: ApiEndpointOut,
    messages: ApiEndpointOut,
}

/// 组装端点视图（base 无尾斜杠）
fn endpoints_view(st: &AppState, headers: &HeaderMap) -> ApiEndpointsResp {
    let eps = st.api_endpoints.read();
    let base = crate::service::endpoints::public_base(st, headers);
    ApiEndpointsResp {
        public_base: base.clone(),
        public_base_override: st.cfg.public_base_url.is_some(),
        responses: ApiEndpointOut {
            path: "/v1/responses",
            address: format!("{base}/v1/responses"),
            enabled: eps.responses_enabled,
            visible: eps.responses_visible,
        },
        messages: ApiEndpointOut {
            path: "/v1/messages",
            address: format!("{base}/v1/messages"),
            enabled: eps.messages_enabled,
            visible: eps.messages_visible,
        },
    }
}

async fn get_api_endpoint_settings(
    State(st): State<AppState>,
    _a: Admin,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    Ok(Json(endpoints_view(&st, &headers)).into_response())
}

#[derive(Deserialize)]
struct ApiEndpointsReq {
    responses_enabled: bool,
    responses_visible: bool,
    messages_enabled: bool,
    messages_visible: bool,
}

/// 保存开关：入库 → 运行时状态即时生效 → audit 留痕（周期 reload 兜底）
async fn put_api_endpoint_settings(
    State(st): State<AppState>,
    admin: Admin,
    headers: HeaderMap,
    Json(req): Json<ApiEndpointsReq>,
) -> Result<Response, AppError> {
    let settings = config::ApiEndpointSettings {
        responses_enabled: req.responses_enabled,
        responses_visible: req.responses_visible,
        messages_enabled: req.messages_enabled,
        messages_visible: req.messages_visible,
    };
    config::save_api_endpoint_settings(&st.pool, &settings)
        .await
        .map_err(AppError::internal)?;
    *st.api_endpoints.write() = settings;
    audit::log(
        &st.pool,
        Some(admin.0.id),
        "api_endpoints.update",
        Some("settings"),
        None,
        Some(json!({
            "responses_enabled": req.responses_enabled,
            "responses_visible": req.responses_visible,
            "messages_enabled": req.messages_enabled,
            "messages_visible": req.messages_visible,
        })),
    )
    .await
    .map_err(AppError::internal)?;
    Ok(Json(endpoints_view(&st, &headers)).into_response())
}

/// 管理员 API 测试请求：走真实代理管线（跳过鉴权/限流上下文，不记账）
#[derive(Deserialize)]
struct ApiTestReq {
    /// "responses" | "messages"
    api: String,
    stream: bool,
    /// 完整请求体（必须含 model；stream 由本字段外的开关注入）
    body: serde_json::Value,
}

/// 测试响应体读取上限（2 MiB；超限截断标记）
const TEST_BODY_LIMIT: usize = 2 * 1024 * 1024;
/// body_preview 文本上限（字符）
const TEST_PREVIEW_CHARS: usize = 8000;
/// 测试整体超时（代理内部另有首字节/静默超时）
const TEST_TIMEOUT: Duration = Duration::from_secs(150);

fn app_error_status(e: &AppError) -> u16 {
    match e {
        AppError::WithRequestId { inner, .. } => app_error_status(inner),
        AppError::Auth(_) | AppError::Unauthorized(_) => 401,
        AppError::Forbidden(_) => 403,
        AppError::RateLimited(_)
        | AppError::ConcurrentLimited(_)
        | AppError::QuotaExceeded
        | AppError::PlanQuotaExceeded(..) => 429,
        AppError::BadRequest(_) => 400,
        AppError::Conflict(_) => 409,
        AppError::Internal(_) => 500,
        AppError::ServiceUnavailable(_) | AppError::UpstreamExhausted(_) => 503,
        AppError::BadGateway(_) => 502,
        AppError::UpstreamTimeout(_) => 504,
    }
}
async fn test_api_endpoint(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<ApiTestReq>,
) -> Result<Response, AppError> {
    let endpoint = match req.api.as_str() {
        "responses" => crate::service::proxy::Endpoint {
            api: "/v1/responses",
            upstream: "/responses",
            responses: true,
            anthropic: false,
        },
        "messages" => crate::service::proxy::Endpoint {
            api: "/v1/messages",
            upstream: "/messages",
            responses: false,
            anthropic: true,
        },
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown api '{other}' (expect 'responses' or 'messages')"
            )));
        }
    };
    let mut body = req.body;
    if !body.is_object() {
        return Err(AppError::BadRequest(
            "body must be a JSON object (the full request body for the API)".into(),
        ));
    }
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AppError::BadRequest("missing 'model' field in body".into()))?;
    body["stream"] = json!(req.stream);
    let bytes = axum::body::Bytes::from(body.to_string());
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );

    let started = Instant::now();
    let attempt = tokio::time::timeout(
        TEST_TIMEOUT,
        crate::service::proxy::proxy_test(
            &st,
            headers,
            bytes,
            endpoint,
            std::net::IpAddr::from([127, 0, 0, 1]),
        ),
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as i64;

    // 结果四元组：(status, content_type, error, body_preview)
    let (status_code, content_type, error, preview) = match attempt {
        Err(_elapsed) => (
            504,
            String::new(),
            format!("test timed out after {}s", TEST_TIMEOUT.as_secs()),
            String::new(),
        ),
        Ok(Err(e)) => (
            app_error_status(&e),
            String::new(),
            e.to_string(),
            String::new(),
        ),
        Ok(Ok(resp)) => {
            let (parts, resp_body) = resp.into_parts();
            let ct = parts
                .headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let status = parts.status.as_u16();
            let (bytes, truncated) = match axum::body::to_bytes(resp_body, TEST_BODY_LIMIT).await {
                Ok(b) => (b, false),
                Err(_) => (axum::body::Bytes::new(), true),
            };
            let mut preview = String::from_utf8_lossy(&bytes).into_owned();
            if preview.chars().count() > TEST_PREVIEW_CHARS {
                preview.truncate(TEST_PREVIEW_CHARS);
                preview.push_str("\n…(truncated)");
            } else if truncated {
                preview.push_str("\n…(body exceeded 2 MiB capture limit)");
            }
            (status, ct, String::new(), preview)
        }
    };
    let ok = (200..400).contains(&status_code);
    // 历史落库（预览截断）；失败仅告警不影响测试结果返回
    if let Err(e) = config::insert_api_test_result(
        &st.pool,
        match endpoint.responses {
            true => "responses",
            false => "messages",
        },
        Some(admin.0.id),
        &model,
        req.stream,
        status_code as i32,
        ok,
        latency_ms,
        &error,
        &preview,
    )
    .await
    {
        tracing::warn!(error = %e, "api test result persist failed");
    }
    if let Err(e) = audit::log(
        &st.pool,
        Some(admin.0.id),
        "api_endpoint.test",
        Some("settings"),
        None,
        Some(json!({
            "api": if endpoint.responses { "responses" } else { "messages" },
            "model": model,
            "stream": req.stream,
            "status_code": status_code,
            "latency_ms": latency_ms,
        })),
    )
    .await
    {
        tracing::warn!(error = %e, "api test audit failed");
    }
    Ok(Json(json!({
        "ok": ok,
        "api": if endpoint.responses { "responses" } else { "messages" },
        "model": model,
        "stream": req.stream,
        "status_code": status_code,
        "latency_ms": latency_ms,
        "error": error,
        "content_type": content_type,
        "body_preview": preview,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct ApiTestResultsParams {
    limit: Option<i64>,
}

/// 最近测试结果（时间倒序；limit 默认 20、上限 100）
async fn list_api_test_results(
    State(st): State<AppState>,
    _a: Admin,
    Query(params): Query<ApiTestResultsParams>,
) -> Result<Response, AppError> {
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let rows = config::list_api_test_results(&st.pool, limit)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(json!({ "results": rows })).into_response())
}
