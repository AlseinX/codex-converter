# codex-conv Implementation Plan (Part 1: Tasks 1-9)

**Date:** 2026-05-19
**Scope:** Foundation + Request Conversion (Tasks 1-9)
**Spec:** `docs/superpowers/specs/2026-05-13-codex-conv-design.md`
**TDD cycle:** failing test -> verify fail -> implement -> verify pass -> commit

---

## Task 1: Cargo Project + Dependencies

**Files:** create `Cargo.toml`, `src/lib.rs`

### Step 1.1 — Create `Cargo.toml`

- [ ] Create `Cargo.toml` with all dependencies listed in the spec.

```toml
[package]
name = "codex-conv"
version = "0.1.0"
edition = "2021"
description = "Reverse proxy converting OpenAI Responses API (Codex CLI) to Anthropic Messages API"

[[bin]]
name = "codex-conv"
path = "src/main.rs"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", default-features = false, features = ["stream", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = "0.3"
tracing-appender = "0.2"
rustls = "0.23"
tokio-rustls = "0.26"
reqwest-eventsource = "0.6"
sha2 = "0.10"
hex = "0.4"
uuid = { version = "1", features = ["v4"] }

[dev-dependencies]
wiremock = "0.6"
tempfile = "3"
```

### Step 1.2 — Create `src/lib.rs`

- [ ] Create `src/lib.rs` as an empty crate root so integration tests can import the library.

```rust
//! codex-conv: OpenAI Responses API ↔ Anthropic Messages API reverse proxy.

pub mod config;
pub mod conversion;
pub mod logging;
pub mod router;
pub mod server;
pub mod sse;
pub mod tls;
```

### Step 1.3 — Stub all module files

- [ ] Create empty stub files so `cargo check` passes.

```
src/config.rs       — `pub struct Config;`
src/server.rs       — empty (will be filled in Task 4)
src/router.rs       — empty
src/tls.rs          — empty
src/logging.rs      — empty
src/conversion/mod.rs       — empty
src/conversion/request.rs   — empty
src/conversion/response.rs  — empty
src/conversion/namespace.rs — empty
src/conversion/id_map.rs    — empty
src/conversion/content.rs   — empty
src/conversion/error.rs     — empty
src/conversion/thinking.rs  — empty
src/conversion/signature_cache.rs — empty
src/sse/mod.rs       — empty
src/sse/anthropic.rs — empty
src/sse/responses.rs — empty
```

### Step 1.4 — Verify build

- [ ] Run `cargo check`. Must succeed with zero errors.

### Step 1.5 — Commit

```
feat: scaffold Cargo project with all dependencies and module stubs
```

---

## Task 2: Config System

**Files:** create/modify `src/config.rs`, test in `src/config.rs` (inline `#[cfg(test)]`)

### Step 2.1 — Write failing tests for config

- [ ] Add test module at bottom of `src/config.rs`.

```rust
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
        let cfg: AppConfig = serde_yaml::from_str(yaml).unwrap();
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
```

### Step 2.2 — Verify tests fail

- [ ] Run `cargo test --lib config`. Confirm compilation errors (structs not defined yet).

### Step 2.3 — Implement config types and merge logic

- [ ] Implement full `src/config.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::fmt;

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    pub log: LogConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            upstream: UpstreamConfig::default(),
            log: LogConfig::default(),
        }
    }
}

impl AppConfig {
    /// Load config from YAML file, falling back to defaults for missing fields.
    pub fn from_yaml_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        let cfg: AppConfig = serde_yaml::from_str(&content)
            .map_err(ConfigError::Yaml)?;
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
                self.upstream.tls.use_system_roots = parse_bool_option(value)
                    .ok_or_else(|| ConfigError::Parse {
                        key: key.to_string(),
                        value: value.to_string(),
                        expected: "bool",
                    })?;
            }
            "upstream.proxy" => self.upstream.proxy = value.to_string(),
            "upstream.anthropic_version" => self.upstream.anthropic_version = value.to_string(),
            "log.console" => {
                self.log.console = parse_option_shell(value);
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
    pub listen: String,
    pub tls: TlsServerConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsServerConfig {
    pub cert: String,
    pub key: String,
}

impl Default for TlsServerConfig {
    fn default() -> Self {
        Self {
            cert: String::new(),
            key: String::new(),
        }
    }
}

impl TlsServerConfig {
    pub fn is_enabled(&self) -> bool {
        !self.cert.is_empty() && !self.key.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub tls: UpstreamTlsConfig,
    pub proxy: String,
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

/// Parse option semantics: "false"/"0"/"null" → None, "true"/"1" → Some(()).
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
    Yaml(serde_yaml::Error),
    UnknownKey { key: String },
    Parse { key: String, value: String, expected: &'static str },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(path, e) => write!(f, "failed to read config file {}: {}", path.display(), e),
            ConfigError::Yaml(e) => write!(f, "YAML parse error: {}", e),
            ConfigError::UnknownKey { key } => write!(f, "unknown config key: {}", key),
            ConfigError::Parse { key, value, expected } => {
                write!(f, "invalid value for {}: expected {}, got '{}'", key, expected, value)
            }
        }
    }
}

impl std::error::Error for ConfigError {}
```

### Step 2.4 — Verify tests pass

- [ ] Run `cargo test --lib config`. All tests green.

### Step 2.5 — Commit

```
feat: implement three-way config injection with Option semantics
```

---

## Task 3: CLI + Logging

**Files:** create `src/main.rs`, `src/logging.rs`

### Step 3.1 — Write `src/main.rs`

- [ ] Implement CLI with clap derive, config loading, and logging init.

```rust
use clap::Parser;
use codex_conv::config::AppConfig;
use codex_conv::logging;
use std::path::PathBuf;

/// codex-conv: OpenAI Responses API to Anthropic Messages API reverse proxy.
#[derive(Parser, Debug)]
#[command(name = "codex-conv", version, about)]
struct Cli {
    /// Config file path (optional YAML)
    #[arg(short = 'c', long = "config-file", value_name = "PATH")]
    config_file: Option<PathBuf>,

    /// Override config item (repeatable). Format: key=value
    #[arg(short = 'C', long = "config-item", value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    config_items: Vec<String>,
}

fn main() {
    let cli = Cli::parse();

    // Step 1: Load base config from file or defaults.
    let mut config = match &cli.config_file {
        Some(path) => {
            AppConfig::from_yaml_file(path).unwrap_or_else(|e| {
                eprintln!("Error loading config file {}: {}", path.display(), e);
                std::process::exit(1);
            })
        }
        None => AppConfig::default(),
    };

    // Step 2: Apply environment variable overrides.
    // Convention: CODEX_CONV_<UPPERCASE_KEY_WITH_UNDERSCORES>
    apply_env_overrides(&mut config);

    // Step 3: Apply CLI overrides (highest priority).
    for item in &cli.config_items {
        let (key, value) = item.split_once('=').unwrap_or_else(|| {
            eprintln!("Invalid config item format: '{}'. Expected key=value", item);
            std::process::exit(1);
        });
        if let Err(e) = config.apply_cli(key.trim(), value.trim()) {
            eprintln!("Config override error: {}", e);
            std::process::exit(1);
        }
    }

    // Step 4: Initialize logging.
    logging::init(&config.log);

    tracing::info!(
        listen = %config.server.listen,
        tls = config.server.tls.is_enabled(),
        "starting codex-conv"
    );

    // Step 5: Start server (Task 4 will fill this in).
    // For now, just log and exit.
    tracing::warn!("server not yet implemented (Task 4)");
}

/// Scan environment variables with `CODEX_CONV_` prefix and apply overrides.
fn apply_env_overrides(config: &mut AppConfig) {
    const PREFIX: &str = "CODEX_CONV_";
    for (key, value) in std::env::vars() {
        if let Some(rest) = key.strip_prefix(PREFIX) {
            if let Err(e) = config.apply_env(rest, &value) {
                tracing::warn!(key = %rest, error = %e, "ignoring invalid env override");
            }
        }
    }
}
```

### Step 3.2 — Implement `src/logging.rs`

```rust
use crate::config::{ConsoleLog, FileLog, LogConfig};
use tracing::Level;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialize the logging system based on config.
pub fn init(log_config: &LogConfig) {
    let mut layers = Vec::new();

    // Console layer.
    if let Some(ref console) = log_config.console {
        let filter = level_filter(&console.level);
        let console_layer = fmt::layer()
            .with_target(true)
            .with_filter(filter);
        layers.push(console_layer.boxed());
    }

    // File layer (async non-blocking appender).
    if let Some(ref file_cfg) = log_config.file {
        let dir = std::path::PathBuf::from(&file_cfg.dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("Warning: could not create log directory {}: {}", dir.display(), e);
        }

        let file_appender = match file_cfg.rotation.as_str() {
            "hourly" => tracing_appender::rolling::hourly(&dir, "codex-conv.log"),
            _ => tracing_appender::rolling::daily(&dir, "codex-conv.log"),
        };
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        // Guard must be leaked to keep the appender alive for the process lifetime.
        std::mem::forget(_guard);

        let filter = level_filter(&file_cfg.level);
        let file_layer = fmt::layer()
            .with_writer(non_blocking)
            .with_target(true)
            .with_filter(filter);
        layers.push(file_layer.boxed());
    }

    if layers.is_empty() {
        // No logging configured — install a no-op subscriber to avoid panics.
        let _ = tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(Level::ERROR)
            .try_init();
        return;
    }

    tracing_subscriber::registry()
        .with(layers)
        .init();
}

/// Convert a string level name to a tracing LevelFilter.
fn level_filter(level: &str) -> EnvFilter {
    let l = match level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" | "warning" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };
    EnvFilter::from(l)
}
```

### Step 3.3 — Verify build + run

- [ ] Run `cargo build`. Must succeed.
- [ ] Run `cargo run -- -h`. Must show help text with `-c`, `-C` flags.
- [ ] Run `cargo run --`. Must log "starting codex-conv" and "server not yet implemented".

### Step 3.4 — Commit

```
feat: implement CLI with clap and async logging with tracing
```

---

## Task 4: Server + Router + TLS

**Files:** `src/server.rs`, `src/router.rs`, `src/tls.rs`

### Step 4.1 — Write failing test for URL routing

- [ ] Add test module to `src/router.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_upstream_base_url_standard() {
        let route = RouteInfo::parse("/https/api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn extracts_upstream_base_url_single_colon() {
        let route = RouteInfo::parse("/https:/api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn extracts_upstream_base_url_double_slash() {
        let route = RouteInfo::parse("/https://api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn extracts_upstream_base_url_with_v1_prefix() {
        let route = RouteInfo::parse("/v1/https/api.anthropic.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://api.anthropic.com");
    }

    #[test]
    fn rejects_non_responses_path() {
        let result = RouteInfo::parse("/v1/chat/completions");
        assert!(result.is_none());
    }

    #[test]
    fn rejects_missing_host() {
        let result = RouteInfo::parse("/https/responses");
        assert!(result.is_none());
    }

    #[test]
    fn extracts_custom_upstream() {
        let route = RouteInfo::parse("/https/my-proxy.example.com/responses").unwrap();
        assert_eq!(route.upstream_base_url, "https://my-proxy.example.com");
    }
}
```

### Step 4.2 — Verify tests fail

- [ ] Run `cargo test --lib router`. Compilation errors expected.

### Step 4.3 — Implement router

- [ ] Implement `src/router.rs`.

```rust
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use axum::body::Body;
use http::StatusCode;

/// Parsed route information extracted from the request URL.
#[derive(Debug, Clone)]
pub struct RouteInfo {
    /// The upstream base URL (e.g., "https://api.anthropic.com").
    /// No trailing slash, no /v1 prefix.
    pub upstream_base_url: String,
}

impl RouteInfo {
    /// Parse the request path to extract the upstream base URL.
    ///
    /// Accepted formats (all normalized to `https://host`):
    /// - `/https/api.anthropic.com/responses`
    /// - `/https:/api.anthropic.com/responses`
    /// - `/https://api.anthropic.com/responses`
    /// - `/v1/https/api.anthropic.com/responses`
    ///
    /// Returns None if the path does not end in `/responses` or the host is missing.
    pub fn parse(path: &str) -> Option<Self> {
        // Must end with /responses.
        if !path.ends_with("/responses") {
            return None;
        }

        // Strip trailing /responses.
        let prefix = &path[..path.len() - "/responses".len()];

        // Strip optional /v1 prefix.
        let prefix = prefix.strip_prefix("/v1").unwrap_or(prefix);

        // Must start with /https.
        let after_https = prefix.strip_prefix("/https")?;

        // Normalize: strip optional :// or :/ or just /.
        let host_part = if after_https.starts_with("://") {
            &after_https[3..]
        } else if after_https.starts_with(":/") {
            &after_https[2..]
        } else if after_https.starts_with('/') {
            &after_https[1..]
        } else {
            return None;
        };

        if host_part.is_empty() {
            return None;
        }

        Some(RouteInfo {
            upstream_base_url: format!("https://{}", host_part),
        })
    }

    /// Build the full upstream URL for the Anthropic Messages API.
    pub fn upstream_messages_url(&self) -> String {
        format!("{}/v1/messages", self.upstream_base_url)
    }
}

/// Shared application state passed to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: crate::config::AppConfig,
}

/// Build the main axum router with all routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Catch-all route that accepts any path ending in /responses.
        .route("/*path", post(handle_responses))
        .with_state(state)
}

/// Main handler for Responses API requests.
async fn handle_responses(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response<Body>, (StatusCode, axum::Json<serde_json::Value>)> {
    let path = req.uri().path().to_string();
    let route_info = RouteInfo::parse(&path).ok_or_else(|| {
        tracing::warn!(path = %path, "invalid route path");
        (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "message": format!("Invalid route path: {}", path),
                    "type": "invalid_request_error",
                    "param": null,
                    "code": "invalid_request"
                }
            })),
        )
    })?;

    tracing::debug!(path = %path, upstream = %route_info.upstream_base_url, "routed request");

    // TODO (Task 5+): Create ConversionTask and delegate.
    let _ = (state, route_info);
    Ok(Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .body(Body::from("not yet implemented"))
        .unwrap())
}
```

### Step 4.4 — Implement server

- [ ] Implement `src/server.rs`.

```rust
use crate::config::AppConfig;
use crate::router::{AppState, build_router};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::time::Duration;

/// Run the HTTP/HTTPS server until a shutdown signal is received.
pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = config.server.listen.parse()?;
    let state = AppState { config: config.clone() };
    let app = build_router(state);

    if config.server.tls.is_enabled() {
        run_tls(addr, app, &config).await
    } else {
        run_plain(addr, app, &config).await
    }
}

async fn run_plain(
    addr: SocketAddr,
    app: axum::Router,
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening (HTTP)");

    let shutdown = shutdown_signal();
    let timeout = Duration::from_secs(config.server.shutdown_timeout);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    tracing::info!(?timeout, "shutdown complete");
    Ok(())
}

async fn run_tls(
    addr: SocketAddr,
    app: axum::Router,
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tls_config = crate::tls::load_server_tls(&config.server.tls)?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening (HTTPS)");

    let shutdown = shutdown_signal();
    let timeout = Duration::from_secs(config.server.shutdown_timeout);

    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    let graceful = axum::serve(listener, app).with_graceful_shutdown(shutdown);

    // TLS accept loop: accept TCP connections, perform TLS handshake, hand off to axum.
    tokio::select! {
        result = graceful => {
            if let Err(e) = result {
                tracing::error!(error = %e, "server error");
            }
        }
        _ = tokio::time::sleep(timeout) => {
            tracing::warn!(?timeout, "shutdown timed out, forcing close");
        }
    }

    tracing::info!("shutdown complete");
    Ok(())
}

/// Wait for SIGTERM or SIGINT.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}
```

### Step 4.5 — Implement TLS

- [ ] Implement `src/tls.rs`.

```rust
use crate::config::{TlsServerConfig, UpstreamTlsConfig};
use std::sync::Arc;

/// Load downstream (server-side) TLS configuration.
/// Returns None if TLS is not configured (cert/key empty).
pub fn load_server_tls(
    config: &TlsServerConfig,
) -> Result<Arc<rustls::ServerConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let cert_pem = std::fs::read(&config.cert)?;
    let key_pem = std::fs::read(&config.key)?;

    let certs: Vec<rustls::pki_types::CertificateDer<'_>> =
        rustls_pemfile::certs(&mut &cert_pem[..])
            .collect::<Result<Vec<_>, _>>()?;

    let key = rustls_pemfile::private_key(&mut &key_pem[..])?
        .ok_or("no private key found in key file")?;

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(Arc::new(server_config))
}

/// Build a reqwest TLS client config for upstream connections.
pub fn build_upstream_tls_config(
    config: &UpstreamTlsConfig,
) -> Result<reqwest::ClientBuilder, Box<dyn std::error::Error + Send + Sync>> {
    // Install the default crypto provider if not already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut builder = reqwest::Client::builder();

    if !config.use_system_roots && config.extra_ca_certs.is_empty() {
        // No roots at all — likely broken, but let the user decide.
        builder = builder.tls_built_in_root_certs(false);
    } else {
        builder = builder.tls_built_in_root_certs(config.use_system_roots);
    }

    // Load extra CA certificates.
    if !config.extra_ca_certs.is_empty() {
        let mut root_store = rustls::RootCertStore::empty();
        if config.use_system_roots {
            root_store = rustls::crypto::CryptoProvider::get_default()
                .map(|p| {
                    let mut store = rustls::RootCertStore::empty();
                    store.extend(p.root_certs.clone());
                    store
                })
                .unwrap_or_default();
        }

        for cert_path in &config.extra_ca_certs {
            let pem = std::fs::read(cert_path)?;
            let certs: Vec<_> = rustls_pemfile::certs(&mut &pem[..])
                .collect::<Result<Vec<_>, _>>()?;
            for cert in certs {
                root_store.add(cert)?;
            }
        }
    }

    Ok(builder)
}
```

### Step 4.6 — Wire server into main.rs

- [ ] Update `src/main.rs` to call `server::run` instead of the placeholder.

Replace the placeholder at the end of `main()`:
```rust
    // Step 5: Start server.
    if let Err(e) = codex_conv::server::run(config).await {
        tracing::error!(error = %e, "server exited with error");
        std::process::exit(1);
    }
```

Make `main` async:
```rust
#[tokio::main]
async fn main() {
    // ... (steps 1-4 unchanged)
    // Step 5: Start server.
    if let Err(e) = codex_conv::server::run(config).await {
        tracing::error!(error = %e, "server exited with error");
        std::process::exit(1);
    }
}
```

### Step 4.7 — Verify router tests pass

- [ ] Run `cargo test --lib router`. All 7 tests pass.
- [ ] Run `cargo build`. Clean build.

### Step 4.8 — Commit

```
feat: implement axum server, URL router with protocol normalization, and TLS
```

---

## Task 5: Conversion Types + State

**Files:** `src/conversion/mod.rs`

### Step 5.1 — Write tests for ConversionTask struct

- [ ] Add test module to `src/conversion/mod.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_has_empty_state() {
        let task = ConversionTask::new("https://api.anthropic.com".to_string());
        assert!(task.upstream_base_url == "https://api.anthropic.com");
        assert!(task.id_map.len() == 0);
        assert!(task.namespace_registry.len() == 0);
    }

    #[test]
    fn id_map_round_trip() {
        let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
        let toolu = task.id_map.insert_call("call_001".to_string());
        assert!(toolu.starts_with("toolu_"));
        let resolved = task.id_map.get_toolu_for_call("call_001").unwrap();
        assert_eq!(resolved, &toolu);
        let call = task.id_map.get_call_for_toolu(&toolu).unwrap();
        assert_eq!(call, "call_001");
    }

    #[test]
    fn namespace_register_and_lookup() {
        let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
        task.namespace_registry.register(
            "mcp__memory__search".to_string(),
            "mcp__memory__".to_string(),
            "search".to_string(),
        );
        let entry = task.namespace_registry.lookup("mcp__memory__search").unwrap();
        assert_eq!(entry.namespace, "mcp__memory__");
        assert_eq!(entry.tool_name, "search");
    }
}
```

### Step 5.2 — Verify tests fail

- [ ] Run `cargo test --lib conversion`. Compilation errors expected.

### Step 5.3 — Implement ConversionTask and in-flight state

- [ ] Implement `src/conversion/mod.rs`.

```rust
pub mod content;
pub mod error;
pub mod id_map;
pub mod namespace;
pub mod request;
pub mod response;
pub mod signature_cache;
pub mod thinking;

