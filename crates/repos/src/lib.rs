// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod go;
pub mod zig;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use url::Url;

pub use base::ContentType;
pub use go::{GoBackend, GoConfig};
pub use zig::{ZigBackend, ZigConfig};

/// Backend operation error.
#[derive(Debug, Clone, Error)]
pub enum BackendError {
    /// Resource not found
    #[error("not found")]
    NotFound,

    /// Upstream network error
    #[error("network: {0}")]
    Network(String),

    /// Internal storage error
    #[error("storage: {0}")]
    Storage(String),

    /// Malformed data from upstream
    #[error("upstream: {0}")]
    Upstream(String),

    /// Internal logic error
    #[error("internal: {0}")]
    Internal(String),
}

/// Operating system
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Os {
    Linux,
    Windows,
    Darwin,
    #[serde(alias = "macos")]
    Macos,
    Freebsd,
    Netbsd,
    Openbsd,
    Illumos,
    Plan9,
    Aix,
    Solaris,
    Dragonfly,
    Android,
    Ios,
    Js,
    Wasip1,
    Wasi,
}

/// CPU architecture
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Arch {
    Amd64,
    #[serde(alias = "x86_64")]
    X86_64,
    Arm64,
    #[serde(alias = "aarch64")]
    Aarch64,
    #[serde(rename = "386")]
    #[strum(serialize = "386")]
    I386,
    Arm,
    Armv6l,
    Armv7a,
    Loong64,
    Mips,
    Mips64,
    Mips64le,
    Mipsle,
    Ppc64,
    Ppc64le,
    Riscv64,
    S390x,
    Wasm32,
    Powerpc,
    Powerpc64,
    Powerpc64le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    TarGz,
    TarXz,
    Zip,
    Msi,
    Pkg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Source,
    Bootstrap,
    Archive,
    Installer,
}

/// Version type for ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionType {
    Stable,
    Rc(u64),
    Beta(u64),
    Dev(u64),
}

/// Result of resolving a file request.
pub enum ResolvedFile {
    /// Proxy request to upstream URL
    Upstream { mime: ContentType, url: Url },
    /// Return content directly
    Content {
        mime: ContentType,
        data: bytes::Bytes,
    },
}

/// Release entry with typed metadata
#[derive(Debug, Clone)]
pub struct Release<M> {
    pub backend: String,
    pub version: String,
    pub sort_key: i64,
    pub meta: M,
}

/// Type alias for storage (untyped meta)
pub type RawRelease = Release<Option<serde_json::Value>>;

impl<M: Serialize> Release<M> {
    pub fn to_raw(&self) -> RawRelease {
        Release {
            backend: self.backend.clone(),
            version: self.version.clone(),
            sort_key: self.sort_key,
            meta: Some(serde_json::to_value(&self.meta).expect("meta serialization")),
        }
    }
}

impl RawRelease {
    pub fn try_into_typed<M: DeserializeOwned>(self) -> Result<Release<M>, BackendError> {
        let meta = self
            .meta
            .ok_or_else(|| BackendError::Internal("missing release meta".into()))?;
        let meta = serde_json::from_value(meta)
            .map_err(|e| BackendError::Internal(format!("release meta: {e}")))?;
        Ok(Release {
            backend: self.backend,
            version: self.version,
            sort_key: self.sort_key,
            meta,
        })
    }
}

/// Release file entry with typed metadata
#[derive(Debug, Clone)]
pub struct ReleaseFile<M> {
    pub backend: String,
    pub version: String,
    pub filename: String,
    pub checksum: String,
    pub size: i64,
    pub os: Option<Os>,
    pub arch: Option<Arch>,
    pub meta: M,
}

/// Type alias for storage (untyped meta)
pub type RawReleaseFile = ReleaseFile<Option<serde_json::Value>>;

impl<M: Serialize> ReleaseFile<M> {
    pub fn to_raw(&self) -> RawReleaseFile {
        ReleaseFile {
            backend: self.backend.clone(),
            version: self.version.clone(),
            filename: self.filename.clone(),
            checksum: self.checksum.clone(),
            size: self.size,
            os: self.os,
            arch: self.arch,
            meta: Some(serde_json::to_value(&self.meta).expect("meta serialization")),
        }
    }
}

impl RawReleaseFile {
    pub fn try_into_typed<M: DeserializeOwned>(self) -> Result<ReleaseFile<M>, BackendError> {
        let meta = self
            .meta
            .ok_or_else(|| BackendError::Internal("missing file meta".into()))?;
        let meta = serde_json::from_value(meta)
            .map_err(|e| BackendError::Internal(format!("file meta: {e}")))?;
        Ok(ReleaseFile {
            backend: self.backend,
            version: self.version,
            filename: self.filename,
            checksum: self.checksum,
            size: self.size,
            os: self.os,
            arch: self.arch,
            meta,
        })
    }
}

