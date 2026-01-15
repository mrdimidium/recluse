// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;
use std::sync::Arc;

use axum::{Router, http};
use tower_http::{
    request_id,
    trace::{DefaultOnFailure, TraceLayer},
};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod controller_web;
mod controller_zig;
mod service_config;
mod service_storage;
mod service_upstream;

static REQUEST_ID_HEADER: http::HeaderName = http::HeaderName::from_static("x-request-id");

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HELP: &str = "\
Usage: zorian [--config=<path>]

Options:
  --config=<path>    Path to config file (optional)
  --help             Show this help message
  --version          Show version
";

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

            service_config::ConfigService::from_file(&path).unwrap_or_else(|e| {
                error!("{e}");
                std::process::exit(1);
            })
        }
        None => {
            info!("configuration file path not provided");
            service_config::ConfigService::default()
        }
    });
    config.validate().unwrap_or_else(|e| {
        error!("invalid config: {e}");
        std::process::exit(1);
    });

    let storage = Arc::new(
        service_storage::StorageService::new(config.clone())
            .await
            .unwrap(),
    );
    let upstream = Arc::new(service_upstream::UpstreamService::new());

    let web_controller = Arc::new(controller_web::WebController::new());
    let zig_controller = Arc::new(controller_zig::ZigController::new(
        config.clone(),
        storage.clone(),
        upstream.clone(),
    ));

    let accept_logger = TraceLayer::new_for_http()
        .make_span_with(|req: &http::Request<_>| {
            let request_id = req
                .headers()
                .get(&REQUEST_ID_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<invalid>");

            tracing::info_span!("http_request", request_id = %request_id)
        })
        .on_request(log_request)
        .on_response(log_response)
        .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR));

    let app = Router::new()
        .merge(web_controller.router())
        .merge(zig_controller.router())
        .layer(accept_logger)
        .layer(request_id::PropagateRequestIdLayer::new(
            REQUEST_ID_HEADER.clone(),
        ))
        .layer(request_id::SetRequestIdLayer::new(
            REQUEST_ID_HEADER.clone(),
            request_id::MakeRequestUuid,
        ));

    let listener = tokio::net::TcpListener::bind(config.listen())
        .await
        .unwrap();

    info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

fn log_request<B>(req: &http::Request<B>, _span: &tracing::Span) {
    let headers = req.headers();

    let client_ip = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok());
    let user_agent = headers
        .get(http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let referer = headers
        .get(http::header::REFERER)
        .and_then(|v| v.to_str().ok());

    info!(
        method = %req.method(),
        uri = %req.uri(),
        client_ip,
        user_agent,
        referer,
        "request started"
    );
}

fn log_response<B>(res: &http::Response<B>, latency: std::time::Duration, _span: &tracing::Span) {
    let headers = res.headers();
    let content_length = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok());
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    info!(
        status = %res.status(),
        latency = ?latency,
        content_length,
        content_type,
        "request finished"
    );
}
