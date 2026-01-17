// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

mod backends;
mod config;
mod proxy;
mod storage;
mod web;

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{
    body::Body,
    extract::{self, connect_info::Connected},
    http::{self, Request, Response},
};
use tokio::signal;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt};

use crate::backends::zig::ZigController;
use crate::web::WebController;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "\
Usage: zorian [--config=<path>]

Options:
  --config=<path>    Path to config file (optional)
  --help             Show this help message
  --version          Show version
";

/// Contains metainfo about one server interface
#[derive(Clone)]
struct ListenerInfo {
    addr: SocketAddr,
    hosts: Vec<String>,
}

/// Contains metainfo about one client connection
#[derive(Clone, Copy, Debug)]
struct ClientInfo(SocketAddr);
impl Connected<axum::serve::IncomingStream<'_, TlsListener>> for ClientInfo {
    fn connect_info(target: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        ClientInfo(*target.remote_addr())
    }
}
impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for ClientInfo {
    fn connect_info(target: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        ClientInfo(*target.remote_addr())
    }
}

/// Request info stored in span extensions for logging
#[derive(Clone)]
struct RequestInfo {
    method: http::Method,
    version: http::Version,
    path: http::Uri,
    host: Option<String>,
    user_agent: Option<String>,
}

/// Key extractor for tower-governor that uses ClientInfo to get the client IP.
#[derive(Clone)]
struct ClientIpKeyExtractor;

impl tower_governor::key_extractor::KeyExtractor for ClientIpKeyExtractor {
    type Key = std::net::IpAddr;

    fn name(&self) -> &'static str {
        "ClientIpKeyExtractor"
    }

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, tower_governor::GovernorError> {
        req.extensions()
            .get::<extract::ConnectInfo<ClientInfo>>()
            .map(|ci| ci.0.0.ip())
            .ok_or(tower_governor::GovernorError::UnableToExtractKey)
    }
}

