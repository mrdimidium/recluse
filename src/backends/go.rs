// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use super::*;
use crate::utils::deserialize_duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GoConfig {
    pub enabled: bool,
    pub upstream: Url,
    #[serde(deserialize_with = "deserialize_duration")]
    pub refresh_interval: Duration,
}

impl Default for GoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            upstream: Url::parse("https://go.dev/dl/").unwrap(),
            refresh_interval: Duration::from_secs(60 * 10),
        }
    }
}

impl BackendConfig for GoConfig {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn refresh_interval(&self) -> Duration {
        self.refresh_interval
    }
}

/// Typed metadata for Go files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoFileMeta {
    pub kind: FileKind,
}

/// Typed metadata for Go releases
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoReleaseMeta {
    pub stable: bool,
}

/// Upstream API file format (only used for parsing JSON from go.dev)
#[derive(Deserialize)]
struct UpstreamFile {
    filename: String,
    os: Option<String>,
    arch: Option<String>,
    sha256: String,
    size: u64,
    kind: FileKind,
}

/// Upstream API release format (only used for parsing JSON from go.dev)
#[derive(Deserialize)]
struct UpstreamRelease {
    version: String,
    stable: bool,
    files: Vec<UpstreamFile>,
}

/// Go backend specification
pub struct GoSpec;

#[async_trait::async_trait]
impl BackendSpec for GoSpec {
    const ID: &'static str = "go";

    type Config = GoConfig;
    type ReleaseMeta = GoReleaseMeta;
    type FileMeta = GoFileMeta;
    type Filename<'a> = GoFilename<'a>;

    async fn fetch_index(
        config: &Self::Config,
        network: &dyn BackendNetwork,
    ) -> Result<Vec<(RawRelease, Vec<RawReleaseFile>)>, BackendError> {
        let mut url = config.upstream.clone();
        url.query_pairs_mut()
            .append_pair("mode", "json")
            .append_pair("include", "all");

        let bytes = network.http_get(&url).await?;
        let upstream: Vec<UpstreamRelease> =
            serde_json::from_slice(&bytes).map_err(|e| BackendError::Upstream(e.to_string()))?;

        let mut releases = Vec::with_capacity(upstream.len());
        for u in upstream {
            let sort_key = match GoVersion::parse(&u.version) {
                Ok(v) => v.sort_key(),
                Err(_) => {
                    tracing::error!(version = u.version, "invalid go version, skipping");
                    continue;
                }
            };

            let release: Release<GoReleaseMeta> = Release {
                backend: Self::ID.to_string(),
                version: u.version.clone(),
                sort_key,
                meta: GoReleaseMeta { stable: u.stable },
            };

            let files: Vec<RawReleaseFile> = u
                .files
                .iter()
                .map(|f| {
                    let file: ReleaseFile<GoFileMeta> = ReleaseFile {
                        backend: Self::ID.to_string(),
                        version: u.version.clone(),
                        filename: f.filename.clone(),
                        checksum: f.sha256.clone(),
                        size: f.size as i64,
                        os: f.os.as_ref().and_then(|s| s.parse().ok()),
                        arch: f.arch.as_ref().and_then(|s| s.parse().ok()),
                        meta: GoFileMeta { kind: f.kind },
                    };
                    file.to_raw()
                })
                .collect();

            releases.push((release.to_raw(), files));
        }

        Ok(releases)
    }
}

