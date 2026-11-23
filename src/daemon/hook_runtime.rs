//! Cross-platform hook dispatch and graceful work draining.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::mcp::dispatch::{DispatchContext, dispatch_call};

use super::registry::RepoRegistry;

/// Max time to wait for hook requests that are already executing.
///
/// `mdkb update` on a large repository legitimately runs for minutes, and the
/// CLI that asked for it waits as long as the daemon answers `ping`.
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

/// A method that ran and was refused by the store before it wrote anything:
/// an unknown memory id or document, a duplicate title, an evolution cycle, an
/// invalid field. The answer is final. The caller prints it and does not run
/// the request itself, which would only fail the same way after loading the
/// store a second time. See [`crate::error::ErrorKind::is_validation_refusal`].
pub const REJECTED_ERROR_CODE: i32 = -32010;

/// Method `mdkb status` uses to ask the running daemon which repos it
/// knows. Like `ping` it names no repository, so it is answered before the
/// `params.root` requirement and never opens a store.
pub const REPOS_METHOD: &str = "daemon.repos";

/// Method `mdkb repos prune` uses to drop Gone roots from the daemon's own map.
/// Names no repository and opens no store, like [`REPOS_METHOD`]. Params:
/// `{"paths": [absolute path, ...], "dry_run": bool}`; no or empty `paths`
/// selects every Gone root. A daemon without this method answers `-32602`
/// ("missing 'params.root'") before dispatch: a refusal that wrote nothing.
pub const PRUNE_METHOD: &str = "daemon.repos_prune";

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

    if method == REPOS_METHOD {
        let owner = Arc::clone(registry);
        let rows = match tokio::task::spawn_blocking(move || owner.listed_rows()).await {
            Ok(rows) => rows,
            Err(error) => {
                return rpc_error(id, -32603, &format!("repos: listing task failed: {error}"));
            }
        };
        let roots: Vec<_> = rows.iter().map(|row| &row.path).collect();
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "roots": roots,
                "rows": rows,
            },
        })
        .to_string();
    }

    if method == PRUNE_METHOD {
        return prune_reply(id, &params, registry).await;
    }

    let Some(root) = params.get("root").and_then(Value::as_str) else {
        return rpc_error(id, -32602, "missing 'params.root' (absolute repo path)");
    };

    let root = Path::new(root);
    if !root.is_absolute() {
        return rpc_error(
            id,
            -32602,
            "'params.root' must be a non-empty absolute repo path",
        );
    }

    // Validate typed mutations before repository acquisition and dispatch so a
    // malformed request remains an admission refusal, safe for local fallback.
    if method == "cli.mutate"
        && let Err(error) =
            serde_json::from_value::<crate::core::cli_mutation::CliMutation>(params.clone())
    {
        return rpc_error(id, -32602, &format!("cli.mutate: invalid params: {error}"));
    }

    if matches!(
        method,
        "external.upsert" | "external.delete" | "external.list"
    ) && let Err(error) = crate::store::external::Request::parse(method, params.clone())
    {
        return rpc_error(id, -32602, &format!("{method}: invalid params: {error}"));
    }

    let handle = match registry.get_or_open(root) {
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

/// Answer [`PRUNE_METHOD`]. Malformed params are refused before the map is
/// touched (`-32602`); a failed write is the post-dispatch code, because the
/// in-memory map was changed and restored on the way.
async fn prune_reply(id: Value, params: &Value, registry: &Arc<RepoRegistry>) -> String {
    let dry_run = match params.get("dry_run") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return rpc_error(
                id,
                -32602,
                "daemon.repos_prune: 'dry_run' must be a boolean",
            );
        }
    };
    let paths: Vec<std::path::PathBuf> = match params.get("paths") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => match serde_json::from_value(value.clone()) {
            Ok(paths) => paths,
            Err(error) => {
                return rpc_error(
                    id,
                    -32602,
                    &format!("daemon.repos_prune: bad 'paths': {error}"),
                );
            }
        },
    };
    if let Some(relative) = paths.iter().find(|p| !p.is_absolute()) {
        return rpc_error(
            id,
            -32602,
            &format!(
                "daemon.repos_prune: '{}' is not an absolute path",
                relative.display()
            ),
        );
    }
    let owner = Arc::clone(registry);
    let outcome = tokio::task::spawn_blocking(move || {
        let selected = (!paths.is_empty()).then_some(paths.as_slice());
        owner.prune_gone_roots(selected, dry_run)
    })
    .await;
    match outcome {
        Ok(Ok(report)) => json!({"jsonrpc": "2.0", "id": id, "result": report}).to_string(),
        Ok(Err(why)) => rpc_error(id, DISPATCHED_ERROR_CODE, &why),
        Err(error) => rpc_error(
            id,
            DISPATCHED_ERROR_CODE,
            &format!("prune task failed: {error}"),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::config::DaemonConfig;
    use crate::daemon::registry::RepoHandle;
    use crate::metrics::UsageMetrics;
    use std::sync::atomic::{AtomicI64, AtomicU64};

    // Run in a child to keep cwd changes out of parallel tests. Git itself
    // creates the .git pointer that empty-path worktree resolution follows.
    fn in_git_worktree(test: &str) -> bool {
        if std::env::var_os("MDKB_EMPTY_ROOT_CHILD").is_some() {
            assert!(Path::new(".git").is_file());
            return true;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let main = tmp.path().join("main");
        let worktree = tmp.path().join("worktree");
        let git = |args: &[&std::ffi::OsStr]| {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init".as_ref(), main.as_os_str()]);
        git(&[
            "-C".as_ref(),
            main.as_os_str(),
            "-c".as_ref(),
            "user.name=Test".as_ref(),
            "-c".as_ref(),
            "user.email=test@example.com".as_ref(),
            "-c".as_ref(),
            "commit.gpgsign=false".as_ref(),
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-m".as_ref(),
            "fixture".as_ref(),
        ]);
        git(&[
            "-C".as_ref(),
            main.as_os_str(),
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            worktree.as_os_str(),
        ]);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("MDKB_EMPTY_ROOT_CHILD", "1")
            .current_dir(&worktree)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    fn registry() -> Arc<RepoRegistry> {
        Arc::new(RepoRegistry::new(DaemonConfig {
            whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().into_owned()],
            ..DaemonConfig::default()
        }))
    }

    /// Catches: blank or relative RPC roots selecting the daemon cwd repository.
    #[tokio::test]
    async fn hook_rejects_empty_and_relative_roots_before_repository_admission() {
        if !in_git_worktree(
            "daemon::hook_runtime::tests::hook_rejects_empty_and_relative_roots_before_repository_admission",
        ) {
            return;
        }
        let registry = registry();
        let dctx = Arc::new(DispatchContext {
            metrics: Arc::new(UsageMetrics::new()),
            session_id: Arc::new(AtomicI64::new(0)),
            persistent_call_count: Arc::new(AtomicU64::new(0)),
            optimize_interval_calls: 200,
            hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
            background: None,
        });
        for root in ["", ".", "..", "relative", " "] {
            let body =
                json!({"jsonrpc": "2.0", "id": 7, "method": "status", "params": {"root": root}});
            let response =
                dispatch_hook_message(body.to_string().as_bytes(), &registry, &dctx).await;
            let reply: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(reply["id"], 7);
            assert_eq!(reply["error"]["code"], -32602, "root={root:?}: {reply}");
            assert_eq!(
                reply["error"]["message"], "'params.root' must be a non-empty absolute repo path",
                "root={root:?}: {reply}"
            );
            assert_eq!(registry.active_count(), 0);
            assert_eq!(registry.listed_roots(), [] as [std::path::PathBuf; 0]);
        }
    }

    /// Catches: a blank registry root opening or returning the cwd worktree handle.
    #[test]
    fn registry_rejects_empty_roots_even_with_a_cached_cwd_repository() {
        if !in_git_worktree(
            "daemon::hook_runtime::tests::registry_rejects_empty_roots_even_with_a_cached_cwd_repository",
        ) {
            return;
        }
        let registry = registry();
        let cwd = std::env::current_dir().unwrap();
        registry.get_or_open(&cwd).unwrap();
        let empty = Path::new("");
        assert!(registry.get(empty).is_none());
        assert_eq!(
            registry.get_or_open(empty).unwrap_err().to_string(),
            "Repo path must not be empty"
        );
        assert_eq!(
            RepoHandle::open(empty, &DaemonConfig::default())
                .unwrap_err()
                .to_string(),
            "Repo path must not be empty"
        );
        assert_eq!(registry.active_count(), 1);
    }

    fn state_registry(state: &Path) -> Arc<RepoRegistry> {
        Arc::new(RepoRegistry::new(DaemonConfig {
            state_dir: Some(state.to_path_buf()),
            whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().into_owned()],
            ..DaemonConfig::default()
        }))
    }

    async fn call(registry: &Arc<RepoRegistry>, method: &str, params: Value) -> Value {
        let dctx = Arc::new(DispatchContext {
            metrics: Arc::new(UsageMetrics::new()),
            session_id: Arc::new(AtomicI64::new(0)),
            persistent_call_count: Arc::new(AtomicU64::new(0)),
            optimize_interval_calls: 200,
            hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
            background: None,
        });
        let body = json!({"jsonrpc": "2.0", "id": 3, "method": method, "params": params});
        let reply = dispatch_hook_message(body.to_string().as_bytes(), registry, &dctx).await;
        serde_json::from_str(&reply).unwrap()
    }

    /// Catches: the daemon's own map keeping a root that vanished while it ran
    /// (what the CLI sees in `repos list`), a dry run that mutates, and a
    /// prune that requires `params.root` like a repo method.
    #[tokio::test]
    async fn prune_method_drops_a_vanished_root_from_the_live_daemon_map() {
        let state = tempfile::TempDir::new().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let (stale, keep) = (tmp.path().join("stale"), tmp.path().join("keep"));
        for r in [&stale, &keep] {
            std::fs::create_dir_all(r.join(".mdkb")).unwrap();
        }
        let registry = state_registry(state.path());
        registry.get_or_open(&stale).unwrap();
        registry.get_or_open(&keep).unwrap();
        let stale = stale.canonicalize().unwrap();
        std::fs::remove_dir_all(&stale).unwrap();

        let dry = call(
            &registry,
            PRUNE_METHOD,
            json!({"paths": [stale], "dry_run": true}),
        )
        .await;
        assert_eq!(dry["result"]["dry_run"], true);
        assert_eq!(dry["result"]["removed"], json!([stale]));
        assert!(registry.listed_roots().contains(&stale));

        let done = call(&registry, PRUNE_METHOD, json!({"paths": [stale]})).await;
        assert_eq!(done["result"]["removed"], json!([stale]));
        let listed = registry.listed_roots();
        assert!(!listed.contains(&stale) && listed.contains(&keep.canonicalize().unwrap()));
        let persisted = std::fs::read_to_string(state.path().join("repos.json")).unwrap();
        assert!(
            !persisted.contains(stale.to_string_lossy().as_ref()),
            "{persisted}"
        );
    }

    /// Catches a relative or malformed selection reaching the map, where it
    /// would be resolved against the daemon's cwd: with a live Gone root and a
    /// real state dir, nothing is removed and `repos.json` is byte-identical.
    #[tokio::test]
    async fn prune_method_refuses_relative_paths_and_bad_params_before_any_change() {
        let state = tempfile::TempDir::new().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let stale = tmp.path().join("stale");
        std::fs::create_dir_all(stale.join(".mdkb")).unwrap();
        let registry = state_registry(state.path());
        registry.get_or_open(&stale).unwrap();
        let stale = stale.canonicalize().unwrap();
        std::fs::remove_dir_all(&stale).unwrap();
        let file = state.path().join("repos.json");
        let before = std::fs::read(&file).unwrap();
        for params in [
            json!({"paths": ["relative/dir"]}),
            json!({"paths": 7}),
            json!({"dry_run": "yes"}),
        ] {
            let reply = call(&registry, PRUNE_METHOD, params.clone()).await;
            assert_eq!(reply["error"]["code"], -32602, "{params}: {reply}");
            assert!(registry.listed_roots().contains(&stale), "{params}");
            assert_eq!(std::fs::read(&file).unwrap(), before, "{params}");
        }
    }

    /// Recorded from this dispatcher, which is what a daemon built before
    /// `daemon.repos_prune` also runs for any method it does not know: the
    /// refusal the CLI must read as "daemon too old", not as a failed prune.
    #[tokio::test]
    async fn a_method_the_daemon_does_not_know_is_a_refusal_the_cli_recognizes() {
        let reply = call(&registry(), "daemon.no_such_method", json!({})).await;
        let (code, message) = (
            &reply["error"]["code"],
            reply["error"]["message"].as_str().unwrap(),
        );
        assert_eq!(code, -32602);
        assert!(crate::cli::repos::daemon_lacks_prune(&format!(
            "daemon error {code}: {message}"
        )));
        assert!(!crate::cli::repos::daemon_lacks_prune(
            "daemon error -32603: could not persist"
        ));
    }
}
