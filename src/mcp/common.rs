//! Shared types and middleware for HTTP/HTTPS MCP servers.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

use super::McpServer;
use crate::daemon::hook_runtime::WorkGate;
use crate::daemon::registry::RepoRegistry;
use crate::mcp::dispatch::DispatchContext;

#[derive(Clone, Debug)]
struct HookRuntime {
    registry: Arc<RepoRegistry>,
    dispatch: Arc<DispatchContext>,
    work_gate: Arc<WorkGate>,
}

/// Shared state for middleware.
#[derive(Clone, Debug)]
pub struct AppState {
    pub token: Option<String>,
    hook: Option<HookRuntime>,
}

/// Health check endpoint.
///
/// Returns JSON with status, version, and optional TLS flag.
#[allow(clippy::unused_async)] // axum handlers must return a Future.
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

/// Serve one lifecycle hook through the daemon's JSON-RPC dispatcher.
///
/// Native HTTP-hook hosts send the event object directly. The path supplies
/// the method name; `cwd` is promoted to `root` when the host has no mdkb-
/// specific field. The response remains the same compact JSON-RPC envelope as
/// the Unix hook socket and always uses HTTP 200, including dispatch errors.
async fn hook_handler(
    Path(path_method): Path<String>,
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let Some(runtime) = state.hook else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut params: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        Ok(_) => {
            return json_rpc_invalid_request("hook body must be a JSON object").into_response();
        }
        Err(error) => {
            return json_rpc_parse_error(&format!("parse error: {error}")).into_response();
        }
    };
    if params.get("root").is_none()
        && let Some(cwd) = params.get("cwd").cloned()
        && let Some(object) = params.as_object_mut()
    {
        object.insert("root".to_string(), cwd);
    }
    let normalized = path_method.replace('-', "_");
    let method = if normalized.starts_with("hook.") {
        normalized
    } else {
        format!("hook.{normalized}")
    };
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "method": method,
        "params": params,
    });
    let body = serde_json::to_vec(&request).expect("JSON-RPC request serializes");

    let executing = runtime.work_gate.enter().await;
    let response = crate::daemon::hook_runtime::dispatch_hook_message(
        &body,
        &runtime.registry,
        &runtime.dispatch,
    )
    .await;
    drop(executing);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response,
    )
        .into_response()
}

/// A body that parsed but is not an object: the shape is wrong, not the JSON.
///
/// The Unix hook socket answers `-32600` here and reserves `-32700` for a
/// `serde_json` failure. Both transports promise the same envelope, so this
/// one says the same thing.
fn json_rpc_invalid_request(message: &str) -> Response {
    json_rpc_error(-32600, message)
}

fn json_rpc_parse_error(message: &str) -> Response {
    json_rpc_error(-32700, message)
}

fn json_rpc_error(code: i32, message: &str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": code, "message": message},
        })
        .to_string(),
    )
        .into_response()
}

