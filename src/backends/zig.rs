// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashMap;
use std::time::Duration;

use semver::Version as SemVersion;
use serde::{Deserialize, Serialize};
use url::Url;

use bytes::Bytes;

use super::*;
use crate::utils::{deserialize_duration, deserialize_size};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ZigConfig {
    pub enabled: bool,
    pub upstream: Url,
    #[serde(deserialize_with = "deserialize_duration")]
    pub refresh_interval: Duration,
}

impl Default for ZigConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            upstream: Url::parse("https://ziglang.org").unwrap(),
            refresh_interval: Duration::from_secs(60 * 10),
        }
    }
}

impl BackendConfig for ZigConfig {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn refresh_interval(&self) -> Duration {
        self.refresh_interval
    }
}

/// Typed metadata for Zig files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZigFileMeta {
    pub target: String,

    #[serde(default)]
    pub minisig: Option<String>,
}

/// Typed metadata for Zig releases (stored in DB)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZigReleaseMeta {
    pub date: Option<String>,
    pub docs: Option<String>,
    #[serde(alias = "stdDocs")]
    pub std_docs: Option<String>,
    pub notes: Option<String>,
}

/// Upstream API tarball format (only used for parsing JSON from ziglang.org)
#[derive(Deserialize)]
struct UpstreamFile {
    #[serde(alias = "tarball")]
    filename: String,
    shasum: String,
    #[serde(deserialize_with = "deserialize_size")]
    size: u64,
}

/// Upstream API release format (only used for parsing JSON from ziglang.org)
#[derive(Deserialize)]
#[allow(dead_code)]
struct UpstreamRelease {
    #[serde(flatten)]
    meta: ZigReleaseMeta,
    /// Present only in the "master" entry
    version: Option<String>,
    src: Option<UpstreamFile>,
    bootstrap: Option<UpstreamFile>,
    #[serde(flatten)]
    targets: HashMap<String, UpstreamFile>,
}

/// Zig file with metadata
pub type ZigFile = ReleaseFile<ZigFileMeta>;

impl ZigFile {
    fn from_upstream(
        version: &str,
        target: &str,
        tarball: &UpstreamFile,
    ) -> Option<RawReleaseFile> {
        let url = Url::parse(&tarball.filename).ok()?;
        let filename = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())?;

        let parsed = ZigFilename::parse(filename).ok()?;

        let file: ReleaseFile<ZigFileMeta> = ReleaseFile {
            backend: ZigSpec::ID.to_string(),
            version: version.to_string(),
            filename: filename.to_string(),
            checksum: tarball.shasum.clone(),
            size: tarball.size as i64,
            os: parsed.os.and_then(|s| s.parse().ok()),
            arch: parsed.arch.and_then(|s| s.parse().ok()),
            meta: ZigFileMeta {
                minisig: None,
                target: target.to_string(),
            },
        };
        Some(file.to_raw())
    }
}

/// Zig backend specification
pub struct ZigSpec;

#[async_trait::async_trait]
impl BackendSpec for ZigSpec {
    const ID: &'static str = "zig";
    const SIGNATURE_META_FIELD: Option<&'static str> = Some("minisig");

    type Config = ZigConfig;
    type ReleaseMeta = ZigReleaseMeta;
    type FileMeta = ZigFileMeta;
    type Filename<'a> = ZigFilename<'a>;