pub use id_map::IdMap;
pub use namespace::NamespaceRegistry;
pub use signature_cache::SignatureCache;

/// Per-request conversion state machine.
///
/// Lifecycle:
/// 1. `new()` — create with upstream base URL.
/// 2. `convert_request()` — parse Responses API request, build namespace registry, ID maps,
///    convert to Anthropic Messages API request. (Task 9)
/// 3. Stream SSE events from upstream, converting each in real-time. (Part 2)
/// 4. On stream end, write accumulated signatures to signature cache. (Part 2)
/// 5. Drop — all in-flight state released.
pub struct ConversionTask {
    /// The upstream Anthropic API base URL (no /v1).
    pub upstream_base_url: String,

    /// Bidirectional ID map: call_id <-> toolu_id.
    pub id_map: IdMap,

    /// Namespace registry: flat_name -> (namespace, tool_name).
    pub namespace_registry: NamespaceRegistry,

    /// Shared signature cache reference (TTL-based, shared across tasks).
    pub signature_cache: std::sync::Arc<SignatureCache>,
}

impl ConversionTask {
    /// Create a new conversion task for the given upstream base URL.
    pub fn new(upstream_base_url: String) -> Self {
        Self {
            upstream_base_url,
            id_map: IdMap::new(),
            namespace_registry: NamespaceRegistry::new(),
            signature_cache: std::sync::Arc::new(SignatureCache::new(std::time::Duration::from_secs(3 * 3600))),
        }
    }

    /// Create with a shared signature cache.
    pub fn with_signature_cache(
        upstream_base_url: String,
        signature_cache: std::sync::Arc<SignatureCache>,
    ) -> Self {
        Self {
            upstream_base_url,
            id_map: IdMap::new(),
            namespace_registry: NamespaceRegistry::new(),
            signature_cache,
        }
    }

    /// Full upstream URL for the Anthropic Messages API endpoint.
    pub fn upstream_messages_url(&self) -> String {
        format!("{}/v1/messages", self.upstream_base_url)
    }
}
```

### Step 5.4 — Implement `src/conversion/id_map.rs` (stub for now)

```rust
use std::collections::HashMap;
use uuid::Uuid;

/// Bidirectional ID map between OpenAI call_id and Anthropic toolu_id.
///
/// - OpenAI uses `call_xxx` (or `ws_xxx`, `fs_xxx`, etc. for built-in tools).
/// - Anthropic uses `toolu_xxx`.
/// - The proxy generates `toolu_` prefixed IDs and maintains the mapping.
#[derive(Debug, Default)]
pub struct IdMap {
    call_to_toolu: HashMap<String, String>,
    toolu_to_call: HashMap<String, String>,
}

impl IdMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries in the map.
    pub fn len(&self) -> usize {
        self.call_to_toolu.len()
    }

    /// Register a call_id and generate a new toolu_id for it.
    /// Returns the generated toolu_id.
    pub fn insert_call(&mut self, call_id: String) -> String {
        let toolu_id = format!("toolu_{}", Uuid::new_v4().simple());
        self.call_to_toolu.insert(call_id.clone(), toolu_id.clone());
        self.toolu_to_call.insert(toolu_id.clone(), call_id);
        toolu_id
    }

    /// Register a mapping with an explicit toolu_id (for built-in tool IDs like ws_xxx -> toolu_ws_xxx).
    pub fn insert_with_toolu(&mut self, call_id: String, toolu_id: String) {
        self.call_to_toolu.insert(call_id.clone(), toolu_id.clone());
        self.toolu_to_call.insert(toolu_id, call_id);
    }

    /// Look up the toolu_id for a given call_id.
    pub fn get_toolu_for_call(&self, call_id: &str) -> Option<&String> {
        self.call_to_toolu.get(call_id)
    }

    /// Look up the call_id for a given toolu_id.
    pub fn get_call_for_toolu(&self, toolu_id: &str) -> Option<&String> {
        self.toolu_to_call.get(toolu_id)
    }
}
```

### Step 5.5 — Implement `src/conversion/namespace.rs` (stub)

```rust
use std::collections::HashMap;
use sha2::{Sha256, Digest};

/// Registry for MCP namespace flattening.
///
/// Maps flattened tool names (e.g., "mcp__memory__search") to their
/// original (namespace, tool_name) components.
#[derive(Debug, Default)]
pub struct NamespaceRegistry {
    /// flat_name -> (namespace, tool_name)
    entries: HashMap<String, NamespaceEntry>,
}

#[derive(Debug, Clone)]
pub struct NamespaceEntry {
    pub namespace: String,
    pub tool_name: String,
}

impl NamespaceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries in the registry.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Register a flattened tool name with its original components.
    pub fn register(&mut self, flat_name: String, namespace: String, tool_name: String) {
        self.entries.insert(flat_name, NamespaceEntry { namespace, tool_name });
    }

    /// Look up a flattened tool name to get its original components.
    pub fn lookup(&self, flat_name: &str) -> Option<&NamespaceEntry> {
        self.entries.get(flat_name)
    }

    /// Flatten an MCP tool name: `mcp__{server}__{tool}`.
    /// Applies truncation + hash suffix for names exceeding 64 characters.
    /// Returns the flattened name. Also registers the truncated alias if truncation was applied.
    pub fn flatten_and_register(
        &mut self,
        namespace: &str,
        tool_name: &str,
    ) -> String {
        // Trim trailing underscores from namespace (Codex sends "mcp__memory__").
        let server = namespace.trim_end_matches('_');
        let flat = format!("mcp__{}__{}", server, tool_name);

        let registered = if flat.len() <= 64 {
            flat.clone()
        } else {
            // Truncate + 12 hex char hash suffix.
            truncate_and_hash(&flat)
        };

        self.register(
            registered.clone(),
            namespace.to_string(),
            tool_name.to_string(),
        );

        // Also register the original (un-truncated) name so lookups work both ways.
        if registered != flat {
            self.register(flat, namespace.to_string(), tool_name.to_string());
        }

        registered
    }
}

/// Truncate a tool name to fit within 64 characters, appending a 12-char hex hash.
/// Format: first 51 chars + "_" + 12 hex chars = 64 chars total.
fn truncate_and_hash(name: &str) -> String {
    let hash = Sha256::digest(name.as_bytes());
    let suffix = hex::encode(&hash[..6]); // 12 hex chars

    // 64 - 1 (underscore) - 12 (hash) = 51 chars of the original name.
    let max_prefix = 64 - 1 - 12;
    let prefix: String = name.chars().take(max_prefix).collect();
    format!("{}_{}", prefix, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_basic() {
        let mut reg = NamespaceRegistry::new();
        let flat = reg.flatten_and_register("mcp__memory__", "search");
        assert_eq!(flat, "mcp__memory__search");
    }

    #[test]
    fn flatten_long_name_truncated() {
        let mut reg = NamespaceRegistry::new();
        let long_tool = "a".repeat(80);
        let flat = reg.flatten_and_register("mcp__svr__", &long_tool);
        assert!(flat.len() <= 64, "truncated name must be <= 64 chars, got {}", flat.len());
        assert!(flat.starts_with("mcp__svr__a"));
    }

    #[test]
    fn lookup_truncated_name() {
        let mut reg = NamespaceRegistry::new();
        let long_tool = "a".repeat(80);
        let flat = reg.flatten_and_register("mcp__svr__", &long_tool);
        let entry = reg.lookup(&flat).unwrap();
        assert_eq!(entry.namespace, "mcp__svr__");
        assert_eq!(entry.tool_name, long_tool);
    }

    #[test]
    fn lookup_original_name_also_works() {
        let mut reg = NamespaceRegistry::new();
        let long_tool = "a".repeat(80);
        let flat = reg.flatten_and_register("mcp__svr__", &long_tool);
        // The original (un-truncated) name should also be in the registry.
        let original = format!("mcp__svr__{}", long_tool);
        let entry = reg.lookup(&original).unwrap();
        assert_eq!(entry.tool_name, long_tool);
    }
}
```

### Step 5.6 — Implement `src/conversion/signature_cache.rs` (stub)

```rust
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// TTL-based cache for Anthropic thinking signatures.
///
/// Keyed by proxy-generated reasoning item ID (`rs_xxx`).
/// Shared across all conversion tasks via `Arc<SignatureCache>`.
pub struct SignatureCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    ttl: Duration,
}

struct CacheEntry {
    signature: String,
    expires_at: Instant,
}

impl SignatureCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Store a signature for the given reasoning item ID.
    pub fn insert(&self, reasoning_id: String, signature: String) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(reasoning_id, CacheEntry {
            signature,
            expires_at: Instant::now() + self.ttl,
        });
    }

    /// Retrieve a signature for the given reasoning item ID.
    /// Returns None if not found or expired.
    pub fn get(&self, reasoning_id: &str) -> Option<String> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get(reasoning_id)?;
        if Instant::now() > entry.expires_at {
            entries.remove(reasoning_id);
            None
        } else {
            Some(entry.signature.clone())
        }
    }
}
```

### Step 5.7 — Verify tests pass

- [ ] Run `cargo test --lib conversion`. All tests pass.

### Step 5.8 — Commit

```
feat: implement ConversionTask state machine with ID map, namespace registry, and signature cache
```

---

## Task 6: ID Map

**Files:** `src/conversion/id_map.rs` (already created in Task 5, now add comprehensive tests)

### Step 6.1 — Write comprehensive ID map tests

- [ ] Add tests to `src/conversion/id_map.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_generates_toolu_prefix() {
        let mut map = IdMap::new();
        let toolu = map.insert_call("call_001".to_string());
        assert!(toolu.starts_with("toolu_"));
        assert!(toolu.len() > 6); // "toolu_" + uuid
    }

    #[test]
    fn round_trip_call_to_toolu() {
        let mut map = IdMap::new();
        let toolu = map.insert_call("call_001".to_string());
        assert_eq!(map.get_toolu_for_call("call_001"), Some(&toolu));
        assert_eq!(map.get_call_for_toolu(&toolu), Some(&String::from("call_001")));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let map = IdMap::new();
        assert!(map.get_toolu_for_call("nonexistent").is_none());
        assert!(map.get_call_for_toolu("nonexistent").is_none());
    }

    #[test]
    fn multiple_mappings() {
        let mut map = IdMap::new();
        let t1 = map.insert_call("call_001".to_string());
        let t2 = map.insert_call("call_002".to_string());
        assert_ne!(t1, t2, "each call should get a unique toolu_id");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn insert_with_explicit_toolu() {
        let mut map = IdMap::new();
        map.insert_with_toolu("ws_abc".to_string(), "toolu_ws_abc".to_string());
        assert_eq!(map.get_toolu_for_call("ws_abc"), Some(&String::from("toolu_ws_abc")));
        assert_eq!(map.get_call_for_toolu("toolu_ws_abc"), Some(&String::from("ws_abc")));
    }
}
```

### Step 6.2 — Verify tests pass

- [ ] Run `cargo test --lib id_map`. All pass.

### Step 6.3 — Commit

```
test: add comprehensive ID map tests for bidirectional call_id/toolu_id mapping
```

---

## Task 7: Namespace Handling

**Files:** `src/conversion/namespace.rs` (expand tests from Task 5)

### Step 7.1 — Write additional namespace tests

- [ ] Add to the test module in `src/conversion/namespace.rs`.

```rust
    #[test]
    fn flatten_trailing_underscore_stripped() {
        let mut reg = NamespaceRegistry::new();
        let flat = reg.flatten_and_register("mcp__memory__", "search");
        assert_eq!(flat, "mcp__memory__search");
        // Should NOT be "mcp__memory___search" (triple underscore).
    }

    #[test]
    fn multiple_namespaces() {
        let mut reg = NamespaceRegistry::new();
        let f1 = reg.flatten_and_register("mcp__memory__", "search");
        let f2 = reg.flatten_and_register("mcp__filesystem__", "read");
        assert_eq!(f1, "mcp__memory__search");
        assert_eq!(f2, "mcp__filesystem__read");
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn lookup_missing_returns_none() {
        let reg = NamespaceRegistry::new();
        assert!(reg.lookup("nonexistent").is_none());
    }

    #[test]
    fn truncate_and_hash_deterministic() {
        // Same input must produce same output.
        let long = "mcp__server__".to_string() + &"x".repeat(100);
        let h1 = truncate_and_hash(&long);
        let h2 = truncate_and_hash(&long);
        assert_eq!(h1, h2);
    }

    #[test]
    fn truncate_and_hash_unique_for_different_inputs() {
        let n1 = "mcp__server_a__".to_string() + &"x".repeat(100);
        let n2 = "mcp__server_b__".to_string() + &"x".repeat(100);
        let h1 = truncate_and_hash(&n1);
        let h2 = truncate_and_hash(&n2);
        assert_ne!(h1, h2, "different inputs must produce different hashes");
    }

    #[test]
    fn flatten_exactly_64_chars_not_truncated() {
        let mut reg = NamespaceRegistry::new();
        // Build a name that is exactly 64 chars.
        // "mcp__sv__" = 9 chars, need 55 more for the tool name.
        let tool_name = "a".repeat(55);
        let flat = reg.flatten_and_register("mcp__sv__", &tool_name);
        assert_eq!(flat.len(), 64);
        // Should NOT be truncated (no hash suffix).
        assert!(flat.ends_with(&tool_name));
    }

    #[test]
    fn flatten_65_chars_is_truncated() {
        let mut reg = NamespaceRegistry::new();
        // "mcp__sv__" = 9 chars, need 56 more for 65 total.
        let tool_name = "a".repeat(56);
        let flat = reg.flatten_and_register("mcp__sv__", &tool_name);
        assert!(flat.len() <= 64);
    }
```

### Step 7.2 — Verify tests pass

- [ ] Run `cargo test --lib namespace`. All pass.

### Step 7.3 — Commit

```
test: add comprehensive namespace handling tests for flattening, truncation, and lookup
```

---

## Task 8: Content Block Conversion

**Files:** `src/conversion/content.rs`

### Step 8.1 — Write failing tests for content block conversion

- [ ] Add test module to `src/conversion/content.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_text_to_text() {
        let input = json!({"type": "input_text", "text": "Hello"});
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["type"], "text");
        assert_eq!(block["text"], "Hello");
    }

    #[test]
    fn input_image_url_to_image_url_source() {
        let input = json!({
            "type": "input_image",
            "image_url": "https://example.com/image.png"
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "https://example.com/image.png");
    }

    #[test]
    fn input_image_base64_to_base64_source() {
        let input = json!({
            "type": "input_image",
            "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], "image/png");
        assert_eq!(block["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn input_image_jpeg_base64() {
        let input = json!({
            "type": "input_image",
            "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["source"]["media_type"], "image/jpeg");
        assert_eq!(block["source"]["data"], "/9j/4AAQ");
    }

    #[test]
    fn input_file_dropped() {
        let input = json!({
            "type": "input_file",
            "file_url": "https://example.com/doc.pdf"
        });
        let result = convert_user_content(&input);
        assert!(result.is_none());
    }

    #[test]
    fn cache_control_passthrough() {
        let input = json!({
            "type": "input_text",
            "text": "cached text",
            "cache_control": {"type": "ephemeral"}
        });
        let block = convert_user_content(&input).unwrap();
        assert_eq!(block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn unknown_content_type_returns_none() {
        let input = json!({"type": "input_video", "url": "..."});
        let result = convert_user_content(&input);
        assert!(result.is_none());
    }
}
```

### Step 8.2 — Verify tests fail

- [ ] Run `cargo test --lib content`. Compilation errors expected.

### Step 8.3 — Implement content block conversion

- [ ] Implement `src/conversion/content.rs`.

```rust
use serde_json::{json, Value};

/// Convert a Responses API user content block to an Anthropic content block.
///
/// Supported types:
/// - `input_text` → `text`
/// - `input_image` with URL string → `image` with `url` source
/// - `input_image` with data URI → `image` with `base64` source
/// - `input_file` → dropped (no Anthropic equivalent)
///
/// `cache_control` is passed through if present.
///
/// Returns `None` for unsupported types (caller should skip or log).
pub fn convert_user_content(input: &Value) -> Option<Value> {
    let obj = input.as_object()?;
    let content_type = obj.get("type")?.as_str()?;

    let mut block = match content_type {
        "input_text" => {
            let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
            json!({"type": "text", "text": text})
        }
        "input_image" => {
            convert_input_image(obj)?
        }
        "input_file" => {
            // Anthropic has no generic file input. Drop silently.
            tracing::debug!("dropping input_file content block (no Anthropic equivalent)");
            return None;
        }
        other => {
            tracing::warn!(content_type = other, "dropping unknown user content block type");
            return None;
        }
    };

    // Pass through cache_control if present.
    if let Some(cc) = obj.get("cache_control") {
        block.as_object_mut().unwrap().insert("cache_control".to_string(), cc.clone());
    }

    Some(block)
}

/// Convert an `input_image` content block to Anthropic `image` block.
fn convert_input_image(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let image_url_field = obj.get("image_url")?;

    // Case 1: image_url is a plain string (URL).
    if let Some(url_str) = image_url_field.as_str() {
        return Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url_str
            }
        }));
    }

    // Case 2: image_url is an object with a "url" field (data URI).
    let url_value = image_url_field.get("url")?.as_str()?;

    if let Some(rest) = url_value.strip_prefix("data:") {
        // Parse data URI: "data:image/png;base64,<data>"
        let parts: Vec<&str> = rest.splitn(2, ',').collect();
        if parts.len() != 2 {
            tracing::warn!("malformed data URI in input_image");
            return None;
        }
        let mime = parts[0].trim_end_matches(";base64");
        let data = parts[1];

        Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": mime,
                "data": data
            }
        }))
    } else {
        // Not a data URI, treat as regular URL.
        Some(json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url_value
            }
        }))
    }
}

/// Convert an array of Responses API user content blocks to Anthropic content blocks.
/// Skips unsupported types with a warning log.
pub fn convert_user_content_array(inputs: &[Value]) -> Vec<Value> {
    let mut result = Vec::with_capacity(inputs.len());
    for input in inputs {
        if let Some(block) = convert_user_content(input) {
            result.push(block);
        }
    }
    result
}

