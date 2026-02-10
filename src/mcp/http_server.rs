//! HTTP transport for the MCP server using axum + rmcp streamable HTTP.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

use super::McpServer;

/// Shared state for middleware.
#[derive(Clone)]
struct AppState {
    token: Option<String>,
}

/// Run the HTTP MCP server.
pub async fn run_http_server(
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
        .route("/health", axum::routing::get(health_handler))
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| crate::error::Error::mcp(format!("Failed to bind to {bind}: {e}")))?;

    tracing::info!("Starting mdkb MCP HTTP server on {bind}");
    eprintln!("mdkb MCP server listening on http://{bind}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal(cancellation_token))
        .await
        .map_err(|e| crate::error::Error::mcp(format!("HTTP server error: {e}")))?;

    Ok(())
}

/// Health check endpoint.
async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Bearer token authentication middleware.
///
/// If no token is configured, all requests are allowed.
/// The /health endpoint is always accessible without auth.
async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Always allow health checks without auth
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    // If no token configured, allow all requests
    let Some(expected_token) = &state.token else {
        return next.run(request).await;
    };

    // Check Authorization header
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(auth) if auth.starts_with("Bearer ") => {
            let provided = auth["Bearer ".len()..].as_bytes();
            let expected = expected_token.as_bytes();
            if provided.ct_eq(expected).into() {
                next.run(request).await
            } else {
                (StatusCode::UNAUTHORIZED, "Invalid bearer token").into_response()
            }
        }
        _ => (StatusCode::UNAUTHORIZED, "Bearer token required").into_response(),
    }
}

/// Wait for shutdown signal (Ctrl+C).
async fn shutdown_signal(cancellation_token: CancellationToken) {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install Ctrl+C handler");
    tracing::info!("Shutdown signal received, stopping server...");
    cancellation_token.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    /// Build a test router with auth middleware and a simple OK handler.
    fn test_router(token: Option<&str>) -> Router {
        let state = AppState {
            token: token.map(String::from),
        };
        Router::new()
            .route("/health", axum::routing::get(health_handler))
            .route("/mcp", axum::routing::get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state)
    }

    /// Send a request to the router and return the response status.
    async fn send_request(
        router: Router,
        uri: &str,
        auth_header: Option<&str>,
    ) -> StatusCode {
        let mut req_builder = Request::builder().uri(uri).method("GET");
        if let Some(auth) = auth_header {
            req_builder = req_builder.header(header::AUTHORIZATION, auth);
        }
        let request = req_builder.body(Body::empty()).unwrap();
        let response = router.oneshot(request).await.unwrap();
        response.status()
    }

    #[tokio::test]
    async fn test_auth_no_token_configured_allows_all() {
        let router = test_router(None);
        assert_eq!(send_request(router, "/mcp", None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_health_bypasses_token_check() {
        let router = test_router(Some("secret"));
        assert_eq!(send_request(router, "/health", None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_valid_token_allowed() {
        let router = test_router(Some("secret"));
        assert_eq!(
            send_request(router, "/mcp", Some("Bearer secret")).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_auth_invalid_token_rejected() {
        let router = test_router(Some("secret"));
        assert_eq!(
            send_request(router, "/mcp", Some("Bearer wrong")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn test_auth_missing_header_rejected() {
        let router = test_router(Some("secret"));
        assert_eq!(
            send_request(router, "/mcp", None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn test_auth_non_bearer_scheme_rejected() {
        let router = test_router(Some("secret"));
        assert_eq!(
            send_request(router, "/mcp", Some("Basic dXNlcjpwYXNz")).await,
            StatusCode::UNAUTHORIZED
        );
    }
}
