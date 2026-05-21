use serde::{Deserialize, Serialize};
use std::fmt;

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub log: LogConfig,
}

impl AppConfig {
    /// Load config from YAML file, falling back to defaults for missing fields.
    pub fn from_yaml_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        let cfg: AppConfig = yaml_serde::from_str(&content).map_err(ConfigError::Yaml)?;
        Ok(cfg)
    }

    /// Apply an environment variable override.
    /// Key format: `SERVER_LISTEN` (uppercase, underscores for dots).
    pub fn apply_env(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let dotted = key.to_lowercase().replace('_', ".");
        self.apply_cli(&dotted, value)
    }

    /// Apply a CLI override (`-C key=value`).
    pub fn apply_cli(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        match key {
            "server.listen" => self.server.listen = value.to_string(),
            "server.tls.cert" => self.server.tls.cert = value.to_string(),
            "server.tls.key" => self.server.tls.key = value.to_string(),
            "server.shutdown_timeout" => {
                self.server.shutdown_timeout = value.parse().map_err(|_| ConfigError::Parse {
                    key: key.to_string(),
                    value: value.to_string(),
                    expected: "u64",
                })?;
            }
            "upstream.tls.extra_ca_certs" => {
                self.upstream.tls.extra_ca_certs =
                    value.split(',').map(|s| s.trim().to_string()).collect();
            }
            "upstream.tls.use_system_roots" => {
                self.upstream.tls.use_system_roots =
                    parse_bool_option(value).ok_or_else(|| ConfigError::Parse {
                        key: key.to_string(),
                        value: value.to_string(),
                        expected: "bool",
                    })?;
            }
            "upstream.proxy" => self.upstream.proxy = value.to_string(),
            "upstream.anthropic_version" => self.upstream.anthropic_version = value.to_string(),
            "log.console" => {
                self.log.console = parse_option_shell(value).map(|_| ConsoleLog::default());
            }
            "log.console.level" => {
                let level = value.to_string();
                if self.log.console.is_none() {
                    self.log.console = Some(ConsoleLog::default());
                }
                if let Some(ref mut c) = self.log.console {
                    c.level = level;
                }
            }
            "log.file" => {
                self.log.file = parse_option_shell(value).map(|_| FileLog::default());
            }
            "log.file.level" => {
                let level = value.to_string();
                if self.log.file.is_none() {
                    self.log.file = Some(FileLog::default());
                }
                if let Some(ref mut f) = self.log.file {
                    f.level = level;
                }
            }
            "log.file.dir" => {
                if self.log.file.is_none() {
                    self.log.file = Some(FileLog::default());
                }
                if let Some(ref mut f) = self.log.file {
                    f.dir = value.to_string();
                }
            }
            "log.file.rotation" => {
                if self.log.file.is_none() {
                    self.log.file = Some(FileLog::default());
                }
                if let Some(ref mut f) = self.log.file {
                    f.rotation = value.to_string();
                }
            }
            _ => {
                return Err(ConfigError::UnknownKey {
                    key: key.to_string(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub tls: TlsServerConfig,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
            tls: TlsServerConfig::default(),
            shutdown_timeout: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TlsServerConfig {
    pub cert: String,
    pub key: String,
}

impl TlsServerConfig {
    pub fn is_enabled(&self) -> bool {
        !self.cert.is_empty() && !self.key.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub tls: UpstreamTlsConfig,
    #[serde(default)]
    pub proxy: String,
    #[serde(default = "default_anthropic_version")]
    pub anthropic_version: String,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            tls: UpstreamTlsConfig::default(),
            proxy: String::new(),
            anthropic_version: "2023-06-01".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamTlsConfig {
    pub extra_ca_certs: Vec<String>,
    pub use_system_roots: bool,
}

impl Default for UpstreamTlsConfig {
    fn default() -> Self {
        Self {
            extra_ca_certs: Vec::new(),
            use_system_roots: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    pub console: Option<ConsoleLog>,
    pub file: Option<FileLog>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            console: Some(ConsoleLog::default()),
            file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleLog {
    pub level: String,
}

impl Default for ConsoleLog {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLog {
    pub level: String,
    pub dir: String,
    pub rotation: String,
}

impl Default for FileLog {
    fn default() -> Self {
        Self {
            level: "debug".to_string(),
            dir: "./logs".to_string(),
            rotation: "daily".to_string(),
        }
    }
}

/// Serde default helper functions.
fn default_listen() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_shutdown_timeout() -> u64 {
    30
}

fn default_anthropic_version() -> String {
    "2023-06-01".to_string()
}

/// Parse option semantics: "false"/"0"/"null" -> None, "true"/"1" -> Some(()).
fn parse_option_shell(value: &str) -> Option<()> {
    match value {
        "false" | "0" | "null" => None,
        "true" | "1" => Some(()),
        _ => Some(()),
    }
}

/// Parse a bool from string, returning Option for option semantics.
fn parse_bool_option(value: &str) -> Option<bool> {
    match value {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::path::PathBuf, std::io::Error),
    Yaml(yaml_serde::Error),
    UnknownKey {
        key: String,
    },
    Parse {
        key: String,
        value: String,
        expected: &'static str,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(path, e) => {
                write!(f, "failed to read config file {}: {}", path.display(), e)
            }
            ConfigError::Yaml(e) => write!(f, "YAML parse error: {}", e),
            ConfigError::UnknownKey { key } => write!(f, "unknown config key: {}", key),
            ConfigError::Parse {
                key,
                value,
                expected,
            } => {
                write!(
                    f,
                    "invalid value for {}: expected {}, got '{}'",
                    key, expected, value
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.server.listen, "0.0.0.0:8080");
        assert_eq!(cfg.server.shutdown_timeout, 30);
        assert!(cfg.server.tls.cert.is_empty());
        assert!(cfg.server.tls.key.is_empty());
        assert!(cfg.upstream.tls.use_system_roots);
        assert!(cfg.upstream.tls.extra_ca_certs.is_empty());
        assert!(cfg.upstream.proxy.is_empty());
        assert_eq!(cfg.upstream.anthropic_version, "2023-06-01");
        assert!(cfg.log.console.is_some());
        assert!(cfg.log.file.is_none());
    }

    #[test]
    fn env_override_beats_default() {
        let mut cfg = AppConfig::default();
        cfg.apply_env("SERVER_LISTEN", "0.0.0.0:9999").unwrap();
        assert_eq!(cfg.server.listen, "0.0.0.0:9999");
    }

    #[test]
    fn cli_override_beats_env() {
        let mut cfg = AppConfig::default();
        cfg.apply_env("SERVER_LISTEN", "0.0.0.0:9999").unwrap();
        cfg.apply_cli("server.listen", "0.0.0.0:7777").unwrap();
        assert_eq!(cfg.server.listen, "0.0.0.0:7777");
    }

    #[test]
    fn option_semantics_false_disables_console() {
        let mut cfg = AppConfig::default();
        assert!(cfg.log.console.is_some());
        cfg.apply_cli("log.console", "false").unwrap();
        assert!(cfg.log.console.is_none());
    }

    #[test]
    fn option_semantics_true_enables_file() {
        let mut cfg = AppConfig::default();
        assert!(cfg.log.file.is_none());
        cfg.apply_cli("log.file", "true").unwrap();
        assert!(cfg.log.file.is_some());
        let file_log = cfg.log.file.as_ref().unwrap();
        assert_eq!(file_log.level, "debug");
        assert_eq!(file_log.dir, "./logs");
        assert_eq!(file_log.rotation, "daily");
    }

    #[test]
    fn yaml_config_loads() {
        let yaml = r#"
server:
  listen: "0.0.0.0:1234"
upstream:
  proxy: "http://proxy:8080"
"#;
        let cfg: AppConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(cfg.server.listen, "0.0.0.0:1234");
        assert_eq!(cfg.upstream.proxy, "http://proxy:8080");
    }

    #[test]
    fn invalid_cli_key_returns_error() {
        let mut cfg = AppConfig::default();
        let result = cfg.apply_cli("nonexistent.field", "value");
        assert!(result.is_err());
    }
}