/// Convert `tool_result.content` from function_call_output.
///
/// The output can be:
/// - A plain string → Anthropic `tool_result.content` as plain string.
/// - An array of content items → Anthropic `tool_result.content` as array of content blocks.
pub fn convert_tool_result_content(output: &Value) -> Value {
    if output.is_string() {
        output.clone()
    } else if output.is_array() {
        let blocks: Vec<Value> = output
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| convert_user_content(item))
            .collect();
        Value::Array(blocks)
    } else {
        // Fallback: serialize as JSON string.
        json!(output.to_string())
    }
}
```

### Step 8.4 — Verify tests pass

- [ ] Run `cargo test --lib content`. All 8 tests pass.

### Step 8.5 — Commit

```
feat: implement content block conversion for input_text, input_image (URL and base64), cache_control passthrough
```

---

## Task 9: Request Conversion

**Files:** `src/conversion/request.rs`, `src/conversion/thinking.rs`

This is the largest task. It converts a full Responses API request body into an Anthropic Messages API request body.

### Step 9.1 — Write failing tests for request conversion

- [ ] Add test module to `src/conversion/request.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_task() -> ConversionTask {
        ConversionTask::new("https://api.anthropic.com".to_string())
    }

    // --- Top-level parameter tests ---

    #[test]
    fn top_level_passthrough() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "stream": true,
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["model"], "claude-sonnet-4-20250514");
        assert_eq!(result["stream"], true);
    }

    #[test]
    fn stream_always_true() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "stream": false,
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["stream"], true);
    }

    #[test]
    fn instructions_to_system() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "You are helpful.",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let system = result["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are helpful.");
    }

    #[test]
    fn instructions_plus_system_input_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "instructions": "Base instructions.",
            "input": [
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "Extra system."}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let system = result["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], "Base instructions.");
        assert_eq!(system[1]["text"], "Extra system.");
    }

    #[test]
    fn no_system_when_empty() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}]
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("system").is_none());
    }

    // --- tool_choice tests ---

    #[test]
    fn tool_choice_auto_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "auto",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
    }

    #[test]
    fn tool_choice_required_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "required",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "any");
    }

    #[test]
    fn tool_choice_none_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "none",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "none");
    }

    #[test]
    fn tool_choice_function_object() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": {"type": "function", "name": "my_tool"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "tool");
        assert_eq!(result["tool_choice"]["name"], "my_tool");
    }

    #[test]
    fn tool_choice_named_by_string() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "my_tool",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "tool");
        assert_eq!(result["tool_choice"]["name"], "my_tool");
    }

    #[test]
    fn tool_choice_absent_defaults_to_auto() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
    }

    // --- parallel_tool_calls tests ---

    #[test]
    fn parallel_tool_calls_false_embeds_disable() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
        assert_eq!(result["tool_choice"]["disable_parallel_tool_use"], true);
    }

    #[test]
    fn parallel_tool_calls_true_no_disable() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "auto");
        assert!(result["tool_choice"].get("disable_parallel_tool_use").is_none());
    }

    #[test]
    fn parallel_tool_calls_false_with_none_no_change() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tool_choice": "none",
            "parallel_tool_calls": false,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["tool_choice"]["type"], "none");
        // "none" means no tools — disable_parallel_tool_use is irrelevant, omit it.
        assert!(result["tool_choice"].get("disable_parallel_tool_use").is_none());
    }

    // --- Input items → Messages ---

    #[test]
    fn user_message_converted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn assistant_message_converted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hi there"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
    }

    #[test]
    fn function_call_creates_assistant_tool_use() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "exec_command", "arguments": "{\"command\":\"ls\"}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["name"], "exec_command");
        assert_eq!(messages[0]["content"][0]["input"]["command"], "ls");
        // ID map should have been populated.
        assert!(task.id_map.get_toolu_for_call("call_001").is_some());
    }

    #[test]
    fn function_call_output_creates_user_tool_result() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "exec_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_001", "output": "file1.txt\nfile2.txt"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        // function_call → assistant, function_call_output → user
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["content"], "file1.txt\nfile2.txt");
    }

    #[test]
    fn function_call_arguments_parse_failure_rejects() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "exec_command", "arguments": "not-valid-json{"}
            ]
        });
        let result = convert_request(&mut task, input);
        assert!(result.is_err());
    }

    // --- Message alternation merge ---

    #[test]
    fn consecutive_function_calls_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_002", "name": "tool_b", "arguments": "{}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "consecutive function_calls should merge into one assistant message");
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn consecutive_function_call_outputs_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_001", "output": "result_a"},
                {"type": "function_call_output", "call_id": "call_001", "output": "result_b"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        // assistant (function_call) + user (merged outputs)
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn reasoning_plus_function_call_merged() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [{"type": "summary_text", "text": "thinking..."}]},
                {"type": "function_call", "call_id": "call_001", "name": "tool_a", "arguments": "{}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "reasoning + function_call should merge into one assistant message");
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        // First block is thinking, second is tool_use.
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "tool_use");
    }

    // --- Reasoning input ---

    #[test]
    fn reasoning_with_summary_only() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [{"type": "summary_text", "text": "I thought about it"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        let content = &messages[0]["content"][0];
        assert_eq!(content["type"], "thinking");
        assert_eq!(content["thinking"], "I thought about it");
        // Empty signature when no cached signature exists.
        assert_eq!(content["signature"], "");
    }

    #[test]
    fn reasoning_with_encrypted_content() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [], "encrypted_content": "ENCRYPTED_DATA_HERE"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let content = &result["messages"].as_array().unwrap()[0]["content"][0];
        assert_eq!(content["type"], "redacted_thinking");
        assert_eq!(content["data"], "ENCRYPTED_DATA_HERE");
    }

    #[test]
    fn reasoning_content_field_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "reasoning", "id": "rs_001", "summary": [{"type": "summary_text", "text": "summary"}], "content": [{"type": "reasoning_text", "text": "raw text"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let content = &result["messages"].as_array().unwrap()[0]["content"][0];
        // The raw content field is dropped; only summary is forwarded.
        assert_eq!(content["thinking"], "summary");
        assert!(content.get("content").is_none());
    }

    // --- Dropped input items ---

    #[test]
    fn compaction_dropped() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "compaction", "encrypted_content": "..."},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "compaction should be dropped");
    }

    #[test]
    fn unknown_item_dropped_with_warning() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "future_unknown_type", "data": "..."},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "unknown items should be dropped");
    }

    // --- Thinking/reasoning params ---

    #[test]
    fn reasoning_effort_maps_to_output_config() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "high", "summary": "auto"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["thinking"]["type"], "adaptive");
        assert_eq!(result["output_config"]["effort"], "high");
    }

    #[test]
    fn reasoning_summary_none_sets_display_omitted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "high", "summary": "none"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["thinking"]["display"], "omitted");
    }

    #[test]
    fn reasoning_effort_none_omits_thinking() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "none"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("thinking").is_none());
        assert!(result.get("output_config").is_none());
    }

    #[test]
    fn reasoning_minimal_maps_to_low() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "minimal", "summary": "auto"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["output_config"]["effort"], "low");
    }

    #[test]
    fn no_reasoning_no_thinking_field() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("thinking").is_none());
        assert!(result.get("output_config").is_none());
    }

    // --- Temperature ---

    #[test]
    fn temperature_omitted_when_thinking_enabled() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "reasoning": {"effort": "high", "summary": "auto"},
            "temperature": 0.7,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("temperature").is_none(), "temperature must be omitted when thinking enabled");
    }

    #[test]
    fn temperature_passed_through_when_no_thinking() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "temperature": 0.7,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["temperature"], 0.7);
    }

    // --- max_output_tokens ---

    #[test]
    fn max_output_tokens_to_max_tokens() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "max_output_tokens": 4096,
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["max_tokens"], 4096);
        assert!(result.get("max_output_tokens").is_none());
    }

    // --- metadata ---

    #[test]
    fn metadata_forwards_user_id_only() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "metadata": {"user_id": "user123", "extra_key": "extra_value"},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["metadata"]["user_id"], "user123");
        assert!(result["metadata"].get("extra_key").is_none());
    }

    // --- service_tier ---

    #[test]
    fn service_tier_auto() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "service_tier": "auto",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["service_tier"], "auto");
    }

    #[test]
    fn service_tier_default_to_standard_only() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "service_tier": "default",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["service_tier"], "standard_only");
    }

    #[test]
    fn service_tier_flex_omitted() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "service_tier": "flex",
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert!(result.get("service_tier").is_none());
    }

    // --- text.format -> output_config.format ---

    #[test]
    fn text_format_json_schema() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "text": {"format": {"type": "json_schema", "name": "codex_output_schema", "schema": {"type": "object"}, "strict": true}},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["output_config"]["format"]["type"], "json_schema");
        assert_eq!(result["output_config"]["format"]["schema"]["type"], "object");
        // name and strict should be dropped.
        assert!(result["output_config"]["format"].get("name").is_none());
        assert!(result["output_config"]["format"].get("strict").is_none());
    }

    #[test]
    fn text_format_text_omits_output_config() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "text": {"format": {"type": "text"}},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        // When reasoning sets output_config, check format is absent.
        // Without reasoning, output_config might not exist at all.
        if let Some(oc) = result.get("output_config") {
            assert!(oc.get("format").is_none());
        }
    }

    #[test]
    fn text_format_json_object() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "text": {"format": {"type": "json_object"}},
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        assert_eq!(result["output_config"]["format"]["type"], "json_object");
    }

    // --- MCP namespace tools ---

    #[test]
    fn namespace_tools_flattened() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [
                {
                    "type": "namespace",
                    "name": "mcp__memory__",
                    "tools": [
                        {"type": "function", "name": "search", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}
                    ]
                }
            ],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "mcp__memory__search");
        assert_eq!(tools[0]["type"], "custom");
        assert!(tools[0]["input_schema"].is_object());
        // Registry should have the mapping.
        let entry = task.namespace_registry.lookup("mcp__memory__search").unwrap();
        assert_eq!(entry.namespace, "mcp__memory__");
        assert_eq!(entry.tool_name, "search");
    }

    // --- function tools ---

    #[test]
    fn function_tools_parameters_to_input_schema() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "tools": [
                {"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}
            ],
            "input": []
        });
        let result = convert_request(&mut task, input).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "exec_command");
        assert!(tools[0].get("input_schema").is_some());
        assert!(tools[0].get("parameters").is_none());
    }

    // --- custom_tool_call and custom_tool_call_output ---

    #[test]
    fn custom_tool_call_same_as_function_call() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "custom_tool_call", "call_id": "call_001", "name": "apply_patch", "input": "{\"patch\":\"...\"}"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[0]["content"][0]["name"], "apply_patch");
    }

    #[test]
    fn custom_tool_call_output_same_as_function_call_output() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "custom_tool_call", "call_id": "call_001", "name": "apply_patch", "input": "{}"},
                {"type": "custom_tool_call_output", "call_id": "call_001", "output": "patch applied"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["content"], "patch applied");
    }

    // --- mcp_tool_call_output ---

    #[test]
    fn mcp_tool_call_output_with_error() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "mcp__memory__search", "arguments": "{}"},
                {"type": "mcp_tool_call_output", "call_id": "call_001", "output": {"content": [{"type": "text", "text": "error"}], "isError": true}}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        let tool_result = &messages[1]["content"][0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["is_error"], true);
    }

    #[test]
    fn mcp_tool_call_output_without_error() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "function_call", "call_id": "call_001", "name": "mcp__memory__search", "arguments": "{}"},
                {"type": "mcp_tool_call_output", "call_id": "call_001", "output": {"content": [{"type": "text", "text": "ok"}]}}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let tool_result = &result["messages"].as_array().unwrap()[1]["content"][0];
        assert!(tool_result.get("is_error").is_none(), "is_error should be omitted when not an error");
    }

    // --- phase stripping ---

    #[test]
    fn phase_field_stripped_from_messages() {
        let mut task = make_task();
        let input = json!({
            "model": "claude-sonnet-4-20250514",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}], "phase": "commentary"}
            ]
        });
        let result = convert_request(&mut task, input).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert!(messages[0].get("phase").is_none());
    }
}
```

### Step 9.2 — Verify tests fail

- [ ] Run `cargo test --lib request`. Compilation errors expected.

### Step 9.3 — Implement `src/conversion/thinking.rs`

- [ ] Implement reasoning/thinking parameter conversion.

```rust
use serde_json::{json, Value};

/// Result of reasoning → thinking conversion.
pub struct ThinkingConfig {
    /// The `thinking` field for the Anthropic request (if reasoning was present and effort != "none").
    pub thinking: Option<Value>,
    /// The `output_config.effort` value (if reasoning was present and effort != "none").
    pub effort: Option<String>,
    /// Whether thinking is enabled (controls temperature omission).
    pub thinking_enabled: bool,
}

/// Convert Responses API `reasoning` field to Anthropic `thinking` + `output_config.effort`.
///
/// - `reasoning.effort` → `output_config.effort` (direct string forwarding, with mappings)
/// - `reasoning.summary` → `thinking.display`
/// - `reasoning.effort: "none"` → omit both `thinking` and `output_config`
/// - `reasoning` absent → omit both
pub fn convert_reasoning(reasoning: Option<&Value>) -> ThinkingConfig {
    let Some(reasoning) = reasoning else {
        return ThinkingConfig {
            thinking: None,
            effort: None,
            thinking_enabled: false,
        };
    };

    let effort_val = reasoning.get("effort").and_then(|v| v.as_str()).unwrap_or("high");
    let summary_val = reasoning.get("summary").and_then(|v| v.as_str());

    // effort "none" means no thinking.
    if effort_val == "none" {
        return ThinkingConfig {
            thinking: None,
            effort: None,
            thinking_enabled: false,
        };
    }

    // Map effort value.
    let mapped_effort = match effort_val {
        "minimal" => "low",  // Anthropic has no "minimal" level.
        other => other,       // "low", "medium", "high", "xhigh" pass through directly.
    };

    // Map summary to thinking.display.
    let display = match summary_val {
        Some("none") => "omitted",
        _ => "summarized", // absent, "auto", "concise", "detailed" all map to "summarized".
    };

    ThinkingConfig {
        thinking: Some(json!({
            "type": "adaptive",
            "display": display,
        })),
        effort: Some(mapped_effort.to_string()),
        thinking_enabled: true,
    }
}

/// Determine the Anthropic content block for a reasoning input item.
///
/// - If `encrypted_content` is present → `redacted_thinking`
/// - Otherwise → `thinking` with summary text and cached or empty signature
pub fn convert_reasoning_input(
    summary: &[Value],
    encrypted_content: Option<&str>,
    signature_cache: &crate::signature_cache::SignatureCache,
    reasoning_id: &str,
) -> Value {
    if let Some(data) = encrypted_content {
        json!({
            "type": "redacted_thinking",
            "data": data
        })
    } else {
        // Concatenate summary texts.
        let thinking_text = summary
            .iter()
            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");

        let signature = signature_cache
            .get(reasoning_id)
            .unwrap_or_default();

        json!({
            "type": "thinking",
            "thinking": thinking_text,
            "signature": signature
        })
    }
}
```

### Step 9.4 — Implement `src/conversion/request.rs`

- [ ] Full request conversion implementation.

```rust
use crate::conversion::content;
use crate::conversion::thinking;
use crate::conversion::ConversionTask;
use crate::conversion::namespace::NamespaceRegistry;
use serde_json::{json, Value};

/// Convert a Responses API request body to an Anthropic Messages API request body.
///
/// This function drives the entire request conversion phase of the state machine:
/// 1. Parse top-level parameters.
/// 2. Build namespace registry from namespace tools.
/// 3. Convert input items to Anthropic messages with alternation merging.
/// 4. Build ID map entries for all call_ids.
/// 5. Return the complete Anthropic request body.
pub fn convert_request(
    task: &mut ConversionTask,
    input: Value,
) -> Result<Value, RequestConversionError> {
    let obj = input.as_object().ok_or_else(|| {
        RequestConversionError::InvalidRequest("request body must be a JSON object".to_string())
    })?;

    // -- Top-level parameters --
    let model = obj.get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Stream is always true (proxy requires streaming from upstream).
    let stream = true;

    // -- System construction --
    let instructions = obj.get("instructions").and_then(|v| v.as_str());
    let system = build_system(instructions, obj.get("input"));

    // -- Reasoning → thinking --
    let thinking_cfg = thinking::convert_reasoning(obj.get("reasoning"));

    // -- Tools (namespace flattening + function tools) --
    let tools = convert_tools(task, obj.get("tools"));

    // -- tool_choice --
    let tool_choice = convert_tool_choice(
        obj.get("tool_choice"),
        obj.get("parallel_tool_calls"),
    );

    // -- Input items → Messages --
    let messages = convert_input_items(task, obj.get("input"))?;

    // -- Build result --
    let mut result = json!({
        "model": model,
        "stream": stream,
        "messages": messages,
    });

    // Conditionally add fields.
    if let Some(sys) = system {
        result["system"] = sys;
    }

    result["tool_choice"] = tool_choice;

    if let Some(tools_arr) = tools {
        result["tools"] = tools_arr;
    }

    // Thinking + output_config.
    if let Some(th) = thinking_cfg.thinking {
        result["thinking"] = th;
    }
    if let Some(effort) = thinking_cfg.effort {
        // Merge into existing output_config or create new.
        if let Some(oc) = result.get_mut("output_config") {
            oc["effort"] = json!(effort);
        } else {
            result["output_config"] = json!({"effort": effort});
        }
    }

    // Temperature: pass through only when thinking not enabled.
    if !thinking_cfg.thinking_enabled {
        if let Some(temp) = obj.get("temperature") {
            result["temperature"] = temp.clone();
        }
    }

    // top_p: pass through.
    if let Some(top_p) = obj.get("top_p") {
        result["top_p"] = top_p.clone();
    }

    // max_output_tokens → max_tokens.
    if let Some(max) = obj.get("max_output_tokens") {
        result["max_tokens"] = max.clone();
    }

    // metadata: forward only user_id.
    if let Some(meta) = obj.get("metadata") {
        if let Some(user_id) = meta.get("user_id") {
            result["metadata"] = json!({"user_id": user_id});
        }
    }

    // service_tier.
    if let Some(st) = obj.get("service_tier").and_then(|v| v.as_str()) {
        match st {
            "auto" => { result["service_tier"] = json!("auto"); }
            "default" => { result["service_tier"] = json!("standard_only"); }
            _ => {} // "flex", "priority", "scale" → omit (no Anthropic equivalent).
        }
    }

    // text.format → output_config.format.
    if let Some(text_fmt) = obj.get("text").and_then(|t| t.get("format")) {
        let fmt_type = text_fmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match fmt_type {
            "json_schema" => {
                let schema = text_fmt.get("schema").cloned().unwrap_or(json!({}));
                if let Some(oc) = result.get_mut("output_config") {
                    oc["format"] = json!({"type": "json_schema", "schema": schema});
                } else {
                    result["output_config"] = json!({"format": {"type": "json_schema", "schema": schema}});
                }
            }
            "json_object" => {
                if let Some(oc) = result.get_mut("output_config") {
                    oc["format"] = json!({"type": "json_object"});
                } else {
                    result["output_config"] = json!({"format": {"type": "json_object"}});
                }
            }
            "text" | _ => {
                // "text" type or unknown → omit output_config.format.
            }
        }
    }

    Ok(result)
}