/// Type alias for Go backend
pub type GoBackend = Backend<GoSpec>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseType {
    Stable,
    ReleaseCandidate(u64),
    Beta(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoVersion {
    major: u64,
    minor: u64,
    patch: Option<u64>,
    release_type: ReleaseType,
}

impl GoVersion {
    /// Parses version string like "go1", "go1.22.3", "go1.23rc1", or "go1.9.2rc2".
    fn parse(s: &str) -> Result<Self, BackendError> {
        let s = s.strip_prefix("go").ok_or(BackendError::NotFound)?;
        let parts: Vec<&str> = s.split('.').collect();
        let (version, consumed) = Self::from_parts(&parts)?;
        if consumed != parts.len() {
            return Err(BackendError::NotFound);
        }
        Ok(version)
    }

    /// Parses version from dot-separated parts, returns (version, parts_consumed).
    fn from_parts(parts: &[&str]) -> Result<(Self, usize), BackendError> {
        let major = parts
            .first()
            .ok_or(BackendError::NotFound)?
            .parse()
            .map_err(|_| BackendError::NotFound)?;

        let Some(minor_str) = parts.get(1) else {
            return Ok((
                Self {
                    major,
                    minor: 0,
                    patch: None,
                    release_type: ReleaseType::Stable,
                },
                1,
            ));
        };
        let (minor, minor_release) = Self::parse_minor_with_release(minor_str)?;

        let Some(patch_str) = parts.get(2) else {
            return Ok((
                Self {
                    major,
                    minor,
                    patch: None,
                    release_type: minor_release,
                },
                2,
            ));
        };
        if let Ok((p, patch_release)) = Self::parse_version_part(patch_str) {
            Ok((
                Self {
                    major,
                    minor,
                    patch: Some(p),
                    release_type: patch_release.unwrap_or(minor_release),
                },
                3,
            ))
        } else {
            Ok((
                Self {
                    major,
                    minor,
                    patch: None,
                    release_type: minor_release,
                },
                2,
            ))
        }
    }

    fn sort_key(&self) -> i64 {
        let vtype = match self.release_type {
            ReleaseType::Beta(n) => super::VersionType::Beta(n),
            ReleaseType::ReleaseCandidate(n) => super::VersionType::Rc(n),
            ReleaseType::Stable => super::VersionType::Stable,
        };
        super::stable_version(self.major, self.minor, self.patch.unwrap_or(0), vtype)
    }

    /// Parses a version part (number with optional rc/beta suffix).
    /// "25" -> (25, None)
    /// "2rc2" -> (2, Some(ReleaseCandidate(2)))
    /// "1beta1" -> (1, Some(Beta(1)))
    fn parse_version_part(s: &str) -> Result<(u64, Option<ReleaseType>), BackendError> {
        if let Some(idx) = s.find("rc") {
            let num = s[..idx]
                .parse::<u64>()
                .map_err(|_| BackendError::NotFound)?;
            let rc_num = s[idx + 2..]
                .parse::<u64>()
                .map_err(|_| BackendError::NotFound)?;
            return Ok((num, Some(ReleaseType::ReleaseCandidate(rc_num))));
        }
        if let Some(idx) = s.find("beta") {
            let num = s[..idx]
                .parse::<u64>()
                .map_err(|_| BackendError::NotFound)?;
            let beta_num = s[idx + 4..]
                .parse::<u64>()
                .map_err(|_| BackendError::NotFound)?;
            return Ok((num, Some(ReleaseType::Beta(beta_num))));
        }
        let num = s.parse::<u64>().map_err(|_| BackendError::NotFound)?;
        Ok((num, None))
    }

    /// Parses minor version with possible release type suffix.
    /// "25" -> (25, Stable)
    /// "26rc2" -> (26, ReleaseCandidate(2))
    /// "26beta1" -> (26, Beta(1))
    fn parse_minor_with_release(s: &str) -> Result<(u64, ReleaseType), BackendError> {
        let (num, release) = GoVersion::parse_version_part(s)?;
        Ok((num, release.unwrap_or(ReleaseType::Stable)))
    }
}

#[cfg(test)]
mod tests_go_version {
    use super::*;

    #[test]
    fn test_parse_go1() {
        let v = GoVersion::parse("go1").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, None);
        assert!(matches!(v.release_type, ReleaseType::Stable));
    }

    #[test]
    fn test_parse_patch_rc() {
        let v = GoVersion::parse("go1.9.2rc2").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 9);
        assert_eq!(v.patch, Some(2));
        assert!(matches!(v.release_type, ReleaseType::ReleaseCandidate(2)));
    }

    #[test]
    fn test_no_collision_go19_and_go192rc2() {
        let v1 = GoVersion::parse("go1.9").unwrap();
        let v2 = GoVersion::parse("go1.9.2rc2").unwrap();
        assert_ne!(v1.sort_key(), v2.sort_key());
        // go1.9 (stable) should sort before go1.9.2rc2
        assert!(v1.sort_key() < v2.sort_key());
    }
}

/// Describes a single file stored at `dl.google.com/go/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoFilename<'a> {
    filename: &'a str,
    version: GoVersion,
    os: Option<&'a str>,
    arch: Option<&'a str>,
    kind: FileKind,
    archive: Archive,
    sha256: bool,
}

impl<'a> BackendFilename<'a, GoConfig> for GoFilename<'a> {
    fn parse(filename: &'a str) -> Result<Self, BackendError> {
        let mut buffer = filename;
        let mut sha256 = false;
        let archive;

        // Strip "go" prefix
        buffer = buffer.strip_prefix("go").ok_or(BackendError::NotFound)?;

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
            return Err(BackendError::NotFound);
        }

        if buffer.is_empty() {
            return Err(BackendError::NotFound);
        }

        // Split by dots: "1.25.6.linux-amd64" -> ["1", "25", "6", "linux-amd64"]
        let parts: Vec<&str> = buffer.split('.').collect();
        if parts.len() < 3 {
            return Err(BackendError::NotFound);
        }

        // Parse version, get how many parts were consumed
        let (version, consumed) = GoVersion::from_parts(&parts)?;

        // Remainder must exist and be either "src" or "os-arch"
        let remainder = parts.get(consumed).ok_or(BackendError::NotFound)?;
        let (os, arch, kind) = if *remainder == "src" {
            (None, None, FileKind::Source)
        } else if let Some((os, arch)) = remainder.split_once('-') {
            let kind = match archive {
                Archive::Msi | Archive::Pkg => FileKind::Installer,
                _ => FileKind::Archive,
            };
            (Some(os), Some(arch), kind)
        } else {
            return Err(BackendError::NotFound);
        };

        Ok(GoFilename {
            filename,
            version,
            os,
            arch,
            kind,
            archive,
            sha256,
        })
    }

    fn upstream_url(&self, config: &GoConfig, source: &str) -> Result<Url, BackendError> {
        let mut url = config.upstream.clone();
        url.path_segments_mut()
            .map_err(|()| BackendError::Internal("cannot build upstream URL".into()))?
            .pop_if_empty()
            .push(self.filename);
        url.query_pairs_mut().append_pair("source", source);
        Ok(url)
    }

    fn is_sha256(&self) -> bool {
        self.sha256
    }
}