    async fn fetch_index(
        config: &Self::Config,
        network: &dyn BackendNetwork,
    ) -> Result<Vec<(RawRelease, Vec<RawReleaseFile>)>, BackendError> {
        let mut url = config.upstream.clone();
        url.path_segments_mut()
            .map_err(|_| BackendError::Internal("cannot-be-a-base URL".into()))?
            .pop_if_empty()
            .extend(["download", "index.json"]);

        let bytes = network.http_get(&url).await?;
        let map: HashMap<String, UpstreamRelease> =
            serde_json::from_slice(&bytes).map_err(|e| BackendError::Upstream(e.to_string()))?;

        let mut releases = Vec::with_capacity(map.len());
        for (version_str, upstream) in map {
            let sort_key = match ZigVersion::parse(&version_str) {
                Ok(v) => v.sort_key(),
                Err(e) => {
                    tracing::error!(version = version_str, "invalid zig version, skipping: {e}");
                    continue;
                }
            };

            let release: Release<ZigReleaseMeta> = Release {
                backend: Self::ID.to_string(),
                version: version_str.to_string(),
                sort_key,
                meta: upstream.meta.clone(),
            };

            let mut files = Vec::new();

            if let Some(ref tarball) = upstream.src
                && let Some(f) = ZigFile::from_upstream(&version_str, "src", tarball)
            {
                files.push(f);
            }
            if let Some(ref tarball) = upstream.bootstrap
                && let Some(f) = ZigFile::from_upstream(&version_str, "bootstrap", tarball)
            {
                files.push(f);
            }
            for (target, tarball) in &upstream.targets {
                if let Some(f) = ZigFile::from_upstream(&version_str, target, tarball) {
                    files.push(f);
                }
            }

            releases.push((release.to_raw(), files));
        }

        Ok(releases)
    }

    async fn fetch_signature(
        file: &RawReleaseFile,
        config: &Self::Config,
        source: &str,
        network: &dyn BackendNetwork,
    ) -> Result<RawReleaseFile, BackendError> {
        let minisig_filename = format!("{}.minisig", file.filename);
        let parsed = ZigFilename::parse(&minisig_filename)
            .map_err(|_| BackendError::Internal(format!("invalid filename: {}", file.filename)))?;

        let url = parsed
            .upstream_url(config, source)
            .map_err(|_| BackendError::Internal("cannot build URL".into()))?;

        let bytes = network.http_get(&url).await?;
        let minisig =
            String::from_utf8(bytes.to_vec()).map_err(|e| BackendError::Upstream(e.to_string()))?;

        let mut typed: ReleaseFile<ZigFileMeta> = file.clone().try_into_typed()?;
        typed.meta.minisig = Some(minisig);

        tracing::debug!(filename = file.filename, "cached minisig");
        Ok(typed.to_raw())
    }
}

/// Type alias for Zig backend
pub type ZigBackend = Backend<ZigSpec>;

/// Wrapper for sort key computation.
pub enum ZigVersion {
    Master,
    Semver(SemVersion),
}

impl ZigVersion {
    fn parse(s: &str) -> Result<Self, BackendError> {
        if s == "master" {
            return Ok(Self::Master);
        }
        SemVersion::parse(s)
            .map(Self::Semver)
            .map_err(|_| BackendError::Upstream(format!("invalid zig version: {s}")))
    }

    fn sort_key(&self) -> i64 {
        match self {
            Self::Master => i64::MAX,
            Self::Semver(v) => {
                let vtype = if v.pre.is_empty() {
                    VersionType::Stable
                } else if v.pre.as_str().starts_with("dev.") {
                    let num = v.pre.as_str()[4..].parse().unwrap_or(0);
                    VersionType::Dev(num)
                } else {
                    VersionType::Stable
                };

                stable_version(v.major, v.minor, v.patch, vtype)
            }
        }
    }
}

/// Describes a single file stored at `ziglang.org/download/`.
///
/// The tarball naming has changed several times. When parsing,
/// we standardize the files, but for the reverse operation
/// (getting a string from a tarball), we preserve the original path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZigFilename<'a> {
    filename: &'a str,
    os: Option<&'a str>,
    arch: Option<&'a str>,
    kind: FileKind,
    minisig: bool,
    archive: Archive,
    version: SemVersion,
    development: bool,
}