/// Build the `system` parameter from `instructions` + system input items.
/// Returns None if both are empty.
fn build_system(instructions: Option<&str>, input: Option<&Value>) -> Option<Value> {
    let mut blocks: Vec<Value> = Vec::new();

    if let Some(text) = instructions {
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
    }

    // Extract system messages from input.
    if let Some(input_arr) = input.and_then(|v| v.as_array()) {
        for item in input_arr {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "message" && role == "system" {
                if let Some(content) = item.get("content") {
                    if let Some(arr) = content.as_array() {
                        for block in arr {
                            if let Some(converted) = content::convert_user_content(block) {
                                blocks.push(converted);
                            }
                        }
                    } else if let Some(text) = content.as_str() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
            }
        }
    }

    if blocks.is_empty() {
        None
    } else {
        Some(Value::Array(blocks))
    }
}

/// Convert the `tools` array, flattening namespace tools and renaming `parameters` → `input_schema`.
fn convert_tools(task: &mut ConversionTask, tools: Option<&Value>) -> Option<Value> {
    let tools_arr = tools.and_then(|v| v.as_array())?;
    if tools_arr.is_empty() {
        return None;
    }

    let mut result = Vec::new();

    for tool in tools_arr {
        let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match tool_type {
            "namespace" => {
                // Flatten namespace tools.
                let namespace = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let children = tool.get("tools").and_then(|v| v.as_array());
                if let Some(children) = children {
                    for child in children {
                        let child_name = child.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let flat_name = task.namespace_registry.flatten_and_register(namespace, child_name);

                        let mut anth_tool = json!({
                            "type": "custom",
                            "name": flat_name,
                        });

                        if let Some(params) = child.get("parameters") {
                            anth_tool["input_schema"] = params.clone();
                        }

                        result.push(anth_tool);
                    }
                }
            }
            "function" => {
                let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let mut anth_tool = json!({
                    "type": "custom",
                    "name": name,
                });
                if let Some(params) = tool.get("parameters") {
                    anth_tool["input_schema"] = params.clone();
                }
                result.push(anth_tool);
            }
            // Built-in tool types → convert to custom tools with derived schemas.
            other => {
                if let Some(converted) = convert_builtin_tool(other, tool) {
                    result.push(converted);
                }
                // If convert_builtin_tool returns None, the tool type is unsupported and dropped.
            }
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(Value::Array(result))
    }
}

/// Convert a built-in Responses API tool to an Anthropic custom tool with a derived input_schema.
fn convert_builtin_tool(tool_type: &str, tool: &Value) -> Option<Value> {
    let name = match tool_type {
        "mcp" => {
            // Server-side hosted MCP tool — cannot convert (proxy cannot execute MCP calls).
            tracing::debug!("dropping mcp tool type (server-side hosted, cannot proxy)");
            return None;
        }
        other => other.to_string(),
    };

    // The derived schemas mirror the exact structure of the corresponding output item's call parameters.
    let schema = match name.as_str() {
        "web_search" | "web_search_preview" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["search", "open_page", "find_in_page"]},
                "query": {"type": "string"},
                "queries": {"type": "array", "items": {"type": "string"}},
                "sources": {"type": "array", "items": {"type": "object", "properties": {"type": {"type": "string"}, "url": {"type": "string"}}}},
                "url": {"type": "string"},
                "pattern": {"type": "string"}
            },
            "required": ["type"]
        }),
        "file_search" => json!({
            "type": "object",
            "properties": {
                "queries": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["queries"]
        }),
        "code_interpreter" => json!({
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "container_id": {"type": "string"}
            },
            "required": ["code"]
        }),
        "computer_use_preview" | "computer" | "computer_use" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["screenshot", "click", "double_click", "drag", "keypress", "move", "scroll", "type", "wait"]},
                "button": {"type": "string", "enum": ["left", "right", "wheel", "back", "forward"]},
                "x": {"type": "integer"},
                "y": {"type": "integer"},
                "text": {"type": "string"},
                "keys": {"type": "array", "items": {"type": "string"}},
                "path": {"type": "array", "items": {"type": "object", "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}}}},
                "scroll_x": {"type": "integer"},
                "scroll_y": {"type": "integer"}
            },
            "required": ["type"]
        }),
        "image_generation" => json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string"}
            },
            "required": ["prompt"]
        }),
        "local_shell" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["exec"]},
                "command": {"type": "array", "items": {"type": "string"}},
                "env": {"type": "object", "additionalProperties": {"type": "string"}},
                "timeout_ms": {"type": "integer"},
                "user": {"type": "string"},
                "working_directory": {"type": "string"}
            },
            "required": ["type", "command"]
        }),
        "shell" => json!({
            "type": "object",
            "properties": {
                "commands": {"type": "array", "items": {"type": "string"}},
                "timeout_ms": {"type": "integer"},
                "max_output_length": {"type": "integer"}
            },
            "required": ["commands"]
        }),
        "apply_patch" => json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "enum": ["create_file", "update_file", "delete_file"]},
                "path": {"type": "string"},
                "diff": {"type": "string"}
            },
            "required": ["type", "path"]
        }),
        "tool_search" => json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string"}
            },
            "required": ["goal"]
        }),
        "custom" => {
            // Custom tools: use parameters if present, otherwise minimal schema.
            let tool_name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("custom");
            if let Some(params) = tool.get("parameters") {
                return Some(json!({
                    "type": "custom",
                    "name": tool_name,
                    "input_schema": params
                }));
            } else if tool.get("format").is_some() {
                // Freeform/grammar tool — provide minimal schema accepting raw string.
                return Some(json!({
                    "type": "custom",
                    "name": tool_name,
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "input": {"type": "string"}
                        },
                        "required": ["input"]
                    }
                }));
            } else {
                return None;
            }
        }
        _ => {
            tracing::warn!(tool_type = tool_type, "dropping unknown built-in tool type");
            return None;
        }
    };

    // Normalize the tool name for computer_use_preview → computer_use.
    let anth_name = match name.as_str() {
        "computer_use_preview" => "computer_use",
        "computer" => "computer_use",
        "web_search_preview" => "web_search",
        other => other,
    };

    Some(json!({
        "type": "custom",
        "name": anth_name,
        "input_schema": schema
    }))
}

/// Convert `tool_choice` from Responses API to Anthropic format.
///
/// Responses API sends bare strings ("auto", "required", "none", or a tool name).
/// Anthropic requires structured objects.
///
/// `parallel_tool_calls: false` → `disable_parallel_tool_use: true` inside the tool_choice object.
fn convert_tool_choice(tool_choice: Option<&Value>, parallel_tool_calls: Option<&Value>) -> Value {
    let disable_parallel = parallel_tool_calls
        .and_then(|v| v.as_bool())
        .map(|b| !b)  // Invert: false → disable=true.
        .unwrap_or(false);

    let mut choice = match tool_choice {
        None => {
            // Default to "auto" when absent.
            json!({"type": "auto"})
        }
        Some(v) if v.is_string() => {
            match v.as_str().unwrap() {
                "auto" => json!({"type": "auto"}),
                "required" => json!({"type": "any"}),
                "none" => json!({"type": "none"}),
                tool_name => json!({"type": "tool", "name": tool_name}),
            }
        }
        Some(v) if v.is_object() => {
            let obj = v.as_object().unwrap();
            match obj.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "function" => {
                    let name = obj.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    json!({"type": "tool", "name": name})
                }
                other => {
                    // Passthrough for unknown object forms.
                    json!({"type": other})
                }
            }
        }
        _ => json!({"type": "auto"}),
    };

    // Embed disable_parallel_tool_use unless tool_choice is "none" (no tools to parallelize).
    let tc_type = choice.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if disable_parallel && tc_type != "none" {
        choice["disable_parallel_tool_use"] = json!(true);
    }

    choice
}

/// Convert the `input` array to Anthropic `messages` array.
///
/// Key behaviors:
/// - System messages are extracted to the `system` parameter (done in build_system).
/// - Consecutive same-role items are merged (alternation enforcement).
/// - function_call → assistant tool_use, function_call_output → user tool_result.
/// - reasoning → assistant thinking/redacted_thinking.
/// - compaction/compaction_trigger/context_compaction → dropped.
/// - Unknown items → dropped with warning log.
fn convert_input_items(
    task: &mut ConversionTask,
    input: Option<&Value>,
) -> Result<Value, RequestConversionError> {
    let input_arr = match input.and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Ok(json!([])),
    };

    let mut messages: Vec<Value> = Vec::new();

    for item in input_arr {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Skip system messages (handled by build_system).
        if item_type == "message" {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "system" {
                continue;
            }
        }

        // Determine the role and content blocks for this input item.
        let (role, content_blocks) = convert_single_input_item(task, item)?;

        // Skip items that produce no content (dropped types).
        let role = match role {
            Some(r) => r,
            None => continue,
        };

        // Merge with previous message if same role (alternation enforcement).
        if let Some(last) = messages.last_mut() {
            let last_role = last.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if last_role == role {
                if let Some(arr) = last.get_mut("content").and_then(|v| v.as_array_mut()) {
                    arr.extend(content_blocks);
                }
                continue;
            }
        }

        messages.push(json!({
            "role": role,
            "content": content_blocks,
        }));
    }

    Ok(Value::Array(messages))
}

/// Convert a single input item into (role, content_blocks).
///
/// Returns (None, []) for items that should be dropped.
fn convert_single_input_item(
    task: &mut ConversionTask,
    item: &Value,
) -> Result<(Option<String>, Vec<Value>), RequestConversionError> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match item_type {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let content = item.get("content").cloned().unwrap_or(json!([]));
            let blocks = convert_message_content(&content);
            Ok((Some(role), blocks))
        }
        "function_call" | "custom_tool_call" => {
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            // Parse arguments from JSON string.
            let raw_args = match item_type {
                "function_call" => item.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}"),
                "custom_tool_call" => item.get("input").and_then(|v| v.as_str()).unwrap_or("{}"),
                _ => "{}",
            };
            let parsed_input: Value = serde_json::from_str(raw_args).map_err(|e| {
                RequestConversionError::InvalidRequest(format!(
                    "failed to parse arguments for call_id '{}': {}",
                    call_id, e
                ))
            })?;

            // Register in ID map.
            let toolu_id = task.id_map.insert_call(call_id.clone());

            let tool_use = json!({
                "type": "tool_use",
                "id": toolu_id,
                "name": name,
                "input": parsed_input,
            });

            Ok((Some("assistant".to_string()), vec![tool_use]))
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();

            // Get output value.
            let output = match item_type {
                "function_call_output" => item.get("output").cloned().unwrap_or(json!("")),
                "custom_tool_call_output" => item.get("output").cloned().unwrap_or(json!("")),
                _ => json!(""),
            };

            // Look up toolu_id from the ID map.
            let toolu_id = task.id_map.get_toolu_for_call(&call_id)
                .cloned()
                .unwrap_or_else(|| {
                    // If the call_id was not registered (e.g., function_call was in a previous
                    // turn), generate a new toolu_id for it.
                    tracing::warn!(call_id = %call_id, "function_call_output references unregistered call_id, generating new toolu_id");
                    let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                    task.id_map.insert_with_toolu(call_id.clone(), id.clone());
                    id
                });

            let content = content::convert_tool_result_content(&output);

            let tool_result = json!({
                "type": "tool_result",
                "tool_use_id": toolu_id,
                "content": content,
            });

            Ok((Some("user".to_string()), vec![tool_result]))
        }
        "mcp_tool_call_output" => {
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let output_obj = item.get("output").cloned().unwrap_or(json!({}));

            let toolu_id = task.id_map.get_toolu_for_call(&call_id)
                .cloned()
                .unwrap_or_else(|| {
                    tracing::warn!(call_id = %call_id, "mcp_tool_call_output references unregistered call_id");
                    let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                    task.id_map.insert_with_toolu(call_id.clone(), id.clone());
                    id
                });

            // Serialize the CallToolResult to a JSON string for tool_result.content.
            let content_str = serde_json::to_string(&output_obj).unwrap_or_default();

            let mut tool_result = json!({
                "type": "tool_result",
                "tool_use_id": toolu_id,
                "content": content_str,
            });

            // Check isError from CallToolResult.
            if let Some(is_error) = output_obj.get("isError").and_then(|v| v.as_bool()) {
                if is_error {
                    tool_result["is_error"] = json!(true);
                }
                // When false or absent, omit is_error entirely.
            }

            Ok((Some("user".to_string()), vec![tool_result]))
        }
        "reasoning" => {
            let reasoning_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let summary = item.get("summary")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let encrypted_content = item.get("encrypted_content").and_then(|v| v.as_str());

            // The `content` field (raw reasoning text) is dropped — only summary forwarded.
            let block = thinking::convert_reasoning_input(
                &summary,
                encrypted_content,
                &task.signature_cache,
                &reasoning_id,
            );

            Ok((Some("assistant".to_string()), vec![block]))
        }
        // Dropped items.
        "compaction" | "context_compaction" | "compaction_trigger" => {
            tracing::debug!(item_type = item_type, "dropping compaction-related item");
            Ok((None, vec![]))
        }
        "tool_search_output" => {
            tracing::debug!("dropping tool_search_output (no Anthropic equivalent)");
            Ok((None, vec![]))
        }
        _ => {
            tracing::warn!(item_type = item_type, "dropping unknown input item type");
            Ok((None, vec![]))
        }
    }
}

/// Convert message content from Responses API format to Anthropic content blocks.
fn convert_message_content(content: &Value) -> Vec<Value> {
    if let Some(arr) = content.as_array() {
        content::convert_user_content_array(arr)
    } else if let Some(text) = content.as_str() {
        vec![json!({"type": "text", "text": text})]
    } else {
        vec![]
    }
}

/// Error type for request conversion failures.
#[derive(Debug)]
pub enum RequestConversionError {
    InvalidRequest(String),
}

impl std::fmt::Display for RequestConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestConversionError::InvalidRequest(msg) => write!(f, "invalid request: {}", msg),
        }
    }
}

impl std::error::Error for RequestConversionError {}
```

### Step 9.5 — Verify tests pass

- [ ] Run `cargo test --lib request`. All tests pass.
- [ ] Run `cargo test --lib`. All library tests pass.

### Step 9.6 — Commit

```
feat: implement full request conversion (top-level params, input items, tool_choice, parallel_tool_calls, reasoning, tools, namespaces)
```

---

## Task Summary

| Task | Files | Key Deliverable |
|------|-------|-----------------|
| 1 | `Cargo.toml`, `src/lib.rs`, stub files | Project builds with all dependencies |
| 2 | `src/config.rs` | Three-way config injection with Option semantics |
| 3 | `src/main.rs`, `src/logging.rs` | CLI with clap, tracing + async appender |
| 4 | `src/server.rs`, `src/router.rs`, `src/tls.rs` | axum server, URL routing, TLS |
| 5 | `src/conversion/mod.rs` | ConversionTask state machine struct |
| 6 | `src/conversion/id_map.rs` | Bidirectional call_id <-> toolu_id map |
| 7 | `src/conversion/namespace.rs` | MCP namespace flattening + truncation+hash |
| 8 | `src/conversion/content.rs` | Content block conversion (text, image, cache_control) |
| 9 | `src/conversion/request.rs`, `src/conversion/thinking.rs` | Full request conversion with 40+ tests |

## Task 10: Anthropic SSE Parsing

**Files:** create `src/sse/anthropic.rs`, test inline

### Step 10.1 — Write failing tests for Anthropic SSE event parsing

- [ ] Add test module to `src/sse/anthropic.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_start() {
        let raw = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01ABC","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0}}}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::MessageStart { message } => {
                assert_eq!(message["id"], "msg_01ABC");
            }
            _ => panic!("expected MessageStart, got {:?}", events[0]),
        }
    }

    #[test]
    fn parse_content_block_start_text() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::ContentBlockStart { index, content_block } => {
                assert_eq!(*index, 0);
                assert_eq!(content_block["type"], "text");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_start_thinking() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStart { index, content_block } => {
                assert_eq!(*index, 1);
                assert_eq!(content_block["type"], "thinking");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_start_tool_use() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01ABC","name":"exec_command","input":{}}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStart { index, content_block } => {
                assert_eq!(*index, 2);
                assert_eq!(content_block["type"], "tool_use");
                assert_eq!(content_block["id"], "toolu_01ABC");
                assert_eq!(content_block["name"], "exec_command");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_start_redacted_thinking() {
        let raw = r#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"ENCRYPTED"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStart { index, content_block } => {
                assert_eq!(*index, 1);
                assert_eq!(content_block["type"], "redacted_thinking");
                assert_eq!(content_block["data"], "ENCRYPTED");
            }
            _ => panic!("expected ContentBlockStart"),
        }
    }

    #[test]
    fn parse_content_block_delta_text() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 0);
                assert_eq!(delta["type"], "text_delta");
                assert_eq!(delta["text"], "Hello");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_delta_thinking() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Let me analyze..."}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 1);
                assert_eq!(delta["type"], "thinking_delta");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_delta_signature() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"ErUB..."}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 1);
                assert_eq!(delta["type"], "signature_delta");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_delta_input_json() {
        let raw = r#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"key\":"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(*index, 2);
                assert_eq!(delta["type"], "input_json_delta");
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn parse_content_block_stop() {
        let raw = r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::ContentBlockStop { index } => {
                assert_eq!(*index, 0);
            }
            _ => panic!("expected ContentBlockStop"),
        }
    }

    #[test]
    fn parse_message_delta() {
        let raw = r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta["stop_reason"], "end_turn");
                assert_eq!(usage["output_tokens"], 15);
            }
            _ => panic!("expected MessageDelta"),
        }
    }

    #[test]
    fn parse_message_delta_max_tokens() {
        let raw = r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":4096}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta["stop_reason"], "max_tokens");
            }
            _ => panic!("expected MessageDelta"),
        }
    }

    #[test]
    fn parse_message_stop() {
        let raw = r#"event: message_stop
data: {"type":"message_stop"}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::MessageStop => {}
            _ => panic!("expected MessageStop"),
        }
    }

    #[test]
    fn parse_error_event() {
        let raw = r#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::Error { error } => {
                assert_eq!(error["type"], "overloaded_error");
                assert_eq!(error["message"], "Overloaded");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn parse_ping_consumed() {
        let raw = r#"event: ping
data: {"type":"ping"}"#;
        let events = parse_sse_events(raw);
        match &events[0] {
            AnthropicEvent::Ping => {}
            _ => panic!("expected Ping"),
        }
    }

    #[test]
    fn parse_done_marker() {
        let raw = "data: [DONE]";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::Done => {}
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn parse_multiple_events() {
        let raw = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 7);
        assert!(matches!(&events[0], AnthropicEvent::MessageStart { .. }));
        assert!(matches!(&events[1], AnthropicEvent::ContentBlockStart { .. }));
        assert!(matches!(&events[2], AnthropicEvent::ContentBlockDelta { .. }));
        assert!(matches!(&events[3], AnthropicEvent::ContentBlockStop { .. }));
        assert!(matches!(&events[4], AnthropicEvent::MessageDelta { .. }));
        assert!(matches!(&events[5], AnthropicEvent::MessageStop));
        assert!(matches!(&events[6], AnthropicEvent::Done));
    }

    #[test]
    fn malformed_data_skipped_with_warning() {
        let raw = r#"event: message_start
data: {bad json}

event: message_stop
data: {"type":"message_stop"}"#;
        let events = parse_sse_events(raw);
        // The malformed event should produce an Unknown variant, message_stop still parsed.
        assert!(events.len() >= 1);
        assert!(matches!(&events[events.len() - 1], AnthropicEvent::MessageStop));
    }

    #[test]
    fn empty_lines_between_events_ok() {
        let raw = r#"event: ping
data: {"type":"ping"}

event: message_stop
data: {"type":"message_stop"}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn unknown_event_type_becomes_unknown() {
        let raw = r#"event: future_event_type
data: {"type":"future_event_type","data":"something"}"#;
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AnthropicEvent::Unknown { event_type, data } => {
                assert_eq!(event_type, "future_event_type");
                assert_eq!(data["type"], "future_event_type");
            }
            _ => panic!("expected Unknown"),
        }
    }
}
```

### Step 10.2 — Verify tests fail

- [ ] Run `cargo test --lib sse::anthropic`. Compilation errors expected.

### Step 10.3 — Implement Anthropic SSE parsing

- [ ] Implement `src/sse/anthropic.rs`.

```rust
use serde_json::Value;

/// Parsed Anthropic SSE event.
#[derive(Debug)]
pub enum AnthropicEvent {
    /// `message_start` — contains the initial message object.
    MessageStart {
        message: Value,
    },
    /// `content_block_start` — a new content block begins.
    ContentBlockStart {
        index: usize,
        content_block: Value,
    },
    /// `content_block_delta` — incremental content for current block.
    ContentBlockDelta {
        index: usize,
        delta: Value,
    },
    /// `content_block_stop` — current content block ended.
    ContentBlockStop {
        index: usize,
    },
    /// `message_delta` — stop_reason and output token usage.
    MessageDelta {
        delta: Value,
        usage: Value,
    },
    /// `message_stop` — the message is complete.
    MessageStop,
    /// `error` — an error occurred during streaming.
    Error {
        error: Value,
    },
    /// `ping` — keep-alive, consumed silently.
    Ping,
    /// `[DONE]` — terminal marker.
    Done,
    /// Unknown event type — preserved for forward compatibility.
    Unknown {
        event_type: String,
        data: Value,
    },
}

/// Parse a raw SSE text stream into a list of AnthropicEvent.
///
/// Handles:
/// - `event: <type>\ndata: <json>\n` pairs
/// - `data: [DONE]` terminal marker
/// - Empty lines between events
/// - Malformed data lines (logged as warnings, returned as Unknown)
pub fn parse_sse_events(raw: &str) -> Vec<AnthropicEvent> {
    let mut events = Vec::new();
    let mut current_event_type: Option<String> = None;
    let mut current_data: Option<String> = None;

    for line in raw.lines() {
        if let Some(ev) = line.strip_prefix("event: ") {
            current_event_type = Some(ev.trim().to_string());
        } else if let Some(data) = line.strip_prefix("data: ") {
            let data_str = data.trim();

            // Check for terminal [DONE] marker.
            if data_str == "[DONE]" {
                events.push(AnthropicEvent::Done);
                current_event_type = None;
                current_data = None;
                continue;
            }

            current_data = Some(data_str.to_string());
        } else if line.is_empty() {
            // Empty line = end of current event. Emit if we have data.
            if let Some(data_str) = current_data.take() {
                let event_type = current_event_type.take();
                if let Some(event) = build_event(event_type.as_deref(), &data_str) {
                    events.push(event);
                }
            }
        }
        // Other lines (comments, etc.) are ignored.
    }

    // Handle final event if stream doesn't end with empty line.
    if let Some(data_str) = current_data.take() {
        let event_type = current_event_type.take();
        if let Some(event) = build_event(event_type.as_deref(), &data_str) {
            events.push(event);
        }
    }

    events
}

