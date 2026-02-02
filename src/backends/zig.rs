// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use semver::Version as SemVersion;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use super::{
    Archive, Backend, BackendDelegate, FileKind, IndexError, ResolveError, ResolvedFile,
    VersionType, stable_version,
};
use crate::utils::{deserialize_duration_secs, deserialize_size};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ZigConfig {
    pub enabled: bool,
    pub upstream: Url,
    #[serde(deserialize_with = "deserialize_duration_secs")]
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

/// Wrapper for sort key computation.
enum ZigVersion {
    Master,
    Semver(SemVersion),
}
impl ZigVersion {
    fn parse(s: &str) -> Result<Self, IndexError> {
        if s == "master" {
            return Ok(Self::Master);
        }
        SemVersion::parse(s)
            .map(Self::Semver)
            .map_err(|_| IndexError::Parse(format!("invalid zig version: {s}")))
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

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid tarball filename")]
struct ParseError;

/// Describes a single file stored at `ziglang.org/download/`.
///
/// The tarball naming has changed several times. When parsing,
/// we standardize the files, but for the reverse operation
/// (getting a string from a tarball), we preserve the original path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ZigFile<'a> {
    filename: &'a str,
    os: Option<&'a str>,
    arch: Option<&'a str>,
    kind: FileKind,
    minisig: bool,
    archive: Archive,
    version: SemVersion,
    development: bool,
}

impl<'a> ZigFile<'a> {
    pub fn parse(filename: &'a str) -> Result<Self, ParseError> {
        let mut buffer = filename;
        let mut minisig = false;
        let archive;

        // (?:|-bootstrap|-[a-zA-Z0-9_]+-[a-zA-Z0-9_]+)-(
        // \d+\.\d+\.\d+(?:-dev\.\d+\+[0-9a-f]+)?
        // )\.(?:tar\.xz|zip)(?:\.minisig)?
        buffer = buffer.strip_prefix("zig-").ok_or(ParseError)?;

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
            return Err(ParseError);
        }

        if buffer.is_empty() {
            return Err(ParseError);
        }

        let mut it = buffer.rsplit('-');
        let last = it.next().ok_or(ParseError)?;

        let development = last.starts_with("dev");

        let version = if !development {
            SemVersion::parse(last).map_err(|_| ParseError)?
        } else {
            let semver = it.next().ok_or(ParseError)?;
            let devver = last;
            let version_str = format!("{}-{}", semver, devver);
            SemVersion::parse(&version_str).map_err(|_| ParseError)?
        };

        let (os, arch, kind) = if let Some(payload) = it.next() {
            if payload == "bootstrap" {
                (None, None, FileKind::Bootstrap)
            } else {
                // Version 0.14.0 is the last one to use the OS-ARCH format in names; newer versions use ARCH-OS.
                let min_version = SemVersion::new(0, 14, 0);
                let (os, arch) = if version > min_version {
                    (payload, it.next().ok_or(ParseError)?)
                } else {
                    (it.next().ok_or(ParseError)?, payload)
                };
                (Some(os), Some(arch), FileKind::Archive)
            }
        } else {
            (None, None, FileKind::Source)
        };

        if it.next().is_some() {
            return Err(ParseError);
        }

        Ok(ZigFile {
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

    /// Builds the upstream URL for this tarball.
    pub fn upstream_url(&self, upstream: &Url, source: &str) -> Result<Url, ()> {
        let mut url = upstream.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|_| ())?;
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZigTarball {
    /// e.g. "zig-x86_64-linux-0.15.2.tar.xz"
    /// Note: upstream API returns full URL in "tarball" field, we extract filename when storing
    #[serde(alias = "tarball")]
    pub filename: String,

    /// e.g. "02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239"
    pub shasum: String,

    /// e.g. 53733924
    #[serde(deserialize_with = "deserialize_size")]
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZigRelease {
    /// e.g. "0.15.2" (older releases don't have this field)
    #[serde(default)]
    pub version: String,

    /// e.g. "2025-10-11"
    pub date: Option<String>,

    /// e.g. "https://ziglang.org/documentation/0.15.2/"
    pub docs: Option<String>,

    /// e.g. "https://ziglang.org/documentation/0.15.2/std/"
    #[serde(rename = "stdDocs")]
    pub std_docs: Option<String>,

    /// e.g. "https://ziglang.org/download/0.15.2/release-notes.html"
    pub notes: Option<String>,

    /// Source tarball
    pub src: Option<ZigTarball>,

    /// Bootstrap tarball
    pub bootstrap: Option<ZigTarball>,

    /// Platform-specific files (e.g., "x86_64-linux", "aarch64-macos")
    #[serde(flatten)]
    pub targets: HashMap<String, ZigTarball>,
}

pub struct ZigBackend {
    config: ZigConfig,
    source: String,
    delegate: Arc<dyn BackendDelegate>,
}
impl ZigBackend {
    pub fn new(config: ZigConfig, source: String, delegate: Arc<dyn BackendDelegate>) -> Self {
        Self {
            config,
            source,
            delegate,
        }
    }
}
#[async_trait::async_trait]
impl Backend for ZigBackend {
    const ID: &'static str = "zig";
    type Release = self::ZigRelease;

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn refresh_interval(&self) -> std::time::Duration {
        self.config.refresh_interval
    }

    async fn resolve_file(&self, filename: &str) -> Result<ResolvedFile, ResolveError> {
        let file = ZigFile::parse(filename).map_err(|_| ResolveError::NotFound)?;

        // For stable builds, check that file exists in index
        if !file.development {
            let exists: Result<Option<i32>, _> =
                sqlx::query_scalar("SELECT 1 FROM zig_files WHERE filename = ?1")
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
        }

        let url = file
            .upstream_url(&self.config.upstream, &self.source)
            .map_err(|_| ResolveError::Internal)?;
        Ok(ResolvedFile::Upstream(url))
    }

    async fn migrate(&self) -> Result<(), IndexError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS zig_versions (
                id           INTEGER PRIMARY KEY,
                version      TEXT    NOT NULL UNIQUE,
                date         TEXT,
                docs         TEXT,
                std_docs     TEXT,
                notes        TEXT
            ) STRICT",
        )
        .execute(self.delegate.db())
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS zig_files (
                version      TEXT    NOT NULL,
                target       TEXT    NOT NULL,
                filename     TEXT    NOT NULL,
                shasum       TEXT    NOT NULL,
                size         INTEGER NOT NULL,
                PRIMARY KEY (version, target),
                FOREIGN KEY (version) REFERENCES zig_versions(version)
            ) STRICT",
        )
        .execute(self.delegate.db())
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        Ok(())
    }

    async fn fetch_index(&self) -> Result<(), IndexError> {
        let mut url = self.config.upstream.clone();
        url.path_segments_mut()
            .map_err(|_| IndexError::Parse("cannot-be-a-base URL".into()))?
            .pop_if_empty()
            .extend(["download", "index.json"]);
        let bytes = self.delegate.http_get(&url).await?;

        let index: HashMap<String, ZigRelease> =
            serde_json::from_slice(&bytes).map_err(|e| IndexError::Parse(e.to_string()))?;

        for (version_str, version) in index {
            if let Err(e) = self.insert_version(&version_str, &version).await {
                tracing::error!(version = version_str, "failed to index version: {e}");
            }
        }

        Ok(())
    }

    async fn get_versions(&self) -> Result<Vec<Self::Release>, IndexError> {
        use futures::{StreamExt, TryStreamExt};

        #[derive(Deserialize)]
        struct FileRow {
            target: String,
            filename: String,
            shasum: String,
            size: u64,
        }

        sqlx::query_as("
            SELECT
                v.version, v.date, v.docs, v.std_docs, v.notes,
                COALESCE(
                    json_group_array(json_object('target', f.target, 'filename', f.filename, 'shasum', f.shasum, 'size', f.size)) FILTER (WHERE f.version IS NOT NULL),
                    '[]'
                )
            FROM zig_versions v
            LEFT JOIN zig_files f ON v.version = f.version
            GROUP BY v.version
            ORDER BY v.id ASC
        ")
        .fetch(self.delegate.db())
        .map(|row| {
            let (version, date, docs, std_docs, notes, files_json):
                (String, Option<String>, Option<String>, Option<String>, Option<String>, String) =
                row.map_err(|e| IndexError::Database(e.to_string()))?;

            let file_rows: Vec<FileRow> = serde_json::from_str(&files_json)
                .map_err(|e| IndexError::Parse(e.to_string()))?;

            let mut src = None;
            let mut bootstrap = None;
            let mut targets = HashMap::new();

            for f in file_rows {
                let file = ZigTarball { filename: f.filename, shasum: f.shasum, size: f.size };
                match f.target.as_str() {
                    "src" => src = Some(file),
                    "bootstrap" => bootstrap = Some(file),
                    _ => { targets.insert(f.target, file); }
                }
            }

            Ok(ZigRelease { version, date, docs, std_docs, notes, src, bootstrap, targets })
        })
        .try_collect()
        .await
    }
}
impl ZigBackend {
    async fn insert_version(
        &self,
        version_str: &str,
        version: &ZigRelease,
    ) -> Result<(), IndexError> {
        let id = ZigVersion::parse(version_str)?.sort_key();

        let mut tx = self
            .delegate
            .db()
            .begin()
            .await
            .map_err(|e| IndexError::Database(e.to_string()))?;

        sqlx::query(
            "INSERT INTO zig_versions (id, version, date, docs, std_docs, notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(version) DO UPDATE SET
                 id = excluded.id,
                 date = excluded.date, docs = excluded.docs,
                 std_docs = excluded.std_docs, notes = excluded.notes
             WHERE id IS NOT excluded.id OR date IS NOT excluded.date
                OR docs IS NOT excluded.docs OR std_docs IS NOT excluded.std_docs
                OR notes IS NOT excluded.notes",
        )
        .bind(id)
        .bind(version_str)
        .bind(&version.date)
        .bind(&version.docs)
        .bind(&version.std_docs)
        .bind(&version.notes)
        .execute(&mut *tx)
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        if let Some(ref file) = version.src {
            Self::insert_file(&mut tx, version_str, "src", file).await?;
        }
        if let Some(ref file) = version.bootstrap {
            Self::insert_file(&mut tx, version_str, "bootstrap", file).await?;
        }
        for (target, file) in &version.targets {
            Self::insert_file(&mut tx, version_str, target, file).await?;
        }

        tx.commit()
            .await
            .map_err(|e| IndexError::Database(e.to_string()))?;
        Ok(())
    }

    async fn insert_file(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        version: &str,
        target: &str,
        file: &ZigTarball,
    ) -> Result<(), IndexError> {
        let url = Url::parse(&file.filename)
            .map_err(|e| IndexError::Parse(format!("invalid tarball URL: {e}")))?;
        let filename = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| IndexError::Parse(format!("no filename in URL: {}", file.filename)))?;

        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM zig_files WHERE version = ?1 AND target = ?2")
                .bind(version)
                .bind(target)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| IndexError::Database(e.to_string()))?;

        let changed: Option<(i32,)> = sqlx::query_as(
            "INSERT INTO zig_files (version, target, filename, shasum, size)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(version, target) DO UPDATE SET
                 filename = excluded.filename, shasum = excluded.shasum, size = excluded.size
             WHERE filename IS NOT excluded.filename
                OR shasum IS NOT excluded.shasum
                OR size IS NOT excluded.size
             RETURNING 1",
        )
        .bind(version)
        .bind(target)
        .bind(filename)
        .bind(&file.shasum)
        .bind(file.size as i64)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| IndexError::Database(e.to_string()))?;

        if exists.is_some() && changed.is_some() {
            tracing::warn!(version, target, "zig index file changed");
        }

        Ok(())
    }
}
