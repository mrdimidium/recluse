// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, http};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use tower::ServiceExt;
use tower_http::follow_redirect::FollowRedirect;
use url::Url;

use super::backends::{BackendError, BackendNetwork};

#[derive(Clone)]
pub struct DownloadRequest {
    pub url: Url,
}

#[derive(Clone)]
pub struct File {
    pub bytes: Bytes,
}

pub struct ProxyService {
    client: FollowRedirect<Client<HttpsConnector<HttpConnector>, Empty<Bytes>>>,
}

impl ProxyService {
    pub fn new() -> Self {
        let https = HttpsConnector::new();
        let client = Client::builder(TokioExecutor::new()).build(https);
        Self {
            client: FollowRedirect::new(client),
        }
    }

    pub async fn fetch(&self, request: DownloadRequest) -> Result<File, http::StatusCode> {
        let request = Request::builder()
            .method(http::Method::GET)
            .uri(request.url.as_str())
            .header(http::header::USER_AGENT, "zorian/0.1")
            .body(Empty::<Bytes>::new())
            .unwrap();

        let response = self
            .client
            .clone()
            .oneshot(request)
            .await
            .map_err(|_| http::StatusCode::GATEWAY_TIMEOUT)?;

        let (parts, body) = response.into_parts();
        let status = parts.status;
        if !status.is_success() {
            return Err(status);
        }

        let bytes = body
            .collect()
            .await
            .map_err(|_| http::StatusCode::GATEWAY_TIMEOUT)?
            .to_bytes();

        Ok(File { bytes })
    }
}

#[async_trait::async_trait]
impl BackendNetwork for ProxyService {
    async fn http_get(&self, url: &url::Url) -> Result<bytes::Bytes, BackendError> {
        self.fetch(DownloadRequest { url: url.clone() })
            .await
            .map(|f| f.bytes)
            .map_err(|e| BackendError::Network(e.to_string()))
    }
}
