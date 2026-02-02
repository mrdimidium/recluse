// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod go;
pub mod zig;

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use thiserror::Error;
use url::Url;

pub use go::{GoBackend, GoConfig};
pub use zig::{ZigBackend, ZigConfig};

/// Error during index operations.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error("fetch error: {0}")]
    Fetch(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("parse error: {0}")]
    Parse(String),
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

/// Error during file resolution.
#[derive(Debug, Clone, Copy)]
pub enum ResolveError {
    /// File not found (404)
    NotFound,
    /// Internal error (500)
    Internal,
}

/// Result of resolving a file request.
pub enum ResolvedFile {
    /// Proxy request to upstream URL
    Upstream(Url),
    /// Return content directly
    Content {
        data: bytes::Bytes,
        mime: &'static str,
    },
}

/// Delegate provides I/O primitives to backends.
#[async_trait]
pub trait BackendDelegate: Send + Sync {
    /// Get SQLite pool
    fn db(&self) -> &Pool<Sqlite>;

    /// HTTP GET request
    async fn http_get(&self, url: &Url) -> Result<Bytes, IndexError>;
}

/// Trait for backend-specific logic (parsing, URL building, version indexing).
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Fixed unique identifier for storage
    const ID: &'static str;

    /// Backend-specific release representation
    type Release;

    /// Whether the backend is enabled
    fn enabled(&self) -> bool;

    /// Index refresh interval
    fn refresh_interval(&self) -> Duration;

    /// Create tables for this backend (called at startup)
    async fn migrate(&self) -> Result<(), IndexError>;

    /// Fetch index from upstream and store in DB
    async fn fetch_index(&self) -> Result<(), IndexError>;

    /// Resolves filename to upstream URL or direct content.
    async fn resolve_file(&self, filename: &str) -> Result<ResolvedFile, ResolveError>;

    /// Load versions from DB
    async fn get_versions(&self) -> Result<Vec<Self::Release>, IndexError>;
}

/// Version type for ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionType {
    Stable,
    Rc(u64),
    Beta(u64),
    Dev(u64),
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
