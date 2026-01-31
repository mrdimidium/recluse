// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::{Router, body, extract, http, response, routing};
use tracing::error;

use crate::backends::Backend;
use crate::proxy;
use crate::storage;

/// Generic controller for backend HTTP handling.
pub struct BackendController<B: Backend> {
    backend: B,
    storage: Arc<storage::StorageService>,
    upstream: Arc<proxy::ProxyService>,
}

impl<B: Backend> BackendController<B> {
    pub fn new(
        backend: B,
        storage: Arc<storage::StorageService>,
        upstream: Arc<proxy::ProxyService>,
    ) -> Self {
        Self {
            backend,
            storage,
            upstream,
        }
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/{filename}", routing::get(Self::handle))
            .with_state(self)
    }

    async fn handle(
        extract::State(controller): extract::State<Arc<Self>>,
        extract::Path(filename): extract::Path<String>,
    ) -> Result<response::Response, http::StatusCode> {
        let url = match controller.backend.upstream_url(&filename) {
            Ok(url) => url,
            Err(()) => {
                error!(backend = B::ID, filename, "invalid filename");
                return Err(http::StatusCode::NOT_FOUND);
            }
        };

        match controller.storage.get(B::ID, &filename).await {
            Ok(Some(entry)) => {
                return Ok(Self::build_response(
                    http::StatusCode::OK,
                    entry.file_bytes.0,
                ));
            }
            Ok(None) => {}
            Err(err) => {
                error!(
                    backend = B::ID,
                    filename, "failed to get file from storage: {err}"
                );
                return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
            }
        }

        let entry = controller
            .upstream
            .fetch(proxy::DownloadRequest { url })
            .await?;

        match controller.storage.put(B::ID, &filename, &entry.bytes).await {
            Ok(()) => {}
            Err(err) => {
                error!(
                    backend = B::ID,
                    filename, "failed to put file to storage: {err}"
                );
                return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
            }
        }

        Ok(Self::build_response(http::StatusCode::OK, entry.bytes))
    }

    fn build_response(status: http::StatusCode, bytes: bytes::Bytes) -> response::Response {
        response::Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, "application/octet-stream")
            .header(http::header::CONTENT_LENGTH, bytes.len())
            .body(body::Body::from(bytes))
            .unwrap()
    }
}
