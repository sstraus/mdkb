//! Shared types and middleware for HTTP/HTTPS MCP servers.

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use subtle::ConstantTimeEq;

/// Shared state for middleware.
#[derive(Clone, Debug)]
pub struct AppState {
    pub token: Option<String>,
}

/// Health check endpoint.
///
/// Returns JSON with status, version, and optional TLS flag.
pub async fn health_handler(tls_enabled: bool) -> impl IntoResponse {
    let mut response = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    });

    if tls_enabled {
        response["tls"] = serde_json::json!(true);
    }

    Json(response)
}

/// Bearer token authentication middleware.
///
/// If no token is configured, all requests are allowed.
/// The /health endpoint is always accessible without auth.
pub async fn auth_middleware(
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::middleware;
    use tower::ServiceExt;

    /// Build a test router with auth middleware and a simple OK handler.
    fn test_router(token: Option<&str>) -> Router {
        let state = AppState {
            token: token.map(String::from),
        };
        Router::new()
            .route("/health", axum::routing::get(|| health_handler(false)))
            .route("/mcp", axum::routing::get(|| async { StatusCode::OK }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state)
    }

    /// Send a request to the router and return the response status.
    async fn send_request(router: Router, uri: &str, auth_header: Option<&str>) -> StatusCode {
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
