// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::{Router, body, extract, http, response, routing};
use tracing::error;

use crate::proxy;
use crate::storage;
use repos::{Backend, BackendError, BackendSpec, ResolvedFile};

/// Generic controller for backend HTTP handling.
pub struct BackendController<S: BackendSpec> {
    backend: Arc<Backend<S>>,
    storage: Arc<storage::StorageService>,
    upstream: Arc<proxy::ProxyService>,
}

impl<S: BackendSpec> BackendController<S> {
    pub fn new(
        backend: Arc<Backend<S>>,
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
        let (url, mime) = match controller.backend.resolve_file(&filename).await {
            Ok(ResolvedFile::Content { data, mime }) => {
                return Ok(Self::build_response(http::StatusCode::OK, data, mime));
            }
            Ok(ResolvedFile::Upstream { url, mime }) => (url, mime),
            Err(BackendError::NotFound) => {
                error!(backend = S::ID, filename, "file not found");
                return Err(http::StatusCode::NOT_FOUND);
            }
            Err(e) => {
                error!(backend = S::ID, filename, "error resolving file: {e}");
                return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
            }
        };

        match controller.storage.get(S::ID, &filename).await {
            Ok(Some(entry)) => {
                return Ok(Self::build_response(
                    http::StatusCode::OK,
                    entry.file_bytes.0,
                    mime,
                ));
            }
            Ok(None) => {}
            Err(err) => {
                error!(
                    backend = S::ID,
                    filename, "failed to get file from storage: {err}"
                );
                return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
            }
        }

        let entry = controller
            .upstream
            .fetch(proxy::DownloadRequest { url })
            .await?;

        match controller.storage.put(S::ID, &filename, &entry.bytes).await {
            Ok(()) => {}
            Err(err) => {
                error!(
                    backend = S::ID,
                    filename, "failed to put file to storage: {err}"
                );
                return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
            }
        }

        Ok(Self::build_response(
            http::StatusCode::OK,
            entry.bytes,
            mime,
        ))
    }

    fn build_response(
        status: http::StatusCode,
        bytes: bytes::Bytes,
        mime: repos::ContentType,
    ) -> response::Response {
        response::Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, mime.as_str())
            .header(http::header::CONTENT_LENGTH, bytes.len())
            .body(body::Body::from(bytes))
            .unwrap()
    }
}