/// Build an AnthropicEvent from the event type and data string.
fn build_event(event_type: Option<&str>, data: &str) -> Option<AnthropicEvent> {
    let parsed: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(data = %data, error = %e, "malformed SSE data, skipping");
            // Return a minimal Unknown so the caller can count events.
            return Some(AnthropicEvent::Unknown {
                event_type: event_type.unwrap_or("unknown").to_string(),
                data: Value::String(data.to_string()),
            });
        }
    };

    // Use the `type` field from the parsed JSON as the primary discriminator.
    // Fall back to the SSE event: line if the JSON lacks a type field.
    let type_str = parsed.get("type").and_then(|v| v.as_str())
        .unwrap_or_else(|| event_type.unwrap_or("unknown"));

    match type_str {
        "message_start" => {
            let message = parsed.get("message").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::MessageStart { message })
        }
        "content_block_start" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let content_block = parsed.get("content_block").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::ContentBlockStart { index, content_block })
        }
        "content_block_delta" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta = parsed.get("delta").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::ContentBlockDelta { index, delta })
        }
        "content_block_stop" => {
            let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            Some(AnthropicEvent::ContentBlockStop { index })
        }
        "message_delta" => {
            let delta = parsed.get("delta").cloned().unwrap_or(Value::Null);
            let usage = parsed.get("usage").cloned().unwrap_or(Value::Null);
            Some(AnthropicEvent::MessageDelta { delta, usage })
        }
        "message_stop" => Some(AnthropicEvent::MessageStop),
        "error" => {
            let error = parsed.get("error").cloned().unwrap_or(parsed.clone());
            Some(AnthropicEvent::Error { error })
        }
        "ping" => Some(AnthropicEvent::Ping),
        _ => {
            tracing::debug!(event_type = type_str, "unknown Anthropic SSE event type");
            Some(AnthropicEvent::Unknown {
                event_type: type_str.to_string(),
                data: parsed,
            })
        }
    }
}
```

### Step 10.4 — Wire into `src/sse/mod.rs`

- [ ] Update `src/sse/mod.rs`.

```rust
pub mod anthropic;
pub mod responses;

pub use anthropic::AnthropicEvent;
```

### Step 10.5 — Verify tests pass

- [ ] Run `cargo test --lib sse::anthropic`. All 20+ tests pass.

### Step 10.6 — Commit

```
feat: implement Anthropic SSE event parsing with all event types
```

---

## Task 11: Responses API SSE Generation

**Files:** create `src/sse/responses.rs`, test inline

### Step 11.1 — Write failing tests for Responses API SSE event generation

- [ ] Add test module to `src/sse/responses.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_created_event() {
        let evt = ResponsesEvent::ResponseCreated {
            response_id: "msg_01ABC".to_string(),
            created_at: 1717000000,
            model: "claude-sonnet-4-20250514".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.created");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["type"], "response.created");
        assert_eq!(parsed["response"]["id"], "msg_01ABC");
        assert_eq!(parsed["response"]["object"], "response");
        assert_eq!(parsed["response"]["created_at"], 1717000000);
        assert_eq!(parsed["response"]["model"], "claude-sonnet-4-20250514");
        assert_eq!(parsed["response"]["status"], "in_progress");
        assert!(parsed.get("sequence_number").is_none(), "NEVER include sequence_number");
    }

    #[test]
    fn output_item_added_message() {
        let evt = ResponsesEvent::OutputItemAdded {
            output_index: 0,
            item: json!({"type": "message", "role": "assistant", "status": "in_progress", "content": []}),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.output_item.added");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 0);
        assert_eq!(parsed["item"]["type"], "message");
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn content_part_added() {
        let evt = ResponsesEvent::ContentPartAdded {
            output_index: 0,
            content_index: 0,
            part: json!({"type": "output_text", "text": "", "annotations": []}),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.content_part.added");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 0);
        assert_eq!(parsed["content_index"], 0);
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn output_text_delta() {
        let evt = ResponsesEvent::OutputTextDelta {
            output_index: 0,
            content_index: 0,
            delta: "Hello".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.output_text.delta");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["delta"], "Hello");
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn output_text_done() {
        let evt = ResponsesEvent::OutputTextDone {
            output_index: 0,
            content_index: 0,
            text: "Hello world".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.output_text.done");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["text"], "Hello world");
    }

    #[test]
    fn content_part_done() {
        let evt = ResponsesEvent::ContentPartDone {
            output_index: 0,
            content_index: 0,
            part: json!({"type": "output_text", "text": "Hello world", "annotations": []}),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.content_part.done");
    }

    #[test]
    fn output_item_done() {
        let evt = ResponsesEvent::OutputItemDone {
            output_index: 0,
            item: json!({"type": "message", "role": "assistant", "status": "completed", "content": []}),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.output_item.done");
    }

    #[test]
    fn reasoning_summary_part_added() {
        let evt = ResponsesEvent::ReasoningSummaryPartAdded {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_part.added");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["output_index"], 1);
        assert_eq!(parsed["item_id"], "rs_001");
        assert_eq!(parsed["summary_index"], 0);
        assert_eq!(parsed["part"]["type"], "summary_text");
    }

    #[test]
    fn reasoning_summary_text_delta() {
        let evt = ResponsesEvent::ReasoningSummaryTextDelta {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
            delta: "Let me think...".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_text.delta");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["delta"], "Let me think...");
    }

    #[test]
    fn reasoning_summary_text_done() {
        let evt = ResponsesEvent::ReasoningSummaryTextDone {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
            text: "Full summary text".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_text.done");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["text"], "Full summary text");
    }

    #[test]
    fn reasoning_summary_part_done() {
        let evt = ResponsesEvent::ReasoningSummaryPartDone {
            output_index: 1,
            item_id: "rs_001".to_string(),
            summary_index: 0,
            part: json!({"type": "summary_text", "text": "Full summary text"}),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.reasoning_summary_part.done");
    }

    #[test]
    fn function_call_arguments_delta() {
        let evt = ResponsesEvent::FunctionCallArgumentsDelta {
            output_index: 2,
            item_id: "fc_002".to_string(),
            delta: "{\"key\":".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.function_call_arguments.delta");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["delta"], "{\"key\":");
    }

    #[test]
    fn function_call_arguments_done() {
        let evt = ResponsesEvent::FunctionCallArgumentsDone {
            output_index: 2,
            item_id: "fc_002".to_string(),
            arguments: "{\"key\":\"value\"}".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.function_call_arguments.done");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["arguments"], "{\"key\":\"value\"}");
    }

    #[test]
    fn response_completed_never_incomplete() {
        let response_obj = json!({
            "id": "msg_01",
            "object": "response",
            "status": "completed",
            "output": [],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
        });
        let evt = ResponsesEvent::ResponseCompleted {
            response: response_obj,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.completed", "NEVER emit response.incomplete");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["type"], "response.completed");
        assert!(parsed.get("sequence_number").is_none());
    }

    #[test]
    fn response_completed_with_incomplete_details() {
        let response_obj = json!({
            "id": "msg_01",
            "status": "completed",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
            "usage": {"input_tokens": 100, "output_tokens": 4096, "total_tokens": 4196}
        });
        let evt = ResponsesEvent::ResponseCompleted {
            response: response_obj,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.completed");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["status"], "completed");
        assert_eq!(parsed["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn response_failed_carries_full_response() {
        let response_obj = json!({
            "id": "msg_01",
            "object": "response",
            "created_at": 1717000000,
            "status": "failed",
            "error": {"code": "rate_limit_exceeded", "message": "Too many requests"},
            "output": [],
            "usage": null,
            "metadata": {}
        });
        let evt = ResponsesEvent::ResponseFailed {
            response: response_obj,
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "response.failed");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["status"], "failed");
        assert_eq!(parsed["response"]["error"]["code"], "rate_limit_exceeded");
    }

    #[test]
    fn error_event() {
        let evt = ResponsesEvent::Error {
            code: "context_length_exceeded".to_string(),
            message: "prompt is too long".to_string(),
        };
        let (event_type, data) = evt.to_sse();
        assert_eq!(event_type, "error");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["code"], "context_length_exceeded");
        assert_eq!(parsed["message"], "prompt is too long");
    }

    #[test]
    fn format_sse_line() {
        let line = format_sse("response.output_text.delta", r#"{"type":"response.output_text.delta","delta":"hi"}"#);
        assert_eq!(line, "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n");
    }

    #[test]
    fn format_done_line() {
        let line = format_done();
        assert_eq!(line, "data: [DONE]\n\n");
    }

    #[test]
    fn no_sequence_number_in_any_event() {
        // Verify that every ResponsesEvent variant omits sequence_number.
        let events = vec![
            ResponsesEvent::ResponseCreated {
                response_id: "msg_01".into(),
                created_at: 0,
                model: "m".into(),
            },
            ResponsesEvent::OutputItemAdded {
                output_index: 0,
                item: json!({}),
            },
            ResponsesEvent::ContentPartAdded {
                output_index: 0,
                content_index: 0,
                part: json!({}),
            },
            ResponsesEvent::OutputTextDelta {
                output_index: 0,
                content_index: 0,
                delta: "x".into(),
            },
            ResponsesEvent::OutputTextDone {
                output_index: 0,
                content_index: 0,
                text: "x".into(),
            },
            ResponsesEvent::ContentPartDone {
                output_index: 0,
                content_index: 0,
                part: json!({}),
            },
            ResponsesEvent::OutputItemDone {
                output_index: 0,
                item: json!({}),
            },
            ResponsesEvent::ReasoningSummaryPartAdded {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
            },
            ResponsesEvent::ReasoningSummaryTextDelta {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
                delta: "x".into(),
            },
            ResponsesEvent::ReasoningSummaryTextDone {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
                text: "x".into(),
            },
            ResponsesEvent::ReasoningSummaryPartDone {
                output_index: 0,
                item_id: "rs_0".into(),
                summary_index: 0,
                part: json!({}),
            },
            ResponsesEvent::FunctionCallArgumentsDelta {
                output_index: 0,
                item_id: "fc_0".into(),
                delta: "x".into(),
            },
            ResponsesEvent::FunctionCallArgumentsDone {
                output_index: 0,
                item_id: "fc_0".into(),
                arguments: "x".into(),
            },
            ResponsesEvent::ResponseCompleted { response: json!({}) },
            ResponsesEvent::ResponseFailed { response: json!({}) },
            ResponsesEvent::Error { code: "x".into(), message: "x".into() },
        ];
        for evt in &events {
            let (_, data) = evt.to_sse();
            let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
            assert!(parsed.get("sequence_number").is_none(),
                "Found sequence_number in {:?}: {}", evt, parsed);
        }
    }
}
```

### Step 11.2 — Verify tests fail

- [ ] Run `cargo test --lib sse::responses`. Compilation errors expected.

### Step 11.3 — Implement Responses API SSE event generation

- [ ] Implement `src/sse/responses.rs`.

```rust
use serde_json::{json, Value};

/// Responses API SSE events.
///
/// CRITICAL CONSTRAINTS:
/// - NEVER include `sequence_number` in any event (Codex ignores it, adds wire overhead).
/// - NEVER emit `response.incomplete` — always use `response.completed`.
/// - NEVER emit `custom_tool_call_input.*` events — all tools are function type.
/// - `response.failed` must carry a FULL response object with error inside.
#[derive(Debug)]
pub enum ResponsesEvent {
    // -- Lifecycle events --
    ResponseCreated {
        response_id: String,
        created_at: u64,
        model: String,
    },

    // -- Output item events --
    OutputItemAdded {
        output_index: usize,
        item: Value,
    },
    OutputItemDone {
        output_index: usize,
        item: Value,
    },

    // -- Content part events (for message output items) --
    ContentPartAdded {
        output_index: usize,
        content_index: usize,
        part: Value,
    },
    ContentPartDone {
        output_index: usize,
        content_index: usize,
        part: Value,
    },

    // -- Text delta events --
    OutputTextDelta {
        output_index: usize,
        content_index: usize,
        delta: String,
    },
    OutputTextDone {
        output_index: usize,
        content_index: usize,
        text: String,
    },

    // -- Reasoning summary events --
    ReasoningSummaryPartAdded {
        output_index: usize,
        item_id: String,
        summary_index: usize,
    },
    ReasoningSummaryTextDelta {
        output_index: usize,
        item_id: String,
        summary_index: usize,
        delta: String,
    },
    ReasoningSummaryTextDone {
        output_index: usize,
        item_id: String,
        summary_index: usize,
        text: String,
    },
    ReasoningSummaryPartDone {
        output_index: usize,
        item_id: String,
        summary_index: usize,
        part: Value,
    },

    // -- Function call events --
    FunctionCallArgumentsDelta {
        output_index: usize,
        item_id: String,
        delta: String,
    },
    FunctionCallArgumentsDone {
        output_index: usize,
        item_id: String,
        arguments: String,
    },

    // -- Terminal events --
    ResponseCompleted {
        response: Value,
    },
    ResponseFailed {
        response: Value,
    },

    // -- Error event --
    Error {
        code: String,
        message: String,
    },
}

impl ResponsesEvent {
    /// Convert to SSE wire format: (event_type, data_json_string).
    ///
    /// NO sequence_number is ever included.
    pub fn to_sse(&self) -> (String, String) {
        match self {
            ResponsesEvent::ResponseCreated { response_id, created_at, model } => {
                let data = json!({
                    "type": "response.created",
                    "response": {
                        "id": response_id,
                        "object": "response",
                        "created_at": created_at,
                        "model": model,
                        "status": "in_progress",
                        "output": [],
                    }
                });
                ("response.created".to_string(), data.to_string())
            }

            ResponsesEvent::OutputItemAdded { output_index, item } => {
                let data = json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": item,
                });
                ("response.output_item.added".to_string(), data.to_string())
            }

            ResponsesEvent::OutputItemDone { output_index, item } => {
                let data = json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item,
                });
                ("response.output_item.done".to_string(), data.to_string())
            }

            ResponsesEvent::ContentPartAdded { output_index, content_index, part } => {
                let data = json!({
                    "type": "response.content_part.added",
                    "output_index": output_index,
                    "content_index": content_index,
                    "part": part,
                });
                ("response.content_part.added".to_string(), data.to_string())
            }

            ResponsesEvent::ContentPartDone { output_index, content_index, part } => {
                let data = json!({
                    "type": "response.content_part.done",
                    "output_index": output_index,
                    "content_index": content_index,
                    "part": part,
                });
                ("response.content_part.done".to_string(), data.to_string())
            }

            ResponsesEvent::OutputTextDelta { output_index, content_index, delta } => {
                let data = json!({
                    "type": "response.output_text.delta",
                    "output_index": output_index,
                    "content_index": content_index,
                    "delta": delta,
                });
                ("response.output_text.delta".to_string(), data.to_string())
            }

            ResponsesEvent::OutputTextDone { output_index, content_index, text } => {
                let data = json!({
                    "type": "response.output_text.done",
                    "output_index": output_index,
                    "content_index": content_index,
                    "text": text,
                });
                ("response.output_text.done".to_string(), data.to_string())
            }

            ResponsesEvent::ReasoningSummaryPartAdded { output_index, item_id, summary_index } => {
                let data = json!({
                    "type": "response.reasoning_summary_part.added",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "part": {"type": "summary_text", "text": ""},
                });
                ("response.reasoning_summary_part.added".to_string(), data.to_string())
            }

            ResponsesEvent::ReasoningSummaryTextDelta { output_index, item_id, summary_index, delta } => {
                let data = json!({
                    "type": "response.reasoning_summary_text.delta",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "delta": delta,
                });
                ("response.reasoning_summary_text.delta".to_string(), data.to_string())
            }

            ResponsesEvent::ReasoningSummaryTextDone { output_index, item_id, summary_index, text } => {
                let data = json!({
                    "type": "response.reasoning_summary_text.done",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "text": text,
                });
                ("response.reasoning_summary_text.done".to_string(), data.to_string())
            }

            ResponsesEvent::ReasoningSummaryPartDone { output_index, item_id, summary_index, part } => {
                let data = json!({
                    "type": "response.reasoning_summary_part.done",
                    "output_index": output_index,
                    "item_id": item_id,
                    "summary_index": summary_index,
                    "part": part,
                });
                ("response.reasoning_summary_part.done".to_string(), data.to_string())
            }

            ResponsesEvent::FunctionCallArgumentsDelta { output_index, item_id, delta } => {
                let data = json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": output_index,
                    "item_id": item_id,
                    "delta": delta,
                });
                ("response.function_call_arguments.delta".to_string(), data.to_string())
            }

            ResponsesEvent::FunctionCallArgumentsDone { output_index, item_id, arguments } => {
                let data = json!({
                    "type": "response.function_call_arguments.done",
                    "output_index": output_index,
                    "item_id": item_id,
                    "arguments": arguments,
                });
                ("response.function_call_arguments.done".to_string(), data.to_string())
            }

            // CRITICAL: Always response.completed, NEVER response.incomplete.
            ResponsesEvent::ResponseCompleted { response } => {
                let mut resp = response.clone();
                if let Some(obj) = resp.as_object_mut() {
                    obj.insert("type".to_string(), json!("response.completed"));
                }
                ("response.completed".to_string(), resp.to_string())
            }

            // CRITICAL: response.failed carries FULL response object with error inside.
            ResponsesEvent::ResponseFailed { response } => {
                let mut resp = response.clone();
                if let Some(obj) = resp.as_object_mut() {
                    obj.insert("type".to_string(), json!("response.failed"));
                }
                ("response.failed".to_string(), resp.to_string())
            }

            ResponsesEvent::Error { code, message } => {
                let data = json!({
                    "type": "error",
                    "code": code,
                    "message": message,
                });
                ("error".to_string(), data.to_string())
            }
        }
    }
}

/// Format an SSE event as a wire-level string: `event: ...\ndata: ...\n\n`.
pub fn format_sse(event_type: &str, data: &str) -> String {
    format!("event: {}\ndata: {}\n\n", event_type, data)
}

/// Format the [DONE] terminal marker.
pub fn format_done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// Format a ResponsesEvent into wire-level SSE string.
pub fn format_responses_event(event: &ResponsesEvent) -> String {
    let (event_type, data) = event.to_sse();
    format_sse(&event_type, &data)
}
```

### Step 11.4 — Verify tests pass

- [ ] Run `cargo test --lib sse::responses`. All 21+ tests pass.

### Step 11.5 — Commit

```
feat: implement Responses API SSE event generation with all event types, no sequence_number
```

---

## Task 12: Content Block to Output Item (Response Direction)

**Files:** modify `src/conversion/content.rs`, add response-direction conversion

### Step 12.1 — Write failing tests for content block → output item

- [ ] Add tests to `src/conversion/content.rs`.

```rust
    // --- Response direction tests ---

    #[test]
    fn text_to_message_output_item() {
        let block = json!({"type": "text", "text": "Hello world"});
        let (item, _reasoning_id) = convert_content_block_to_output(&block, &NamespaceRegistry::new(), 0, &mut 0);
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["content"][0]["type"], "output_text");
        assert_eq!(item["content"][0]["text"], "Hello world");
        assert_eq!(item["content"][0]["annotations"], json!([]));
    }

    #[test]
    fn thinking_to_reasoning_output_item() {
        let block = json!({"type": "thinking", "thinking": "Let me analyze..."});
        let mut fc_counter = 0u64;
        let (item, reasoning_id) = convert_content_block_to_output(&block, &NamespaceRegistry::new(), 1, &mut fc_counter);
        assert_eq!(item["type"], "reasoning");
        assert!(reasoning_id.starts_with("rs_"), "reasoning ID should start with rs_, got {}", reasoning_id);
        assert_eq!(item["id"], reasoning_id);
        assert_eq!(item["summary"][0]["type"], "summary_text");
        assert_eq!(item["summary"][0]["text"], "Let me analyze...");
        assert!(item.get("encrypted_content").is_none(), "encrypted_content should be absent");
        assert!(item.get("content").is_none(), "content field should be absent");
    }

    #[test]
    fn redacted_thinking_to_reasoning_with_encrypted_content() {
        let block = json!({"type": "redacted_thinking", "data": "ENCRYPTED_BLOB"});
        let mut fc_counter = 0u64;
        let (item, reasoning_id) = convert_content_block_to_output(&block, &NamespaceRegistry::new(), 2, &mut fc_counter);
        assert_eq!(item["type"], "reasoning");
        assert!(reasoning_id.starts_with("rs_"));
        assert_eq!(item["encrypted_content"], "ENCRYPTED_BLOB");
        assert_eq!(item["summary"], json!([]));
    }

    #[test]
    fn tool_use_to_function_call_no_namespace() {
        let mut reg = NamespaceRegistry::new();
        // No registration — tool not in namespace registry.
        let block = json!({"type": "tool_use", "id": "toolu_01ABC", "name": "exec_command", "input": {"command": "ls"}});
        let mut fc_counter = 0u64;
        let (item, _) = convert_content_block_to_output(&block, &reg, 3, &mut fc_counter);
        assert_eq!(item["type"], "function_call");
        assert!(item["id"].as_str().unwrap().starts_with("fc_"), "function call ID should start with fc_");
        assert!(item["call_id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(item["name"], "exec_command");
        assert_eq!(item["arguments"], "{\"command\":\"ls\"}");
        assert!(item.get("namespace").is_none(), "no namespace when not in registry");
        assert_eq!(item["status"], "completed");
        assert_eq!(fc_counter, 1);
    }

    #[test]
    fn tool_use_to_function_call_with_namespace() {
        let mut reg = NamespaceRegistry::new();
        reg.register("mcp__memory__search".to_string(), "mcp__memory__".to_string(), "search".to_string());
        let block = json!({"type": "tool_use", "id": "toolu_01ABC", "name": "mcp__memory__search", "input": {"query": "test"}});
        let mut fc_counter = 0u64;
        let (item, _) = convert_content_block_to_output(&block, &reg, 4, &mut fc_counter);
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["name"], "search");
        assert_eq!(item["namespace"], "mcp__memory__");
        assert_eq!(item["arguments"], "{\"query\":\"test\"}");
    }

    #[test]
    fn fc_counter_increments_per_tool_use() {
        let reg = NamespaceRegistry::new();
        let block1 = json!({"type": "tool_use", "id": "toolu_01", "name": "a", "input": {}});
        let block2 = json!({"type": "tool_use", "id": "toolu_02", "name": "b", "input": {}});
        let mut fc_counter = 0u64;
        let (item1, _) = convert_content_block_to_output(&block1, &reg, 0, &mut fc_counter);
        let (item2, _) = convert_content_block_to_output(&block2, &reg, 1, &mut fc_counter);
        assert_eq!(fc_counter, 2);
        assert_eq!(item1["id"], "fc_0");
        assert_eq!(item2["id"], "fc_1");
    }

    #[test]
    fn rs_ids_are_unique_uuids() {
        let block1 = json!({"type": "thinking", "thinking": "a"});
        let block2 = json!({"type": "thinking", "thinking": "b"});
        let mut fc_counter = 0u64;
        let (_, id1) = convert_content_block_to_output(&block1, &NamespaceRegistry::new(), 0, &mut fc_counter);
        let (_, id2) = convert_content_block_to_output(&block2, &NamespaceRegistry::new(), 1, &mut fc_counter);
        assert_ne!(id1, id2, "reasoning IDs must be unique");
    }
```

### Step 12.2 — Verify tests fail

- [ ] Run `cargo test --lib content`. Compilation errors for new tests.

### Step 12.3 — Implement response-direction conversion

- [ ] Add to `src/conversion/content.rs`.

```rust
use crate::conversion::namespace::NamespaceRegistry;
use uuid::Uuid;

/// Convert an Anthropic content block to a Responses API output item.
///
/// Returns (output_item, reasoning_id).
/// `reasoning_id` is Some only for thinking/redacted_thinking blocks.
/// `fc_counter` is incremented for each tool_use block to generate sequential fc_N IDs.
///
/// ID generation:
/// - reasoning: `rs_{uuid_v4}` (proxy-generated)
/// - function_call: `fc_{sequential}` with `call_{sequential}` call_id (proxy-generated)
/// - message: same ID as the response (passed in by caller)
pub fn convert_content_block_to_output(
    block: &Value,
    namespace_registry: &NamespaceRegistry,
    output_index: usize,
    fc_counter: &mut u64,
) -> (Value, String) {
    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match block_type {
        "text" => {
            let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let item = json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                }]
            });
            (item, String::new())
        }
        "thinking" => {
            let thinking_text = block.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
            let reasoning_id = format!("rs_{}", Uuid::new_v4().simple());
            let item = json!({
                "type": "reasoning",
                "id": reasoning_id,
                "summary": [{
                    "type": "summary_text",
                    "text": thinking_text,
                }],
                // encrypted_content and content fields are OMITTED (not null, not empty).
            });
            (item, reasoning_id)
        }
        "redacted_thinking" => {
            let data = block.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let reasoning_id = format!("rs_{}", Uuid::new_v4().simple());
            let item = json!({
                "type": "reasoning",
                "id": reasoning_id,
                "summary": [],
                "encrypted_content": data,
            });
            (item, reasoning_id)
        }
        "tool_use" => {
            let toolu_id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let raw_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let input = block.get("input").cloned().unwrap_or(json!({}));

            // Serialize input to JSON string for arguments field.
            let arguments = serde_json::to_string(&input).unwrap_or_default();

            // Look up namespace registry to split name.
            let fc_id = format!("fc_{}", fc_counter);
            let call_id = format!("call_{}", fc_counter);
            *fc_counter += 1;

            let mut item = json!({
                "type": "function_call",
                "id": fc_id,
                "call_id": call_id,
                "name": raw_name,
                "status": "completed",
                "arguments": arguments,
            });

            // Check namespace registry for the tool name.
            if let Some(entry) = namespace_registry.lookup(raw_name) {
                item["name"] = json!(entry.tool_name);
                item["namespace"] = json!(entry.namespace);
            }
            // Registry miss → plain function_call without namespace field.

            (item, String::new())
        }
        _ => {
            tracing::warn!(block_type = block_type, "unknown Anthropic content block type in response");
            (json!({}), String::new())
        }
    }
}
```

### Step 12.4 — Verify tests pass

- [ ] Run `cargo test --lib content`. All tests pass (request + response direction).

### Step 12.5 — Commit

```
feat: implement content block to output item conversion (response direction) with ID generation
```

---

## Task 13: Streaming Pipeline

**Files:** `src/conversion/response.rs`

This is the core streaming pipeline that wires Anthropic SSE events to Responses API SSE events.

### Step 13.1 — Write failing tests for streaming pipeline

- [ ] Add test module to `src/conversion/response.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::anthropic::AnthropicEvent;
    use crate::conversion::namespace::NamespaceRegistry;
    use serde_json::json;

    fn make_state() -> StreamingState {
        StreamingState::new(
            "msg_test".to_string(),
            "claude-sonnet-4-20250514".to_string(),
            NamespaceRegistry::new(),
            1717000000,
            "auto".to_string(),
            Some("You are helpful.".to_string()),
            None,
        )
    }

    // --- Text streaming ---

    #[test]
    fn text_stream_produces_correct_events() {
        let mut state = make_state();
        let mut all_events = Vec::new();

        // message_start
        let events = state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "claude-sonnet-4-20250514", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });
        all_events.extend(events);
        // Should produce response.created + response.in_progress (or combined)
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.created"
        }), "should emit response.created");

        // content_block_start (text)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        all_events.extend(events);
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.added"
        }));

        // content_block_delta (text)
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Hello"}),
        });
        all_events.extend(events);
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_text.delta"
        }));

        // content_block_stop
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        all_events.extend(events);
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_text.done"
        }));
        assert!(all_events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }));
    }

    // --- Thinking streaming ---

    #[test]
    fn thinking_stream_produces_reasoning_events() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        // content_block_start (thinking)
        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "thinking", "thinking": ""}),
        });
        assert!(events.iter().any(|e| {
            let (t, d) = e.to_sse();
            t == "response.output_item.added" && d.contains("\"reasoning\"")
        }), "should emit reasoning output_item.added");
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_part.added"
        }));

        // thinking_delta
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "thinking_delta", "thinking": "Let me..."}),
        });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_text.delta"
        }));

        // signature_delta — should NOT produce any SSE event (consumed and accumulated)
        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "signature_delta", "signature": "ErUB..."}),
        });
        assert!(events.is_empty(), "signature_delta should produce no SSE output");

        // content_block_stop
        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_text.done"
        }));
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.reasoning_summary_part.done"
        }));
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.output_item.done"
        }));
    }

    // --- Tool use streaming ---

    #[test]
    fn tool_use_stream_produces_function_call_events() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        let events = state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "tool_use", "id": "toolu_01", "name": "exec_command", "input": {}}),
        });
        assert!(events.iter().any(|e| {
            let (t, d) = e.to_sse();
            t == "response.output_item.added" && d.contains("\"function_call\"")
        }));

        let events = state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "input_json_delta", "partial_json": "{\"cmd\":"}),
        });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.function_call_arguments.delta"
        }));

        let events = state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.function_call_arguments.done"
        }));
    }

    // --- Ping consumed ---

    #[test]
    fn ping_produces_no_output() {
        let mut state = make_state();
        let events = state.process_event(AnthropicEvent::Ping);
        assert!(events.is_empty());
    }

    // --- Completion always response.completed ---

    #[test]
    fn completion_always_response_completed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });

        let events = state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 15}),
        });
        // MessageDelta does not produce SSE events — it records internally.

        let events = state.process_event(AnthropicEvent::MessageStop);
        assert!(events.iter().any(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }), "message_stop must produce response.completed, NEVER response.incomplete");
    }

    #[test]
    fn max_tokens_still_response_completed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "partial"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });

        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "max_tokens", "stop_sequence": null}),
            usage: json!({"output_tokens": 4096}),
        });

        let events = state.process_event(AnthropicEvent::MessageStop);
        let completed_event = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).expect("must produce response.completed");
        let (_, data) = completed_event.to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["status"], "completed");
        assert_eq!(parsed["response"]["incomplete_details"]["reason"], "max_output_tokens");
    }

    // --- end_turn only when tool_use ---

    #[test]
    fn end_turn_false_only_when_tool_use() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "tool_use", "stop_sequence": null}),
            usage: json!({"output_tokens": 50}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["response"]["end_turn"], false);
    }

    #[test]
    fn end_turn_absent_when_not_tool_use() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert!(parsed["response"].get("end_turn").is_none(), "end_turn should be absent when stop_reason != tool_use");
    }

    // --- Usage mapping ---

    #[test]
    fn usage_mapping_in_completed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0, "cache_read_input_tokens": 50}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 15}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let usage = &parsed["response"]["usage"];
        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["output_tokens"], 15);
        assert_eq!(usage["total_tokens"], 115);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 50);
        assert!(usage.get("reasoning_tokens").is_none(), "reasoning_tokens should be omitted");
    }

    // --- Output item accumulator ---

    #[test]
    fn output_items_accumulated_in_completed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Hello"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 5}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let output = parsed["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 1, "output should contain 1 item");
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "Hello");
    }
}
```

### Step 13.2 — Verify tests fail

- [ ] Run `cargo test --lib response`. Compilation errors expected.

### Step 13.3 — Implement streaming pipeline

- [ ] Implement `src/conversion/response.rs`.

```rust
use crate::conversion::content::convert_content_block_to_output;
use crate::conversion::namespace::NamespaceRegistry;
use crate::sse::anthropic::AnthropicEvent;
use crate::sse::responses::{ResponsesEvent, format_responses_event, format_done};
use crate::conversion::error::{convert_error_type, map_http_status};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// In-flight state for a single streaming conversion.
pub struct StreamingState {
    /// Response ID from Anthropic message_start.
    response_id: String,
    /// Model from request (echoed back).
    model: String,
    /// Namespace registry from request phase.
    namespace_registry: NamespaceRegistry,
    /// Timestamp when response.created was emitted.
    created_at: u64,
    /// Accumulated output items for response.completed.
    output_items: Vec<Value>,
    /// Delta accumulators keyed by content block index.
    text_accumulator: String,
    thinking_accumulator: String,
    arguments_accumulator: String,
    /// Signature accumulator keyed by content block index.
    signature_accumulator: String,
    /// Current reasoning item ID for the active thinking block.
    current_reasoning_id: String,
    /// Current function call item ID for the active tool_use block.
    current_fc_item_id: String,
    /// Current function call call_id.
    current_fc_call_id: String,
    /// Function call counter for sequential ID generation.
    fc_counter: u64,
    /// Tool use ID mapping for the current request (toolu_id -> (fc_id, call_id, name)).
    tool_use_map: Vec<(String, String, String, String)>, // (toolu_id, fc_id, call_id, name)
    /// Stop reason from message_delta.
    stop_reason: Option<String>,
    /// Input tokens from message_start usage.
    input_tokens: u64,
    /// Cache read tokens from message_start usage.
    cache_read_tokens: u64,
    /// Output tokens from message_delta usage.
    output_tokens: u64,
    /// Echo-back fields from request.
    tool_choice_echo: String,
    instructions_echo: Option<String>,
    parallel_tool_calls_echo: Option<bool>,
}