#[cfg(test)]
mod tests_go_filename {
    use super::*;

    #[test]
    fn test_parse_stable_binary() {
        let t = GoFilename::parse("go1.25.6.linux-amd64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 25);
        assert_eq!(t.version.patch, Some(6));
        assert!(matches!(t.version.release_type, ReleaseType::Stable));
        assert_eq!(t.os, Some("linux"));
        assert_eq!(t.arch, Some("amd64"));
        assert!(matches!(t.kind, FileKind::Archive));
        assert_eq!(t.archive, Archive::TarGz);
        assert!(!t.sha256);
    }

    #[test]
    fn test_parse_first_minor_release() {
        let t = GoFilename::parse("go1.25.linux-amd64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 25);
        assert_eq!(t.version.patch, None);
        assert!(matches!(t.version.release_type, ReleaseType::Stable));
    }

    #[test]
    fn test_parse_rc() {
        let t = GoFilename::parse("go1.26rc2.linux-amd64.tar.gz").unwrap();
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
        let t = GoFilename::parse("go1.26beta1.darwin-arm64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 26);
        assert!(matches!(t.version.release_type, ReleaseType::Beta(1)));
    }

    #[test]
    fn test_parse_source() {
        let t = GoFilename::parse("go1.25.6.src.tar.gz").unwrap();
        assert!(matches!(t.kind, FileKind::Source));
        assert_eq!(t.os, None);
        assert_eq!(t.arch, None);
        assert_eq!(t.version.patch, Some(6));
    }

    #[test]
    fn test_parse_windows_zip() {
        let t = GoFilename::parse("go1.25.6.windows-amd64.zip").unwrap();
        assert_eq!(t.archive, Archive::Zip);
        assert_eq!(t.os, Some("windows"));
        assert_eq!(t.arch, Some("amd64"));
        assert!(matches!(t.kind, FileKind::Archive));
    }

    #[test]
    fn test_parse_msi() {
        let t = GoFilename::parse("go1.25.6.windows-amd64.msi").unwrap();
        assert_eq!(t.archive, Archive::Msi);
        assert!(matches!(t.kind, FileKind::Installer));
    }

    #[test]
    fn test_parse_pkg() {
        let t = GoFilename::parse("go1.25.6.darwin-arm64.pkg").unwrap();
        assert_eq!(t.archive, Archive::Pkg);
        assert!(matches!(t.kind, FileKind::Installer));
    }

    #[test]
    fn test_parse_sha256() {
        let t = GoFilename::parse("go1.25.6.linux-amd64.tar.gz.sha256").unwrap();
        assert!(t.sha256);
        assert_eq!(t.archive, Archive::TarGz);
        assert_eq!(t.version.patch, Some(6));
    }

    #[test]
    fn test_upstream_url() {
        let t = GoFilename::parse("go1.25.6.linux-amd64.tar.gz").unwrap();
        let config = GoConfig {
            upstream: Url::parse("https://dl.google.com/go/").unwrap(),
            ..Default::default()
        };
        let url = t.upstream_url(&config, "zorian:test").unwrap();
        assert_eq!(
            url.as_str(),
            "https://dl.google.com/go/go1.25.6.linux-amd64.tar.gz?source=zorian%3Atest"
        );
    }

    #[test]
    fn test_parse_patch_rc() {
        let t = GoFilename::parse("go1.9.2rc2.linux-amd64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 9);
        assert_eq!(t.version.patch, Some(2));
        assert!(matches!(
            t.version.release_type,
            ReleaseType::ReleaseCandidate(2)
        ));
    }

    #[test]
    fn test_invalid_prefix() {
        assert!(GoFilename::parse("rust1.25.6.linux-amd64.tar.gz").is_err());
    }

    #[test]
    fn test_invalid_extension() {
        assert!(GoFilename::parse("go1.25.6.linux-amd64.tar.bz2").is_err());
    }
}
