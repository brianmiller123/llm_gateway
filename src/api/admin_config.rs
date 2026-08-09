//! 配置管理 API：供应商 / 路由规则 / 限流 / 配额 / 单价 CRUD。
//! 全部写操作：入库 → `reload()` 即时生效 → audit 留痕。仅管理员可访问。

use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, Path, State};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
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
            get(list_routes)
                .post(create_route)
                .layer(admin.clone()),
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
            get(list_prices)
                .post(create_price)
                .layer(admin.clone()),
        )
        .route("/api/admin/prices/{id}", delete(delete_price).layer(admin.clone()))
        .route("/api/admin/models", get(list_models).layer(admin.clone()))
        .route(
            "/api/admin/models/refresh",
            post(refresh_models).layer(admin.clone()),
        )
        .route(
            "/api/admin/models/test",
            post(test_models).layer(admin.clone()),
        )
        .route("/api/admin/models/{id}", delete(delete_model).layer(admin))
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
                            .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
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
    let mut detail = last_err
        .as_ref()
        .map(|e| e.to_string())
        .unwrap_or_default();
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
        req.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()),
        req.provider_id,
    ) {
        (Some(k), _) => Some(k.to_string()),
        (None, Some(pid)) => match config::find_provider(&st.pool, pid).await.map_err(AppError::internal)? {
            Some(p) => crate::crypto::decrypt(&p.api_key_encrypted, &st.cfg.master_key).ok(),
            None => None,
        },
        (None, None) => None,
    };
    let models = fetch_upstream_models(&st.client, base, key.as_deref()).await.map_err(AppError::BadRequest)?;
    // 测试的是已保存的供应商 → 把拉到的模型同步进模型库
    if let Some(pid) = req.provider_id {
        config::replace_provider_models(&st.pool, pid, &models)
            .await
            .map_err(AppError::internal)?;
        // 自动兜底路由立即生效（无需等 30s 周期重载）
        let _ = st.reload().await;
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
    let models = config::list_models(&st.pool).await.map_err(AppError::internal)?;
    Ok(Json(json!({ "models": models })).into_response())
}

async fn delete_model(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    if !config::delete_model(&st.pool, id).await.map_err(AppError::internal)? {
        return Err(AppError::BadRequest("model not found".into()));
    }
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
                // 自动兜底路由立即生效（无需等 30s 周期重载）
                let _ = st.reload().await;
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
    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));
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
        None => models,
    };

    let semaphore = Arc::new(tokio::sync::Semaphore::new(5));
    let mut handles = Vec::with_capacity(targets.len());
    for m in targets {
        let st = st.clone();
        let providers = providers.clone();
        let sem = semaphore.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await;
            let result = |ok: bool,
                          latency_ms: Option<i64>,
                          error: Option<String>|
             -> ModelTestResult {
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
                Err(e) => {
                    return result(
                        false,
                        None,
                        Some(format!("key decrypt failed: {e}")),
                    )
                }
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
        return Err(AppError::BadRequest("provider name must be 1-64 chars".into()));
    }
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
        Some(json!({"name": name})),
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
}

async fn update_provider(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
    Json(req): Json<ProviderPatch>,
) -> Result<Response, AppError> {
    if let Some(name) = req.name.as_deref() {
        if name.trim().is_empty() || name.trim().len() > 64 {
            return Err(AppError::BadRequest("provider name must be 1-64 chars".into()));
        }
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

async fn list_routes(State(st): State<AppState>, _a: Admin) -> Result<Response, AppError> {
    let routes = config::list_routes(&st.pool).await.map_err(AppError::internal)?;
    Ok(Json(json!({"routes": routes})).into_response())
}

async fn create_route(
    State(st): State<AppState>,
    admin: Admin,
    Json(req): Json<RouteReq>,
) -> Result<Response, AppError> {
    let pattern = req.model_pattern.trim();
    if pattern.is_empty() || pattern.len() > 128 {
        return Err(AppError::BadRequest("model_pattern must be 1-128 chars".into()));
    }
    let upstream_model = normalize_upstream_model(req.upstream_model.as_deref())?;
    let route = config::create_route(
        &st.pool,
        pattern,
        req.provider_id,
        req.priority,
        &req.fallback_ids,
        upstream_model.as_deref(),
        req.enabled,
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
}

async fn update_route(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
    Json(req): Json<RoutePatch>,
) -> Result<Response, AppError> {
    let upstream_model = req
        .upstream_model
        .map(|v| normalize_upstream_model(v.as_deref()))
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
    #[serde(default = "default_rpm")]
    rpm: i32,
    #[serde(default = "default_burst")]
    burst: i32,
    #[serde(default = "default_true")]
    enabled: bool,
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
    if req.rpm <= 0 || req.burst <= 0 {
        return Err(AppError::BadRequest("rpm and burst must be > 0".into()));
    }
    let rule = config::create_rate_rule(
        &st.pool,
        &scope,
        req.scope_id,
        req.rpm,
        req.burst,
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
        Some(json!({"scope": scope})),
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
    rpm: Option<i32>,
    burst: Option<i32>,
    enabled: Option<bool>,
}

async fn update_rate_limit(
    State(st): State<AppState>,
    admin: Admin,
    Path(id): Path<i64>,
    Json(req): Json<RateLimitPatch>,
) -> Result<Response, AppError> {
    let rule = config::update_rate_rule(
        &st.pool,
        id,
        req.scope.as_deref().map(str::trim),
        req.scope_id,
        req.rpm,
        req.burst,
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
    let quotas = config::list_quotas(&st.pool).await.map_err(AppError::internal)?;
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
    let prices = config::list_prices(&st.pool).await.map_err(AppError::internal)?;
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
        req.effective_from.unwrap_or_else(|| chrono::Local::now().date_naive()),
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