impl StreamingState {
    pub fn new(
        response_id: String,
        model: String,
        namespace_registry: NamespaceRegistry,
        created_at: u64,
        tool_choice_echo: String,
        instructions_echo: Option<String>,
        parallel_tool_calls_echo: Option<bool>,
    ) -> Self {
        Self {
            response_id,
            model,
            namespace_registry,
            created_at,
            output_items: Vec::new(),
            text_accumulator: String::new(),
            thinking_accumulator: String::new(),
            arguments_accumulator: String::new(),
            signature_accumulator: String::new(),
            current_reasoning_id: String::new(),
            current_fc_item_id: String::new(),
            current_fc_call_id: String::new(),
            fc_counter: 0,
            tool_use_map: Vec::new(),
            stop_reason: None,
            input_tokens: 0,
            cache_read_tokens: 0,
            output_tokens: 0,
            tool_choice_echo,
            instructions_echo,
            parallel_tool_calls_echo,
        }
    }

    /// Process a single Anthropic SSE event and return zero or more Responses API SSE events.
    pub fn process_event(&mut self, event: AnthropicEvent) -> Vec<ResponsesEvent> {
        match event {
            AnthropicEvent::MessageStart { message } => self.handle_message_start(message),
            AnthropicEvent::ContentBlockStart { index, content_block } => {
                self.handle_content_block_start(index, content_block)
            }
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                self.handle_content_block_delta(index, delta)
            }
            AnthropicEvent::ContentBlockStop { index } => {
                self.handle_content_block_stop(index)
            }
            AnthropicEvent::MessageDelta { delta, usage } => {
                self.handle_message_delta(delta, usage)
            }
            AnthropicEvent::MessageStop => self.handle_message_stop(),
            AnthropicEvent::Error { error } => self.handle_error(error),
            AnthropicEvent::Ping => vec![], // Consumed silently.
            AnthropicEvent::Done => vec![], // [DONE] handled by caller.
            AnthropicEvent::Unknown { .. } => vec![], // Forward compat: ignore unknown.
        }
    }

    /// Get accumulated signatures for writing to cache after stream ends.
    /// Returns (reasoning_id, signature) pairs.
    pub fn drain_signatures(&mut self) -> Vec<(String, String)> {
        // Signatures are accumulated during thinking content blocks.
        // For now, return the current accumulated signature if any.
        let mut result = Vec::new();
        if !self.current_reasoning_id.is_empty() && !self.signature_accumulator.is_empty() {
            result.push((self.current_reasoning_id.clone(), self.signature_accumulator.clone()));
        }
        result
    }

    fn handle_message_start(&mut self, message: Value) -> Vec<ResponsesEvent> {
        // Record response ID and usage from the initial message.
        if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
            self.response_id = id.to_string();
        }
        if let Some(usage) = message.get("usage") {
            self.input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            self.cache_read_tokens = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        }

        vec![
            ResponsesEvent::ResponseCreated {
                response_id: self.response_id.clone(),
                created_at: self.created_at,
                model: self.model.clone(),
            },
        ]
    }

    fn handle_content_block_start(&mut self, index: usize, content_block: Value) -> Vec<ResponsesEvent> {
        let block_type = content_block.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match block_type {
            "text" => {
                self.text_accumulator.clear();
                let output_index = self.output_items.len();
                let item = json!({
                    "type": "message",
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [],
                });
                vec![
                    ResponsesEvent::OutputItemAdded { output_index, item },
                    ResponsesEvent::ContentPartAdded {
                        output_index,
                        content_index: 0,
                        part: json!({"type": "output_text", "text": "", "annotations": []}),
                    },
                ]
            }
            "thinking" => {
                self.thinking_accumulator.clear();
                self.signature_accumulator.clear();
                let output_index = self.output_items.len();
                let reasoning_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
                self.current_reasoning_id = reasoning_id.clone();
                let item = json!({
                    "type": "reasoning",
                    "id": reasoning_id,
                    "summary": [],
                });
                vec![
                    ResponsesEvent::OutputItemAdded { output_index, item },
                    ResponsesEvent::ReasoningSummaryPartAdded {
                        output_index,
                        item_id: reasoning_id,
                        summary_index: 0,
                    },
                ]
            }
            "redacted_thinking" => {
                let data = content_block.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let output_index = self.output_items.len();
                let reasoning_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
                self.current_reasoning_id = reasoning_id.clone();
                let item = json!({
                    "type": "reasoning",
                    "id": reasoning_id,
                    "summary": [],
                    "encrypted_content": data,
                });
                vec![
                    ResponsesEvent::OutputItemAdded { output_index, item },
                ]
            }
            "tool_use" => {
                let toolu_id = content_block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = content_block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                self.arguments_accumulator.clear();

                let fc_id = format!("fc_{}", self.fc_counter);
                let call_id = format!("call_{}", self.fc_counter);
                self.current_fc_item_id = fc_id.clone();
                self.current_fc_call_id = call_id.clone();
                self.tool_use_map.push((toolu_id, fc_id.clone(), call_id.clone(), name.clone()));
                self.fc_counter += 1;

                let output_index = self.output_items.len();

                // Look up namespace for the tool name.
                let (display_name, namespace) = if let Some(entry) = self.namespace_registry.lookup(&name) {
                    (entry.tool_name.clone(), Some(entry.namespace.clone()))
                } else {
                    (name.clone(), None)
                };

                let mut item = json!({
                    "type": "function_call",
                    "id": fc_id,
                    "call_id": call_id,
                    "name": display_name,
                    "status": "in_progress",
                    "arguments": "",
                });
                if let Some(ns) = namespace {
                    item["namespace"] = json!(ns);
                }

                vec![
                    ResponsesEvent::OutputItemAdded { output_index, item },
                ]
            }
            _ => {
                tracing::warn!(block_type = block_type, "unknown content block type in content_block_start");
                vec![]
            }
        }
    }

    fn handle_content_block_delta(&mut self, index: usize, delta: Value) -> Vec<ResponsesEvent> {
        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match delta_type {
            "text_delta" => {
                let text = delta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                self.text_accumulator.push_str(text);
                let output_index = self.output_items.len();
                vec![
                    ResponsesEvent::OutputTextDelta {
                        output_index,
                        content_index: 0,
                        delta: text.to_string(),
                    },
                ]
            }
            "thinking_delta" => {
                let thinking = delta.get("thinking").and_then(|v| v.as_str()).unwrap_or("");
                self.thinking_accumulator.push_str(thinking);
                let output_index = self.output_items.len();
                vec![
                    ResponsesEvent::ReasoningSummaryTextDelta {
                        output_index,
                        item_id: self.current_reasoning_id.clone(),
                        summary_index: 0,
                        delta: thinking.to_string(),
                    },
                ]
            }
            "signature_delta" => {
                // Accumulate signature for cache, do NOT emit any SSE event.
                let sig = delta.get("signature").and_then(|v| v.as_str()).unwrap_or("");
                self.signature_accumulator.push_str(sig);
                vec![]
            }
            "input_json_delta" => {
                let partial = delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or("");
                self.arguments_accumulator.push_str(partial);
                let output_index = self.output_items.len();
                vec![
                    ResponsesEvent::FunctionCallArgumentsDelta {
                        output_index,
                        item_id: self.current_fc_item_id.clone(),
                        delta: partial.to_string(),
                    },
                ]
            }
            _ => {
                tracing::debug!(delta_type = delta_type, "unknown delta type, ignoring");
                vec![]
            }
        }
    }

    fn handle_content_block_stop(&mut self, index: usize) -> Vec<ResponsesEvent> {
        let output_index = self.output_items.len();
        let mut events = Vec::new();

        // Determine what type of block is ending based on what accumulators are active.
        if !self.text_accumulator.is_empty() || (!self.thinking_accumulator.is_empty() && self.signature_accumulator.is_empty() && self.arguments_accumulator.is_empty() && self.current_reasoning_id.is_empty()) {
            // Text block ending.
            let text = std::mem::take(&mut self.text_accumulator);
            events.push(ResponsesEvent::OutputTextDone {
                output_index,
                content_index: 0,
                text: text.clone(),
            });
            events.push(ResponsesEvent::ContentPartDone {
                output_index,
                content_index: 0,
                part: json!({"type": "output_text", "text": text, "annotations": []}),
            });
            let item = json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            });
            self.output_items.push(item.clone());
            events.push(ResponsesEvent::OutputItemDone { output_index, item });
            self.text_accumulator.clear();
        } else if !self.current_reasoning_id.is_empty() && (!self.thinking_accumulator.is_empty() || !self.signature_accumulator.is_empty()) {
            // Thinking block ending.
            let thinking_text = std::mem::take(&mut self.thinking_accumulator);
            let reasoning_id = std::mem::take(&mut self.current_reasoning_id);
            events.push(ResponsesEvent::ReasoningSummaryTextDone {
                output_index,
                item_id: reasoning_id.clone(),
                summary_index: 0,
                text: thinking_text.clone(),
            });
            events.push(ResponsesEvent::ReasoningSummaryPartDone {
                output_index,
                item_id: reasoning_id.clone(),
                summary_index: 0,
                part: json!({"type": "summary_text", "text": thinking_text}),
            });
            let item = json!({
                "type": "reasoning",
                "id": reasoning_id,
                "summary": [{"type": "summary_text", "text": thinking_text}],
            });
            self.output_items.push(item.clone());
            events.push(ResponsesEvent::OutputItemDone { output_index, item });
            self.signature_accumulator.clear();
        } else if !self.current_reasoning_id.is_empty() {
            // Redacted thinking block ending (no accumulated thinking text).
            let reasoning_id = std::mem::take(&mut self.current_reasoning_id);
            // Find the redacted_thinking item from the last output_item.added.
            // We need to construct the done item with the encrypted_content.
            let mut item = json!({
                "type": "reasoning",
                "id": reasoning_id,
                "summary": [],
            });
            // Check if the last added output item had encrypted_content.
            if let Some(last) = self.output_items.last() {
                if last["type"] == "reasoning" && last.get("encrypted_content").is_some() {
                    item["encrypted_content"] = last["encrypted_content"].clone();
                }
            }
            self.output_items.push(item.clone());
            events.push(ResponsesEvent::OutputItemDone { output_index, item });
        } else if !self.arguments_accumulator.is_empty() || !self.current_fc_item_id.is_empty() {
            // Tool use block ending.
            let args = std::mem::take(&mut self.arguments_accumulator);
            let fc_id = std::mem::take(&mut self.current_fc_item_id);
            let call_id = std::mem::take(&mut self.current_fc_call_id);

            // Get the name and namespace from the tool_use_map.
            let (name, namespace) = self.tool_use_map.iter()
                .find(|(_, fid, _, _)| fid == &fc_id)
                .map(|(_, _, _, n)| {
                    let ns = self.namespace_registry.lookup(n)
                        .map(|e| e.namespace.clone());
                    let display_name = ns.as_ref()
                        .map(|_| self.namespace_registry.lookup(n).unwrap().tool_name.clone())
                        .unwrap_or_else(|| n.clone());
                    (display_name, ns)
                })
                .unwrap_or_else(|| ("unknown".to_string(), None));

            events.push(ResponsesEvent::FunctionCallArgumentsDone {
                output_index,
                item_id: fc_id.clone(),
                arguments: args.clone(),
            });

            let mut item = json!({
                "type": "function_call",
                "id": fc_id,
                "call_id": call_id,
                "name": name,
                "status": "completed",
                "arguments": args,
            });
            if let Some(ns) = namespace {
                item["namespace"] = json!(ns);
            }
            self.output_items.push(item.clone());
            events.push(ResponsesEvent::OutputItemDone { output_index, item });
        }

        events
    }

    fn handle_message_delta(&mut self, delta: Value, usage: Value) -> Vec<ResponsesEvent> {
        // Record stop reason and output usage.
        if let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str()) {
            self.stop_reason = Some(sr.to_string());
        }
        self.output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        vec![]
    }

    fn handle_message_stop(&mut self) -> Vec<ResponsesEvent> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let stop_reason = self.stop_reason.take().unwrap_or_else(|| "end_turn".to_string());

        // Map stop_reason to status (always "completed", never "incomplete").
        let status = "completed";

        // Build usage object.
        let total_tokens = self.input_tokens + self.output_tokens;
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": total_tokens,
        });
        if self.cache_read_tokens > 0 {
            usage["input_tokens_details"] = json!({"cached_tokens": self.cache_read_tokens});
        }

        // Build response object for response.completed.
        let mut response_obj = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "completed_at": now,
            "model": self.model,
            "status": status,
            "output": self.output_items.clone(),
            "usage": usage,
            "metadata": Value::Null,
        });

        // Incomplete details when max_tokens or model_context_window_exceeded.
        if stop_reason == "max_tokens" || stop_reason == "model_context_window_exceeded" {
            response_obj["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }

        // end_turn: false only when stop_reason is tool_use, omit otherwise.
        if stop_reason == "tool_use" {
            response_obj["end_turn"] = json!(false);
        }

        // Echo back request fields.
        response_obj["parallel_tool_calls"] = json!(self.parallel_tool_calls_echo);
        response_obj["tool_choice"] = json!(self.tool_choice_echo);
        if let Some(ref instructions) = self.instructions_echo {
            response_obj["instructions"] = json!(instructions);
        }

        vec![
            ResponsesEvent::ResponseCompleted { response: response_obj },
        ]
    }

    fn handle_error(&mut self, error: Value) -> Vec<ResponsesEvent> {
        let error_type = error.get("type").and_then(|v| v.as_str()).unwrap_or("api_error");
        let message = error.get("message").and_then(|v| v.as_str()).unwrap_or("Unknown error");

        let (code, _) = convert_error_type(error_type, message);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let response_obj = json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": "failed",
            "error": {"code": code, "message": message},
            "output": self.output_items,
            "usage": Value::Null,
            "metadata": {},
        });

        vec![
            ResponsesEvent::Error {
                code: convert_error_type(error_type, message).0.to_string(),
                message: message.to_string(),
            },
            ResponsesEvent::ResponseFailed { response: response_obj },
        ]
    }
}
```

### Step 13.4 — Verify tests pass

- [ ] Run `cargo test --lib response`. All tests pass.
- [ ] Run `cargo test --lib`. All library tests pass.

### Step 13.5 — Commit

```
feat: implement streaming pipeline (Anthropic SSE → Responses API SSE) with delta accumulators and output item accumulator
```

---

## Task 14: Error Handling

**Files:** `src/conversion/error.rs`

### Step 14.1 — Write failing tests for error conversion

- [ ] Add test module to `src/conversion/error.rs`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_request_maps_to_invalid_request() {
        let (code, status) = convert_error_type("invalid_request_error", "bad parameter");
        assert_eq!(code, "invalid_request");
        assert_eq!(status, 400);
    }

    #[test]
    fn invalid_request_context_overflow_maps_to_context_length() {
        let (code, status) = convert_error_type("invalid_request_error", "prompt is too long: 210000 tokens > context window 200000");
        assert_eq!(code, "context_length_exceeded");
        assert_eq!(status, 400);
    }

    #[test]
    fn invalid_request_too_many_tokens_keyword() {
        let (code, _) = convert_error_type("invalid_request_error", "too many tokens: 300000 > 200000");
        assert_eq!(code, "context_length_exceeded");
    }

    #[test]
    fn invalid_request_context_length_keyword() {
        let (code, _) = convert_error_type("invalid_request_error", "exceeds context length");
        assert_eq!(code, "context_length_exceeded");
    }

    #[test]
    fn invalid_request_context_window_keyword() {
        let (code, _) = convert_error_type("invalid_request_error", "exceeds the context window");
        assert_eq!(code, "context_length_exceeded");
    }

    #[test]
    fn authentication_error_maps_to_invalid_api_key() {
        let (code, status) = convert_error_type("authentication_error", "invalid x-api-key");
        assert_eq!(code, "invalid_api_key");
        assert_eq!(status, 401);
    }

    #[test]
    fn permission_error_maps_to_invalid_api_key() {
        let (code, status) = convert_error_type("permission_error", "forbidden");
        assert_eq!(code, "invalid_api_key");
        assert_eq!(status, 403);
    }

    #[test]
    fn not_found_error_maps_to_model_not_found() {
        let (code, status) = convert_error_type("not_found_error", "model not found");
        assert_eq!(code, "model_not_found");
        assert_eq!(status, 404);
    }

    #[test]
    fn request_too_large_maps_to_context_length() {
        let (code, status) = convert_error_type("request_too_large", "request too large");
        assert_eq!(code, "context_length_exceeded");
        assert_eq!(status, 413);
    }

    #[test]
    fn rate_limit_error_maps_to_rate_limit() {
        let (code, status) = convert_error_type("rate_limit_error", "too many requests");
        assert_eq!(code, "rate_limit_exceeded");
        assert_eq!(status, 429);
    }

    #[test]
    fn billing_error_maps_to_insufficient_quota() {
        let (code, status) = convert_error_type("billing_error", "insufficient funds");
        assert_eq!(code, "insufficient_quota");
        assert_eq!(status, 402);
    }

    #[test]
    fn overloaded_error_maps_to_server_overloaded() {
        let (code, status) = convert_error_type("overloaded_error", "overloaded");
        assert_eq!(code, "server_is_overloaded");
        assert_eq!(status, 503, "overloaded_error must map to 503, NOT 500");
    }

    #[test]
    fn api_error_maps_to_server_error() {
        let (code, status) = convert_error_type("api_error", "internal error");
        assert_eq!(code, "server_error");
        assert_eq!(status, 500);
    }

    #[test]
    fn unknown_4xx_maps_to_400() {
        let status = map_http_status(418);
        assert_eq!(status, 400);
    }

    #[test]
    fn unknown_5xx_maps_to_500() {
        let status = map_http_status(599);
        assert_eq!(status, 500);
    }

    #[test]
    fn http_529_maps_to_503() {
        let status = map_http_status(529);
        assert_eq!(status, 503);
    }

    #[test]
    fn known_http_statuses_passthrough() {
        assert_eq!(map_http_status(400), 400);
        assert_eq!(map_http_status(401), 401);
        assert_eq!(map_http_status(402), 402);
        assert_eq!(map_http_status(403), 403);
        assert_eq!(map_http_status(404), 404);
        assert_eq!(map_http_status(413), 413);
        assert_eq!(map_http_status(429), 429);
        assert_eq!(map_http_status(500), 500);
    }

    #[test]
    fn non_streaming_error_body_conversion() {
        let anthropic_body = json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": "Rate limit reached"
            },
            "request_id": "req_123"
        });
        let (status, responses_body) = convert_non_streaming_error(&anthropic_body);
        assert_eq!(status, 429);
        assert_eq!(responses_body["error"]["code"], "rate_limit_exceeded");
        assert_eq!(responses_body["error"]["message"], "Rate limit reached");
        assert_eq!(responses_body["error"]["type"], "rate_limit_error");
        assert_eq!(responses_body["error"]["param"], Value::Null);
        assert!(responses_body.get("request_id").is_none(), "request_id should not be in response");
    }

    #[test]
    fn proxy_error_body_format() {
        let body = proxy_error_response(400, "invalid_request", "Bad request from proxy");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["message"], "Bad request from proxy");
        assert_eq!(body["error"]["param"], Value::Null);
    }
}
```

