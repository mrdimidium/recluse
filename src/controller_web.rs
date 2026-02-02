// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, Response, StatusCode, header};
use axum::{extract, routing};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use rust_embed::Embed;
use sqlx::types::chrono;
use tower::{Layer, Service};
use tracing::error;

use crate::backends::{Backend, GoBackend, ZigBackend};

#[derive(Embed)]
#[folder = "src/assets/"]
struct Assets;

const CSP: &str = "default-src 'self'; base-uri 'none'; img-src 'self'; font-src 'self'; style-src 'self'; script-src 'self'; object-src 'none'; frame-ancestors 'none'";

/// Handles html pages rendering and static files
pub struct WebController {
    zig: Option<Arc<ZigBackend>>,
    go: Option<Arc<GoBackend>>,
}

impl WebController {
    pub fn new(zig: Option<Arc<ZigBackend>>, go: Option<Arc<GoBackend>>) -> Self {
        Self { zig, go }
    }
}

impl WebController {
    pub fn router(self: Arc<Self>) -> axum::Router {
        let pages = axum::Router::new()
            .route("/", axum::routing::get(Self::index))
            .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            ))
            .with_state(self.clone());

        let assets = axum::Router::new().route("/assets/{*path}", routing::get(Self::assets));

        axum::Router::new()
            .merge(pages)
            .merge(assets)
            .layer(LastModifiedLayer {})
            .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CSP),
            ))
    }

    async fn assets(extract::Path(path): extract::Path<String>) -> Response<Body> {
        match Assets::get(&path) {
            Some(file) => {
                let mime = mime_guess::from_path(&path).first_or_octet_stream();
                let is_font = matches!(
                    path.rsplit('.').next(),
                    Some("ttf" | "otf" | "woff" | "woff2" | "eot")
                );

                let mut builder = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, mime.as_ref());

                if is_font {
                    builder = builder
                        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
                }

                builder.body(Body::from(file.data.into_owned())).unwrap()
            }
            None => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap(),
        }
    }

    async fn index(extract::State(ctrl): extract::State<Arc<Self>>) -> Markup {
        let zig_versions = if let Some(ref backend) = ctrl.zig {
            match backend.get_versions().await {
                Ok(v) => v,
                Err(e) => {
                    error!("failed to get zig versions: {e}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let go_versions = if let Some(ref backend) = ctrl.go {
            match backend.get_versions().await {
                Ok(v) => v,
                Err(e) => {
                    error!("failed to get go versions: {e}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        html! {
            (maud::DOCTYPE)

            html lang="en" {
                meta charset="UTF-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                meta http-equiv="X-UA-Compatible" content="ie=edge";

                link rel="stylesheet" href="assets/base.css";
                title { "Earth PKG — tiny & opinionated packages mirror" }
            }

            body {
                h1 { "Zorian — tiny & opinionated packages mirror." }

                main {
                    p {
                     r#"This site provides a caching proxy for downloading Zig and Go installation files.
                        It reduces load on upstream servers and makes your infrastructure more reliable by adding redundancy."#
                    }

                    p {
                        "Zorian is open source software licensed under " a href="https://www.gnu.org/licenses/agpl-3.0.html" { "AGPL-3.0" } ". "
                        "Source code is available on " a href="https://github.com/mrdimidium/zorian" { "GitHub" }
                    }

                    h2 { "Usage" }

                    p {
                        "Replace official download URLs with " code { "https://pkg.earth/{tool}/{filename}" } ". "
                        "Files are cached automatically after the first download."
                    }

                    h3 id="zig" {
                        a href="#zig" { "Zig" }
                    }

                    @if !zig_versions.is_empty() {
                        details {
                            summary { "Available versions (" (zig_versions.len()) ")" }

                            p { "You can take actual minisig public key at " a href="https://ziglang.org/download/" { "ziglang.org/download" } "." }
                            table {
                                thead {
                                    tr {
                                        th { "Version" }
                                        th { "Date" }
                                        th { "Docs" }
                                        th { "Targets" }
                                    }
                                }
                                tbody {
                                    @for v in zig_versions.iter().rev() {
                                        tr {
                                            td { (v.version) }
                                            td { (v.date.as_deref().unwrap_or("-")) }
                                            td {
                                                @if let Some(ref url) = v.docs {
                                                    a href=(url) { "docs" }
                                                }
                                                " "
                                                @if let Some(ref url) = v.std_docs {
                                                    a href=(url) { "std" }
                                                }
                                                " "
                                                @if let Some(ref url) = v.notes {
                                                    a href=(url) { "notes" }
                                                }
                                            }
                                            td {
                                                @if let Some(ref src) = v.src {
                                                    a href=(format!("/zig/{}", src.filename)) { code { "src" } }
                                                    " "
                                                }
                                                @if let Some(ref bootstrap) = v.bootstrap {
                                                    a href=(format!("/zig/{}", bootstrap.filename)) { code { "bootstrap" } }
                                                    " "
                                                }
                                                @for (target, tarball) in v.targets.iter() {
                                                    a href=(format!("/zig/{}", tarball.filename)) { code { (target) } }
                                                    " "
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }


                    p {
                        "Read more about community mirrors in the " a href="https://ziglang.org/download/community-mirrors/" { "blog post" } ". "
                        "Information on how to deploy your own mirror is available " a href="https://github.com/ziglang/www.ziglang.org/blob/main/MIRRORS.md" { "in the documentation" } "."
                    }

                    p {
                        "For simplicity, you can use tools like " a href="https://github.com/prantlf/zigup" { "prantlf/zigup" }
                        " and " a href="https://github.com/mlugg/setup-zig" { "mlugg/setup-zig" } "."
                    }

                    p {
                        "To install manually:"
                        ol {
                            li { "download zig dist file:" br; code { "wget https://pkg.earth/zig/zig-x86_64-linux-0.15.1.tar.xz" } ";" }
                            li { "download zig minisig file:" br; code { "wget https://pkg.earth/zig/zig-x86_64-linux-0.15.1.tar.xz.minisig" } ";" }
                            li { "check archive integrity:" br; code { "minisign -Vm zig-x86_64-linux-0.15.1.tar.xz -P RWSGOq2NVecA2UPNdBUZykf1CCb147pkmdtYxgb3Ti+JO/wCYvhbAb/U" } ";" }
                            li { "unpack archive:" br; code { "tar -xf 'zig-x86_64-linux-0.15.1.tar.xz'" } ";" }
                            li { "check installed zig:" br; code { "./zig-x86_64-linux-0.15.1/zig --version" } ";" }
                        }
                    }

                    h3 id="go" { a href="#go" { "Go" } }

                    @if !go_versions.is_empty() {
                        details {
                            summary { "Available versions (" (go_versions.len()) ")" }

                            p { "You can find available versions at " a href="https://go.dev/dl/" { "go.dev/dl" } "." }
                            table {
                                thead {
                                    tr {
                                        th { "Version" }
                                        th { "Stable" }
                                        th { "Files" }
                                    }
                                }
                                tbody {
                                    @for v in go_versions.iter().rev() {
                                        tr {
                                            td { (v.version) }
                                            td { @if v.stable { "✓" } @else { "" } }
                                            td {
                                                @for file in &v.files {
                                                    a href=(format!("/go/{}", file.filename)) { code { (file.filename) } }
                                                    " "
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    p { "To install manually:" }

                    ol {
                        li { "download go dist file:" br; code { "wget https://pkg.earth/go/go1.23.0.linux-amd64.tar.gz" } ";" }
                        li { "download go sha256 file:" br; code { "wget https://pkg.earth/go/go1.23.0.linux-amd64.tar.gz.sha256" } ";" }
                        li { "check archive integrity:" br; code { "sha256sum -c go1.23.0.linux-amd64.tar.gz.sha256" } ";" }
                        li { "unpack archive:" br; code { "tar -xzf go1.23.0.linux-amd64.tar.gz" } ";" }
                        li { "check installed go:" br; code { "./go/bin/go version" } ";" }
                    }

                    h2 { "Privacy policy" }

                    p { "This mirror is a non-profit project available on a voluntary basis. The author has no plans to fund it." }

                    p { r#"Since the mirror is hosted on hardware, we collect access logs to combat bots and brute-force attacks.
                        The logs are used for security purposes and load planning, are not shared with third parties,
                        and are deleted after 30 days."# }

                    p { "Third-party analytics systems are not used, same as client-side trackers." }
                }
            }
        }
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