/// Release view for display (version + metadata + files)
#[derive(Debug, Clone)]
pub struct ReleaseView<M, F> {
    pub version: String,
    pub meta: M,
    pub files: Vec<F>,
}

/// Trait for backend configuration with common fields.
pub trait BackendConfig {
    fn enabled(&self) -> bool;
    fn refresh_interval(&self) -> Duration;
}

/// Trait for parsing filenames and handling checksums/signatures.
pub trait BackendFilename<'a, C>: Sized {
    /// Backend-specific signature suffix (e.g., ".minisig").
    /// SHA256 is handled automatically by the base.
    const SIGNATURE_SUFFIX: Option<&'static str> = None;

    fn parse(filename: &'a str) -> Result<Self, BackendError>;

    fn upstream_url(&self, config: &C, source: &str) -> Result<Url, BackendError>;

    /// Is this a .sha256 checksum file?
    fn is_sha256(&self) -> bool;

    /// Is this a signature file (minisig, gpg)?
    fn is_signature(&self) -> bool {
        false
    }

    /// Can bypass index check? (for dev builds)
    fn can_bypass_index(&self) -> bool {
        false
    }

    /// Get signature content from file meta (not sha256)
    fn signature_content(&self, _file: &RawReleaseFile) -> Option<Bytes> {
        None
    }
}

/// Storage interface for backend version/file index
#[async_trait]
pub trait BackendStorage: Send + Sync {
    /// Query versions by backend, ordered by sort_key.
    async fn query_releases(&self, backend: &str) -> Result<Vec<RawRelease>, BackendError>;

    /// Insert or update releases with their files in a single transaction.
    async fn insert_releases(
        &self,
        releases: &[(RawRelease, Vec<RawReleaseFile>)],
    ) -> Result<(), BackendError>;

    /// Query files by backend with optional filters.
    async fn query_files(
        &self,
        backend: &str,
        version: Option<&str>,
        filename: Option<&str>,
        meta_null_field: Option<&str>,
    ) -> Result<Vec<RawReleaseFile>, BackendError>;

    /// Update a single file.
    async fn update_file(&self, file: &RawReleaseFile) -> Result<bool, BackendError>;
}

/// Delegate provides I/O primitives to backends.
#[async_trait]
pub trait BackendNetwork: Send + Sync {
    /// HTTP GET request
    async fn http_get(&self, url: &Url) -> Result<Bytes, BackendError>;
}

/// Trait for backend-specific types and methods.
#[async_trait]
pub trait BackendSpec: Send + Sync + 'static {
    /// Fixed unique identifier for storage
    const ID: &'static str;

    /// Meta field where signature is stored (for query_files with meta_null_field)
    /// None = signatures not supported by this backend
    const SIGNATURE_META_FIELD: Option<&'static str> = None;

    type Config: BackendConfig + Send + Sync;
    type Filename<'a>: BackendFilename<'a, Self::Config> + Send;

    type FileMeta: DeserializeOwned;
    type ReleaseMeta: DeserializeOwned;

    /// Fetch release index from upstream and convert to storage format
    async fn fetch_index(
        config: &Self::Config,
        network: &dyn BackendNetwork,
    ) -> Result<Vec<(RawRelease, Vec<RawReleaseFile>)>, BackendError>;

    /// Fetch signature for a file and return updated file
    async fn fetch_signature(
        _file: &RawReleaseFile,
        _config: &Self::Config,
        _source: &str,
        _network: &dyn BackendNetwork,
    ) -> Result<RawReleaseFile, BackendError> {
        unimplemented!("fetch_signature unimplemented"); // default: not supported
    }
}

/// Generic backend implementation
pub struct Backend<S: BackendSpec> {
    pub config: S::Config,
    pub source: String,
    pub storage: Arc<dyn BackendStorage>,
    pub network: Arc<dyn BackendNetwork>,
}

