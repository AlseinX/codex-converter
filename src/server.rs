use crate::config::AppConfig;
use crate::router::{build_router, AppState};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::time::Duration;

/// Run the HTTP/HTTPS server until a shutdown signal is received.
pub async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = config.server.listen.parse()?;
    let state = AppState {
        config: config.clone(),
    };
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
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(tls_config);

    tracing::info!(%addr, "listening (HTTPS)");

    let timeout = Duration::from_secs(config.server.shutdown_timeout);
    let handle = axum_server::Handle::new();

    // Spawn shutdown signal watcher.
    let shutdown_handle = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_handle.graceful_shutdown(Some(timeout));
    });

    axum_server::bind_rustls(addr, rustls_config)
        .handle(handle)
        .serve(app.into_make_service())
        .await?;

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