impl<'a> BackendFilename<'a, ZigConfig> for ZigFilename<'a> {
    const SIGNATURE_SUFFIX: Option<&'static str> = Some(".minisig");

    /// Builds the upstream URL for this tarball.
    fn parse(filename: &'a str) -> Result<Self, BackendError> {
        let mut buffer = filename;
        let mut minisig = false;
        let archive;

        // (?:|-bootstrap|-[a-zA-Z0-9_]+-[a-zA-Z0-9_]+)-(
        // \d+\.\d+\.\d+(?:-dev\.\d+\+[0-9a-f]+)?
        // )\.(?:tar\.xz|zip)(?:\.minisig)?
        buffer = buffer.strip_prefix("zig-").ok_or(BackendError::NotFound)?;

        // (?:|bootstrap|[a-zA-Z0-9_]+-[a-zA-Z0-9_]+)-(
        // \d+\.\d+\.\d+(?:-dev\.\d+\+[0-9a-f]+)?
        // )\.(?:tar\.xz|zip)
        if let Some(it) = buffer.strip_suffix(".minisig") {
            buffer = it;
            minisig = true;
        }

        // (?:|bootstrap|[a-zA-Z0-9_]+-[a-zA-Z0-9_]+)-(
        // \d+\.\d+\.\d+(?:-dev\.\d+\+[0-9a-f]+)?
        // )
        if let Some(it) = buffer.strip_suffix(".zip") {
            buffer = it;
            archive = Archive::Zip;
        } else if let Some(it) = buffer.strip_suffix(".tar.xz") {
            buffer = it;
            archive = Archive::TarXz;
        } else {
            return Err(BackendError::NotFound);
        }

        if buffer.is_empty() {
            return Err(BackendError::NotFound);
        }

        let mut it = buffer.rsplit('-');
        let last = it.next().ok_or(BackendError::NotFound)?;

        let development = last.starts_with("dev");

        let version = if !development {
            SemVersion::parse(last).map_err(|_| BackendError::NotFound)?
        } else {
            let semver = it.next().ok_or(BackendError::NotFound)?;
            let devver = last;
            let version_str = format!("{}-{}", semver, devver);
            SemVersion::parse(&version_str).map_err(|_| BackendError::NotFound)?
        };

        let (os, arch, kind) = if let Some(payload) = it.next() {
            if payload == "bootstrap" {
                (None, None, FileKind::Bootstrap)
            } else {
                // Filename format changed over time:
                // - <= 0.2.0: used zig-win64 for windows and zig-linux-x86_64 for linux ¯\_(ツ)_/¯
                // - 0.2.0 to 0.14.0: zig-OS-ARCH-VERSION (e.g. zig-linux-x86_64-0.13.0)
                // - > 0.14.0: zig-ARCH-OS-VERSION (e.g. zig-x86_64-linux-0.15.0)
                let (os, arch) = if version <= SemVersion::new(0, 2, 0) && payload == "win64" {
                    ("windows", "x86_64")
                } else if version <= SemVersion::new(0, 14, 0) {
                    (it.next().ok_or(BackendError::NotFound)?, payload)
                } else {
                    (payload, it.next().ok_or(BackendError::NotFound)?)
                };
                (Some(os), Some(arch), FileKind::Archive)
            }
        } else {
            (None, None, FileKind::Source)
        };

        if it.next().is_some() {
            return Err(BackendError::NotFound);
        }

        Ok(ZigFilename {
            filename,
            os,
            arch,
            kind,
            minisig,
            archive,
            version,
            development,
        })
    }

    fn upstream_url(&self, config: &ZigConfig, source: &str) -> Result<Url, BackendError> {
        let mut url = config.upstream.clone();

        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| BackendError::Internal("cannot build upstream URL".into()))?;
            segments.pop_if_empty();
            if self.development {
                segments.push("builds");
            } else {
                segments.push("download").push(&self.version.to_string());
            }
            segments.push(self.filename);
        }

        url.query_pairs_mut().append_pair("source", source);
        Ok(url)
    }

    fn is_sha256(&self) -> bool {
        false // Zig uses minisig, not sha256
    }

    fn is_signature(&self) -> bool {
        self.minisig
    }

    fn can_bypass_index(&self) -> bool {
        self.development
    }

    fn signature_content(&self, file: &RawReleaseFile) -> Option<Bytes> {
        if !self.minisig {
            return None;
        }
        let typed: ReleaseFile<ZigFileMeta> = file.clone().try_into_typed().ok()?;
        typed.meta.minisig.map(|s| s.into_bytes().into())
    }
}

#[cfg(test)]
mod tests_zig_filename {
    use super::*;

