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
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ConfigService {
    listen: String,
    appname: String,
    dirname: PathBuf,
}

impl Default for ConfigService {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:3000".to_string(),
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

        Ok(())
    }

    pub fn appname(&self) -> &str {
        &self.appname
    }

    pub fn listen(&self) -> &str {
        &self.listen
    }

    pub fn dirname(&self) -> &Path {
        &self.dirname
    }
}
