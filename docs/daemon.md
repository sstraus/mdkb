# Daemon and network transport

One mdkb daemon per user serves every repository: MCP sessions through
`mdkb mcp`, the hooks, and the CLI write path. This page covers how to run it,
how to control it, and its machine-wide configuration. The socket protocol is
in [daemon-rpc.md](daemon-rpc.md); cross-repository behaviour is in the
[README](../README.md#cross-repository-search).

## Daemon Control

```bash
mdkb status    # one page; the Daemon section shows pid, uptime and repos held
mdkb daemon stop      # send SIGTERM and wait until it has shut down
mdkb daemon restart   # stop if running, then start it again detached
```

The daemon finishes in-flight work before it exits. `mdkb mcp` and the hooks
start it again on their next call. Settings in `~/.mdkb/daemon.toml` apply
after a restart (see [Daemon configuration](#daemon-configuration-mdkbdaemontoml) below).

## Network transport (`mdkb serve`)

`mdkb serve` with no flags is the legacy in-process stdio server; use `mdkb mcp`
for new MCP configurations. The network transports serve MCP at `/mcp`,
native HTTP hooks at `POST /hook/{event}`, and an unauthenticated `GET /health`
(`{"status":"ok","version":…}`, plus `"tls":true` over HTTPS):

```bash
export MDKB_TOKEN='replace-with-a-secret'
mdkb serve --http  --token "$MDKB_TOKEN"   # http://127.0.0.1:8080
mdkb serve --https --token "$MDKB_TOKEN"   # https://127.0.0.1:8443, self-signed
mdkb serve --http --bind 0.0.0.0:9000 --token "$MDKB_TOKEN"
```

- `--token` (or `MDKB_TOKEN`) is required. Every request except `/health` must
  send `Authorization: Bearer <token>`. `--allow-no-auth` starts the server
  with no token, and then **every request is accepted**. Use it only on a
  loopback address that no other user of the machine can reach.
- `--https` creates a self-signed certificate for `localhost`, `127.0.0.1` and
  `::1` on first use, valid for 365 days, in `~/.mdkb/certs/`. On start, a certificate that has expired, expires within 30 days
  or cannot be read is replaced with a new one.
- `--global` serves several repositories over stdio. It finds them through the
  MCP `roots/list` request and applies the `daemon.toml` whitelist.
- `--daemon` runs the singleton daemon (Unix only): one process per user,
  locked through `~/.mdkb/daemon.pid`. A second start exits 0. `--detach` also
  double-forks, calls `setsid` and sends output to `~/.mdkb/logs/daemon.log`.
  `mdkb mcp` and the hooks start the daemon this way when it is not running. In
  daemon mode, `--http`, `--https`, `--bind` and `--token` have no effect.

The daemon's own socket protocol is described in [docs/daemon-rpc.md](daemon-rpc.md).

## Daemon configuration (`~/.mdkb/daemon.toml`)

`.mdkb/config.toml` configures one repository. `~/.mdkb/daemon.toml` configures
the daemon for the whole machine. `mdkb init` does not write this file: create it
by hand. A missing file means all defaults apply. The daemon reads it at startup,
so restart it after a change (`mdkb daemon restart`). The one exception is
`[telemetry] hook_events`, which is read again on every hook.

```toml
# Repositories the daemon may open. Empty (the default) means your home
# directory only: a client cannot point the daemon at any other path.
whitelist_dirs = ["~/Gits", "/srv/repos"]

max_active_repos = 5        # stores held open at once (LRU beyond this)
discovery_cache_secs = 60   # reuse a nested-store discovery walk; 0 disables
repo_expiry_days = 30       # forget unpinned roots unused this long; 0 disables
ignore = ["~/Gits/.tmp"]    # never discover or record stores at or below these

[[repos]]                   # pinned repositories; never expire
root = "/Users/me/Gits/mdkb"
scope = "home"              # optional; overrides [[scopes]]

[[scopes]]                  # root="*" leaves out repos of another scope
prefix = "~/Gits/work"
scope = "work"

[priors]                    # machine-wide base for every repo's [priors]
# distiller_program = "..."  see docs/hooks.md

[embedding]
model = "granite107m"       # model for every repo that does not set [embedding] model

[telemetry]
hook_events = false         # write hook-events.jsonl / hook-slow.jsonl in each store
# query_events = true       # machine-wide default for repos that do not set it
# retention_days = 30
```

- **`whitelist_dirs`** is a security boundary. In daemon and `--global` mode,
  opening a root creates `.mdkb/` and starts a file watcher there, so the daemon
  refuses any root outside the list, and outside your home directory when the
  list is empty. Paths are canonicalised, and `~/` is expanded. A refused root
  returns an error that names the whitelist. Single-repository CLI use does not
  check the whitelist.
- **`hook_events`** turns on the hook diagnostic logs in every store the hooks
  touch. It costs I/O on every tool call and shows the files as changes in agent
  UIs, so it is off by default. `MDKB_HOOK_TELEMETRY=1` or `=0` overrides it for
  one process.
- **`[embedding] model`** (`minilm` or `granite107m`) selects the embedding
  model of every repository whose `.mdkb/config.toml` does not set
  `[embedding] model`; a repository that sets it keeps its own. The Granite
  floor defaults follow the effective model. It is read each time a repository
  is opened, by the daemon, the CLI and in-process hooks alike, so run
  `mdkb daemon restart` to apply a change to repositories the daemon already
  holds. Changing it makes the vectors of every affected store stale: the
  daemon rebuilds them in the background, one store at a time, as it meets
  them, and serves lexical results meanwhile (see the `[embedding]` section of
  the README). An invalid value is ignored with a warning. Processes started by
  `cargo test` or `cargo run` in this repository ignore the key
  (`MDKB_IGNORE_DAEMON_TOML`).
- **`query_events`** and **`retention_days`**: see
  [docs/hooks.md](hooks.md). A key set in a repository's `[telemetry]` wins.
- The sockets are always `~/.mdkb/daemon.sock` and `~/.mdkb/daemon-hook.sock`.
  The former `socket_path` key is ignored, and the daemon logs a warning if it
  finds it. `mdkb mcp --socket` gives a one-off override.

The daemon sockets and their protocol: [docs/daemon-rpc.md](daemon-rpc.md).

