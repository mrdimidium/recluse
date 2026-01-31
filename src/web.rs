// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract;
use axum::http::{HeaderValue, Method, Request, Response, StatusCode, header};
use axum::response;
use chrono::{DateTime, Utc};
use sqlx::types::chrono;
use tower::{Layer, Service};
use tracing::error;

const CSP: &str = "default-src 'self'; base-uri 'none'; img-src 'self'; font-src 'self'; style-src 'self'; script-src 'self'; object-src 'none'; frame-ancestors 'none'";

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
    pub fn router(self: Arc<Self>) -> axum::Router {
        axum::Router::new()
            .route("/index.css", axum::routing::get(Self::styles))
            .route("/", axum::routing::get(Self::index))
            .layer(LastModifiedLayer {})
            .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            ))
            .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CSP),
            ))
            .with_state(self)
    }

    async fn index(
        extract::State(controller): extract::State<Arc<Self>>,
    ) -> Result<axum::response::Html<String>, StatusCode> {
        let tmpl = controller.jinja.get_template("index");

        tmpl.and_then(|template| template.render(minijinja::context! {}))
            .map(response::Html)
            .map_err(|err| {
                error!("html rendering failed: {err}");
                StatusCode::INTERNAL_SERVER_ERROR
            })
    }

    async fn styles() -> axum_extra::response::Css<&'static str> {
        axum_extra::response::Css(CSS)
    }
}

#[derive(Clone)]
pub struct LastModifiedLayer {}
impl<S> Layer<S> for LastModifiedLayer {
    type Service = LastModifiedService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        LastModifiedService {
            inner,

            // Since the static file is packaged in a binary, we cache it across restarts.
            // In the future, we can use the build time.
            last_moidified: Utc::now(),
        }
    }
}

#[derive(Clone)]
pub struct LastModifiedService<S> {
    inner: S,
    last_moidified: DateTime<Utc>,
}
impl<S> Service<Request<Body>> for LastModifiedService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let method = req.method().clone();
        let headers = req.headers().clone();
        let last_modified = self.last_moidified;

        let mut inner = self.inner.clone();
        Box::pin(async move {
            let raw = last_modified
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string();
            let lmh = HeaderValue::from_str(raw.as_str()).unwrap();

            if (method == Method::GET || method == Method::HEAD)
                && headers
                    .get(header::IF_MODIFIED_SINCE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_http_date)
                    .is_some_and(|ims| last_modified <= ims)
            {
                match Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::LAST_MODIFIED, lmh.clone())
                    .body(Body::empty())
                {
                    Ok(resp) => {
                        return Ok(resp);
                    }
                    Err(err) => {
                        error!("build response: {err}")
                    }
                }
            }

            let mut resp = inner.call(req).await?;
            resp.headers_mut().insert(header::LAST_MODIFIED, lmh);
            Ok(resp)
        })
    }
}

/// Parse HTTP-date (IMF-fixdate per RFC 7231) or RFC 2822
fn parse_http_date(value: &str) -> Option<DateTime<Utc>> {
    // IMF-fixdate: "Tue, 14 Jan 2026 12:34:56 GMT"
    DateTime::parse_from_rfc2822(value.trim())
        .map(|d| d.with_timezone(&Utc))
        .ok()
}

const HTML: &str = r##"
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
    <h1>Zorian — tiny & opinionated packages mirror.</h1>

    <main>
      <p>This site provides a caching proxy for downloading Zig and Go installation files.
         It reduces load on upstream servers and makes your infrastructure more reliable by adding redundancy.</p>

      <p>Zorian is open source software licensed under <a href="https://www.gnu.org/licenses/agpl-3.0.html">AGPL-3.0</a>.
         Source code is available on <a href="https://github.com/mrdimidium/zorian">GitHub</a>.</p>

      <h2>Usage</h2>

      <p>Replace official download URLs with <code>https://pkg.earth/{tool}/{filename}</code>.
         Files are cached automatically after the first download.</p>

      <h3 id="zig"><a href="#zig">Zig</a></h3>

      <p>Read more about community mirrors in the <a href="https://ziglang.org/download/community-mirrors/">blog post</a>.
         Information on how to deploy your own mirror is available
         <a href="https://github.com/ziglang/www.ziglang.org/blob/main/MIRRORS.md">in the documentation</a>.</p>

      <p>For simplicity, you can use tools like <a href="https://github.com/prantlf/zigup">prantlf/zigup</a> and
         <a href="https://github.com/mlugg/setup-zig">mlugg/setup-zig</a>.</p>

      <p>
        To install manually:
        <ol>
          <li>download zig dist file:<br><code>wget https://pkg.earth/zig/zig-x86_64-linux-0.15.1.tar.xz</code>;</li>
          <li>download zig minisig file:<br><code>wget https://pkg.earth/zig/zig-x86_64-linux-0.15.1.tar.xz.minisig</code>;</li>
          <li>check archive integrity:<br><code>minisign -Vm zig-x86_64-linux-0.15.1.tar.xz -P RWSGOq2NVecA2UPNdBUZykf1CCb147pkmdtYxgb3Ti+JO/wCYvhbAb/U</code>;</li>
          <li>unpack archive:<br><code>tar -xf "zig-x86_64-linux-0.15.1.tar.xz"</code>;</li>
          <li>check installed zig:<br><code>./zig-x86_64-linux-0.15.1/zig --version</code>.</li>
        </ol>
        You can take actual minisig public key at <a href="https://ziglang.org/download/">ziglang.org/download</a>.
      </p>

      <h3 id="go"><a href="#go">Go</a></h3>

      <p>
        To install manually:
        <ol>
          <li>download go dist file:<br><code>wget https://pkg.earth/go/go1.23.0.linux-amd64.tar.gz</code>;</li>
          <li>download go sha256 file:<br><code>wget https://pkg.earth/go/go1.23.0.linux-amd64.tar.gz.sha256</code>;</li>
          <li>check archive integrity:<br><code>sha256sum -c go1.23.0.linux-amd64.tar.gz.sha256</code>;</li>
          <li>unpack archive:<br><code>tar -xzf go1.23.0.linux-amd64.tar.gz</code>;</li>
          <li>check installed go:<br><code>./go/bin/go version</code>.</li>
        </ol>
        You can find available versions at <a href="https://go.dev/dl/">go.dev/dl</a>.
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
"##;

const CSS: &str = r##"
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

h3 a {
    color: inherit;
    text-decoration: none;
}

h3 a:hover {
    text-decoration: underline;
}
"##;
