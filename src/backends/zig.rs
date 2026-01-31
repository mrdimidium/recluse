// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use semver::Version;
use serde::Deserialize;
use thiserror::Error;

use super::Backend;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ZigConfig {
    pub enabled: bool,
    pub upstream: String,
}

impl Default for ZigConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            upstream: String::from("https://ziglang.org"),
        }
    }
}

pub struct ZigBackend {
    config: ZigConfig,
    source: String,
}

impl ZigBackend {
    pub fn new(config: ZigConfig, source: String) -> Self {
        Self { config, source }
    }
}

impl Backend for ZigBackend {
    const ID: &'static str = "zig";

    fn upstream_url(&self, filename: &str) -> Result<String, ()> {
        let tarball = Tarball::parse(filename).map_err(|_| ())?;
        Ok(tarball.upstream_url(&self.config.upstream, &self.source))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Archive {
    Zip,
    TarXz,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TarballType<'a> {
    Source,
    Bootstrap,
    Binary { os: &'a str, arch: &'a str },
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
struct Tarball<'a> {
    filename: &'a str,
    tarball_type: TarballType<'a>,
    minisig: bool,
    archive: Archive,
    version: Version,
    development: bool,
}

impl<'a> Tarball<'a> {
    pub fn parse(filename: &'a str) -> Result<Self, ParseError> {
        let mut buffer = filename;
        let mut minisig = false;
        let archive;
        let tarball_type;

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
            Version::parse(last).map_err(|_| ParseError)?
        } else {
            let semver = it.next().ok_or(ParseError)?;
            let devver = last;
            let version_str = format!("{}-{}", semver, devver);
            Version::parse(&version_str).map_err(|_| ParseError)?
        };

        if let Some(payload) = it.next() {
            if payload == "bootstrap" {
                tarball_type = TarballType::Bootstrap;
            } else {
                // Version 0.14.0 is the last one to use the OS-ARCH format in names; newer versions use ARCH-OS.
                let min_version = Version::new(0, 14, 0);
                if version > min_version {
                    tarball_type = TarballType::Binary {
                        os: payload,
                        arch: it.next().ok_or(ParseError)?,
                    };
                } else {
                    tarball_type = TarballType::Binary {
                        arch: payload,
                        os: it.next().ok_or(ParseError)?,
                    };
                }
            }
        } else {
            tarball_type = TarballType::Source;
        }

        if it.next().is_some() {
            return Err(ParseError);
        }

        Ok(Tarball {
            filename,
            tarball_type,
            minisig,
            archive,
            version,
            development,
        })
    }

    /// Builds the upstream URL for this tarball.
    pub fn upstream_url(&self, upstream: &str, source: &str) -> String {
        if self.development {
            format!("{}/builds/{}?source={}", upstream, self.filename, source)
        } else {
            format!(
                "{}/download/{}/{}?source={}",
                upstream, self.version, self.filename, source,
            )
        }
    }
}
