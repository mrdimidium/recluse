// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

mod config;
mod proxy;
mod storage;
mod telemetry;

mod controller_backend;
mod controller_web;

use std::future::Future;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{
    body::Body,
    extract::{self, ConnectInfo},
    http::{self, Request, Response},
};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
#[cfg(target_os = "linux")]
use sd_notify::NotifyState;
use tokio::signal;
use tracing::{error, info, trace};
use tracing_subscriber::registry::LookupSpan;

use crate::controller_backend::BackendController;
use crate::controller_web::WebController;
use repos::{Backend, BackendSpec, GoBackend, ZigBackend};

async fn init_backend<S: BackendSpec>(
    backend: Backend<S>,
    index_tasks: &mut tokio::task::JoinSet<()>,
    index_cancel: tokio_util::sync::CancellationToken,
) -> Option<Arc<Backend<S>>> {
    if !backend.enabled() {
        return None;
    }
    let backend = Arc::new(backend);
    let interval = backend.refresh_interval();
    if !interval.is_zero() {
        index_tasks.spawn(run_index_refresh(
            S::ID,
            backend.clone(),
            interval,
            index_cancel,
        ));
    }
    Some(backend)
}

async fn run_index_refresh<S: BackendSpec>(
    name: &'static str,
    backend: Arc<Backend<S>>,
    interval: std::time::Duration,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(backend = name, "index refresh stopped");
                break;
            }
            _ = ticker.tick() => {
                info!(backend = name, "refreshing index");
                match backend.refresh().await {
                    Ok(()) => info!(backend = name, "index refreshed"),
                    Err(e) => error!(backend = name, "index refresh failed: {e}"),
                }
            }
        }
    }
}

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

/// Request info stored in span extensions for logging
#[derive(Clone)]
struct RequestInfo {
    method: http::Method,
    version: http::Version,
    path: http::Uri,
    host: Option<String>,
    user_agent: Option<String>,
}

/// Key extractor for tower-governor that uses ConnectInfo to get the client IP.
#[derive(Clone)]
struct ClientIpKeyExtractor;
impl tower_governor::key_extractor::KeyExtractor for ClientIpKeyExtractor {
    type Key = std::net::IpAddr;

