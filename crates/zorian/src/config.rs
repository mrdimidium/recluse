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
use tracing::info;

use base::serde::{deserialize_duration, deserialize_listener_addr};
use repos::{GoConfig, ZigConfig};

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
    #[serde(deserialize_with = "deserialize_duration")]
    pub shutdown_timeout: Duration,

    /// Request timeout - maximum time to process a request (protects against Slowloris)
    #[serde(deserialize_with = "deserialize_duration")]
    pub request_timeout: Duration,

    /// Maximum request body size
    pub max_body_size: ByteSize,

    /// Maximum number of concurrent requests across all clients
    pub max_concurrent_requests: usize,

    /// Rate limit: requests per second per client IP
    #[serde(deserialize_with = "deserialize_duration")]
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
    #[serde(deserialize_with = "deserialize_duration")]
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
pub struct ConfigService {
    appname: String,
    dirname: PathBuf,
    listen: Vec<ListenerConfig>,
    server: ServerConfig,
    telemetry: TelemetryConfig,
    backends: BackendsConfig,
}

impl ConfigService {
    pub fn load(config_path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let config = match config_path {
            Some(path) => {
                info!("use config file from {}", path.to_str().unwrap());

                let content = fs::read_to_string(path)?;
                let config: Self = toml::from_str(&content)?;
                config
            }
            None => {
                info!("configuration file path not provided");
                Self {
                    appname: "zorian".to_string(),
                    dirname: PathBuf::from("./.zorian-state"),
                    server: ServerConfig::default(),
                    listen: vec![ListenerConfig::default()],
                    telemetry: TelemetryConfig::default(),
                    backends: BackendsConfig::default(),
                }
            }
        };

        config.validate()
    }

    fn validate(self) -> Result<Self, ConfigError> {
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

        Ok(self)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn base_config(dir: PathBuf) -> ConfigService {
        ConfigService {
            appname: "valid-name".to_string(),
            dirname: dir,
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

    fn write_temp_file(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "x").unwrap();
        path
    }

    mod defaults_tests {
        use super::*;

        #[test]
        fn test_for_test_config() {
            let dir = TempDir::new().unwrap();
            let cfg = ConfigService::for_test(dir.path().to_path_buf());
            assert_eq!(cfg.appname(), "test");
            assert_eq!(cfg.dirname(), dir.path());
        }

        #[test]
        fn test_listener_default() {
            let cfg = ListenerConfig::default();
            assert!(cfg.hostnames.contains(&"localhost".to_string()));
        }
    }

    mod load_tests {
        use super::*;

        #[test]
        fn test_load_none_defaults() {
            let temp = TempDir::new().unwrap();
            let state = temp.path().join(".zorian-state");
            std::fs::create_dir_all(&state).unwrap();
            let cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(temp.path()).unwrap();

            let cfg = ConfigService::load(None).unwrap();
            assert_eq!(cfg.appname(), "zorian");

            std::env::set_current_dir(cwd).unwrap();
        }

        #[test]
        fn test_load_some_and_parse_error() {
            let dir = TempDir::new().unwrap();
            let good = dir.path().join("good.toml");
            let bad = dir.path().join("bad.toml");
            let missing = dir.path().join("missing.toml");
            let state = dir.path().join("state");
            std::fs::create_dir_all(&state).unwrap();

            std::fs::write(
                &good,
                format!(
                    "appname = \"ok\"\n\
                     dirname = \"{}\"\n\
                     listen = []\n\
                     [server]\n\
                     shutdown_timeout = 1\n\
                     request_timeout = 1\n\
                     max_body_size = \"1 MB\"\n\
                     max_concurrent_requests = 1\n\
                     rate_limit_period = 1\n\
                     rate_limit_burst_size = 1\n\
                     [telemetry]\n\
                     [telemetry.stdout]\n\
                     enabled = true\n\
                     log_level = \"info\"\n\
                     log_format = \"pretty\"\n\
                     [backends]\n\
                     [backends.go]\n\
                     [backends.zig]\n",
                    state.display()
                ),
            )
            .unwrap();

            std::fs::write(&bad, "not = [valid").unwrap();

            let cfg = ConfigService::load(Some(good)).unwrap();
            assert_eq!(cfg.appname(), "ok");

            let err = ConfigService::load(Some(missing)).unwrap_err();
            assert!(err.to_string().contains("failed to read"));

            let err = ConfigService::load(Some(bad)).unwrap_err();
            assert!(err.to_string().contains("failed to parse"));
        }
    }

    mod validation_tests {
        use super::*;

        #[test]
        fn test_validate_invalid_appname() {
            let dir = TempDir::new().unwrap();
            let mut cfg = base_config(dir.path().to_path_buf());
            cfg.appname = "bad name!".to_string();
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("appname 'bad name!'"));
        }

        #[test]
        fn test_validate_dir_not_found() {
            let temp = TempDir::new().unwrap();
            let missing = temp.path().join("missing-dir");
            let cfg = base_config(missing);
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("does not exist"));
        }

        #[test]
        fn test_validate_not_a_directory() {
            let dir = TempDir::new().unwrap();
            let file_path = dir.path().join("file");
            std::fs::write(&file_path, "x").unwrap();
            let cfg = base_config(file_path);
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("is not a directory"));
        }