### Step 14.2 — Verify tests fail

- [ ] Run `cargo test --lib error`. Compilation errors expected.

### Step 14.3 — Implement error handling

- [ ] Implement `src/conversion/error.rs`.

```rust
use serde_json::{json, Value};

/// Context overflow keyword detection heuristic.
/// Anthropic uses `invalid_request_error` for both context overflow and other invalid requests.
/// These keywords distinguish them.
const CONTEXT_OVERFLOW_KEYWORDS: &[&str] = [
    "context window",
    "context length",
    "too many tokens",
    "prompt is too long",
];

/// Convert an Anthropic error type and message to a Responses API error code and HTTP status.
///
/// Returns (error_code, http_status).
///
/// CRITICAL mappings:
/// - `overloaded_error` → `server_is_overloaded` (NOT `server_error`)
/// - `billing_error` → `insufficient_quota`
/// - `request_too_large` → `context_length_exceeded`
/// - Context overflow detected by keyword heuristic on message text
pub fn convert_error_type(error_type: &str, message: &str) -> (&'static str, u16) {
    match error_type {
        "invalid_request_error" => {
            // Check for context overflow via keyword heuristic.
            if is_context_overflow(message) {
                ("context_length_exceeded", 400)
            } else {
                ("invalid_request", 400)
            }
        }
        "authentication_error" => ("invalid_api_key", 401),
        "permission_error" => ("invalid_api_key", 403),
        "not_found_error" => ("model_not_found", 404),
        "request_too_large" => ("context_length_exceeded", 413),
        "rate_limit_error" => ("rate_limit_exceeded", 429),
        "billing_error" => ("insufficient_quota", 402),
        "overloaded_error" => ("server_is_overloaded", 503),
        "api_error" => ("server_error", 500),
        _ => {
            // Unknown error type — classify by HTTP status family.
            ("server_error", 500)
        }
    }
}

/// Detect context overflow from Anthropic error message using keyword heuristic.
fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_lowercase();
    CONTEXT_OVERFLOW_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// Map an Anthropic HTTP status code to Responses API status code.
///
/// Special mappings:
/// - 529 → 503 (overloaded)
/// - Unknown 4XX → 400
/// - Unknown 5XX → 500
pub fn map_http_status(status: u16) -> u16 {
    match status {
        400 | 401 | 402 | 403 | 404 | 413 | 429 | 500 => status,
        529 => 503,
        400..=499 => 400,   // Unknown 4XX → 400
        500..=599 => 500,   // Unknown 5XX → 500
        _ => 500,
    }
}

/// Convert a non-streaming Anthropic error body to Responses API error format.
///
/// Anthropic format:
/// ```json
/// {"type": "error", "error": {"type": "...", "message": "..."}, "request_id": "..."}
/// ```
///
/// Responses API format:
/// ```json
/// {"error": {"message": "...", "type": "...", "param": null, "code": "..."}}
/// ```
pub fn convert_non_streaming_error(body: &Value) -> (u16, Value) {
    let error_obj = body.get("error").cloned().unwrap_or(json!({}));
    let error_type = error_obj.get("type").and_then(|v| v.as_str()).unwrap_or("api_error");
    let message = error_obj.get("message").and_then(|v| v.as_str()).unwrap_or("Unknown error");

    let (code, http_status) = convert_error_type(error_type, message);

    // Log request_id for debugging (not returned in response).
    if let Some(req_id) = body.get("request_id").and_then(|v| v.as_str()) {
        tracing::debug!(request_id = req_id, "Anthropic error request_id");
    }

    let responses_body = json!({
        "error": {
            "message": message,
            "type": error_type,
            "param": Value::Null,
            "code": code,
        }
    });

    (http_status, responses_body)
}

/// Build a proxy-originated error response body.
pub fn proxy_error_response(http_status: u16, code: &str, message: &str) -> Value {
    json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": Value::Null,
            "code": code,
        }
    })
}
```

### Step 14.4 — Verify tests pass

- [ ] Run `cargo test --lib error`. All tests pass.

### Step 14.5 — Commit

```
feat: implement error handling with type mapping, context overflow detection, and HTTP status mapping
```

---

## Task 15: Response Completion

**Files:** modify `src/conversion/response.rs` (usage mapping, response object assembly)

This task validates and refines the response completion logic already implemented in Task 13. The tests below verify the complete response object structure against the spec.

### Step 15.1 — Write tests for response object structure

- [ ] Add tests to `src/conversion/response.rs` test module.

```rust
    // --- Response object structure tests ---

    #[test]
    fn completed_response_has_all_required_fields() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_01XFDUDYJgAACzvnptvVoYEL", "type": "message", "role": "assistant", "content": [], "model": "claude-sonnet-4-20250514", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: json!({"type": "text_delta", "text": "Hello world"}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 15}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let resp = &parsed["response"];

        // Required fields.
        assert_eq!(resp["id"], "msg_01XFDUDYJgAACzvnptvVoYEL");
        assert_eq!(resp["object"], "response");
        assert!(resp["created_at"].as_u64().unwrap() > 0);
        assert!(resp["completed_at"].as_u64().unwrap() > 0);
        assert_eq!(resp["model"], "claude-sonnet-4-20250514");
        assert_eq!(resp["status"], "completed");
        assert!(resp["output"].is_array());
        assert!(resp["usage"].is_object());
        assert!(resp["metadata"].is_null());
        assert_eq!(resp["tool_choice"], "auto");
        assert_eq!(resp["instructions"], "You are helpful.");
    }

    #[test]
    fn usage_computed_fields() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0, "cache_read_input_tokens": 50, "cache_creation_input_tokens": 25}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 30}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let usage = &parsed["response"]["usage"];

        assert_eq!(usage["input_tokens"], 100);
        assert_eq!(usage["output_tokens"], 30);
        assert_eq!(usage["total_tokens"], 130);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 50);
        // cache_creation_input_tokens is folded into input_tokens (no separate field).
        assert!(usage.get("cache_creation_input_tokens").is_none());
        // reasoning_tokens is omitted.
        assert!(usage.get("reasoning_tokens").is_none());
    }

    #[test]
    fn usage_no_cache_read_omits_details() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 50, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "end_turn", "stop_sequence": null}),
            usage: json!({"output_tokens": 10}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let usage = &parsed["response"]["usage"];
        assert!(usage.get("input_tokens_details").is_none(), "input_tokens_details should be omitted when no cached tokens");
    }

    #[test]
    fn max_tokens_has_incomplete_details_but_status_completed() {
        let mut state = make_state();
        state.process_event(AnthropicEvent::MessageStart {
            message: json!({"id": "msg_test", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 100, "output_tokens": 0}}),
        });
        state.process_event(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: json!({"type": "text", "text": ""}),
        });
        state.process_event(AnthropicEvent::ContentBlockStop { index: 0 });
        state.process_event(AnthropicEvent::MessageDelta {
            delta: json!({"stop_reason": "max_tokens", "stop_sequence": null}),
            usage: json!({"output_tokens": 4096}),
        });
        let events = state.process_event(AnthropicEvent::MessageStop);
        let (_, data) = events.iter().find(|e| {
            let (t, _) = e.to_sse();
            t == "response.completed"
        }).unwrap().to_sse();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        let resp = &parsed["response"];
        assert_eq!(resp["status"], "completed", "status must be completed even with max_tokens");
        assert_eq!(resp["incomplete_details"]["reason"], "max_output_tokens");
    }
```

### Step 15.2 — Verify tests pass

- [ ] Run `cargo test --lib response`. All tests pass including new ones.

### Step 15.3 — Commit

```
test: verify response object structure, usage mapping, and max_tokens incomplete_details
```

---

## Task 16: Thinking/Signature Cache Integration

**Files:** `src/conversion/thinking.rs` (expand), `src/conversion/signature_cache.rs` (expand)

### Step 16.1 — Write failing tests for signature cache integration

- [ ] Add tests to `src/conversion/thinking.rs` and `src/conversion/signature_cache.rs`.

```rust
// In src/conversion/thinking.rs tests:
#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::signature_cache::SignatureCache;
    use std::time::Duration;
    use serde_json::json;

    #[test]
    fn reasoning_input_summary_only_empty_signature_when_no_cache() {
        let cache = SignatureCache::new(Duration::from_secs(10800));
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "I thought"})],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["thinking"], "I thought");
        assert_eq!(result["signature"], "");
    }

    #[test]
    fn reasoning_input_summary_with_cached_signature() {
        let cache = SignatureCache::new(Duration::from_secs(10800));
        cache.insert("rs_001".to_string(), "ErUB_signature_data".to_string());
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "I thought"})],
            None,
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "thinking");
        assert_eq!(result["signature"], "ErUB_signature_data");
    }

    #[test]
    fn reasoning_input_encrypted_content_overrides_summary() {
        let cache = SignatureCache::new(Duration::from_secs(10800));
        cache.insert("rs_001".to_string(), "cached_sig".to_string());
        let result = convert_reasoning_input(
            &[json!({"type": "summary_text", "text": "I thought"})],
            Some("ENCRYPTED_BLOB"),
            &cache,
            "rs_001",
        );
        assert_eq!(result["type"], "redacted_thinking");
        assert_eq!(result["data"], "ENCRYPTED_BLOB");
    }

    #[test]
    fn reasoning_input_multiple_summaries_concatenated() {
        let cache = SignatureCache::new(Duration::from_secs(10800));
        let result = convert_reasoning_input(
            &[
                json!({"type": "summary_text", "text": "Part 1. "}),
                json!({"type": "summary_text", "text": "Part 2."}),
            ],
            None,
            &cache,
            "rs_002",
        );
        assert_eq!(result["thinking"], "Part 1. Part 2.");
    }
}

