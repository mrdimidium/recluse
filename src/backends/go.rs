// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::{Router, body, extract, http, response, routing};
use thiserror::Error;
use tracing::error;

use crate::config;
use crate::proxy;
use crate::storage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Archive {
    TarGz,
    Zip,
    Msi,
    Pkg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseType {
    Stable,
    ReleaseCandidate(u32),
    Beta(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TarballType<'a> {
    Source,
    Binary { os: &'a str, arch: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid tarball filename")]
struct ParseError;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoVersion {
    major: u32,
    minor: u32,
    patch: Option<u32>,
    release_type: ReleaseType,
}

/// Describes a single file stored at `dl.google.com/go/`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tarball<'a> {
    filename: &'a str,
    version: GoVersion,
    tarball_type: TarballType<'a>,
    archive: Archive,
    sha256: bool,
}

impl<'a> Tarball<'a> {
    pub fn parse(filename: &'a str) -> Result<Self, ParseError> {
        let mut buffer = filename;
        let mut sha256 = false;
        let archive;

        // Strip "go" prefix
        buffer = buffer.strip_prefix("go").ok_or(ParseError)?;

        // Check for .sha256 suffix
        if let Some(it) = buffer.strip_suffix(".sha256") {
            buffer = it;
            sha256 = true;
        }

        // Determine archive type
        if let Some(it) = buffer.strip_suffix(".tar.gz") {
            buffer = it;
            archive = Archive::TarGz;
        } else if let Some(it) = buffer.strip_suffix(".zip") {
            buffer = it;
            archive = Archive::Zip;
        } else if let Some(it) = buffer.strip_suffix(".msi") {
            buffer = it;
            archive = Archive::Msi;
        } else if let Some(it) = buffer.strip_suffix(".pkg") {
            buffer = it;
            archive = Archive::Pkg;
        } else {
            return Err(ParseError);
        }

        if buffer.is_empty() {
            return Err(ParseError);
        }

        // Split by dots: "1.25.6.linux-amd64" -> ["1", "25", "6", "linux-amd64"]
        // or "1.26rc2.linux-amd64" -> ["1", "26rc2", "linux-amd64"]
        let parts: Vec<&str> = buffer.split('.').collect();
        if parts.len() < 3 {
            return Err(ParseError);
        }

        // Parse major version
        let major = parts[0].parse::<u32>().map_err(|_| ParseError)?;

        // Parse minor version with possible release type (rc/beta)
        let (minor, release_type) = parse_minor_with_release(parts[1])?;

        // Determine patch and remainder
        let (patch, remainder) = if parts.len() >= 4 {
            // Try to parse parts[2] as patch number
            if let Ok(p) = parts[2].parse::<u32>() {
                (Some(p), parts[3])
            } else {
                (None, parts[2])
            }
        } else {
            // parts.len() == 3, no patch
            (None, parts[2])
        };

        // Determine tarball type
        let tarball_type = if remainder == "src" {
            TarballType::Source
        } else if let Some((os, arch)) = remainder.split_once('-') {
            TarballType::Binary { os, arch }
        } else {
            return Err(ParseError);
        };

        Ok(Tarball {
            filename,
            version: GoVersion {
                major,
                minor,
                patch,
                release_type,
            },
            tarball_type,
            archive,
            sha256,
        })
    }

    /// Builds the upstream URL for this tarball.
    pub fn upstream_url(&self, source: &str) -> String {
        // Use direct URL (go.dev/dl/ redirects to dl.google.com/go/)
        format!(
            "https://dl.google.com/go/{}?source={}",
            self.filename, source
        )
    }
}

/// Parses minor version with possible release type suffix.
/// "25" -> (25, Stable)
/// "26rc2" -> (26, ReleaseCandidate(2))
/// "26beta1" -> (26, Beta(1))
fn parse_minor_with_release(s: &str) -> Result<(u32, ReleaseType), ParseError> {
    if let Some(idx) = s.find("rc") {
        let minor = s[..idx].parse::<u32>().map_err(|_| ParseError)?;
        let rc_num = s[idx + 2..].parse::<u32>().map_err(|_| ParseError)?;
        return Ok((minor, ReleaseType::ReleaseCandidate(rc_num)));
    }
    if let Some(idx) = s.find("beta") {
        let minor = s[..idx].parse::<u32>().map_err(|_| ParseError)?;
        let beta_num = s[idx + 4..].parse::<u32>().map_err(|_| ParseError)?;
        return Ok((minor, ReleaseType::Beta(beta_num)));
    }
    let minor = s.parse::<u32>().map_err(|_| ParseError)?;
    Ok((minor, ReleaseType::Stable))
}

pub struct GoController {
    config: Arc<config::ConfigService>,
    storage: Arc<storage::StorageService>,
    upstream: Arc<proxy::ProxyService>,
}

impl GoController {
    pub fn new(
        config: Arc<config::ConfigService>,
        storage: Arc<storage::StorageService>,
        upstream: Arc<proxy::ProxyService>,
    ) -> Self {
        Self {
            config,
            storage,
            upstream,
        }
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/go/{filename}", routing::get(Self::handle))
            .with_state(self)
    }

    async fn handle(
        extract::State(controller): extract::State<Arc<Self>>,
        extract::Path(filename): extract::Path<String>,
    ) -> Result<response::Response, http::StatusCode> {
        let tarball = Tarball::parse(&filename).map_err(|_| http::StatusCode::NOT_FOUND)?;
        let url = tarball.upstream_url(controller.config.appname());

        match controller.storage.get("go", &filename).await {
            Ok(Some(entry)) => {
                return Ok(Self::build_response(
                    http::StatusCode::OK,
                    entry.file_bytes.0,
                ));
            }
            Ok(None) => {}
            Err(err) => {
                error!("failed get file from storage: {err}");
                return Err(http::StatusCode::INTERNAL_SERVER_ERROR);
            }
        }

        let entry = controller
            .upstream
            .fetch(proxy::DownloadRequest { url })
            .await?;

        match controller.storage.put("go", &filename, &entry.bytes).await {
            Ok(()) => {}
            Err(_) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stable_binary() {
        let t = Tarball::parse("go1.25.6.linux-amd64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 25);
        assert_eq!(t.version.patch, Some(6));
        assert!(matches!(t.version.release_type, ReleaseType::Stable));
        assert!(matches!(
            t.tarball_type,
            TarballType::Binary {
                os: "linux",
                arch: "amd64"
            }
        ));
        assert_eq!(t.archive, Archive::TarGz);
        assert!(!t.sha256);
    }

    #[test]
    fn test_parse_first_minor_release() {
        let t = Tarball::parse("go1.25.linux-amd64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 25);
        assert_eq!(t.version.patch, None);
        assert!(matches!(t.version.release_type, ReleaseType::Stable));
    }

    #[test]
    fn test_parse_rc() {
        let t = Tarball::parse("go1.26rc2.linux-amd64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 26);
        assert_eq!(t.version.patch, None);
        assert!(matches!(
            t.version.release_type,
            ReleaseType::ReleaseCandidate(2)
        ));
    }

    #[test]
    fn test_parse_beta() {
        let t = Tarball::parse("go1.26beta1.darwin-arm64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 26);
        assert!(matches!(t.version.release_type, ReleaseType::Beta(1)));
    }

    #[test]
    fn test_parse_source() {
        let t = Tarball::parse("go1.25.6.src.tar.gz").unwrap();
        assert!(matches!(t.tarball_type, TarballType::Source));
        assert_eq!(t.version.patch, Some(6));
    }

    #[test]
    fn test_parse_windows_zip() {
        let t = Tarball::parse("go1.25.6.windows-amd64.zip").unwrap();
        assert_eq!(t.archive, Archive::Zip);
        assert!(matches!(
            t.tarball_type,
            TarballType::Binary {
                os: "windows",
                arch: "amd64"
            }
        ));
    }

    #[test]
    fn test_parse_msi() {
        let t = Tarball::parse("go1.25.6.windows-amd64.msi").unwrap();
        assert_eq!(t.archive, Archive::Msi);
    }

    #[test]
    fn test_parse_pkg() {
        let t = Tarball::parse("go1.25.6.darwin-arm64.pkg").unwrap();
        assert_eq!(t.archive, Archive::Pkg);
    }

    #[test]
    fn test_parse_sha256() {
        let t = Tarball::parse("go1.25.6.linux-amd64.tar.gz.sha256").unwrap();
        assert!(t.sha256);
        assert_eq!(t.archive, Archive::TarGz);
        assert_eq!(t.version.patch, Some(6));
    }

    #[test]
    fn test_upstream_url() {
        let t = Tarball::parse("go1.25.6.linux-amd64.tar.gz").unwrap();
        let url = t.upstream_url("zorian");
        assert_eq!(
            url,
            "https://dl.google.com/go/go1.25.6.linux-amd64.tar.gz?source=zorian"
        );
    }

    #[test]
    fn test_invalid_prefix() {
        assert!(Tarball::parse("rust1.25.6.linux-amd64.tar.gz").is_err());
    }

    #[test]
    fn test_invalid_extension() {
        assert!(Tarball::parse("go1.25.6.linux-amd64.tar.bz2").is_err());
    }
}