impl<S: BackendSpec> Backend<S> {
    pub fn new(
        config: S::Config,
        source: String,
        storage: Arc<dyn BackendStorage>,
        network: Arc<dyn BackendNetwork>,
    ) -> Self {
        Self {
            config,
            source,
            storage,
            network,
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled()
    }

    pub fn refresh_interval(&self) -> Duration {
        self.config.refresh_interval()
    }

    /// Fetch index from upstream and store in DB
    pub async fn refresh(&self) -> Result<(), BackendError> {
        let releases = S::fetch_index(&self.config, &*self.network).await?;

        self.storage.insert_releases(&releases).await?;

        // Fetch signatures if supported
        if let Some(field) = S::SIGNATURE_META_FIELD {
            let files = self
                .storage
                .query_files(S::ID, None, None, Some(field))
                .await?;

            for file in files {
                match S::fetch_signature(&file, &self.config, &self.source, &*self.network).await {
                    Ok(updated) => {
                        if let Err(e) = self.storage.update_file(&updated).await {
                            tracing::debug!(
                                filename = file.filename,
                                "failed to store signature: {e}"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!(filename = file.filename, "failed to fetch signature: {e}");
                    }
                }
            }
        }

        Ok(())
    }

    /// Load releases from DB
    pub async fn get_releases(
        &self,
    ) -> Result<Vec<ReleaseView<S::ReleaseMeta, ReleaseFile<S::FileMeta>>>, BackendError> {
        let raw_releases = self.storage.query_releases(S::ID).await?;
        let raw_files = self.storage.query_files(S::ID, None, None, None).await?;

        let mut files_by_version: HashMap<String, Vec<ReleaseFile<S::FileMeta>>> = HashMap::new();
        for raw_file in raw_files {
            if let Ok(file) = raw_file.try_into_typed::<S::FileMeta>() {
                files_by_version
                    .entry(file.version.clone())
                    .or_default()
                    .push(file);
            }
        }

        let releases = raw_releases
            .into_iter()
            .filter_map(|raw| {
                let typed: Release<S::ReleaseMeta> = raw.try_into_typed().ok()?;
                let files = files_by_version.remove(&typed.version).unwrap_or_default();
                Some(ReleaseView {
                    version: typed.version,
                    meta: typed.meta,
                    files,
                })
            })
            .collect();

        Ok(releases)
    }

    /// Resolves filename to upstream URL or direct content
    pub async fn resolve_file(&self, filename: &str) -> Result<ResolvedFile, BackendError> {
        let parsed = S::Filename::parse(filename)?;

        let is_checksum_or_sig = parsed.is_sha256() || parsed.is_signature();
        let mime = if is_checksum_or_sig {
            ContentType::TextPlain
        } else {
            ContentType::OctetStream
        };

        let url = parsed.upstream_url(&self.config, &self.source)?;

        // Strip suffix to get base filename
        let base = filename
            .strip_suffix(".sha256")
            .or_else(|| {
                <S::Filename<'_> as BackendFilename<'_, S::Config>>::SIGNATURE_SUFFIX
                    .and_then(|s| filename.strip_suffix(s))
            })
            .unwrap_or(filename);

        let files = self
            .storage
            .query_files(S::ID, None, Some(base), None)
            .await?;

        let Some(file) = files.into_iter().next() else {
            return if parsed.can_bypass_index() {
                Ok(ResolvedFile::Upstream { url, mime })
            } else {
                Err(BackendError::NotFound)
            };
        };

        // SHA256 — universal, always in file.checksum
        if parsed.is_sha256() {
            return Ok(ResolvedFile::Content {
                mime,
                data: file.checksum.clone().into(),
            });
        }

        // Signature — backend supports it, may or may not be indexed
        if parsed.is_signature() {
            if let Some(data) = parsed.signature_content(&file) {
                return Ok(ResolvedFile::Content { mime, data });
            }
            // Not indexed yet, proxy to upstream
            return Ok(ResolvedFile::Upstream { url, mime });
        }

        Ok(ResolvedFile::Upstream { url, mime })
    }
}

/// Computes numeric sort key for correct version ordering.
///
/// Formula: major × 10^12 + minor × 10^9 + patch × 10^6 + type × 10^4 + num
/// Where type: dev=10, beta=25, rc=50, stable=99
///
/// Examples:
/// - 1.21.3 stable       → 1_021_003_990_000
/// - 1.22.0 beta(1)      → 1_022_000_250_001
/// - 1.22.0 rc(2)        → 1_022_000_500_002
/// - 0.13.0 dev(1234)    → 0_013_000_101_234
pub fn stable_version(major: u64, minor: u64, patch: u64, vtype: VersionType) -> i64 {
    let (type_val, num) = match vtype {
        VersionType::Stable => (99, 0),
        VersionType::Rc(n) => (50, n),
        VersionType::Beta(n) => (25, n),
        VersionType::Dev(n) => (10, n),
    };

    (major as i64) * 1_000_000_000_000
        + (minor as i64) * 1_000_000_000
        + (patch as i64) * 1_000_000
        + (type_val as i64) * 10_000
        + (num as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use proptest::prelude::*;

    #[test]
    fn test_stable_version_ordering() {
        // dev < beta < rc < stable
        let dev = stable_version(1, 0, 0, VersionType::Dev(1));
        let beta = stable_version(1, 0, 0, VersionType::Beta(1));
        let rc = stable_version(1, 0, 0, VersionType::Rc(1));
        let stable = stable_version(1, 0, 0, VersionType::Stable);

        assert!(dev < beta);
        assert!(beta < rc);
        assert!(rc < stable);
    }

    #[test]
    fn test_stable_version_patch_ordering() {
        let v1_0_0 = stable_version(1, 0, 0, VersionType::Stable);
        let v1_0_1 = stable_version(1, 0, 1, VersionType::Stable);
        let v1_1_0 = stable_version(1, 1, 0, VersionType::Stable);
        let v2_0_0 = stable_version(2, 0, 0, VersionType::Stable);

        assert!(v1_0_0 < v1_0_1);
        assert!(v1_0_1 < v1_1_0);
        assert!(v1_1_0 < v2_0_0);
    }

    #[test]
    fn test_stable_version_rc_before_stable() {
        // 1.22.0-rc1 < 1.22.0-rc2 < 1.22.0
        let rc1 = stable_version(1, 22, 0, VersionType::Rc(1));
        let rc2 = stable_version(1, 22, 0, VersionType::Rc(2));
        let stable = stable_version(1, 22, 0, VersionType::Stable);

        assert!(rc1 < rc2);
        assert!(rc2 < stable);
    }

    #[test]
    fn test_stable_version_no_collisions() {
        let mut keys = HashSet::new();

        // Test a range of realistic versions
        for major in 0..3 {
            for minor in 0..30 {
                for patch in 0..20 {
                    let key = stable_version(major, minor, patch, VersionType::Stable);
                    assert!(keys.insert(key), "collision at {major}.{minor}.{patch}");
                }
                // Also test pre-release versions
                for n in 1..10 {
                    let rc = stable_version(major, minor, 0, VersionType::Rc(n));
                    assert!(keys.insert(rc), "collision at {major}.{minor}.0-rc{n}");

                    let beta = stable_version(major, minor, 0, VersionType::Beta(n));
                    assert!(keys.insert(beta), "collision at {major}.{minor}.0-beta{n}");

                    let dev = stable_version(major, minor, 0, VersionType::Dev(n));
                    assert!(keys.insert(dev), "collision at {major}.{minor}.0-dev{n}");
                }
            }
        }
    }

    #[test]
    fn test_stable_version_expected_values() {
        // Verify the documented examples
        assert_eq!(
            stable_version(1, 21, 3, VersionType::Stable),
            1_021_003_990_000
        );
        assert_eq!(
            stable_version(1, 22, 0, VersionType::Beta(1)),
            1_022_000_250_001
        );
        assert_eq!(
            stable_version(1, 22, 0, VersionType::Rc(2)),
            1_022_000_500_002
        );
        assert_eq!(
            stable_version(0, 13, 0, VersionType::Dev(1234)),
            13_000_101_234
        );
    }

    fn version_type_strategy() -> impl Strategy<Value = VersionType> {
        prop_oneof![
            Just(VersionType::Stable),
            (1..100u64).prop_map(VersionType::Rc),
            (1..100u64).prop_map(VersionType::Beta),
            (1..9999u64).prop_map(VersionType::Dev),
        ]
    }

    fn to_semver(major: u64, minor: u64, patch: u64, vtype: VersionType) -> semver::Version {
        let pre = match vtype {
            VersionType::Stable => semver::Prerelease::EMPTY,
            VersionType::Rc(n) => semver::Prerelease::new(&format!("rc.{n}")).unwrap(),
            VersionType::Beta(n) => semver::Prerelease::new(&format!("beta.{n}")).unwrap(),
            VersionType::Dev(n) => semver::Prerelease::new(&format!("dev.{n}")).unwrap(),
        };
        semver::Version {
            major,
            minor,
            patch,
            pre,
            build: semver::BuildMetadata::EMPTY,
        }
    }

    proptest! {
        #[test]
        fn fuzz_stable_version_matches_semver(
            major1 in 0..100u64,
            minor1 in 0..1000u64,
            patch1 in 0..1000u64,
            vtype1 in version_type_strategy(),
            major2 in 0..100u64,
            minor2 in 0..1000u64,
            patch2 in 0..1000u64,
            vtype2 in version_type_strategy(),
        ) {
            let key1 = stable_version(major1, minor1, patch1, vtype1);
            let key2 = stable_version(major2, minor2, patch2, vtype2);

            let sem1 = to_semver(major1, minor1, patch1, vtype1);
            let sem2 = to_semver(major2, minor2, patch2, vtype2);

            let key_cmp = key1.cmp(&key2);
            let sem_cmp = sem1.cmp(&sem2);

            prop_assert_eq!(key_cmp, sem_cmp,
                "Mismatch: {} vs {}: stable_version gives {:?}, semver gives {:?}",
                sem1, sem2, key_cmp, sem_cmp);
        }
    }
}