#[tokio::main]
async fn main() {
    let mut config_path = None;
    for arg in std::env::args().skip(1) {
        if arg == "--help" || arg == "-h" {
            print!("{HELP}");
            return;
        }
        if arg == "--version" || arg == "-V" {
            println!("zorian {VERSION}");
            return;
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            config_path = Some(PathBuf::from(path));
        }
    }

    tracing_subscriber::registry()
        .with({
            #[cfg(debug_assertions)]
            let fmt = tracing_subscriber::fmt::layer().pretty();
            #[cfg(not(debug_assertions))]
            let fmt = tracing_subscriber::fmt::layer().json();
            fmt
        })
        .with(match std::env::var_os("ZORIAN_LOG") {
            None => tracing_subscriber::EnvFilter::new("info,tower_http=info"),
            Some(val) => tracing_subscriber::EnvFilter::try_new(val.to_string_lossy())
                .expect("Invalid ZORIAN_LOG"),
        })
        .init();

    let config = Arc::new(match config_path {
        Some(path) => {
            info!("use config file from {}", path.to_str().unwrap());

            config::ConfigService::from_file(&path).unwrap_or_else(|e| {
                error!("{e}");
                std::process::exit(1);
            })
        }
        None => {
            info!("configuration file path not provided");
            config::ConfigService::default()
        }
    });
    config.validate().unwrap_or_else(|e| {
        error!("invalid config: {e}");
        std::process::exit(1);
    });

    let storage = Arc::new(storage::StorageService::new(config.clone()).await.unwrap());
    let upstream = Arc::new(proxy::ProxyService::new());

    let web_controller = Arc::new(WebController::default());
    let zig_controller = Arc::new(ZigController::new(
        config.clone(),
        storage.clone(),
        upstream.clone(),
    ));

    const REQUEST_ID_HEADER: http::HeaderName = http::HeaderName::from_static("x-request-id");
    let trace_layer = tower_http::trace::TraceLayer::new_for_http()
        .make_span_with(|req: &http::Request<Body>| {
            let request_id = req
                .headers()
                .get(&REQUEST_ID_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<invalid>");
            let local_addr = req.extensions().get::<ListenerInfo>().map(|a| a.addr);
            let remote_addr = req
                .extensions()
                .get::<extract::ConnectInfo<ClientInfo>>()
                .map(|ci| ci.0.0.ip());

            tracing::info_span!(
                "http_request",
                request_id = %request_id,
                local_addr = ?local_addr,
                remote_addr = ?remote_addr,
            )
        })
        .on_request(|req: &Request<Body>, span: &tracing::Span| {
            let info = RequestInfo {
                method: req.method().clone(),
                path: req.uri().clone(),
                version: req.version(),
                host: req
                    .headers()
                    .get(http::header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from),
                user_agent: req
                    .headers()
                    .get(http::header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from),
            };

            span.with_subscriber(|(id, dispatch)| {
                if let Some(reg) = dispatch.downcast_ref::<tracing_subscriber::Registry>()
                    && let Some(span_ref) = reg.span(id)
                {
                    span_ref.extensions_mut().insert(info);
                }
            });
        })
        .on_response(
            |res: &Response<Body>, latency: std::time::Duration, span: &tracing::Span| {
                use axum::body::HttpBody as _;

                let status = res.status().as_u16();
                let content_length = res.body().size_hint().exact();
                let content_type = res
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok());

                let req_info = span.with_subscriber(|(id, dispatch)| {
                    dispatch
                        .downcast_ref::<tracing_subscriber::Registry>()
                        .and_then(|reg| reg.span(id))
                        .and_then(|span_ref| span_ref.extensions().get::<RequestInfo>().cloned())
                });

                if let Some(Some(req_info)) = req_info {
                    info!(
                        method = %req_info.method,
                        version = ?req_info.version,
                        path = %req_info.path,
                        host = req_info.host,
                        user_agent = req_info.user_agent,
                        status,
                        latency = latency.as_nanos() as u64,
                        content_type,
                        content_length,
                        "on_response",
                    );
                } else {
                    info!(
                        status,
                        latency = latency.as_nanos() as u64,
                        content_type,
                        content_length,
                        "on_response",
                    );
                }
            },
        )
        .on_failure(tower_http::trace::DefaultOnFailure::new().level(tracing::Level::ERROR));

    let governor_config = tower_governor::governor::GovernorConfigBuilder::default()
        .period(config.server().rate_limit_period)
        .burst_size(config.server().rate_limit_burst_size)
        .key_extractor(ClientIpKeyExtractor)
        .finish()
        .unwrap();

    let app = axum::Router::new()
        .merge(web_controller.router())
        .merge(zig_controller.router())
        .layer(HostValidationLayer)
        .layer(trace_layer)
        .layer(tower_http::request_id::PropagateRequestIdLayer::new(
            REQUEST_ID_HEADER.clone(),
        ))
        .layer(tower_http::request_id::SetRequestIdLayer::new(
            REQUEST_ID_HEADER.clone(),
            tower_http::request_id::MakeRequestUuid,
        ))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            http::StatusCode::REQUEST_TIMEOUT,
            config.server().request_timeout,
        ))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            config.server().max_body_size.as_u64() as usize,
        ))
        .layer(tower_governor::GovernorLayer::new(Arc::new(
            governor_config,
        )))
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            config.server().max_concurrent_requests,
        ));

    let mut tasks = tokio::task::JoinSet::new();

    // The channel is used to broadcast SIGINT/SIGTERM to all listeners.
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    for listener_config in config.listeners() {
        let tcp_listener = tokio::net::TcpListener::bind(&listener_config.addr)
            .await
            .unwrap();

        let tls_enabled = listener_config.tls_crt.is_some();
        info!(
            "listening {} on {} (hostnames: {})",
            if tls_enabled { "HTTPS" } else { "HTTP" },
            tcp_listener.local_addr().unwrap(),
            if listener_config.hostnames.is_empty() {
                "*".to_string()
            } else {
                listener_config.hostnames.join(", ")
            },
        );

        let local_addr = tcp_listener.local_addr().unwrap();
        let app = app
            .clone()
            .layer(axum::Extension(ListenerInfo {
                addr: local_addr,
                hosts: listener_config.hostnames.clone(),
            }))
            .into_make_service_with_connect_info::<ClientInfo>();

        let mut shutdown_rx = shutdown_tx.subscribe();
        let shutdown_signal = async move {
            shutdown_rx.recv().await.ok();
        };

        if let (Some(crt_path), Some(key_path)) =
            (&listener_config.tls_crt, &listener_config.tls_key)
        {
            let tls_listener = TlsListener::new(tcp_listener, crt_path, key_path);
            tasks.spawn(async move {
                axum::serve(tls_listener, app)
                    .with_graceful_shutdown(shutdown_signal)
                    .await
                    .unwrap();
            });
        } else {
            tasks.spawn(async move {
                axum::serve(tcp_listener, app)
                    .with_graceful_shutdown(shutdown_signal)
                    .await
                    .unwrap();
            });
        }
    }

    // Graceful shutdown
    let sigint = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    let sigterm = {
        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install signal handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        terminate
    };
    tokio::select! {
        _ = sigint => info!("received SIGINT, shutting down"),
        _ = sigterm => info!("received SIGTERM, shutting down"),
        result = tasks.join_next() => {
            match result {
                Some(Ok(())) => error!("listener exited unexpectedly, shutting down"),
                Some(Err(e)) => error!("listener failed: {e}, shutting down"),
                None => {
                    error!("no listeners running");
                    return;
                }
            }
        }
    }

    drop(shutdown_tx); // broadcast

    // Wait for all listeners to finish with timeout
    let shutdown_result = tokio::time::timeout(config.server().shutdown_timeout, async {
        while let Some(result) = tasks.join_next().await {
            if let Err(e) = result {
                error!("listener task failed: {e}");
            }
        }
    })
    .await;

    if shutdown_result.is_err() {
        error!(
            "shutdown timeout after {:?}, aborting {} remaining tasks",
            config.server().shutdown_timeout,
            tasks.len()
        );
        tasks.abort_all();
    } else {
        info!("shutdown complete");
    }
}

