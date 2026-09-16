//! Cross-platform hook dispatch and graceful work draining.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::mcp::dispatch::{DispatchContext, dispatch_call};

use super::registry::RepoRegistry;

/// Max time to wait for hook requests that are already executing.
///
/// `mdkb update` on a large repository legitimately runs for minutes, and the
/// CLI that asked for it budgets an hour (`cli::hook_client::MUTATION_TIMEOUT`).
/// Draining it under the five-second grace sized for idle sockets cut it off and
/// reported a failed mutation for a write the daemon then finished anyway while
/// the runtime dropped its blocking pool — the client saw `early eof`, the index
/// was updated. Ten minutes covers the work while staying under the caller's own
/// deadline. An operator who will not wait sends a second signal, which exits
/// the process outright (see `main::ShutdownSignals`) — stopping the wait alone
/// would not, because the runtime still owns the blocking write.
pub(crate) const WORK_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(600);

/// Marks the window during which a hook request is executing.
///
/// Every dispatched request holds it shared; shutdown takes it exclusively,
/// which resolves exactly when no handler is mid-flight. An idle connection
/// therefore cannot delay shutdown, while an executing mutation is protected.
#[derive(Debug, Default)]
pub(crate) struct WorkGate(tokio::sync::RwLock<()>);

impl WorkGate {
    /// Hold the gate for as long as the returned guard lives.
    pub(crate) async fn enter(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.0.read().await
    }

    /// Resolve once every holder has released it. Tokio's `RwLock` is
    /// write-preferring, so a steady stream of requests cannot starve this.
    pub(crate) async fn quiesced(&self) {
        let _exclusive = self.0.write().await;
    }
}

/// How [`drain_in_flight_work`] stopped waiting.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DrainOutcome {
    /// No handler is executing any more.
    Quiesced,
    /// The grace period elapsed with work still running.
    TimedOut,
}

/// Wait for executing hook requests to finish, bounded by `grace`.
///
/// There is deliberately no in-band cancel here. Returning early would not end
/// the process: `Runtime::drop` waits for `spawn_blocking` work that has already
/// started, and that work — the document update inside `update_impl` — is
/// exactly what a long drain is waiting for. An operator who will not wait is
/// served by `main`, which exits the process outright on a second signal.
pub(crate) async fn drain_in_flight_work(
    gate: &WorkGate,
    grace: std::time::Duration,
) -> DrainOutcome {
    tokio::select! {
        biased;
        () = gate.quiesced() => DrainOutcome::Quiesced,
        () = tokio::time::sleep(grace) => DrainOutcome::TimedOut,
    }
}

/// The only error code emitted after a method has entered dispatch.
///
/// Every other code is a refusal that wrote nothing — a parse failure, a missing
/// `root`, an unknown method, a repo outside the whitelist, a malformed typed
/// mutation. `cli::hook_client` keys on exactly that: a refusal lets the caller
/// run the mutation itself, and this code does not, because a method that got as
/// far as running may have written before it failed.
///
/// Pinned by `refusals_that_wrote_nothing_never_use_the_dispatched_error_code`.
/// Anything that widens this — a second post-dispatch code, or reusing this one
/// for a refusal — silently changes when the CLI is allowed to write.
pub const DISPATCHED_ERROR_CODE: i32 = -32603;

/// Parse a JSON-RPC request and route it through the shared dispatch layer.
///
/// `params.root` is the absolute target repository path for every method except
/// `ping`; registry acquisition enforces the daemon whitelist.
pub(crate) async fn dispatch_hook_message(
    body: &[u8],
    registry: &Arc<RepoRegistry>,
    dctx: &Arc<DispatchContext>,
) -> String {
    let req: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(error) => return rpc_error(Value::Null, -32700, &format!("parse error: {error}")),
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    if method == "ping" {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "pong": true,
                "version": env!("CARGO_PKG_VERSION")
            }
        })
        .to_string();
    }

    if method.is_empty() {
        return rpc_error(id, -32600, "missing 'method'");
    }

    let Some(root) = params.get("root").and_then(Value::as_str) else {
        return rpc_error(id, -32602, "missing 'params.root' (absolute repo path)");
    };

    // Validate typed mutations before repository acquisition and dispatch so a
    // malformed request remains an admission refusal, safe for local fallback.
    if method == "cli.mutate"
        && let Err(error) =
            serde_json::from_value::<crate::core::cli_mutation::CliMutation>(params.clone())
    {
        return rpc_error(id, -32602, &format!("cli.mutate: invalid params: {error}"));
    }

    let handle = match registry.get_or_open(Path::new(root)) {
        Ok(handle) => handle,
        Err(error) => return rpc_error(id, -32602, &format!("repo registry: {error}")),
    };

    match dispatch_call(method, params, handle, dctx).await {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string(),
        Err(error) => rpc_error(id, error.code.0, &error.message),
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
    .to_string()
}