    #[test]
    fn parse_old_combined_platform() {
        // <= 0.2.0: zig-PLATFORM-VERSION (only Windows used this format)
        let file = ZigFilename::parse("zig-win64-0.1.1.zip").unwrap();
        assert_eq!(file.os, Some("windows"));
        assert_eq!(file.arch, Some("x86_64"));
        assert_eq!(file.version, SemVersion::new(0, 1, 1));
        assert_eq!(file.archive, Archive::Zip);
        assert_eq!(file.kind, FileKind::Archive);

        let file = ZigFilename::parse("zig-win64-0.2.0.zip").unwrap();
        assert_eq!(file.os, Some("windows"));
        assert_eq!(file.arch, Some("x86_64"));
        assert_eq!(file.version, SemVersion::new(0, 2, 0));
    }

    #[test]
    fn parse_middle_os_arch_format() {
        // 0.2.0 to 0.14.0: zig-OS-ARCH-VERSION
        let file = ZigFilename::parse("zig-linux-x86_64-0.13.0.tar.xz").unwrap();
        assert_eq!(file.os, Some("linux"));
        assert_eq!(file.arch, Some("x86_64"));
        assert_eq!(file.version, SemVersion::new(0, 13, 0));
        assert_eq!(file.archive, Archive::TarXz);

        let file = ZigFilename::parse("zig-windows-x86_64-0.10.0.zip").unwrap();
        assert_eq!(file.os, Some("windows"));
        assert_eq!(file.arch, Some("x86_64"));
    }

    #[test]
    fn parse_new_arch_os_format() {
        // > 0.14.0: zig-ARCH-OS-VERSION
        let file = ZigFilename::parse("zig-x86_64-linux-0.15.0.tar.xz").unwrap();
        assert_eq!(file.os, Some("linux"));
        assert_eq!(file.arch, Some("x86_64"));
        assert_eq!(file.version, SemVersion::new(0, 15, 0));

        let file = ZigFilename::parse("zig-aarch64-macos-0.15.0.tar.xz").unwrap();
        assert_eq!(file.os, Some("macos"));
        assert_eq!(file.arch, Some("aarch64"));
    }

    #[test]
    fn parse_dev_version() {
        let file = ZigFilename::parse("zig-x86_64-linux-0.14.0-dev.123+abc123.tar.xz").unwrap();
        assert!(file.development);
        assert_eq!(file.version.major, 0);
        assert_eq!(file.version.minor, 14);
        assert_eq!(file.version.patch, 0);
    }

    #[test]
    fn parse_source_tarball() {
        let file = ZigFilename::parse("zig-0.13.0.tar.xz").unwrap();
        assert_eq!(file.os, None);
        assert_eq!(file.arch, None);
        assert_eq!(file.kind, FileKind::Source);
    }

    #[test]
    fn parse_bootstrap() {
        let file = ZigFilename::parse("zig-bootstrap-0.13.0.tar.xz").unwrap();
        assert_eq!(file.kind, FileKind::Bootstrap);
    }

    #[test]
    fn parse_minisig() {
        let file = ZigFilename::parse("zig-win64-0.1.1.zip.minisig").unwrap();
        assert!(file.minisig);
        assert_eq!(file.os, Some("windows"));
        assert_eq!(file.arch, Some("x86_64"));

        let file = ZigFilename::parse("zig-x86_64-linux-0.15.0.tar.xz.minisig").unwrap();
        assert!(file.minisig);
        assert_eq!(file.os, Some("linux"));
    }

    #[test]
    fn parse_boundary_version() {
        // 0.14.0 should use OS-ARCH format
        let file = ZigFilename::parse("zig-linux-x86_64-0.14.0.tar.xz").unwrap();
        assert_eq!(file.os, Some("linux"));
        assert_eq!(file.arch, Some("x86_64"));

        // 0.14.1 should use ARCH-OS format
        let file = ZigFilename::parse("zig-x86_64-linux-0.14.1.tar.xz").unwrap();
        assert_eq!(file.os, Some("linux"));
        assert_eq!(file.arch, Some("x86_64"));
    }
}
