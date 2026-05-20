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
        rustls_pemfile::certs(&mut &cert_pem[..]).collect::<Result<Vec<_>, _>>()?;

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

    // Load extra CA certificates and add them to the reqwest client.
    for cert_path in &config.extra_ca_certs {
        let pem = std::fs::read(cert_path)?;
        let certs: Vec<_> = rustls_pemfile::certs(&mut &pem[..]).collect::<Result<Vec<_>, _>>()?;
        if certs.is_empty() {
            return Err(format!("no certificates found in {}", cert_path).into());
        }
        for cert in certs {
            let reqwest_cert = reqwest::Certificate::from_der(cert.as_ref())?;
            builder = builder.add_root_certificate(reqwest_cert);
        }
    }

    Ok(builder)
}
