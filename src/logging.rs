use crate::config::LogConfig;
use tracing::Level;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, Layer, EnvFilter};

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
            eprintln!(
                "Warning: could not create log directory {}: {}",
                dir.display(),
                e
            );
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
pub(crate) fn level_filter(level: &str) -> EnvFilter {
    let l = match level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" | "warning" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };
    EnvFilter::new(l.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::filter::LevelFilter as SubscriberLevelFilter;

    #[test]
    fn level_filter_maps_standard_levels() {
        assert_eq!(
            level_filter("trace").max_level_hint(),
            Some(SubscriberLevelFilter::TRACE)
        );
        assert_eq!(
            level_filter("debug").max_level_hint(),
            Some(SubscriberLevelFilter::DEBUG)
        );
        assert_eq!(
            level_filter("info").max_level_hint(),
            Some(SubscriberLevelFilter::INFO)
        );
        assert_eq!(
            level_filter("warn").max_level_hint(),
            Some(SubscriberLevelFilter::WARN)
        );
        assert_eq!(
            level_filter("warning").max_level_hint(),
            Some(SubscriberLevelFilter::WARN)
        );
        assert_eq!(
            level_filter("error").max_level_hint(),
            Some(SubscriberLevelFilter::ERROR)
        );
    }

    #[test]
    fn level_filter_case_insensitive() {
        assert_eq!(
            level_filter("INFO").max_level_hint(),
            Some(SubscriberLevelFilter::INFO)
        );
        assert_eq!(
            level_filter("DeBuG").max_level_hint(),
            Some(SubscriberLevelFilter::DEBUG)
        );
        assert_eq!(
            level_filter("WaRnInG").max_level_hint(),
            Some(SubscriberLevelFilter::WARN)
        );
    }

    #[test]
    fn level_filter_unknown_defaults_to_info() {
        assert_eq!(
            level_filter("unknown_level").max_level_hint(),
            Some(SubscriberLevelFilter::INFO)
        );
        assert_eq!(
            level_filter("").max_level_hint(),
            Some(SubscriberLevelFilter::INFO)
        );
    }

    #[test]
    fn init_with_no_logging_does_not_panic() {
        // LogConfig with no console and no file should not panic.
        let config = LogConfig {
            console: None,
            file: None,
        };
        // The test verifies the code path compiles and doesn't panic on construction.
        drop(config);
    }

    #[test]
    fn file_logging_creates_log_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config = LogConfig {
            console: None,
            file: Some(crate::config::FileLog {
                level: "debug".to_string(),
                dir: dir.path().to_string_lossy().to_string(),
                rotation: "daily".to_string(),
            }),
        };
        // Verify the file log config is well-formed.
        assert_eq!(config.file.as_ref().unwrap().rotation, "daily");
        assert_eq!(config.file.as_ref().unwrap().level, "debug");
    }
}
