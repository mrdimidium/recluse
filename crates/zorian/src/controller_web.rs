// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, Response, StatusCode, header};
use axum::{extract, routing};
use maud::{Markup, html};
use serde::Deserialize;
use tower::{Layer, Service};
use tracing::error;

use base::ContentType;
use repos::{GoBackend, ZigBackend};

fn get_asset(path: &str) -> Option<(&'static [u8], ContentType)> {
    Some(match path {
        "base.css" => (include_bytes!("assets/base.css"), ContentType::TextCss),
        "favicon.svg" => (
            include_bytes!("assets/favicon.svg"),
            ContentType::ImageSvgXml,
        ),
        "favicon.ico" => (
            include_bytes!("assets/favicon.ico"),
            ContentType::ImageXIcon,
        ),
        "favicon-192.png" | "apple-touch-icon.png" => (
            include_bytes!("assets/favicon-192.png"),
            ContentType::ImagePng,
        ),
        "favicon-512.png" => (
            include_bytes!("assets/favicon-512.png"),
            ContentType::ImagePng,
        ),
        "manifest.webmanifest" => (
            include_bytes!("assets/manifest.webmanifest"),
            ContentType::ApplicationManifestJson,
        ),
        "jetbrainsmono/JetBrainsMono[wght].woff2" => (
            include_bytes!("assets/jetbrainsmono/JetBrainsMono[wght].woff2"),
            ContentType::FontWoff2,
        ),
        "jetbrainsmono/JetBrainsMono-Italic[wght].woff2" => (
            include_bytes!("assets/jetbrainsmono/JetBrainsMono-Italic[wght].woff2"),
            ContentType::FontWoff2,
        ),
        _ => return None,
    })
}

const CSP: &str = "default-src 'self'; base-uri 'none'; img-src 'self'; font-src 'self'; style-src 'self'; script-src 'self'; object-src 'none'; frame-ancestors 'none'";

#[derive(Deserialize)]
struct LicenseList {
    overview: Vec<LicenseOverview>,
    licenses: Vec<LicenseEntry>,
}

#[derive(Deserialize)]
struct LicenseOverview {
    id: String,
    name: String,
    count: usize,
}

#[derive(Deserialize)]
struct LicenseEntry {
    id: String,
    name: String,
    text: String,
    first_of_kind: bool,
    used_by: Vec<LicenseUsedBy>,
}