/// The router both network transports serve: `/health` open, the rmcp
/// streamable-HTTP endpoint at `/mcp` behind the bearer-token middleware.
///
/// rmcp validates the `Host` header of every `/mcp` request against
/// [`allowed_hosts`] and answers 403 to any other value. That is the
/// DNS-rebinding guard (RUSTSEC-2026-0189): a web page the operator visits
/// cannot reach this server through a name it controls.
pub(crate) fn mcp_router(
    server: McpServer,
    bind: &str,
    token: Option<&str>,
    tls_enabled: bool,
    cancellation_token: CancellationToken,
    work_gate: Arc<WorkGate>,
) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(true)
        .with_cancellation_token(cancellation_token)
        .with_allowed_hosts(allowed_hosts(bind));

    let session_manager = Arc::new(LocalSessionManager::default());

    let (registry, dispatch) = server.hook_runtime();
    let mcp_service =
        StreamableHttpService::new(move || Ok(server.clone()), session_manager, config);

    let state = AppState {
        token: token.map(String::from),
        hook: Some(HookRuntime {
            registry,
            dispatch,
            work_gate,
        }),
    };

    Router::new()
        .route(
            "/health",
            axum::routing::get(move || health_handler(tls_enabled)),
        )
        .route("/hook/{method}", axum::routing::post(hook_handler))
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// The `Host` values rmcp accepts: its loopback defaults plus the concrete
/// address in `bind`, so a server bound to one LAN address answers clients
/// that name that address. A wildcard bind (`0.0.0.0`, `[::]`) names no
/// address, so it adds nothing and such a server answers loopback clients
/// only. Entries carry no port: rmcp then accepts any port for that host.
pub fn allowed_hosts(bind: &str) -> Vec<String> {
    let mut hosts = StreamableHttpServerConfig::default().allowed_hosts;
    let bound = match bind.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_unspecified() => None,
        Ok(addr) => Some(addr.ip().to_string()),
        Err(_) => bind.rsplit_once(':').map(|(host, _)| host.to_string()),
    };
    if let Some(host) = bound
        && !hosts.contains(&host)
    {
        hosts.push(host);
    }
    hosts
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
            let provided = &auth.as_bytes()["Bearer ".len()..];
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
    use tower::ServiceExt;

    fn real_hook_router(
        root: &std::path::Path,
        token: Option<&str>,
    ) -> (Router, Arc<RepoRegistry>, Arc<DispatchContext>) {
        let server = McpServer::new(root.to_path_buf());
        let (registry, dispatch) = server.hook_runtime();
        let router = mcp_router(
            server,
            "127.0.0.1:8080",
            token,
            false,
            CancellationToken::new(),
            Arc::new(WorkGate::default()),
        );
        (router, registry, dispatch)
    }

    /// Build a test router with auth middleware and a simple OK handler.
    fn test_router(token: Option<&str>) -> Router {
        let state = AppState {
            token: token.map(String::from),
            hook: None,
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

    /// The loopback names rmcp ships stay on every list: a browser on the
    /// same machine reaches the server as `localhost` whatever it is bound as.
    fn assert_loopback(hosts: &[String]) {
        for name in ["localhost", "127.0.0.1", "::1"] {
            assert!(
                hosts.iter().any(|h| h == name),
                "{name} missing from {hosts:?}"
            );
        }
    }

    #[test]
    fn allowed_hosts_default_bind_is_loopback_only() {
        let hosts = allowed_hosts("127.0.0.1:8080");
        assert_loopback(&hosts);
        assert_eq!(hosts.len(), 3, "loopback bind adds no duplicate: {hosts:?}");
    }

    #[test]
    fn allowed_hosts_lan_bind_adds_that_address() {
        let hosts = allowed_hosts("192.168.1.20:8080");
        assert_loopback(&hosts);
        assert!(hosts.contains(&"192.168.1.20".to_string()), "{hosts:?}");
    }

    #[test]
    fn allowed_hosts_ipv6_bind_adds_bare_address() {
        // rmcp strips the brackets from the `Host` header before matching,
        // so the entry must be the bare address.
        let hosts = allowed_hosts("[fd00::7]:8080");
        assert!(hosts.contains(&"fd00::7".to_string()), "{hosts:?}");
    }

    #[test]
    fn allowed_hosts_wildcard_bind_adds_nothing() {
        for bind in ["0.0.0.0:8080", "[::]:8080"] {
            let hosts = allowed_hosts(bind);
            assert_loopback(&hosts);
            assert_eq!(
                hosts.len(),
                3,
                "{bind} must not allow every Host: {hosts:?}"
            );
        }
    }

    #[test]
    fn allowed_hosts_hostname_bind_adds_the_name() {
        let hosts = allowed_hosts("mdkb.internal:8080");
        assert!(hosts.contains(&"mdkb.internal".to_string()), "{hosts:?}");
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

    #[tokio::test]
    async fn hook_http_matches_the_unix_dispatch_for_every_lifecycle_method() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (router, registry, dispatch) = real_hook_router(&root, Some("secret"));

        for method in [
            "session_start",
            "user_prompt_submit",
            "pre_tool_use",
            "post_tool_use",
            "stop",
        ] {
            let params = serde_json::json!({
                "cwd": root,
                "session_id": format!("hook-http-{method}"),
                "prompt": "",
                "tool_name": "Read",
                "tool_input": {},
                "tool_response": {},
            });
            let request = Request::builder()
                .method("POST")
                .uri(format!("/hook/{method}"))
                .header(header::AUTHORIZATION, "Bearer secret")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(params.to_string()))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            let actual = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();

            let mut socket_params = params;
            socket_params["root"] = serde_json::json!(root);
            let socket_request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": null,
                "method": format!("hook.{method}"),
                "params": socket_params,
            });
            let expected = crate::daemon::hook_runtime::dispatch_hook_message(
                &serde_json::to_vec(&socket_request).unwrap(),
                &registry,
                &dispatch,
            )
            .await;
            assert_eq!(actual.as_ref(), expected.as_bytes(), "{method}");
        }
    }

    #[tokio::test]
    async fn hook_http_bad_token_is_rejected_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let (router, _, _) = real_hook_router(temp.path(), Some("secret"));
        let request = Request::builder()
            .method("POST")
            .uri("/hook/session_start")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::from("not json"))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn hook_http_dispatch_errors_remain_json_rpc_http_200() {
        let temp = tempfile::tempdir().unwrap();
        let (router, _, _) = real_hook_router(temp.path(), Some("secret"));
        let request = Request::builder()
            .method("POST")
            .uri("/hook/not_a_lifecycle_method")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("error").is_some(), "{json}");
    }

    #[tokio::test]
    async fn hook_http_parse_errors_are_json_rpc_http_200() {
        let temp = tempfile::tempdir().unwrap();
        let (router, _, _) = real_hook_router(temp.path(), Some("secret"));
        let request = Request::builder()
            .method("POST")
            .uri("/hook/session_start")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("not json"))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert!(json["id"].is_null());
        assert_eq!(json["error"]["code"], -32700);
        assert!(
            json["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("parse error")),
            "{json}"
        );
    }

    #[tokio::test]
    async fn standalone_hook_http_refuses_a_sibling_repo() {
        let temp = tempfile::tempdir().unwrap();
        let server_root = temp.path().join("served");
        let sibling_root = temp.path().join("sibling");
        std::fs::create_dir_all(&server_root).unwrap();
        std::fs::create_dir_all(&sibling_root).unwrap();
        crate::core::Context::init(&sibling_root).unwrap();
        let (router, _, _) = real_hook_router(&server_root, Some("secret"));
        let request = Request::builder()
            .method("POST")
            .uri("/hook/session_start")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "root": sibling_root,
                    "session_id": "outside-standalone-root"
                })
                .to_string(),
            ))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], -32602);
        assert!(
            json["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("whitelist")),
            "{json}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hook_http_work_gate_drains_an_in_flight_request() {
        let gate = Arc::new(WorkGate::default());
        let executing = gate.enter().await;
        let drain_gate = Arc::clone(&gate);
        let drain = tokio::spawn(async move {
            crate::daemon::hook_runtime::drain_in_flight_work(
                &drain_gate,
                crate::daemon::hook_runtime::WORK_DRAIN_GRACE,
            )
            .await
        });

        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(
            !drain.is_finished(),
            "in-flight hook must hold shutdown open"
        );
        drop(executing);
        assert_eq!(
            drain.await.unwrap(),
            crate::daemon::hook_runtime::DrainOutcome::Quiesced
        );
    }
}
