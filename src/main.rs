mod api;
mod config;
mod crypto;
mod error;
mod service;
mod state;
mod store;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tower_service::Service;
use tracing_subscriber::EnvFilter;

use config::AppConfig;
use state::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_gateway=info,tower_http=info".into()),
        )
        .init();

    // reqwest(aws-lc-rs) 与 hyper-rustls(ring) 同时启用时，显式选定 provider
    let _ = tokio_rustls::rustls::crypto::CryptoProvider::install_default(
        tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
    );

    let cfg = Arc::new(AppConfig::from_env()?);
    let state = AppState::init(cfg.clone()).await?;
    let app = api::build_router(state.clone());

    // 后台任务：配置热加载 + 用量日聚合
    state.spawn_reload_tasks();
    tokio::spawn(worker::aggregator::run(state.pool.clone()));
    tokio::spawn(worker::plan_sync::run(state.clone()));

    // 关停：SIGINT/SIGTERM 统一走 watch 通道（docker stop 发 SIGTERM，只听 ctrl_c 会被硬杀、
    // 在途 SSE 计费直接丢失）
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let signal_task = tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            )
            .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl_c handler");
        tracing::info!("shutdown signal received");
        let _ = shutdown_tx.send(true);
    });

    // HTTPS 主服务（443）；HTTP（80）：开启重定向时仅 301，关闭时直接服务完整应用
    let https = tokio::spawn(serve_https(app.clone(), cfg.clone(), shutdown_rx.clone()));
    let http = if cfg.http_redirect_enabled {
        tokio::spawn(serve_http_redirect(cfg.clone(), shutdown_rx.clone()))
    } else {
        tokio::spawn(serve_http_app(app, cfg.clone(), shutdown_rx.clone()))
    };

    // 服务错误立即传播；信号到达后进入有界排水：等存量请求（含 SSE）收尾再退出
    tokio::select! {
        res = https => res??,
        res = http => res??,
        _ = signal_task => {
            tracing::info!("draining in-flight requests (up to 15s)");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while state.active_requests.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                if std::time::Instant::now() >= deadline {
                    tracing::warn!("drain timeout; exiting with requests still in flight");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            tracing::info!("drain finished, exiting");
        }
    }
    Ok(())
}

/// HTTPS 服务：hyper-util + tokio-rustls 手动 accept 循环（axum 官方示例同款）
async fn serve_https(
    app: Router,
    cfg: Arc<AppConfig>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = rustls_server_config(PathBuf::from(&cfg.tls_key), PathBuf::from(&cfg.tls_cert))?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind(cfg.https_addr).await?;
    tracing::info!("HTTPS listening on {}", cfg.https_addr);
    loop {
        tokio::select! {
            // 关停：停止 accept 新连接；已接连接任务继续跑（排水期内自然收尾）
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                // accept 瞬时错误（EMFILE 等）只告警不退出，避免 HTTPS 服务整体挂掉
                let (tcp, addr) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let app = app.clone();
                tokio::spawn(async move {
                    let stream = match acceptor.accept(tcp).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(%addr, error = %e, "TLS handshake failed");
                            return;
                        }
                    };
                    let stream = TokioIo::new(stream);
                    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let mut app = app.clone();
                        let peer = addr;
                        async move {
                            let mut req = req;
                            // ConnectInfo 注入：与 axum::serve(with_connect_info) 对齐，供 v1 代理取客户端 IP
                            req.extensions_mut()
                                .insert(axum::extract::ConnectInfo(peer));
                            app.call(req).await
                        }
                    });
                    if let Err(e) =
                        hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                            .serve_connection_with_upgrades(stream, service)
                            .await
                    {
                        tracing::warn!(%addr, error = %e, "connection error");
                    }
                });
            }
        }
    }
    Ok(())
}

fn rustls_server_config(
    key: PathBuf,
    cert: PathBuf,
) -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let key = PrivateKeyDer::from_pem_file(key)?;
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(cert)?.collect::<Result<_, _>>()?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    // ALPN：h2 + http/1.1（SSE 在两者上均正常）
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// HTTP：直接服务完整应用（前端 + API），明文无 TLS
async fn serve_http_app(
    app: Router,
    cfg: Arc<AppConfig>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(cfg.http_addr).await?;
    tracing::info!("HTTP serving on {} (redirect disabled)", cfg.http_addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        // 收到关停信号：axum 停止 accept，存量连接跑完；整体退出上限由 main 的排水控制
        let _ = shutdown.changed().await;
    })
    .await?;
    Ok(())
}

/// HTTP：全部 301 到 HTTPS（Location 端口取自 https_addr，而非请求 Host）
async fn serve_http_redirect(
    cfg: Arc<AppConfig>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let https_port = cfg
        .redirect_https_port
        .unwrap_or_else(|| cfg.https_addr.port());
    let app = Router::new().fallback(move |req: Request| redirect_to_https(req, https_port));
    let listener = TcpListener::bind(cfg.http_addr).await?;
    tracing::info!("HTTP redirect listening on {}", cfg.http_addr);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await?;
    Ok(())
}

async fn redirect_to_https(req: Request, https_port: u16) -> Response {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    // 剥离 Host 的端口（含 IPv6 带端口写法 [::1]:8080），改用 HTTPS 端口
    let bare_host = host
        .rsplit_once(':')
        .filter(|(_, port)| port.parse::<u16>().is_ok())
        .map(|(h, _)| h)
        .unwrap_or(host);
    let host = if https_port == 443 {
        bare_host.to_string()
    } else {
        format!("{bare_host}:{https_port}")
    };
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("Location", format!("https://{host}{path}"))
        .body(Body::empty())
        .expect("static response is valid")
}