    fn name(&self) -> &'static str {
        "ClientIpKeyExtractor"
    }

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, tower_governor::GovernorError> {
        req.extensions()
            .get::<extract::ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip())
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

    let config = Arc::new(
        config::ConfigService::load(config_path).unwrap_or_else(|e| {
            eprintln!("invalid config: {e}");
            std::process::exit(1);
        }),
    );

    let mut telemetry =
        telemetry::TelemetryService::init(config.telemetry(), config.appname(), VERSION);

    let storage = Arc::new(storage::StorageService::new(config.clone()).await.unwrap());
    let network = Arc::new(proxy::ProxyService::new());

    let source = format!("zorian:{}", config.appname());
    let backends = config.backends();

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
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0.ip());

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
                host: extract_host(req),
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

    let mut index_tasks = tokio::task::JoinSet::new();
    let index_cancel = tokio_util::sync::CancellationToken::new();

    let zig_backend = init_backend(
        ZigBackend::new(
            backends.zig.clone(),
            source.clone(),
            storage.clone(),
            network.clone(),
        ),
        &mut index_tasks,
        index_cancel.clone(),
    )
    .await;

    let go_backend = init_backend(
        GoBackend::new(
            backends.go.clone(),
            source.clone(),
            storage.clone(),
            network.clone(),
        ),
        &mut index_tasks,
        index_cancel.clone(),
    )
    .await;

    let web_controller = Arc::new(WebController::new(zig_backend.clone(), go_backend.clone()));
    let mut app = axum::Router::new().merge(web_controller.router());

    if let Some(ref backend) = zig_backend {
        let ctrl = Arc::new(BackendController::new(
            backend.clone(),
            storage.clone(),
            network.clone(),
        ));
        app = app.nest("/zig", ctrl.router());
    }

    if let Some(ref backend) = go_backend {
        let ctrl = Arc::new(BackendController::new(
            backend.clone(),
            storage.clone(),
            network.clone(),
        ));
        app = app.nest("/go", ctrl.router());
    }

    let app = app
        // Opt-in layers
        .layer(tower_http::compression::CompressionLayer::new())
        // request limits
        .layer(HostValidationLayer)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            config.server().max_body_size.as_u64() as usize,
        ))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            http::StatusCode::REQUEST_TIMEOUT,
            config.server().request_timeout,
        ))
        // logging
        .layer(trace_layer)
        // rate-limits
        .layer(tower_governor::GovernorLayer::new(Arc::new(
            governor_config,
        )))
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            config.server().max_concurrent_requests,
        ))
        // global headers
        .layer(tower_http::request_id::PropagateRequestIdLayer::new(
            REQUEST_ID_HEADER.clone(),
        ))
        .layer(tower_http::request_id::SetRequestIdLayer::new(
            REQUEST_ID_HEADER.clone(),
            tower_http::request_id::MakeRequestUuid,
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            http::header::SERVER,
            http::HeaderValue::from_static(concat!("zorian/", env!("CARGO_PKG_VERSION"))),
        ));

    let mut tasks = tokio::task::JoinSet::new();
    let handle = Handle::new();

    for listener_config in config.listeners() {
        let std_listener = TcpListener::bind(listener_config.addr).unwrap();
        std_listener.set_nonblocking(true).unwrap();

        let addr = std_listener.local_addr().unwrap();

        let app = app
            .clone()
            .layer(axum::Extension(ListenerInfo {
                addr,
                hosts: listener_config.hostnames.clone(),
            }))
            .into_make_service_with_connect_info::<SocketAddr>();

        let handle = handle.clone();

        let tls_enabled =
            if let (Some(crt), Some(key)) = (&listener_config.tls_crt, &listener_config.tls_key) {
                let rustls_config = RustlsConfig::from_pem_file(crt, key)
                    .await
                    .expect("failed to load TLS config");

                tasks.spawn(async move {
                    let server = axum_server::from_tcp_rustls(std_listener, rustls_config).unwrap();
                    server.handle(handle).serve(app).await.unwrap();
                });

                true
            } else {
                tasks.spawn(async move {
                    let server = axum_server::from_tcp(std_listener).unwrap();
                    server.handle(handle).serve(app).await.unwrap();
                });

                false
            };

        info!(
            "listening {} on {} (hostnames: {})",
            if tls_enabled { "HTTPS" } else { "HTTP" },
            addr,
            if listener_config.hostnames.is_empty() {
                "*".to_string()
            } else {
                listener_config.hostnames.join(", ")
            },
        );
    }

    let mut watchdog_ticker = tokio::time::interval(std::time::Duration::from_secs(60));

    #[cfg(target_os = "linux")]
    if sd_notify::booted().unwrap_or(false) {
        sd_notify::notify(false, &[NotifyState::Ready]).ok();

        let mut usec = 0u64;
        (sd_notify::watchdog_enabled(true, &mut usec) && usec > 0).then(|| {
            let interval = std::time::Duration::from_micros(usec) / 2;
            info!(
                interval_ms = interval.as_millis() as u64,
                "watchdog enabled"
            );
            watchdog_ticker = tokio::time::interval(interval);
        });
    };

    #[cfg(unix)]
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
        .expect("failed to install signal handler");
    #[cfg(windows)]
    let mut sigint = signal::windows::signal(signal::windows::SignalKind::interrupt())
        .expect("failed to install signal handler");

    #[cfg(unix)]
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to install signal handler");

    loop {
        let watchdog = watchdog_ticker.tick();

        #[cfg(unix)]
        let sigterm = sigterm.recv();
        #[cfg(not(unix))]
        let sigterm = std::future::pending::<()>();

        tokio::select! {
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
                break;
            },
            _ = sigterm => {
                info!("received SIGTERM, shutting down");
                break;
            },
            _ = watchdog => {
                trace!("server is alive");

                #[cfg(target_os = "linux")]
                sd_notify::notify(false, &[NotifyState::Watchdog]).ok();
            },
            result = tasks.join_next() => {
                match result {
                    Some(Ok(())) => error!("listener exited unexpectedly, shutting down"),
                    Some(Err(e)) => error!("listener failed: {e}, shutting down"),
                    None => {
                        error!("no listeners running");
                        return;
                    }
                }
                break;
            },
        }
    }

    #[cfg(target_os = "linux")]
    sd_notify::notify(false, &[NotifyState::Stopping]).ok();

    index_cancel.cancel();
    handle.graceful_shutdown(None);

    // Wait for all tasks to finish with timeout
    let shutdown_result = tokio::time::timeout(config.server().shutdown_timeout, async {
        while let Some(result) = tasks.join_next().await {
            if let Err(e) = result {
                error!("listener task failed: {e}");
            }
        }
        while let Some(result) = index_tasks.join_next().await {
            if let Err(e) = result {
                error!("index task failed: {e}");
            }
        }
    })
    .await;

    if shutdown_result.is_err() {
        error!(
            "shutdown timeout after {:?}, aborting remaining tasks",
            config.server().shutdown_timeout,
        );
        tasks.abort_all();
        index_tasks.abort_all();
    } else {
        info!("shutdown complete");
    }

    telemetry.shutdown();
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
            let is_valid = extract_host(&req)
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

fn extract_host(req: &Request<Body>) -> Option<String> {
    // HTTP/1.1 uses HOST header, HTTP/2 uses :authority (available via URI)
    let raw = if let Some(host) = req.headers().get(http::header::HOST) {
        host.to_str().ok().map(|raw| {
            if let Some((host, port)) = raw.rsplit_once(':')
                && port.parse::<u16>().is_ok()
                && (host.ends_with(']') || !host.contains('['))
            {
                host
            } else {
                raw
            }
        })
    } else {
        req.uri().host()
    };

    raw.and_then(|raw| url::Host::parse(raw).ok())
        .map(|h| h.to_string().trim_end_matches('.').to_string())
}
