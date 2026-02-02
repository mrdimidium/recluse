// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use super::{Archive, Backend, BackendDelegate, FileKind, IndexError, ResolveError, ResolvedFile};
use crate::utils::deserialize_duration_secs;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GoConfig {
    pub enabled: bool,
    pub upstream: Url,
    #[serde(deserialize_with = "deserialize_duration_secs")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseType {
    Stable,
    ReleaseCandidate(u64),
    Beta(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid tarball filename")]
struct ParseError;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoVersion {
    major: u64,
    minor: u64,
    patch: Option<u64>,
    release_type: ReleaseType,
}

impl GoVersion {
    /// Parses version string like "go1", "go1.22.3", "go1.23rc1", or "go1.9.2rc2".
    fn parse(s: &str) -> Result<Self, ParseError> {
        let s = s.strip_prefix("go").ok_or(ParseError)?;
        let parts: Vec<&str> = s.split('.').collect();
        let (version, consumed) = Self::from_parts(&parts)?;
        if consumed != parts.len() {
            return Err(ParseError);
        }
        Ok(version)
    }

    /// Parses version from dot-separated parts, returns (version, parts_consumed).
    fn from_parts(parts: &[&str]) -> Result<(Self, usize), ParseError> {
        let major = parts
            .first()
            .ok_or(ParseError)?
            .parse()
            .map_err(|_| ParseError)?;

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
    fn parse_version_part(s: &str) -> Result<(u64, Option<ReleaseType>), ParseError> {
        if let Some(idx) = s.find("rc") {
            let num = s[..idx].parse::<u64>().map_err(|_| ParseError)?;
            let rc_num = s[idx + 2..].parse::<u64>().map_err(|_| ParseError)?;
            return Ok((num, Some(ReleaseType::ReleaseCandidate(rc_num))));
        }
        if let Some(idx) = s.find("beta") {
            let num = s[..idx].parse::<u64>().map_err(|_| ParseError)?;
            let beta_num = s[idx + 4..].parse::<u64>().map_err(|_| ParseError)?;
            return Ok((num, Some(ReleaseType::Beta(beta_num))));
        }
        let num = s.parse::<u64>().map_err(|_| ParseError)?;
        Ok((num, None))
    }

    /// Parses minor version with possible release type suffix.
    /// "25" -> (25, Stable)
    /// "26rc2" -> (26, ReleaseCandidate(2))
    /// "26beta1" -> (26, Beta(1))
    fn parse_minor_with_release(s: &str) -> Result<(u64, ReleaseType), ParseError> {
        let (num, release) = GoVersion::parse_version_part(s)?;
        Ok((num, release.unwrap_or(ReleaseType::Stable)))
    }
}

#[cfg(test)]
mod version_tests {
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
struct GoFile<'a> {
    filename: &'a str,
    version: GoVersion,
    os: Option<&'a str>,
    arch: Option<&'a str>,
    kind: FileKind,
    archive: Archive,
    sha256: bool,
}

impl<'a> GoFile<'a> {
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
        let parts: Vec<&str> = buffer.split('.').collect();
        if parts.len() < 3 {
            return Err(ParseError);
        }

        // Parse version, get how many parts were consumed
        let (version, consumed) = GoVersion::from_parts(&parts)?;

        // Remainder must exist and be either "src" or "os-arch"
        let remainder = parts.get(consumed).ok_or(ParseError)?;
        let (os, arch, kind) = if *remainder == "src" {
            (None, None, FileKind::Source)
        } else if let Some((os, arch)) = remainder.split_once('-') {
            let kind = match archive {
                Archive::Msi | Archive::Pkg => FileKind::Installer,
                _ => FileKind::Archive,
            };
            (Some(os), Some(arch), kind)
        } else {
            return Err(ParseError);
        };

        Ok(GoFile {
            filename,
            version,
            os,
            arch,
            kind,
            archive,
            sha256,
        })
    }

    /// Builds the upstream URL for this tarball.
    pub fn upstream_url(&self, upstream: &Url, source: &str) -> Result<Url, ()> {
        let mut url = upstream.clone();
        url.path_segments_mut()
            .map_err(|_| ())?
            .pop_if_empty()
            .push(self.filename);
        url.query_pairs_mut().append_pair("source", source);
        Ok(url)
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    #[test]
    fn test_parse_stable_binary() {
        let t = GoFile::parse("go1.25.6.linux-amd64.tar.gz").unwrap();
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
        let t = GoFile::parse("go1.25.linux-amd64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 25);
        assert_eq!(t.version.patch, None);
        assert!(matches!(t.version.release_type, ReleaseType::Stable));
    }

    #[test]
    fn test_parse_rc() {
        let t = GoFile::parse("go1.26rc2.linux-amd64.tar.gz").unwrap();
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
        let t = GoFile::parse("go1.26beta1.darwin-arm64.tar.gz").unwrap();
        assert_eq!(t.version.major, 1);
        assert_eq!(t.version.minor, 26);
        assert!(matches!(t.version.release_type, ReleaseType::Beta(1)));
    }

    #[test]
    fn test_parse_source() {
        let t = GoFile::parse("go1.25.6.src.tar.gz").unwrap();
        assert!(matches!(t.kind, FileKind::Source));
        assert_eq!(t.os, None);
        assert_eq!(t.arch, None);
        assert_eq!(t.version.patch, Some(6));
    }

    #[test]
    fn test_parse_windows_zip() {
        let t = GoFile::parse("go1.25.6.windows-amd64.zip").unwrap();
        assert_eq!(t.archive, Archive::Zip);
        assert_eq!(t.os, Some("windows"));
        assert_eq!(t.arch, Some("amd64"));
        assert!(matches!(t.kind, FileKind::Archive));
    }

    #[test]
    fn test_parse_msi() {
        let t = GoFile::parse("go1.25.6.windows-amd64.msi").unwrap();
        assert_eq!(t.archive, Archive::Msi);
        assert!(matches!(t.kind, FileKind::Installer));
    }

    #[test]
    fn test_parse_pkg() {
        let t = GoFile::parse("go1.25.6.darwin-arm64.pkg").unwrap();
        assert_eq!(t.archive, Archive::Pkg);
        assert!(matches!(t.kind, FileKind::Installer));
    }

    #[test]
    fn test_parse_sha256() {
        let t = GoFile::parse("go1.25.6.linux-amd64.tar.gz.sha256").unwrap();
        assert!(t.sha256);
        assert_eq!(t.archive, Archive::TarGz);
        assert_eq!(t.version.patch, Some(6));
    }

    #[test]
    fn test_upstream_url() {
        let t = GoFile::parse("go1.25.6.linux-amd64.tar.gz").unwrap();
        let upstream = Url::parse("https://dl.google.com/go/").unwrap();
        let url = t.upstream_url(&upstream, "zorian:test").unwrap();
        assert_eq!(
            url.as_str(),
            "https://dl.google.com/go/go1.25.6.linux-amd64.tar.gz?source=zorian%3Atest"
        );
    }

    #[test]
    fn test_parse_patch_rc() {
        let t = GoFile::parse("go1.9.2rc2.linux-amd64.tar.gz").unwrap();
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
        assert!(GoFile::parse("rust1.25.6.linux-amd64.tar.gz").is_err());
    }

    #[test]
    fn test_invalid_extension() {
        assert!(GoFile::parse("go1.25.6.linux-amd64.tar.bz2").is_err());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoTarball {
    pub filename: String,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
    pub sha256: String,
    pub size: u64,
    pub kind: FileKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoRelease {
    pub version: String,
    pub stable: bool,
    pub files: Vec<GoTarball>,
}

pub struct GoBackend {
    config: GoConfig,
    source: String,
    delegate: Arc<dyn BackendDelegate>,
}
impl GoBackend {
    pub fn new(config: GoConfig, source: String, delegate: Arc<dyn BackendDelegate>) -> Self {
        Self {
            config,
            source,
            delegate,
        }
    }
}
#[async_trait::async_trait]
impl Backend for GoBackend {
    const ID: &'static str = "go";
    type Release = self::GoRelease;

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn refresh_interval(&self) -> std::time::Duration {
        self.config.refresh_interval
    }

    async fn resolve_file(&self, filename: &str) -> Result<ResolvedFile, ResolveError> {
        // For .sha256 files, return hash directly from the index
        if let Some(base) = filename.strip_suffix(".sha256") {
            let result: Result<Option<String>, _> =
                sqlx::query_scalar("SELECT sha256 FROM go_files WHERE filename = ?1")
                    .bind(base)
                    .fetch_optional(self.delegate.db())
                    .await;

            match result {
                Ok(Some(hash)) => {
                    return Ok(ResolvedFile::Content {
                        data: hash.into(),
                        mime: "text/plain",
                    });
                }
                Ok(None) => return Err(ResolveError::NotFound),
                Err(e) => {
                    tracing::error!(filename, "failed to query sha256: {e}");
                    return Err(ResolveError::Internal);
                }
            }
        }

        // Check that file exists in index before proxying to upstream
        let exists: Result<Option<i32>, _> =
            sqlx::query_scalar("SELECT 1 FROM go_files WHERE filename = ?1")
                .bind(filename)
                .fetch_optional(self.delegate.db())
                .await;

        match exists {
            Ok(None) => return Err(ResolveError::NotFound),
            Err(e) => {
                tracing::error!(filename, "failed to check file existence: {e}");
                return Err(ResolveError::Internal);
            }
            Ok(Some(_)) => {}
        }

        let file = GoFile::parse(filename).map_err(|_| ResolveError::NotFound)?;
        let url = file
            .upstream_url(&self.config.upstream, &self.source)
            .map_err(|_| ResolveError::Internal)?;
        Ok(ResolvedFile::Upstream(url))
    }

    async fn migrate(&self) -> Result<(), IndexError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS go_versions (
                id           INTEGER PRIMARY KEY,
                version      TEXT    NOT NULL UNIQUE,
                stable       INTEGER NOT NULL
            ) STRICT",
        )
        .execute(self.delegate.db())
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS go_files (
                version      TEXT    NOT NULL,
                filename     TEXT    NOT NULL,
                os           TEXT,
                arch         TEXT,
                sha256       TEXT    NOT NULL,
                size         INTEGER NOT NULL,
                kind         TEXT    NOT NULL,
                PRIMARY KEY (version, filename),
                FOREIGN KEY (version) REFERENCES go_versions(version)
            ) STRICT",
        )
        .execute(self.delegate.db())
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        Ok(())
    }

    async fn fetch_index(&self) -> Result<(), IndexError> {
        let mut url = self.config.upstream.clone();
        url.query_pairs_mut()
            .append_pair("mode", "json")
            .append_pair("include", "all");
        let bytes = self.delegate.http_get(&url).await?;

        let versions: Vec<GoRelease> =
            serde_json::from_slice(&bytes).map_err(|e| IndexError::Parse(e.to_string()))?;

        for version in versions {
            if let Err(e) = self.insert_version(&version).await {
                tracing::error!(version = version.version, "failed to index version: {e}");
            }
        }

        Ok(())
    }

    async fn get_versions(&self) -> Result<Vec<Self::Release>, IndexError> {
        use futures::{StreamExt, TryStreamExt};

        sqlx::query_as(
            "
            SELECT
                v.version, v.stable,
                COALESCE(
                    json_group_array(json_object(
                        'filename', f.filename, 'os', f.os, 'arch', f.arch,
                        'version', f.version, 'sha256', f.sha256, 'size', f.size, 'kind', f.kind
                    )) FILTER (WHERE f.filename IS NOT NULL),
                    '[]'
                )
            FROM go_versions v
            LEFT JOIN go_files f ON v.version = f.version
            GROUP BY v.version
            ORDER BY v.id ASC
        ",
        )
        .fetch(self.delegate.db())
        .map(|row| {
            let (version, stable, files_json): (String, bool, String) =
                row.map_err(|e| IndexError::Database(e.to_string()))?;
            let files =
                serde_json::from_str(&files_json).map_err(|e| IndexError::Parse(e.to_string()))?;
            Ok(GoRelease {
                version,
                stable,
                files,
            })
        })
        .try_collect()
        .await
    }
}
impl GoBackend {
    async fn insert_version(&self, version: &GoRelease) -> Result<(), IndexError> {
        let id = GoVersion::parse(&version.version)
            .map(|v| v.sort_key())
            .map_err(|_| IndexError::Parse(format!("invalid go version: {}", version.version)))?;

        let mut tx = self
            .delegate
            .db()
            .begin()
            .await
            .map_err(|e| IndexError::Database(e.to_string()))?;

        sqlx::query(
            "INSERT INTO go_versions (id, version, stable)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(version) DO UPDATE SET
                 id = excluded.id,
                 stable = excluded.stable
             WHERE id IS NOT excluded.id OR stable IS NOT excluded.stable",
        )
        .bind(id)
        .bind(&version.version)
        .bind(version.stable)
        .execute(&mut *tx)
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        for file in &version.files {
            Self::insert_file(&mut tx, &version.version, file).await?;
        }

        tx.commit()
            .await
            .map_err(|e| IndexError::Database(e.to_string()))?;
        Ok(())
    }

    async fn insert_file(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        version: &str,
        file: &GoTarball,
    ) -> Result<(), IndexError> {
        let kind = match file.kind {
            FileKind::Source => "source",
            FileKind::Archive => "archive",
            FileKind::Installer => "installer",
            FileKind::Bootstrap => unreachable!("go does not have bootstrap files"),
        };

        let os = file.os.as_deref().filter(|s| !s.is_empty());

        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM go_files WHERE version = ?1 AND filename = ?2")
                .bind(version)
                .bind(&file.filename)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| IndexError::Database(e.to_string()))?;

        let changed: Option<(i32,)> = sqlx::query_as(
            "INSERT INTO go_files (version, filename, os, arch, sha256, size, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(version, filename) DO UPDATE SET
                 os = excluded.os, arch = excluded.arch, sha256 = excluded.sha256,
                 size = excluded.size, kind = excluded.kind
             WHERE os IS NOT excluded.os OR arch IS NOT excluded.arch
                OR sha256 IS NOT excluded.sha256 OR size IS NOT excluded.size
                OR kind IS NOT excluded.kind
             RETURNING 1",
        )
        .bind(version)
        .bind(&file.filename)
        .bind(os)
        .bind(&file.arch)
        .bind(&file.sha256)
        .bind(file.size as i64)
        .bind(kind)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        if exists.is_some() && changed.is_some() {
            tracing::warn!(version, filename = file.filename, "go index file changed");
        }

        Ok(())
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;

    #[test]
    fn test_deserialize_go_release() {
        let json = r#"{
            "version": "go1.22.0",
            "stable": true,
            "files": [
                {
                    "filename": "go1.22.0.windows-amd64.msi",
                    "os": "windows",
                    "arch": "amd64",
                    "version": "go1.22.0",
                    "sha256": "11a47de052db9971359e8c2f3a1667f8d56fa4c6bbec0687cf4cf2403a07628a",
                    "size": 63172608,
                    "kind": "installer"
                }
            ]
        }"#;

        let release: GoRelease = serde_json::from_str(json).unwrap();
        assert_eq!(release.version, "go1.22.0");
        assert_eq!(release.files.len(), 1);
        assert_eq!(release.files[0].filename, "go1.22.0.windows-amd64.msi");
        assert_eq!(release.files[0].size, 63172608);
    }
}