        #[cfg(unix)]
        #[test]
        fn test_validate_metadata_io_error() {
            use std::os::unix::fs::PermissionsExt;

            let dir = TempDir::new().unwrap();
            let child = dir.path().join("child");
            std::fs::create_dir_all(&child).unwrap();

            let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
            perms.set_mode(0o000);
            std::fs::set_permissions(dir.path(), perms).unwrap();

            let cfg = base_config(child.clone());
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("failed to read"));

            let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(dir.path(), perms).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn test_validate_not_writable() {
            use std::os::unix::fs::PermissionsExt;

            let dir = TempDir::new().unwrap();
            let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
            perms.set_mode(0o400);
            std::fs::set_permissions(dir.path(), perms).unwrap();

            let cfg = base_config(dir.path().to_path_buf());
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("not writable"));
        }

        #[test]
        fn test_validate_tls_key_missing() {
            let dir = TempDir::new().unwrap();
            let mut cfg = base_config(dir.path().to_path_buf());
            cfg.listen = vec![ListenerConfig {
                addr: "127.0.0.1:0".parse().unwrap(),
                hostnames: Vec::new(),
                tls_crt: Some(PathBuf::from("/tmp/does-not-matter.crt")),
                tls_key: None,
            }];
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("tls_key is missing"));
        }

        #[test]
        fn test_validate_tls_crt_missing() {
            let dir = TempDir::new().unwrap();
            let mut cfg = base_config(dir.path().to_path_buf());
            cfg.listen = vec![ListenerConfig {
                addr: "127.0.0.1:0".parse().unwrap(),
                hostnames: Vec::new(),
                tls_crt: None,
                tls_key: Some(PathBuf::from("/tmp/does-not-matter.key")),
            }];
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("tls_crt is missing"));
        }

        #[test]
        fn test_validate_tls_files_not_found() {
            let dir = TempDir::new().unwrap();
            let mut cfg = base_config(dir.path().to_path_buf());
            cfg.listen = vec![ListenerConfig {
                addr: "127.0.0.1:0".parse().unwrap(),
                hostnames: Vec::new(),
                tls_crt: Some(PathBuf::from("/tmp/missing.crt")),
                tls_key: Some(PathBuf::from("/tmp/missing.key")),
            }];
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("TLS crtificate file not found"));
        }

        #[test]
        fn test_validate_tls_key_not_found() {
            let dir = TempDir::new().unwrap();
            let mut cfg = base_config(dir.path().to_path_buf());
            let crt = write_temp_file(dir.path(), "cert.pem");
            cfg.listen = vec![ListenerConfig {
                addr: "127.0.0.1:0".parse().unwrap(),
                hostnames: Vec::new(),
                tls_crt: Some(crt),
                tls_key: Some(dir.path().join("missing.key")),
            }];
            let err = cfg.validate().unwrap_err();
            assert!(err.to_string().contains("TLS key file not found"));
        }

        #[test]
        fn test_validate_tls_files_exist() {
            let dir = TempDir::new().unwrap();
            let mut cfg = base_config(dir.path().to_path_buf());
            let crt = write_temp_file(dir.path(), "cert.pem");
            let key = write_temp_file(dir.path(), "key.pem");
            cfg.listen = vec![ListenerConfig {
                addr: "127.0.0.1:0".parse().unwrap(),
                hostnames: Vec::new(),
                tls_crt: Some(crt),
                tls_key: Some(key),
            }];
            cfg.validate().unwrap();
        }

        #[test]
        fn test_validate_ok_and_getters() {
            let dir = TempDir::new().unwrap();
            let mut cfg = base_config(dir.path().to_path_buf());
            cfg.listen = vec![ListenerConfig::default()];

            let cfg = cfg.validate().unwrap();
            assert_eq!(cfg.appname(), "valid-name");
            assert_eq!(cfg.dirname(), dir.path());
            let _ = cfg.server();
            let _ = cfg.listeners();
            let _ = cfg.telemetry();
            let _ = cfg.backends();
        }
    }
}
