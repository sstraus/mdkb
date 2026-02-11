//! HTTPS transport for the MCP server with self-signed certificate generation.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum_server::tls_rustls::RustlsConfig;
use chrono::Datelike;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use tokio_util::sync::CancellationToken;

use super::McpServer;
use super::common::{AppState, auth_middleware, health_handler};

/// Run the HTTPS MCP server with self-signed certificates.
pub async fn run_https_server(
    server: McpServer,
    bind: &str,
    token: Option<&str>,
) -> crate::error::Result<()> {
    let cancellation_token = CancellationToken::new();

    let config = StreamableHttpServerConfig {
        stateful_mode: true,
        cancellation_token: cancellation_token.clone(),
        ..Default::default()
    };

    let session_manager = Arc::new(LocalSessionManager::default());

    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        session_manager,
        config,
    );

    let state = AppState {
        token: token.map(String::from),
    };

    let router = Router::new()
        .route("/health", axum::routing::get(|| health_handler(true)))
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    // Generate or load self-signed certificate
    let (cert_path, key_path) = ensure_self_signed_cert()?;

    let tls_config = RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .map_err(|e| crate::error::Error::mcp(format!("Failed to load TLS config: {e}")))?;

    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| crate::error::Error::mcp(format!("Invalid bind address '{bind}': {e}")))?;

    tracing::info!("Starting mdkb MCP HTTPS server on {bind}");
    eprintln!("mdkb MCP server listening on https://{bind}/mcp");
    eprintln!("Using self-signed certificate at {}", cert_path.display());

    let server_handle = axum_server::Handle::new();
    let shutdown_handle = server_handle.clone();

    // Spawn shutdown listener
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        tracing::info!("Shutdown signal received, stopping HTTPS server...");
        cancellation_token.cancel();
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });

    axum_server::bind_rustls(addr, tls_config)
        .handle(server_handle)
        .serve(router.into_make_service())
        .await
        .map_err(|e| crate::error::Error::mcp(format!("HTTPS server error: {e}")))?;

    Ok(())
}

/// Ensure a self-signed certificate exists, generating one if needed.
///
/// Certificates are cached in `~/.config/mdkb/certs/`.
fn ensure_self_signed_cert() -> crate::error::Result<(PathBuf, PathBuf)> {
    let cert_dir = directories::ProjectDirs::from("", "", "mdkb")
        .map(|dirs| dirs.config_dir().join("certs"))
        .unwrap_or_else(|| PathBuf::from(".mdkb/certs"));

    let cert_path = cert_dir.join("server.pem");
    let key_path = cert_dir.join("server-key.pem");

    // Return existing cert if it exists and is not expired
    if cert_path.exists() && key_path.exists() {
        tracing::info!("Using existing self-signed certificate");
        return Ok((cert_path, key_path));
    }

    std::fs::create_dir_all(&cert_dir)
        .map_err(|e| crate::error::Error::mcp(format!("Failed to create cert directory: {e}")))?;

    tracing::info!("Generating self-signed certificate...");

    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .map_err(|e| crate::error::Error::mcp(format!("Failed to create cert params: {e}")))?;

    // Add SANs for local development
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        )));
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V6(
            std::net::Ipv6Addr::LOCALHOST,
        )));

    // Valid for 365 days from now
    let now = chrono::Utc::now();
    let expiry = now + chrono::Duration::days(365);
    params.not_after = rcgen::date_time_ymd(
        expiry.year(),
        expiry.month() as u8,
        expiry.day() as u8,
    );

    let key_pair = rcgen::KeyPair::generate()
        .map_err(|e| crate::error::Error::mcp(format!("Failed to generate key pair: {e}")))?;

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| crate::error::Error::mcp(format!("Failed to generate certificate: {e}")))?;

    std::fs::write(&cert_path, cert.pem())
        .map_err(|e| crate::error::Error::mcp(format!("Failed to write certificate: {e}")))?;

    // Write private key with restricted permissions (owner read/write only)
    std::fs::write(&key_path, key_pair.serialize_pem())
        .map_err(|e| crate::error::Error::mcp(format!("Failed to write private key: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&key_path)
            .map_err(|e| crate::error::Error::mcp(format!("Failed to read key metadata: {e}")))?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&key_path, perms)
            .map_err(|e| crate::error::Error::mcp(format!("Failed to set key permissions: {e}")))?;
    }

    tracing::info!("Self-signed certificate generated at {}", cert_dir.display());

    Ok((cert_path, key_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_self_signed_cert() {
        // This test generates a real cert in the default location.
        // Skip in CI or if we want to avoid side effects.
        let result = ensure_self_signed_cert();
        assert!(result.is_ok());
        let (cert_path, key_path) = result.unwrap();
        assert!(cert_path.exists());
        assert!(key_path.exists());
    }
}