// In src/conversion/signature_cache.rs tests:
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn insert_and_retrieve() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "sig_abc".to_string());
        assert_eq!(cache.get("rs_001"), Some("sig_abc".to_string()));
    }

    #[test]
    fn expired_entry_returns_none() {
        let cache = SignatureCache::new(Duration::from_millis(1));
        cache.insert("rs_001".to_string(), "sig_abc".to_string());
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(cache.get("rs_001"), None);
    }

    #[test]
    fn missing_entry_returns_none() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        assert_eq!(cache.get("rs_nonexistent"), None);
    }

    #[test]
    fn overwrite_existing() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "sig_old".to_string());
        cache.insert("rs_001".to_string(), "sig_new".to_string());
        assert_eq!(cache.get("rs_001"), Some("sig_new".to_string()));
    }

    #[test]
    fn multiple_entries_independent() {
        let cache = SignatureCache::new(Duration::from_secs(3600));
        cache.insert("rs_001".to_string(), "sig_a".to_string());
        cache.insert("rs_002".to_string(), "sig_b".to_string());
        assert_eq!(cache.get("rs_001"), Some("sig_a".to_string()));
        assert_eq!(cache.get("rs_002"), Some("sig_b".to_string()));
    }
}
```

### Step 16.2 — Verify tests fail

- [ ] Run `cargo test --lib thinking`. Compilation errors for the new thinking tests.

### Step 16.3 — Update thinking.rs to use SignatureCache

- [ ] The `convert_reasoning_input` function already takes `&SignatureCache` (implemented in Task 9). Verify the signature cache tests pass.

### Step 16.4 — Verify tests pass

- [ ] Run `cargo test --lib signature_cache`. All tests pass.
- [ ] Run `cargo test --lib thinking`. All tests pass.

### Step 16.5 — Commit

```
test: verify signature cache integration with TTL expiry, overwrite, and reasoning input conversion
```

---

## Task 17: Integration Tests

**Files:** create `tests/integration/` directory with test files

### Step 17.1 — Create test infrastructure

- [ ] Create `tests/integration/mod.rs` (if using `#[test]` convention) or standalone test files.

### Step 17.2 — Write integration test: simple text streaming

- [ ] Create `tests/integration/simple_text_test.rs`.

```rust
//! Integration test: simple text streaming through the full conversion pipeline.

use codex_conv::conversion::{ConversionTask, request::convert_request};
use codex_conv::conversion::response::StreamingState;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

/// Simulates a complete text streaming request-response cycle.
#[test]
fn simple_text_streaming_end_to_end() {
    // -- Request phase --
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "stream": true,
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Say hello"}]}
        ]
    });
    let anth_request = convert_request(&mut task, request).unwrap();
    assert_eq!(anth_request["model"], "claude-sonnet-4-20250514");
    assert_eq!(anth_request["stream"], true);

    // -- Simulated Anthropic SSE response --
    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01ABC","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello!"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);

    // -- Response conversion phase --
    let mut state = StreamingState::new(
        "msg_01ABC".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task.namespace_registry.clone(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut output_sse = String::new();
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            output_sse.push_str("data: [DONE]\n\n");
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // -- Verify output --
    assert!(output_sse.contains("event: response.created"));
    assert!(output_sse.contains("event: response.output_item.added"));
    assert!(output_sse.contains("event: response.output_text.delta"));
    assert!(output_sse.contains("\"delta\":\"Hello!\""));
    assert!(output_sse.contains("event: response.output_text.done"));
    assert!(output_sse.contains("\"text\":\"Hello!\""));
    assert!(output_sse.contains("event: response.output_item.done"));
    assert!(output_sse.contains("event: response.completed"));
    assert!(output_sse.contains("data: [DONE]"));
    // NEVER response.incomplete
    assert!(!output_sse.contains("response.incomplete"));
    // NEVER sequence_number
    assert!(!output_sse.contains("sequence_number"));
}
```

### Step 17.3 — Write integration test: tool call round-trip

- [ ] Create `tests/integration/tool_call_test.rs`.

```rust
//! Integration test: tool call round-trip with namespace.

use codex_conv::conversion::{ConversionTask, request::convert_request};
use codex_conv::conversion::response::StreamingState;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

#[test]
fn tool_call_round_trip_with_namespace() {
    // -- Request with MCP namespace tools --
    let mut task = ConversionTask::new("https://api.anthropic.com".to_string());
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "stream": true,
        "tools": [
            {
                "type": "namespace",
                "name": "mcp__memory__",
                "tools": [
                    {"type": "function", "name": "search", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}
                ]
            }
        ],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Search memory"}]}
        ]
    });
    let anth_request = convert_request(&mut task, request).unwrap();
    // Verify namespace was flattened.
    let tools = anth_request["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "mcp__memory__search");
    assert_eq!(tools[0]["type"], "custom");

    // -- Simulated Anthropic response with tool_use --
    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_tool","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":50,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"mcp__memory__search","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"test\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":30}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut state = StreamingState::new(
        "msg_tool".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task.namespace_registry.clone(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut output_sse = String::new();
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            output_sse.push_str("data: [DONE]\n\n");
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // Verify namespace restoration in function_call.
    assert!(output_sse.contains("\"name\":\"search\""), "tool name should be restored to 'search'");
    assert!(output_sse.contains("\"namespace\":\"mcp__memory__\""), "namespace should be restored");
    assert!(output_sse.contains("event: response.function_call_arguments.delta"));
    assert!(output_sse.contains("event: response.function_call_arguments.done"));
    assert!(output_sse.contains("event: response.completed"));
    // end_turn: false when stop_reason is tool_use
    assert!(output_sse.contains("\"end_turn\":false"));
}
```

### Step 17.4 — Write integration test: error handling

- [ ] Create `tests/integration/error_handling_test.rs`.

```rust
//! Integration test: error handling scenarios.

use codex_conv::conversion::response::StreamingState;
use codex_conv::conversion::namespace::NamespaceRegistry;
use codex_conv::sse::anthropic::AnthropicEvent;
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

fn make_state() -> StreamingState {
    StreamingState::new(
        "msg_err".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        NamespaceRegistry::new(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    )
}

#[test]
fn streaming_rate_limit_error() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_err", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    let events = state.process_event(AnthropicEvent::Error {
        error: json!({"type": "rate_limit_error", "message": "Too many requests"}),
    });

    let mut output_sse = String::new();
    for re in &events {
        output_sse.push_str(&format_responses_event(re));
    }
    output_sse.push_str("data: [DONE]\n\n");

    // Verify: error event + response.failed + [DONE].
    assert!(output_sse.contains("event: error"));
    assert!(output_sse.contains("\"code\":\"rate_limit_exceeded\""));
    assert!(output_sse.contains("event: response.failed"));
    assert!(output_sse.contains("\"status\":\"failed\""));
    assert!(output_sse.contains("data: [DONE]"));
}

#[test]
fn streaming_context_overflow_error() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_err", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    let events = state.process_event(AnthropicEvent::Error {
        error: json!({"type": "invalid_request_error", "message": "prompt is too long: 210000 tokens > context window 200000"}),
    });

    let mut output_sse = String::new();
    for re in &events {
        output_sse.push_str(&format_responses_event(re));
    }

    assert!(output_sse.contains("\"code\":\"context_length_exceeded\""), "context overflow should map to context_length_exceeded");
    assert!(output_sse.contains("event: response.failed"));
}

#[test]
fn streaming_overloaded_error_not_server_error() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_err", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    let events = state.process_event(AnthropicEvent::Error {
        error: json!({"type": "overloaded_error", "message": "Overloaded"}),
    });

    let mut output_sse = String::new();
    for re in &events {
        output_sse.push_str(&format_responses_event(re));
    }

    assert!(output_sse.contains("\"code\":\"server_is_overloaded\""), "overloaded_error must map to server_is_overloaded, NOT server_error");
    assert!(!output_sse.contains("\"code\":\"server_error\""), "should NOT be generic server_error");
}

#[test]
fn streaming_billing_error() {
    let mut state = make_state();
    state.process_event(AnthropicEvent::MessageStart {
        message: json!({"id": "msg_err", "type": "message", "role": "assistant", "content": [], "model": "m", "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 10, "output_tokens": 0}}),
    });

    let events = state.process_event(AnthropicEvent::Error {
        error: json!({"type": "billing_error", "message": "Insufficient funds"}),
    });

    let mut output_sse = String::new();
    for re in &events {
        output_sse.push_str(&format_responses_event(re));
    }

    assert!(output_sse.contains("\"code\":\"insufficient_quota\""));
}
```

### Step 17.5 — Write integration test: extended thinking with signature cache

- [ ] Create `tests/integration/thinking_test.rs`.

```rust
//! Integration test: extended thinking with signature cache round-trip.

use codex_conv::conversion::{ConversionTask, request::convert_request, SignatureCache};
use codex_conv::conversion::response::StreamingState;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn thinking_stream_with_signature_cache() {
    let cache = Arc::new(SignatureCache::new(Duration::from_secs(10800)));

    // -- First turn: thinking response --
    let mut task = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request = json!({
        "model": "claude-sonnet-4-20250514",
        "reasoning": {"effort": "high", "summary": "auto"},
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Think about it"}]}]
    });
    let anth_request = convert_request(&mut task, request).unwrap();
    assert_eq!(anth_request["thinking"]["type"], "adaptive");
    assert_eq!(anth_request["output_config"]["effort"], "high");

    // Simulated Anthropic thinking response.
    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_think","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":50,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to analyze..."}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"ErUB_cachesig"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Here is my answer."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":100}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut state = StreamingState::new(
        "msg_think".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task.namespace_registry.clone(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut output_sse = String::new();
    let mut reasoning_id = String::new();
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            break;
        }
        // Track the reasoning ID.
        if let AnthropicEvent::ContentBlockStart { content_block, .. } = &event {
            if content_block["type"] == "thinking" {
                // The StreamingState generates the rs_ ID internally.
            }
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            let (t, d) = re.to_sse();
            if t == "response.output_item.added" && d.contains("\"reasoning\"") {
                let parsed: serde_json::Value = serde_json::from_str(&d).unwrap();
                reasoning_id = parsed["item"]["id"].as_str().unwrap().to_string();
            }
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // Verify reasoning events emitted.
    assert!(output_sse.contains("event: response.reasoning_summary_part.added"));
    assert!(output_sse.contains("event: response.reasoning_summary_text.delta"));
    assert!(output_sse.contains("event: response.reasoning_summary_text.done"));
    // Signature should NOT appear in SSE output (consumed and accumulated).
    assert!(!output_sse.contains("ErUB_cachesig"));

    // Write signature to cache.
    let signatures = state.drain_signatures();
    for (id, sig) in signatures {
        cache.insert(id, sig);
    }

    // -- Second turn: reasoning input with cached signature --
    let mut task2 = ConversionTask::with_signature_cache(
        "https://api.anthropic.com".to_string(),
        cache.clone(),
    );
    let request2 = json!({
        "model": "claude-sonnet-4-20250514",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Follow up"}]},
            {"type": "reasoning", "id": reasoning_id, "summary": [{"type": "summary_text", "text": "I need to analyze..."}]}
        ]
    });
    let anth_request2 = convert_request(&mut task2, request2).unwrap();
    let messages = anth_request2["messages"].as_array().unwrap();
    // Find the thinking block in the messages.
    let thinking_block = messages.iter()
        .flat_map(|m| m["content"].as_array().unwrap_or(&vec![]))
        .find(|b| b["type"] == "thinking")
        .expect("should have a thinking block");
    assert_eq!(thinking_block["signature"], "ErUB_cachesig", "cached signature should be used");
}
```

### Step 17.6 — Write integration test: max_tokens never response.incomplete

- [ ] Create `tests/integration/max_tokens_test.rs`.

```rust
//! Integration test: max_tokens truncation always produces response.completed.

use codex_conv::conversion::response::StreamingState;
use codex_conv::conversion::namespace::NamespaceRegistry;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

#[test]
fn max_tokens_produces_response_completed_not_incomplete() {
    let mut state = StreamingState::new(
        "msg_max".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        NamespaceRegistry::new(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let anthropic_sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_max","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"This is a partial response that got cut off because of max tok"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":4096}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events = parse_sse_events(anthropic_sse);
    let mut output_sse = String::new();
    for event in events {
        if matches!(event, AnthropicEvent::Done) {
            output_sse.push_str("data: [DONE]\n\n");
            break;
        }
        let responses_events = state.process_event(event);
        for re in &responses_events {
            output_sse.push_str(&format_responses_event(re));
        }
    }

    // CRITICAL: Must be response.completed, NEVER response.incomplete.
    assert!(output_sse.contains("event: response.completed"), "Must emit response.completed");
    assert!(!output_sse.contains("response.incomplete"), "Must NEVER emit response.incomplete");
    // Must have incomplete_details as informational metadata.
    assert!(output_sse.contains("\"incomplete_details\""));
    assert!(output_sse.contains("\"reason\":\"max_output_tokens\""));
    assert!(output_sse.contains("\"status\":\"completed\""), "status must be completed");
}
```

### Step 17.7 — Write integration test: multi-turn conversation

- [ ] Create `tests/integration/multi_turn_test.rs`.

```rust
//! Integration test: multi-turn conversation with tool calls.

use codex_conv::conversion::{ConversionTask, request::convert_request};
use codex_conv::conversion::response::StreamingState;
use codex_conv::sse::anthropic::{parse_sse_events, AnthropicEvent};
use codex_conv::sse::responses::format_responses_event;
use serde_json::json;

#[test]
fn multi_turn_with_tool_execution() {
    // -- Turn 1: User asks, model calls tool --
    let mut task1 = ConversionTask::new("https://api.anthropic.com".to_string());
    let request1 = json!({
        "model": "claude-sonnet-4-20250514",
        "tools": [{"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List files"}]}
        ]
    });
    let anth_req1 = convert_request(&mut task1, request1).unwrap();
    assert_eq!(anth_req1["tools"][0]["name"], "exec_command");

    // Simulated response: model calls exec_command
    let response1 = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_turn1","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-20250514","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":30,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_exec1","name":"exec_command","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":20}}

event: message_stop
data: {"type":"message_stop"}

data: [DONE]"#;

    let events1 = parse_sse_events(response1);
    let mut state1 = StreamingState::new(
        "msg_turn1".to_string(),
        "claude-sonnet-4-20250514".to_string(),
        task1.namespace_registry.clone(),
        1717000000,
        "auto".to_string(),
        None,
        None,
    );

    let mut output1 = String::new();
    for event in events1 {
        if matches!(event, AnthropicEvent::Done) { break; }
        for re in state1.process_event(event) {
            output1.push_str(&format_responses_event(&re));
        }
    }
    assert!(output1.contains("\"name\":\"exec_command\""));
    assert!(output1.contains("\"end_turn\":false"));

    // -- Turn 2: User provides tool result, model responds with text --
    let mut task2 = ConversionTask::new("https://api.anthropic.com".to_string());
    let request2 = json!({
        "model": "claude-sonnet-4-20250514",
        "tools": [{"type": "function", "name": "exec_command", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "List files"}]},
            {"type": "function_call", "call_id": "call_0", "name": "exec_command", "arguments": "{\"command\":\"ls\"}"},
            {"type": "function_call_output", "call_id": "call_0", "output": "file1.txt\nfile2.txt"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What files are there?"}]}
        ]
    });
    let anth_req2 = convert_request(&mut task2, request2).unwrap();
    let messages = anth_req2["messages"].as_array().unwrap();
    // Should have: user, assistant (tool_use), user (tool_result), user
    assert!(messages.len() >= 3, "should have at least 3 messages in multi-turn");

    // Verify tool_result content.
    let tool_result_msg = messages.iter()
        .find(|m| m["content"].as_array().map(|a| a.iter().any(|b| b["type"] == "tool_result")).unwrap_or(false))
        .expect("should have tool_result message");
    let tool_result = &tool_result_msg["content"][0];
    assert_eq!(tool_result["type"], "tool_result");
    assert_eq!(tool_result["content"], "file1.txt\nfile2.txt");
}
```

### Step 17.8 — Verify all integration tests pass

- [ ] Run `cargo test`. All unit + integration tests pass.
- [ ] Run `cargo clippy`. Zero warnings.
- [ ] Run `cargo fmt --check`. Clean.

### Step 17.9 — Commit

```
test: add integration tests for text streaming, tool calls, error handling, thinking/signature cache, max_tokens, and multi-turn
```

---

## Part 2 Task Summary

| Task | Files | Key Deliverable |
|------|-------|-----------------|
| 10 | `src/sse/anthropic.rs` | Anthropic SSE event parsing with all event types + forward compat |
| 11 | `src/sse/responses.rs` | Responses API SSE event generation, NO sequence_number |
| 12 | `src/conversion/content.rs` | Content block to output item conversion with proxy-generated IDs |
| 13 | `src/conversion/response.rs` | Streaming pipeline: delta accumulators, output item accumulator, signature accumulation |
| 14 | `src/conversion/error.rs` | Error type mapping, context overflow heuristic, HTTP status mapping, non-streaming conversion |
| 15 | `src/conversion/response.rs` (tests) | Response object structure verification, usage mapping, max_tokens behavior |
| 16 | `src/conversion/thinking.rs`, `src/conversion/signature_cache.rs` | Signature cache integration with TTL, reasoning input conversion |
| 17 | `tests/integration/` | 6 integration tests: text, tool call, error handling, thinking, max_tokens, multi-turn |

## Behavioral Constraint Checklist

- [ ] NEVER emit `response.incomplete` — always `response.completed` (Task 13, 15, 17)
- [ ] NEVER include `sequence_number` in SSE events (Task 11, verified in tests)
- [ ] NEVER emit `custom_tool_call_input.*` events (Task 11 — not in enum)
- [ ] `response.failed` carries FULL response object with error inside (Task 13, 14)
- [ ] `end_turn: false` only when `stop_reason: "tool_use"`, omit otherwise (Task 13)
- [ ] Context overflow → `context_length_exceeded` (Task 14, keyword heuristic)
- [ ] `overloaded_error` → `server_is_overloaded` (NOT `server_error`) (Task 14)
- [ ] `billing_error` → `insufficient_quota` (Task 14)
- [ ] `request_too_large` → `context_length_exceeded` (Task 14)
- [ ] Signature accumulation during streaming, discarded from SSE output (Task 13)
- [ ] Proxy-generated IDs: `rs_{uuid}` for reasoning, `fc_{sequential}` for function_calls (Task 12)
- [ ] Usage: `cache_read_input_tokens` → `cached_tokens`, omit when 0 (Task 15)
- [ ] `reasoning_tokens` omitted from response usage (Task 15)

---

## Execution Choice

This plan is ready for execution. Two options:

**Option A: Subagent-driven development** — Each task (10-17) dispatched to a parallel subagent. Best for speed. Tasks 10-12 are independent; Task 13 depends on 10-12; Tasks 14-15 depend on 13; Task 16 depends on 9; Task 17 depends on all.

**Option B: Inline sequential execution** — Execute tasks in order (10 → 11 → 12 → 13 → 14 → 15 → 16 → 17) with TDD cycle per task. Best for careful review at each step.

Recommend: **Option A** with dependency ordering:
- Wave 1 (parallel): Task 10, Task 11, Task 12, Task 14, Task 16
- Wave 2 (after Wave 1): Task 13
- Wave 3 (after Task 13): Task 15
- Wave 4 (after all): Task 17