#[derive(Deserialize)]
struct LicenseUsedBy {
    #[serde(rename = "crate")]
    crate_: LicenseCrate,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct LicenseCrate {
    name: String,
    version: String,
    repository: Option<String>,
}

static LICENSES: LazyLock<LicenseList> = LazyLock::new(|| {
    let json = include_str!(concat!(env!("OUT_DIR"), "/licenses.json"));
    serde_json::from_str(json).expect("failed to parse licenses.json")
});

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
        axum::Router::new()
            .route("/", axum::routing::get(Self::index))
            .route("/{*path}", routing::get(Self::assets))
            .route("/about/licenses", axum::routing::get(Self::licenses))
            .layer(CacheLayer)
            .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CSP),
            ))
            .with_state(self.clone())
    }

    async fn assets(extract::Path(path): extract::Path<String>) -> Response<Body> {
        let Some((data, content_type)) = get_asset(&path) else {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        };

        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type.as_str());

        if matches!(content_type, ContentType::FontWoff2) {
            builder = builder.header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
        }

        builder.body(Body::from(data)).unwrap()
    }

    async fn index(extract::State(ctrl): extract::State<Arc<Self>>) -> Markup {
        let zig_versions = if let Some(ref backend) = ctrl.zig {
            match backend.get_releases().await {
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
            match backend.get_releases().await {
                Ok(v) => v,
                Err(e) => {
                    error!("failed to get go versions: {e}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        WebController::layout(
            "Zorian — tiny & opinionated packages mirror",
            html! {
                h1 { "Zorian — tiny & opinionated packages mirror." }

                p {
                    r#"This site provides a caching proxy for downloading Zig and Go installation files.
                    It reduces load on upstream servers and makes your infrastructure more reliable by adding redundancy."#
                }

                p {
                    "Zorian is open source software licensed under " a href="https://www.gnu.org/licenses/agpl-3.0.html" { "AGPL-3.0" } ". "
                    "Source code is available on " a href="https://github.com/mrdimidium/zorian" { "GitHub" } ". "
                    "A list of dependency licenses " a href="/about/licenses" { "is available" } "."
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
                                        td { (v.meta.date.as_deref().unwrap_or("-")) }
                                        td {
                                            @if let Some(ref url) = v.meta.docs {
                                                a href=(url) { "docs" }
                                            }
                                            " "
                                            @if let Some(ref url) = v.meta.std_docs {
                                                a href=(url) { "std" }
                                            }
                                            " "
                                            @if let Some(ref url) = v.meta.notes {
                                                a href=(url) { "notes" }
                                            }
                                        }
                                        td {
                                            @for file in &v.files {
                                                a href=(format!("/zig/{}", file.filename)) { (&file.meta.target) }
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
                                        td { @if v.meta.stable { "✓" } @else { "" } }
                                        td {
                                            @for file in &v.files {
                                                a href=(format!("/go/{}", file.filename)) { (file.filename) }
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
            },
        )
    }

    async fn licenses() -> Markup {
        let data = &*LICENSES;

        WebController::layout(
            "Third Party Licenses",
            html! {
                h1 { "Third Party Licenses" }

                p { "This page lists the licenses of the projects used in Zorian." }

                h2 { "Overview of licenses:" }
                ul {
                    @for ov in &data.overview {
                        li { a href=(format!("#{}", ov.id)) { (&ov.name) } " (" (ov.count) ")" }
                    }
                }

                h2 { "All license text:" }
                @for (i, lic) in data.licenses.iter().enumerate() {
                    div.license {
                        @if lic.first_of_kind {
                            h3 id=(&lic.id) { (&lic.name) }
                        }
                        h4 id=(format!("{}-{}", lic.id, i)) { (&lic.name) }

                        p { "Used by:" }

                        ul .license-used-by {
                            @for usage in &lic.used_by {
                                li {
                                    a href=(
                                        usage.crate_.repository.as_deref()
                                            .unwrap_or(&format!("https://crates.io/crates/{}", usage.crate_.name))
                                    ) {
                                        (&usage.crate_.name) "@" (&usage.crate_.version)
                                    }
                                }
                            }
                        }

                        pre .license-text { (&lic.text) }
                    }
                }
            },
        )
    }

    fn layout(title: &str, content: Markup) -> Markup {
        html! {
            (maud::DOCTYPE)

            html lang="en" {
                head {
                    meta charset="UTF-8";
                    meta http-equiv="X-UA-Compatible" content="ie=edge";
                    meta name="viewport" content="width=device-width,initial-scale=1";

                    title { (title) }
                    link rel="stylesheet" href="/base.css";

                    link rel="manifest" href="/manifest.webmanifest";
                    link rel="icon" href="/favicon.ico" sizes="32x32";
                    link rel="icon" href="/favicon.svg" type="image/svg+xml";
                    link rel="apple-touch-icon" href="/apple-touch-icon.png";
                }

                body {
                    (content)
                }
            }
        }
    }
}

#[derive(Clone)]
struct CacheLayer;

impl<S> Layer<S> for CacheLayer {
    type Service = CacheService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        CacheService { inner }
    }
}

#[derive(Clone)]
struct CacheService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for CacheService<S>
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
        let if_none_match = req.headers().get(header::IF_NONE_MATCH).cloned();

        let mut inner = self.inner.clone();
        Box::pin(async move {
            let resp = inner.call(req).await?;

            if !resp.status().is_success() {
                return Ok(resp);
            }

            let (parts, body) = resp.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();

            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&bytes);
            let etag = format!("\"{}\"", hex::encode(hasher.finalize().to_be_bytes()));
            let etag_header = HeaderValue::from_str(&etag).unwrap();
            let cache_control = parts
                .headers
                .get(header::CACHE_CONTROL)
                .cloned()
                .unwrap_or(HeaderValue::from_static("no-cache"));

            if (method == Method::GET || method == Method::HEAD)
                && if_none_match
                    .as_ref()
                    .is_some_and(|v| v.as_bytes() == etag.as_bytes())
            {
                let mut resp = Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .body(Body::empty())
                    .unwrap();
                resp.headers_mut().insert(header::ETAG, etag_header);
                resp.headers_mut()
                    .insert(header::CACHE_CONTROL, cache_control);
                return Ok(resp);
            }

            let mut resp = Response::from_parts(parts, Body::from(bytes));
            resp.headers_mut().insert(header::ETAG, etag_header);
            resp.headers_mut()
                .entry(header::CACHE_CONTROL)
                .or_insert(HeaderValue::from_static("no-cache"));
            Ok(resp)
        })
    }
}
