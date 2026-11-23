//! HTTPS transport for the MCP server with self-signed certificate generation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig;
use chrono::Datelike;
use tokio_util::sync::CancellationToken;

use super::McpServer;
use super::common::mcp_router;

/// Run the HTTPS MCP server with self-signed certificates.
pub async fn run_https_server(
    server: McpServer,
    bind: &str,
    token: Option<&str>,
) -> crate::error::Result<()> {
    let cancellation_token = CancellationToken::new();
    let work_gate = Arc::new(crate::daemon::hook_runtime::WorkGate::default());
    let router = mcp_router(
        server,
        bind,
        token,
        true,
        cancellation_token.clone(),
        Arc::clone(&work_gate),
    );

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
        if let Err(e) = super::wait_for_shutdown_signal().await {
            tracing::warn!("signal: {e}");
        }
        tracing::info!("Shutdown signal received, stopping HTTPS server...");
        cancellation_token.cancel();
        shutdown_handle.graceful_shutdown(Some(
            crate::daemon::hook_runtime::WORK_DRAIN_GRACE + std::time::Duration::from_secs(5),
        ));
    });

    axum_server::bind_rustls(addr, tls_config)
        .handle(server_handle)
        .serve(router.into_make_service())
        .await
        .map_err(|e| crate::error::Error::mcp(format!("HTTPS server error: {e}")))?;

    // On the return path, like the HTTP transport — not in the detached
    // listener above. `main` gives the runtime one second after this function
    // returns, and axum-server counts connections, not the `spawn_blocking`
    // write a handler is still holding the gate across. Draining here is what
    // makes the return mean the writes finished.
    let _ = crate::daemon::hook_runtime::drain_in_flight_work(
        &work_gate,
        crate::daemon::hook_runtime::WORK_DRAIN_GRACE,
    )
    .await;

    Ok(())
}

/// A cached certificate that expires within this window is replaced on start,
/// so a running server does not reach its expiry date shortly after start.
const RENEW_BEFORE_DAYS: i64 = 30;

/// Lifetime of a generated certificate.
const VALID_DAYS: i64 = 365;

/// Ensure a valid self-signed certificate exists, generating one if needed.
///
/// Certificates are cached in `~/.mdkb/certs/`, next to the rest of mdkb's
/// machine-wide state.
fn ensure_self_signed_cert() -> crate::error::Result<(PathBuf, PathBuf)> {
    let cert_dir = crate::DaemonConfig::daemon_home().join("certs");
    ensure_cert_in(&cert_dir, chrono::Utc::now())
}

/// Reuse the certificate in `cert_dir` while it stays valid past
/// `now + RENEW_BEFORE_DAYS`; otherwise write a new one.
fn ensure_cert_in(
    cert_dir: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<(PathBuf, PathBuf)> {
    let cert_path = cert_dir.join("server.pem");
    let key_path = cert_dir.join("server-key.pem");

    if key_path.exists() {
        match cert_not_after(&cert_path) {
            Some(not_after) if not_after > now + chrono::Duration::days(RENEW_BEFORE_DAYS) => {
                tracing::info!("Using existing self-signed certificate");
                return Ok((cert_path, key_path));
            }
            Some(not_after) => {
                tracing::info!("Self-signed certificate expires {not_after}; regenerating");
            }
            None if cert_path.exists() => {
                tracing::warn!("Cannot read {}; regenerating", cert_path.display());
            }
            None => {}
        }
    }

    write_self_signed_cert(cert_dir, now, now + chrono::Duration::days(VALID_DAYS))?;
    Ok((cert_path, key_path))
}

/// Expiry date of the PEM certificate at `path`, or `None` when it is missing
/// or does not parse.
fn cert_not_after(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let pem = std::fs::read_to_string(path).ok()?;
    let params = rcgen::CertificateParams::from_ca_cert_pem(&pem).ok()?;
    chrono::DateTime::from_timestamp(params.not_after.unix_timestamp(), 0)
}

/// Write `server.pem` and `server-key.pem` into `cert_dir`.
fn write_self_signed_cert(
    cert_dir: &Path,
    not_before: chrono::DateTime<chrono::Utc>,
    not_after: chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<()> {
    let cert_path = cert_dir.join("server.pem");
    let key_path = cert_dir.join("server-key.pem");

    std::fs::create_dir_all(cert_dir)
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

    let ymd = |d: chrono::DateTime<chrono::Utc>| {
        rcgen::date_time_ymd(d.year(), d.month() as u8, d.day() as u8)
    };
    params.not_before = ymd(not_before);
    params.not_after = ymd(not_after);

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

    tracing::info!(
        "Self-signed certificate generated at {}",
        cert_dir.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(n: i64) -> chrono::Duration {
        chrono::Duration::days(n)
    }

    #[test]
    fn expired_cached_certificate_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        write_self_signed_cert(dir.path(), now - days(400), now - days(35)).unwrap();

        let (cert_path, _) = ensure_cert_in(dir.path(), now).unwrap();

        let not_after = cert_not_after(&cert_path).unwrap();
        assert!(
            not_after > now + days(360),
            "expiry {not_after} was not renewed"
        );
    }

    #[test]
    fn certificate_inside_the_renewal_window_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        write_self_signed_cert(dir.path(), now - days(355), now + days(10)).unwrap();

        let (cert_path, _) = ensure_cert_in(dir.path(), now).unwrap();

        assert!(cert_not_after(&cert_path).unwrap() > now + days(360));
    }

    #[test]
    fn valid_cached_certificate_is_reused_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        write_self_signed_cert(dir.path(), now, now + days(200)).unwrap();
        let cert_before = std::fs::read(dir.path().join("server.pem")).unwrap();
        let key_before = std::fs::read(dir.path().join("server-key.pem")).unwrap();

        let (cert_path, key_path) = ensure_cert_in(dir.path(), now).unwrap();

        assert_eq!(std::fs::read(cert_path).unwrap(), cert_before);
        assert_eq!(std::fs::read(key_path).unwrap(), key_before);
    }

    #[test]
    fn unreadable_cached_certificate_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.pem"), "not a certificate").unwrap();
        std::fs::write(dir.path().join("server-key.pem"), "not a key").unwrap();

        let (cert_path, _) = ensure_cert_in(dir.path(), chrono::Utc::now()).unwrap();

        assert!(cert_not_after(&cert_path).is_some());
    }
}
