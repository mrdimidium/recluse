// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

mod backends;
mod config;
mod storage;
mod upstream;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::{
    Router,
    body::Body,
    extract,
    http::{self, Request, Response},
    response::{self, IntoResponse},
    routing,
};
use tower_http::{
    request_id,
    trace::{DefaultOnFailure, TraceLayer},
};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::backends::zig::ZigController;

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
    let upstream = Arc::new(upstream::UpstreamService::new());

    let web_controller = Arc::new(WebController::default());
    let zig_controller = Arc::new(ZigController::new(
        config.clone(),
        storage.clone(),
        upstream.clone(),
    ));

    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(|req: &http::Request<_>| {
            let request_id = req
                .headers()
                .get(&REQUEST_ID_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<invalid>");

            tracing::info_span!("http_request", request_id = %request_id)
        })
        .on_request(())
        .on_response(())
        .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR));

    let app = Router::new()
        .merge(web_controller.router())
        .merge(zig_controller.router())
        .layer(LoggingLayer)
        .layer(trace_layer)
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

#[derive(Clone)]
pub struct LoggingLayer;

impl<S> tower::Layer<S> for LoggingLayer {
    type Service = LoggingService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoggingService { inner }
    }
}

#[derive(Clone)]
pub struct LoggingService<S> {
    inner: S,
}

impl<S> tower::Service<Request<Body>> for LoggingService<S>
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
        Box::pin(log_request(self.inner.clone(), req))
    }
}

async fn log_request<S>(mut inner: S, req: Request<Body>) -> Result<Response<Body>, S::Error>
where
    S: tower::Service<Request<Body>, Response = Response<Body>>,
{
    let start = std::time::Instant::now();

    let method = req.method().clone();
    let uri = req.uri().clone();

    let headers = req.headers();
    let client_ip = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let user_agent = headers
        .get(http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let referer = headers
        .get(http::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let res = inner.call(req).await?;

    let latency = start.elapsed();
    let status = res.status();
    let headers = res.headers();
    let content_length = headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok());
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    info!(
        method = %method,
        uri = %uri,
        status = %status,
        latency = ?latency,
        client_ip = client_ip.as_deref(),
        user_agent = user_agent.as_deref(),
        referer = referer.as_deref(),
        content_length,
        content_type,
        "request"
    );

    Ok(res)
}

/// Handles html pages rendering and static files
pub struct WebController {
    jinja: minijinja::Environment<'static>,
}

impl Default for WebController {
    fn default() -> Self {
        let mut jinja = minijinja::Environment::new();
        jinja.add_template("index", HTML).unwrap();

        Self { jinja }
    }
}

impl WebController {
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/index.css", routing::get(Self::styles))
            .route("/", routing::get(Self::index))
            .with_state(self)
    }

    async fn index(
        extract::State(controller): extract::State<Arc<Self>>,
    ) -> Result<response::Response, http::StatusCode> {
        let template = controller.jinja.get_template("index").unwrap();
        let rendered = template.render(minijinja::context! {}).unwrap();

        let mut response = response::Html(rendered).into_response();
        response.headers_mut().insert(
            http::header::CONTENT_SECURITY_POLICY,
            http::HeaderValue::from_static(
                "img-src 'self'; base-uri 'none'; font-src 'self'; style-src 'self'; script-src 'self'; object-src 'none'; default-src 'self'; frame-ancestors 'none'",
            ),
        );

        Ok(response)
    }

    async fn styles() -> Result<axum_extra::response::Css<&'static str>, http::StatusCode> {
        Ok(axum_extra::response::Css(CSS))
    }
}

const HTML: &str = r#"
<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta http-equiv="X-UA-Compatible" content="ie=edge">

    <title>Earth PKG — tiny & opinionated packages mirror</title>
    <link rel="stylesheet" href="index.css">
  </head>

  <body>
    <h1>Earth PKG — tiny & opinionated packages mirror.</h1>

    <main>
      <p>This site provides a proxy for downloading zig installation files and dependencies.
         On the one hand, this reduces the load on the original project's site
         and makes your infrastructure more reliable by adding redundancy.</p>

      <p>Read more about community mirrors in the <a href="https://ziglang.org/download/community-mirrors/">blog post</a>.
         Information on how to deploy your own mirror is available 
         <a href="https://github.com/ziglang/www.ziglang.org/blob/main/MIRRORS.md">in the documentation</a>.</p>

      <h2>Direct usage:</h2>

      <p>For simplicity, you can use tools like <a href="https://github.com/prantlf/zigup">prantlf/zigup</a> and
         <a href="https://github.com/mlugg/setup-zig">mlugg/setup-zig</a>.</p>

      <p>
        To install manually:
        <ol>
          <li>download zig dist file:<br><code>wget https://pkg.earth/zig/zig-x86_64-linux-0.15.1.tar.xz</code>;</li>
          <li>download zig minisign file:<br><code>wget https://pkg.earth/zig/zig-x86_64-linux-0.15.1.tar.xz.minisig</code>;</li>
          <li>check archive integrity:<br><code>minisign -Vm zig-x86_64-linux-0.15.1.tar.xz -P RWSGOq2NVecA2UPNdBUZykf1CCb147pkmdtYxgb3Ti+JO/wCYvhbAb/U</code>;</li>
          <li>unpack archive:<br><code>tar -xf "zig-x86_64-linux-0.15.1.tar.xz"</code>;</li>
          <li>check installed zig:<br><code>./zig-x86_64-linux-0.15.1/zig --version</code>.</li>
        </ol>

        You can take actual minisign public key in <a href="https://ziglang.org/download/">download page</a>.
      </p>

      <h2>Privacy policy</h2>

      <p>This mirror is a non-profit project available on a voluntary basis. The author has no plans to fund it.</p>

      <p>Since the mirror is hosted on hardware, we collect access logs to combat bots and brute-force attacks.
         The logs are used for security purposes and load planning, are not shared with third parties,
         and are deleted after 30 days.</p>

      <p>Third-party analytics systems are not used, same as client-side trackers.</p>
    </main>
  </body>
</html>
"#;

const CSS: &str = "
:root {
    font-size: 1.125rem;
    line-height: 1.4;
    font-family:
    'Alegreya Sans', -apple-system, BlinkMacSystemFont,
    'Segoe UI', Roboto, Oxygen, Ubuntu, Cantarell,
    'Open Sans', 'Helvetica Neue', sans-serif;
}

html {
    margin: 0;
    padding: 0;
}

body {
    margin: 0 auto;
    padding: 1.5em 2em;
    max-width: 680px;
}

code {
    font-size: .75rem;
}

h1, h2, h3, h4, h5, h6 {
    font-weight: 700;
    line-height: 1.2;
    margin: 0;
}

h1 {
    font-size: 2.75rem;
}

h1:first-child {
    margin-top: 0;
}

th {
    text-align: start;
}
";
