// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("appname '{0}' contains invalid characters (only a-z, A-Z, 0-9, -, _ allowed)")]
    InvalidAppname(String),

    #[error("dirname '{0}' does not exist")]
    DirNotFound(PathBuf),

    #[error("dirname '{0}' is not a directory")]
    NotADirectory(PathBuf),

    #[error("dirname '{0}' is not writable: {1}")]
    NotWritable(PathBuf, std::io::Error),

    #[error("listener '{0}': tls_crt is set but tls_key is missing")]
    TlsKeyMissing(String),

    #[error("listener '{0}': tls_key is set but tls_crt is missing")]
    TlsCrtMissing(String),

    #[error("listener '{0}': TLS crtificate file not found: {1}")]
    TlsCrtNotFound(String, PathBuf),

    #[error("listener '{0}': TLS key file not found: {1}")]
    TlsKeyNotFound(String, PathBuf),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenerConfig {
    pub addr: String,

    /// Hostnames to accept for this listener. Empty means accept all.
    #[serde(default)]
    pub hostnames: Vec<String>,

    /// Path to TLS certificate file (PEM format). If set, tls_key must also be set.
    pub tls_crt: Option<PathBuf>,

    /// Path to TLS private key file (PEM format). If set, tls_crt must also be set.
    pub tls_key: Option<PathBuf>,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:3000".to_string(),
            hostnames: Vec::new(),
            tls_crt: None,
            tls_key: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ConfigService {
    listen: Vec<ListenerConfig>,
    appname: String,
    dirname: PathBuf,
}

impl Default for ConfigService {
    fn default() -> Self {
        Self {
            listen: vec![ListenerConfig::default()],
            appname: "zorian".to_string(),
            dirname: PathBuf::from("./.zorian-state"),
        }
    }
}

impl ConfigService {
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut chars = self.appname.chars();
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(ConfigError::InvalidAppname(self.appname.clone()));
        }

        let metadata = fs::metadata(&self.dirname).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ConfigError::DirNotFound(self.dirname.clone())
            } else {
                ConfigError::Io(e)
            }
        })?;

        if !metadata.is_dir() {
            return Err(ConfigError::NotADirectory(self.dirname.clone()));
        }

        let testfile = self.dirname.join(".health");
        fs::write(&testfile, std::process::id().to_string())
            .map_err(|e| ConfigError::NotWritable(self.dirname.clone(), e))?;
        fs::remove_file(&testfile)?;

        // Validate TLS configuration for each listener
        for listener in &self.listen {
            match (&listener.tls_crt, &listener.tls_key) {
                (Some(_crt), None) => {
                    return Err(ConfigError::TlsKeyMissing(listener.addr.clone()));
                }
                (None, Some(_)) => {
                    return Err(ConfigError::TlsCrtMissing(listener.addr.clone()));
                }
                (Some(crt), Some(key)) => {
                    if !crt.exists() {
                        return Err(ConfigError::TlsCrtNotFound(
                            listener.addr.clone(),
                            crt.clone(),
                        ));
                    }
                    if !key.exists() {
                        return Err(ConfigError::TlsKeyNotFound(
                            listener.addr.clone(),
                            key.clone(),
                        ));
                    }
                }
                (None, None) => {}
            }
        }

        Ok(())
    }

    pub fn appname(&self) -> &str {
        &self.appname
    }

    pub fn listeners(&self) -> &[ListenerConfig] {
        &self.listen
    }

    pub fn dirname(&self) -> &Path {
        &self.dirname
    }
}

#[cfg(test)]
impl ConfigService {
    pub fn for_test(dirname: PathBuf) -> Self {
        Self {
            listen: vec![ListenerConfig {
                addr: "127.0.0.1:0".to_string(),
                hostnames: Vec::new(),
                tls_crt: None,
                tls_key: None,
            }],
            appname: "test".to_string(),
            dirname,
        }
    }
}
