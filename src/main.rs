#![forbid(unsafe_code)]

use clap::Parser;
use codex_conv::catalog;
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

    /// Listen address (overrides server.listen)
    #[arg(long = "listen", value_name = "ADDR:PORT")]
    listen: Option<String>,

    /// Extra CA certs for upstream TLS (comma-separated paths, overrides upstream.tls.extra_ca_certs)
    #[arg(long = "upstream-tls-extra-ca-certs", value_name = "PATH")]
    upstream_tls_extra_ca_certs: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Step 1: Load base config from file or defaults.
    let mut config = match &cli.config_file {
        Some(path) => AppConfig::from_yaml_file(path).unwrap_or_else(|e| {
            eprintln!("Error loading config file {}: {}", path.display(), e);
            std::process::exit(1);
        }),
        None => AppConfig::default(),
    };

    // Step 2: Apply environment variable overrides.
    // Convention: CODEX_CONV_<UPPERCASE_KEY_WITH_UNDERSCORES>
    apply_env_overrides(&mut config);

    // Step 3: Apply convenience CLI flags.
    if let Some(ref listen) = cli.listen {
        config.server.listen = listen.clone();
    }
    if let Some(ref certs) = cli.upstream_tls_extra_ca_certs {
        config.upstream.tls.extra_ca_certs =
            certs.split(',').map(|s| s.trim().to_string()).collect();
    }

    // Step 4: Apply CLI -C overrides (highest priority).
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

    // Step 5: Initialize logging.
    logging::init(&config.log);

    tracing::info!(
        listen = %config.server.listen,
        tls = config.server.tls.is_enabled(),
        "starting codex-conv"
    );

    // Step 6: Validate and load model catalog (if configured).
    let catalog = match catalog::validate_and_load_catalog(&config, cli.config_file.as_deref()) {
        Ok(c) if c.models.is_empty() => None,
        Ok(c) => {
            tracing::info!(models = c.models.len(), "loaded model catalog");
            Some(c)
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    // Step 7: Start server.
    if let Err(e) = codex_conv::server::run(config, catalog).await {
        tracing::error!(error = %e, "server exited with error");
        std::process::exit(1);
    }
}

/// Scan environment variables with `CODEX_CONV_` prefix and apply overrides.
fn apply_env_overrides(config: &mut AppConfig) {
    const PREFIX: &str = "CODEX_CONV_";
    for (key, value) in std::env::vars() {
        if let Some(rest) = key.strip_prefix(PREFIX)
            && let Err(e) = config.apply_env(rest, &value)
        {
            tracing::warn!(key = %rest, error = %e, "ignoring invalid env override");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parse_empty() {
        let cli = Cli::try_parse_from(["codex-conv"]).unwrap();
        assert!(cli.config_file.is_none());
        assert!(cli.config_items.is_empty());
        assert!(cli.listen.is_none());
        assert!(cli.upstream_tls_extra_ca_certs.is_none());
    }

    #[test]
    fn cli_parse_config_file() {
        let cli = Cli::try_parse_from(["codex-conv", "-c", "/etc/codex-conv.yaml"]).unwrap();
        assert_eq!(cli.config_file, Some(PathBuf::from("/etc/codex-conv.yaml")));
    }

    #[test]
    fn cli_parse_config_items() {
        let cli = Cli::try_parse_from([
            "codex-conv",
            "-C",
            "server.listen=0.0.0.0:9999",
            "-C",
            "log.console.level=debug",
        ])
        .unwrap();
        assert_eq!(cli.config_items.len(), 2);
        assert_eq!(cli.config_items[0], "server.listen=0.0.0.0:9999");
        assert_eq!(cli.config_items[1], "log.console.level=debug");
    }

    #[test]
    fn cli_parse_listen() {
        let cli = Cli::try_parse_from(["codex-conv", "--listen", "127.0.0.1:3000"]).unwrap();
        assert_eq!(cli.listen, Some("127.0.0.1:3000".to_string()));
    }

    #[test]
    fn cli_parse_upstream_tls_certs() {
        let cli = Cli::try_parse_from([
            "codex-conv",
            "--upstream-tls-extra-ca-certs",
            "/etc/certs/ca1.pem,/etc/certs/ca2.pem",
        ])
        .unwrap();
        assert_eq!(
            cli.upstream_tls_extra_ca_certs,
            Some("/etc/certs/ca1.pem,/etc/certs/ca2.pem".to_string())
        );
    }

    #[test]
    fn cli_parse_all_flags() {
        let cli = Cli::try_parse_from([
            "codex-conv",
            "-c",
            "config.yaml",
            "-C",
            "log.file.level=trace",
            "--listen",
            "0.0.0.0:9090",
            "--upstream-tls-extra-ca-certs",
            "/path/to/cert.pem",
        ])
        .unwrap();
        assert_eq!(cli.config_file, Some(PathBuf::from("config.yaml")));
        assert_eq!(cli.config_items, vec!["log.file.level=trace"]);
        assert_eq!(cli.listen, Some("0.0.0.0:9090".to_string()));
        assert_eq!(
            cli.upstream_tls_extra_ca_certs,
            Some("/path/to/cert.pem".to_string())
        );
    }

    #[test]
    fn cli_listen_long_form_config_file() {
        let cli = Cli::try_parse_from(["codex-conv", "--config-file", "/tmp/conv.yaml"]).unwrap();
        assert_eq!(cli.config_file, Some(PathBuf::from("/tmp/conv.yaml")));
    }
}