/// A TLS listener that wraps a TCP listener and performs TLS handshakes.
struct TlsListener {
    inner: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
}
impl TlsListener {
    fn new(inner: tokio::net::TcpListener, crt_path: &Path, key_path: &Path) -> Self {
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(crt_path)
            .expect("failed to open certificate file")
            .collect::<Result<_, _>>()
            .expect("failed to parse certificates");

        let key = PrivateKeyDer::from_pem_file(key_path).expect("failed to read private key");

        let config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("failed to build TLS config");

        let acceptor = TlsAcceptor::from(Arc::new(config));
        Self { inner, acceptor }
    }
}
impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.inner.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    error!("failed to accept TCP connection: {}", e);
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls_stream) => return (tls_stream, addr),
                Err(e) => {
                    error!("TLS handshake failed from {}: {}", addr, e);
                    continue;
                }
            }
        }
    }
}

/// Layer that validates the Host header against configured hostnames.
#[derive(Clone)]
struct HostValidationLayer;
impl<S> tower::Layer<S> for HostValidationLayer {
    type Service = HostValidationService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HostValidationService { inner }
    }
}

#[derive(Clone)]
struct HostValidationService<S> {
    inner: S,
}
impl<S> tower::Service<Request<Body>> for HostValidationService<S>
where
    S: tower::Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let interface = req.extensions().get::<ListenerInfo>().cloned();

        if let Some(iface) = interface
            && !iface.hosts.is_empty()
        {
            let host = req
                .headers()
                .get(http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .and_then(|raw| {
                    let without_port = if let Some((host, port)) = raw.rsplit_once(':')
                        && port.parse::<u16>().is_ok()
                        && (host.ends_with(']') || !host.contains('['))
                    {
                        host
                    } else {
                        raw
                    };
                    url::Host::parse(without_port)
                        .ok()
                        .map(|h| h.to_string().trim_end_matches('.').to_string())
                });

            let is_valid = host
                .map(|h| {
                    iface
                        .hosts
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(&h))
                })
                .unwrap_or(false);

            if !is_valid {
                return Box::pin(async move {
                    Ok(Response::builder()
                        .status(http::StatusCode::MISDIRECTED_REQUEST)
                        .body(Body::empty())
                        .unwrap())
                });
            }
        }

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}
