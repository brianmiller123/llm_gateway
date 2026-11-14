mod api;
mod config;
mod crypto;
mod error;
mod metrics;
mod service;
mod state;
mod store;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
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
    tokio::spawn(worker::aggregator::run(
        state.pool.clone(),
        state.cfg.usage_retention_days,
    ));
    tokio::spawn(worker::plan_sync::run(state.clone()));

    // 关停：SIGINT/SIGTERM 统一走 watch 通道（docker stop 发 SIGTERM，只听 ctrl_c 会被硬杀、
    // 在途 SSE 计费直接丢失）
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let signal_task = tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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
            // 排水预算 = GATEWAY_DRAIN_SECS（cfg.drain_secs）；整体仍受 docker compose
            // stop_grace_period 约束（宽限期到期发 SIGKILL），部署侧须保证
            // stop_grace_period > drain_secs，否则进程被硬杀、在途 SSE 计费丢失
            tracing::info!("draining in-flight requests (up to {}s)", cfg.drain_secs);
            let deadline = std::time::Instant::now() + Duration::from_secs(cfg.drain_secs);
            while state.active_requests.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                if std::time::Instant::now() >= deadline {
                    tracing::warn!("drain timeout; exiting with requests still in flight");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            tracing::info!(
                "drain finished, exiting (active={})",
                state.active_requests.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }
    Ok(())
}

/// HTTP/1.1 header 读空闲超时：回收空闲 keep-alive 连接；值须小于中间设备
/// （LB/conntrack）的 idle 阈值，否则连接先被中间设备单方面掐掉、网关侧不可感知
const HTTP1_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// HTTP/2 keep-alive PING 发送间隔：持续探测连接活性
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// HTTP/2 keep-alive PING 判定窗口：超时未收到 PONG 即判定半开连接并回收
const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// HTTPS 与 HTTP 明文两条服务路径共用的连接 Builder：统一 HTTP/1.1 空闲读
/// 超时与 HTTP/2 keep-alive 配置（半开/僵死连接回收，防止排水期被空连接吊住）
fn conn_builder() -> hyper_util::server::conn::auto::Builder<TokioExecutor> {
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(Some(HTTP1_HEADER_READ_TIMEOUT));
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Some(HTTP2_KEEP_ALIVE_INTERVAL))
        .keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT);
    builder
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
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            // 关停：停止 accept 新连接，随后进入下方有界排水
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                // accept 瞬时错误（EMFILE 等）只告警不退出，避免 HTTPS 服务整体挂掉
                let (tcp, addr) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // select 两臂同轮就绪的竞态：信号已触发则不再受理新连接（其 watcher
                // 订阅会晚于关停广播，收不到关停通知，排水被空连接无限吊住）
                if *shutdown.borrow() {
                    break;
                }
                // SSE 高频小包写：禁用 Nagle，避免与延迟 ACK 互作用造成
                // 每 token 数十毫秒级停顿（上游 reqwest 侧默认已开）
                if let Err(e) = tcp.set_nodelay(true) {
                    tracing::warn!(%addr, error = %e, "failed to set TCP_NODELAY");
                }
                let acceptor = acceptor.clone();
                let app = app.clone();
                let watcher = graceful.watcher();
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
                    // watch：关停广播后连接收到 GOAWAY/Connection: close，在途响应收尾即返回
                    if let Err(e) = watcher
                        .watch(conn_builder().serve_connection_with_upgrades(stream, service))
                        .await
                    {
                        tracing::warn!(%addr, error = %e, "connection error");
                    }
                });
            }
        }
    }
    drain_connections(graceful, cfg.drain_secs).await;
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

/// HTTP：直接服务完整应用（前端 + API），明文无 TLS。
/// 手动 accept 循环（与 HTTPS 路径同一套配置化 Builder + 有界排水）：
/// axum::serve 的 with_graceful_shutdown 只停 accept、不向存量 keep-alive
/// 连接发关闭通知，排水期会被空闲连接吊满预算，故两条路径统一走
/// GracefulShutdown。
async fn serve_http_app(
    app: Router,
    cfg: Arc<AppConfig>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(cfg.http_addr).await?;
    tracing::info!("HTTP serving on {} (redirect disabled)", cfg.http_addr);
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let (tcp, addr) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                // 同 serve_https：信号已触发不再受理新连接（watcher 竞态防护）
                if *shutdown.borrow() {
                    break;
                }
                // SSE 高频小包写：禁用 Nagle，避免与延迟 ACK 互作用造成
                // 每 token 数十毫秒级停顿（上游 reqwest 侧默认已开）
                if let Err(e) = tcp.set_nodelay(true) {
                    tracing::warn!(error = %e, "failed to set TCP_NODELAY");
                }
                let app = app.clone();
                let service = hyper::service::service_fn(move |mut req: Request<Incoming>| {
                    let mut app = app.clone();
                    let peer = addr;
                    async move {
                        // ConnectInfo 注入：与 HTTPS 路径的 service_fn 模式对齐，
                        // 供 v1 代理 / 控制台登录 IP 桶取客户端地址
                        req.extensions_mut()
                            .insert(axum::extract::ConnectInfo(peer));
                        app.call(req).await
                    }
                });
                let watcher = graceful.watcher();
                tokio::spawn(async move {
                    if let Err(e) = watcher
                        .watch(conn_builder().serve_connection_with_upgrades(
                            TokioIo::new(tcp),
                            service,
                        ))
                        .await
                    {
                        tracing::warn!(%addr, error = %e, "connection error");
                    }
                });
            }
        }
    }
    drain_connections(graceful, cfg.drain_secs).await;
    Ok(())
}

/// 有界排水：向全部受管连接广播优雅关停（HTTP/2 GOAWAY、HTTP/1.1
/// Connection: close），等在途响应收尾；超过 drain_secs 预算即放弃等待
/// （超时后未完成连接随进程退出终止）。
/// 与 docker compose stop_grace_period 的配套关系：SIGTERM 后宽限期到期即
/// SIGKILL，故 stop_grace_period 必须 > drain_secs（本函数）+ main 在途请求
/// 等待（同一预算），留足余量，否则排水被硬杀截断、在途 SSE 计费丢失。
async fn drain_connections(graceful: GracefulShutdown, drain_secs: u64) {
    if tokio::time::timeout(Duration::from_secs(drain_secs), graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            "drain budget ({}s) exhausted; dropping remaining connections",
            drain_secs
        );
    }
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
    // LB 已终结 TLS（x-forwarded-proto: https）时再 301 会造成重定向死循环；
    // 完整应用应由 GATEWAY_HTTP_REDIRECT=false 的 80 端口服务，此处仅回非重定向
    // 响应保证探活/直达请求不被循环消耗
    let xf_proto = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim);
    if xf_proto.is_some_and(|v| v.eq_ignore_ascii_case("https")) {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("static response is valid");
    }
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
