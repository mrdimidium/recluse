// SPDX-FileCopyrightText: 2026 Nikolay Govorov <me@govorov.online>
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytesize::ByteSize;
use serde::Deserialize;
use thiserror::Error;

use crate::backends::{GoConfig, ZigConfig};

fn deserialize_duration_secs<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let secs = u64::deserialize(deserializer)?;
    Ok(Duration::from_secs(secs))
}

fn deserialize_listener_addr<'de, D>(deserializer: D) -> Result<SocketAddr, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    let raw = raw.trim();

    // Hostnames are not supported, but localhost shorthands are useful
    if let Some(port) = raw.strip_prefix("localhost:") {
        return port
            .parse::<u16>()
            .map(|port| SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port))
            .map_err(|err| serde::de::Error::custom(format!("invalid port in '{raw}': {err}")));
    }

    raw.parse::<SocketAddr>()
        .map_err(|err| serde::de::Error::custom(format!("invalid address '{raw}': {err}",)))
}

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
    TlsKeyMissing(SocketAddr),

    #[error("listener '{0}': tls_key is set but tls_crt is missing")]
    TlsCrtMissing(SocketAddr),

    #[error("listener '{0}': TLS crtificate file not found: {1}")]
    TlsCrtNotFound(SocketAddr, PathBuf),

    #[error("listener '{0}': TLS key file not found: {1}")]
    TlsKeyNotFound(SocketAddr, PathBuf),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// When receiving a SIGINT/SIGTERM signal, we will wait for the proposed timeout before terminating workers
    #[serde(deserialize_with = "deserialize_duration_secs")]
    pub shutdown_timeout: Duration,

    /// Request timeout - maximum time to process a request (protects against Slowloris)
    #[serde(deserialize_with = "deserialize_duration_secs")]
    pub request_timeout: Duration,

    /// Maximum request body size
    pub max_body_size: ByteSize,

    /// Maximum number of concurrent requests across all clients
    pub max_concurrent_requests: usize,

    /// Rate limit: requests per second per client IP
    #[serde(deserialize_with = "deserialize_duration_secs")]
    pub rate_limit_period: Duration,

    /// Rate limit: burst size (max requests allowed in a burst) per client IP
    pub rate_limit_burst_size: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            shutdown_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
            max_body_size: ByteSize::mb(64),
            max_concurrent_requests: 512,
            rate_limit_period: Duration::from_secs(10),
            rate_limit_burst_size: 50,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenerConfig {
    #[serde(deserialize_with = "deserialize_listener_addr")]
    pub addr: SocketAddr,

    /// Hostnames to accept for this listener. Empty means accept all.
    pub hostnames: Vec<String>,

    /// Path to TLS certificate file (PEM format). If set, tls_key must also be set.
    pub tls_crt: Option<PathBuf>,

    /// Path to TLS private key file (PEM format). If set, tls_crt must also be set.
    pub tls_key: Option<PathBuf>,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:2025".parse().unwrap(),
            hostnames: vec![
                String::from("[::1]"),
                String::from("127.0.0.1"),
                String::from("localhost"),
            ],
            tls_crt: None,
            tls_key: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StdoutFormat {
    #[default]
    Pretty,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StdoutConfig {
    /// Enables sending logs to the stdout
    pub enabled: bool,

    /// Controls which logs will be sent to stdout
    pub log_level: LogLevel,

    /// Controls the format of logs in stdout
    pub log_format: StdoutFormat,
}
impl Default for StdoutConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_level: LogLevel::Info,
            log_format: StdoutFormat::Pretty,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OtelcolConfig {
    /// Enables sending telemetry to the otlp collector
    pub enabled: bool,

    /// Send logs to OTLP at this level (None = disabled)
    pub logs: bool,

    /// Send traces to OTLP
    pub traces: bool,

    /// Send traces to OTLP
    pub metrics: bool,

    /// OTLP endpoint (grpc:// or http://)
    pub endpoint: String,

    /// Export timeout in seconds
    #[serde(deserialize_with = "deserialize_duration_secs")]
    pub timeout: Duration,

    /// Controls which logs will be sent to otlp
    pub log_level: LogLevel,

    /// Path to CA certificate for TLS (required for grpcs://)
    pub tls_ca: Option<PathBuf>,

    /// Path to client certificate for mTLS
    pub tls_crt: Option<PathBuf>,

    /// Path to client key for mTLS
    pub tls_key: Option<PathBuf>,

    /// HTTP headers for authentication
    pub headers: HashMap<String, String>,
}

impl Default for OtelcolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            logs: true,
            traces: true,
            metrics: true,
            timeout: Duration::from_secs(10),
            endpoint: "http://localhost:4317".into(),
            log_level: LogLevel::Info,
            tls_ca: None,
            tls_crt: None,
            tls_key: None,
            headers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct TelemetryConfig {
    pub stdout: StdoutConfig,
    pub otelcol: Option<OtelcolConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct BackendsConfig {
    pub go: GoConfig,
    pub zig: ZigConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ConfigService {
    appname: String,
    dirname: PathBuf,
    listen: Vec<ListenerConfig>,
    server: ServerConfig,
    telemetry: TelemetryConfig,
    backends: BackendsConfig,
}
impl Default for ConfigService {
    fn default() -> Self {
        Self {
            appname: "zorian".to_string(),
            dirname: PathBuf::from("./.zorian-state"),
            server: ServerConfig::default(),
            listen: vec![ListenerConfig::default()],
            telemetry: TelemetryConfig::default(),
            backends: BackendsConfig::default(),
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

        // Validate listener configuration
        for listener in &self.listen {
            match (&listener.tls_crt, &listener.tls_key) {
                (Some(_crt), None) => {
                    return Err(ConfigError::TlsKeyMissing(listener.addr));
                }
                (None, Some(_)) => {
                    return Err(ConfigError::TlsCrtMissing(listener.addr));
                }
                (Some(crt), Some(key)) => {
                    if !crt.exists() {
                        return Err(ConfigError::TlsCrtNotFound(listener.addr, crt.clone()));
                    }
                    if !key.exists() {
                        return Err(ConfigError::TlsKeyNotFound(listener.addr, key.clone()));
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

    pub fn dirname(&self) -> &Path {
        &self.dirname
    }

    pub fn server(&self) -> &ServerConfig {
        &self.server
    }

    pub fn listeners(&self) -> &[ListenerConfig] {
        &self.listen
    }

    pub fn telemetry(&self) -> &TelemetryConfig {
        &self.telemetry
    }

    pub fn backends(&self) -> &BackendsConfig {
        &self.backends
    }
}

#[cfg(test)]
impl ConfigService {
    pub fn for_test(dirname: PathBuf) -> Self {
        Self {
            appname: "test".to_string(),
            dirname,
            server: ServerConfig::default(),
            listen: vec![ListenerConfig {
                addr: "127.0.0.1:0".parse().unwrap(),
                hostnames: Vec::new(),
                tls_crt: None,
                tls_key: None,
            }],
            telemetry: TelemetryConfig::default(),
            backends: BackendsConfig::default(),
        }
    }
}
