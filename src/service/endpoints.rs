//! API 端点地址工具：对外公开基址推导。
//!
//! 优先级：`GATEWAY_PUBLIC_BASE_URL`（反代/自定义域名部署）> 请求头
//! `X-Forwarded-Proto`/`X-Forwarded-Host`（可信反代）> 部署形态启发式
//! （http_redirect 关闭 = 纯明文服务 → http；否则 https）+ Host 头。
//! 仅用于展示调用地址，非安全边界。

use axum::http::HeaderMap;

use crate::state::AppState;

/// 推导对外公开基址（无尾斜杠），如 `https://llm.example.com`
pub fn public_base(st: &AppState, headers: &HeaderMap) -> String {
    derive_public_base(
        st.cfg.public_base_url.as_deref(),
        headers,
        st.cfg.http_redirect_enabled,
    )
}

/// 纯推导（可单测）：override > X-Forwarded-Proto/Host > 部署启发式 + Host
fn derive_public_base(
    override_url: Option<&str>,
    headers: &HeaderMap,
    http_redirect_enabled: bool,
) -> String {
    if let Some(base) = override_url.filter(|v| !v.is_empty()) {
        return base.to_string();
    }
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| *v == "http" || *v == "https")
        .unwrap_or_else(|| {
            if http_redirect_enabled {
                "https"
            } else {
                "http"
            }
            .to_string()
        });
    format!("{scheme}://{host}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(kv: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in kv {
            m.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn override_wins() {
        let headers = h(&[("host", "gw.internal:8080")]);
        assert_eq!(
            derive_public_base(Some("https://llm.example.com"), &headers, true),
            "https://llm.example.com"
        );
        // 空串覆盖视为未设置
        assert_eq!(
            derive_public_base(Some(""), &headers, true),
            "https://gw.internal:8080"
        );
    }

    #[test]
    fn forwarded_headers_respected() {
        let headers = h(&[
            ("host", "gw.internal:8080"),
            ("x-forwarded-host", "api.example.com"),
            ("x-forwarded-proto", "http"),
        ]);
        assert_eq!(derive_public_base(None, &headers, true), "http://api.example.com");
    }

    #[test]
    fn heuristic_and_host_fallback() {
        // 纯明文部署（redirect 关）→ http + Host
        let headers = h(&[("host", "gw:8080")]);
        assert_eq!(derive_public_base(None, &headers, false), "http://gw:8080");
        // 默认 TLS 部署 → https
        assert_eq!(derive_public_base(None, &headers, true), "https://gw:8080");
        // 无 Host → localhost
        assert_eq!(derive_public_base(None, &HeaderMap::new(), true), "https://localhost");
        // 非法 XFP 回退启发式
        let headers = h(&[("host", "gw"), ("x-forwarded-proto", "gopher")]);
        assert_eq!(derive_public_base(None, &headers, true), "https://gw");
    }
}