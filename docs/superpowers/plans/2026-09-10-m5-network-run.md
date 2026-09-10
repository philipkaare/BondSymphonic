# Milestone 5: Network and Run Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents inside the sandbox reach the network only through a per-workspace allowlisting proxy; a web app started in a workspace is reachable from the Windows browser through a port bridge; run configurations come from `bondsymphonic.toml` or auto-detection; the IDE gets a Run panel (config, start/stop, URL, open in browser, output) and shows network denials as a toast with an "Allow host" action.

**Architecture:** The daemon gains `net/` (allowlist matcher, CONNECT/absolute-URI proxy on a per-workspace Unix socket, TCP↔Unix port bridge) and `runs/` (`bondsymphonic.toml` parsing, detection heuristics, run manager). Two new subcommands of the daemon binary run *inside* the sandbox: `proxy-shim` (127.0.0.1:3128 → `/run/bs/proxy.sock`) and `forward` (`/run/bs/fwd-<P>.sock` → 127.0.0.1:P with a one-byte connect status). The sandbox env carries `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`. Network denials are `daemon.log warn` events with the `host` field, built and parsed through proto helpers like the drop notice. The IDE routes `run.*` events by run id through the router, keeps run state in a `RunPanelModel` QObject, and paints a `RunPanel` in the bottom dock that follows the active tab.

**Tech Stack:** Rust 1.98, tokio (TCP + Unix sockets), `toml` 0.8, `regex` 1, cxx-qt 0.10, Qt 6.9.2 Widgets, bubblewrap 0.9 (`--unshare-net`), WSL2 localhost forwarding.

**Spec:** daemon spec §6.2–6.3 (sandbox: `/run/bs`, one bwrap per workspace, `--unshare-net`), §7 (network: allowlist, proxy, port bridge), §10 (runs: toml, detection, manager), §13; overview §6.3 (`repo.detect_run_configs`, `workspace.set_allowlist`, `run.*`), §6.4 (`run.output`, `run.state`, `daemon.log`), §10 milestone 5; IDE spec §2 (bottom dock Run panel), §3 (`run_config.rs`, `run_panel.rs`, `RunPanel.{h,cpp}`), §10 (New Agent dialog run config), §12 (network denial toast with "Allow host").

## Global Constraints

- **Boundary rule (IDE spec §3):** `model/` and `client/` never import Qt; QObjects are thin adapters; C++ paints and forwards. Which config is selected, what a run's state/url/lines are, and whether a denial belongs to the visible workspace are decided in Rust.
- **Every in-sandbox process is spawned through the workspace's `SandboxHandle`** (daemon §6.1): the proxy shim, the forwarder and the run command included. The daemon binary is already bound at `/tmp/.bs-init` inside bwrap (`linux_bwrap.rs:60`), so in-sandbox helpers are `["/tmp/.bs-init", "proxy-shim", ...]` on bwrap; on noop they are `std::env::current_exe()`.
- **Allowlist (daemon §7.1):** `HostPattern` = exact host or suffix wildcard `*.example.com` (matches `a.example.com` and `b.a.example.com`, not `example.com`); case-insensitive; ports unrestricted. Default list exactly: `api.anthropic.com`, `*.anthropic.com`, `registry.npmjs.org`, `*.npmjs.org`, `pypi.org`, `files.pythonhosted.org`, `crates.io`, `static.crates.io`, `index.crates.io`, `github.com`, `*.github.com`, `*.githubusercontent.com`. The repo's `bondsymphonic.toml` `[network] allow` extends it at workspace creation; `workspace.set_allowlist` replaces the effective list at runtime and persists it in the registry (`Workspace.allowlist`).
- **Proxy (daemon §7.2):** Unix socket `~/.bondsymphonic/run/<id>/proxy.sock` bound at `/run/bs/proxy.sock`; shim on `127.0.0.1:3128`; env `HTTP_PROXY=http://127.0.0.1:3128`, `HTTPS_PROXY` same, `ALL_PROXY` same, `NO_PROXY=localhost,127.0.0.1`. CONNECT for TLS, absolute-URI `GET/POST/…` forwarding for plain HTTP. Denied → `403` with a body naming the host and `bondsymphonic.toml [network] allow`, plus `daemon.log {level: warn, host}` for the workspace.
- **Port bridge (daemon §7.3):** host port `H` on `127.0.0.1` chosen by binding port 0; socket `fwd-<P>.sock`; forwarder inside the sandbox writes one status byte (`1` connected, `0` failed) before bridging; the host side drops the TCP connection on `0`. Bridging is raw bytes both ways (WebSockets/HMR work). URL presented is `http://localhost:<H>`.
- **Runs (daemon §10.3):** env = config env + `PORT=<port>` + `HOST=0.0.0.0`; stdout/stderr lines → `run.output`; state `starting` → `ready` (regex match on output, or the first successful in-sandbox TCP connect through the bridge, polled every 500 ms) → `stopped`/`failed`; `run.stop` = SIGTERM to the group, SIGKILL after 5 s, bridge torn down; `workspace.destroy` stops runs first. On the noop backend (no network namespace) there is no bridge: `host_port = port`, url `http://localhost:<port>`, readiness probed by a direct TCP connect.
- **Config (daemon §10.1–10.2):** `bondsymphonic.toml` shapes exactly as the spec; detection heuristics in the spec's order; guessed ports flagged `port_guessed`; `docker-compose.yml` listed with `disabled_reason`.
- **IDE (spec §2, §12):** Run panel in the bottom dock follows the active tab: config combo (editable port when guessed), Start/Stop, URL label, Open (system browser via `QDesktopServices::openUrl` — the only non-daemon spawn the IDE may do, overview §7), output log (ring buffer 2000 lines per run in Rust); denial toast with "Allow host" calling `workspace.set_allowlist` with the current list plus the host.
- **Quality gates:** `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, zero MSVC `warning C`; daemon suite green on Windows (noop; Unix-socket tests gated `#[cfg(unix)]`) and in WSL (`scripts\test-daemon.ps1`, including the bwrap network tests).
- **No desktop input injection, ever.** GUI verification uses env-gated in-process hooks (removed before commit), own-window `grab()`, and the offscreen smoke test. Checking a bridged URL from Windows with `Invoke-WebRequest` is allowed (no input injection).
- **The user's live workspace `ws_eebdd832` (repo `/mnt/c/git/fredsholm-toollib`) is off-limits**: never destroy, run, or modify it; throwaway repos only.
- **Environment:** PowerShell 5.1 (no `&&`); `. .\scripts\env.ps1` before cargo on Windows; WSL distro `bondsymphonic` (python3 3.12 available for `python3 -m http.server` as a test web app); `.\launch.ps1`.
- **Commit trailers:** every commit ends with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01DcYV4WREYNvdE2MsWATepg`.

## File structure

Daemon:
- `crates/daemon/src/net/mod.rs`, `net/allowlist.rs` (new), `net/proxy.rs` (new), `net/shim.rs` (new, in-sandbox `proxy-shim`), `net/bridge.rs` (new, host side), `net/forward.rs` (new, in-sandbox `forward`).
- `crates/daemon/src/runs/mod.rs`, `runs/config.rs` (toml + detection), `runs/manager.rs` (new).
- `crates/daemon/src/main.rs` (subcommands `proxy-shim`, `forward`), `daemon.rs` (`proxies`, `runs` fields), `server/handlers.rs`, `workspace/lifecycle.rs` (allowlist at create; proxy + shim at sandbox start; stop runs at destroy), `workspace/mod.rs`, `Cargo.toml` (`toml`, `regex`).
- Tests: `runs/config.rs` unit tests over `tests/fixtures/repos/*`; `net/allowlist.rs` unit tests; `net/proxy.rs` request-parsing unit tests; `tests/network_integration.rs` (Linux); `tests/run_integration.rs` (Windows + WSL); `tests/sandbox_integration.rs` (bwrap bridge).

Proto: `crates/proto/src/event.rs` (`Event::network_denied`, `denied_host`), roundtrip tests.

IDE Rust: `crates/ide/src/model/run_config.rs` (new), `client/router.rs` (`subscribe_run`), `qobjects/run_panel.rs` (new), `qobjects/app_controller.rs` (`detectRunConfigs`, `setAllowlist`, `networkDenied`), `model/app_state.rs` (`AgentTab.run_config`), `qobjects/group_model.rs`, `Cargo.toml` if needed. Tests: `tests/run_tests.rs`, `tests/router_tests.rs`, `tests/qobject_smoke.rs`, `tests/smoke.rs`.

IDE C++: `cpp/RunPanel.{h,cpp}` (new), `cpp/NewAgentDialog.*`, `cpp/MainWindow.*`, `cpp/app.cpp`, `build.rs`.

---

### Task 1: Allowlist matcher, `bondsymphonic.toml`, run-config detection

**Files:**
- Create: `crates/daemon/src/net/mod.rs`, `crates/daemon/src/net/allowlist.rs`, `crates/daemon/src/runs/mod.rs`, `crates/daemon/src/runs/config.rs`, `crates/daemon/tests/fixtures/repos/{vite-app,next-app,cargo-axum,django,fastapi,compose-only,with-toml}/…` (minimal marker files: `package.json`, `vite.config.ts`, `pnpm-lock.yaml`, `Cargo.toml`, `manage.py`, `pyproject.toml`, `docker-compose.yml`, `bondsymphonic.toml`)
- Modify: `crates/daemon/src/lib.rs` (`pub mod net; pub mod runs;`), `crates/daemon/Cargo.toml` (`toml = "0.8"`, `regex = "1"`), `crates/daemon/src/server/handlers.rs` (`RepoDetectRunConfigs` arm), `crates/daemon/src/workspace/lifecycle.rs` (`create`: `ws.allowlist = allowlist::effective(&repo_config)`), `crates/proto/src/event.rs` (this task owns every proto change of the milestone so the daemon and IDE tasks can run in parallel: `pub const NETWORK_DENIED_PREFIX: &str = "network denied: "`, `Event::network_denied(host: &str) -> Event` = `DaemonLog { level: Warn, message: format!("{NETWORK_DENIED_PREFIX}{host}"), host: Some(host.into()) }`, `pub fn denied_host(&self) -> Option<&str>` (Warn level, prefix present, returns the `host` field), and `#[serde(default, skip_serializing_if = "Option::is_none")] pub detail: Option<String>` on `Event::RunStateChanged`; roundtrip tests for both, including a plain warn without the prefix yielding `None`)

**Interfaces:**
- Produces (`net/allowlist.rs`): `#[derive(Clone, Debug, PartialEq, Eq)] pub struct HostPattern(String)` with `pub fn parse(s: &str) -> Result<HostPattern, String>` (lowercases; rejects empty, whitespace, `*` alone, wildcard not at the start as `*.`, and anything with `/` or `:`), `pub fn matches(&self, host: &str) -> bool`; `pub const DEFAULT_ALLOW: [&str; 12]`; `#[derive(Clone, Debug, Default)] pub struct Allowlist { patterns: Vec<HostPattern> }` with `pub fn from_strings(items: &[String]) -> Allowlist` (invalid entries skipped with a `warn!`), `pub fn allows(&self, host: &str) -> bool`, `pub fn to_strings(&self) -> Vec<String>`; `pub fn effective(repo: Option<&RepoConfig>) -> Vec<String>` = defaults ∪ `[network].allow`, de-duplicated, order kept.
- Produces (`runs/config.rs`): `#[derive(Debug, Default, Deserialize, PartialEq)] pub struct RepoConfig { #[serde(default)] pub run: Vec<RunEntry>, #[serde(default)] pub network: NetworkSection, #[serde(default)] pub claude: ClaudeSection }` with `RunEntry { name, command, port: u16, cwd: Option<String>, env: BTreeMap<String,String>, ready_regex: Option<String> }`, `NetworkSection { allow: Vec<String> }`, `ClaudeSection { settings: Option<String> }`; `pub fn load_repo_config(root: &Path) -> Result<Option<RepoConfig>, String>` (missing file → `Ok(None)`; parse error → `Err(message with line)`); `pub fn configs_for(root: &Path) -> Vec<RunConfig>` = the toml's `[[run]]` entries as `RunConfig { source: ConfigFile, port_guessed: false, .. }` when the file exists and has any run, else `detect(root)`; `pub fn detect(root: &Path) -> Vec<RunConfig>` per the spec's ordered heuristics (package manager from `pnpm-lock.yaml`/`yarn.lock`/`package-lock.json`; port from `vite.config.*` `server.port` if present else 5173 for vite, 3000 for `next`, 4200 for `angular.json`, else 3000, all `port_guessed: true` unless read from the config file; `docker-compose.yml`/`compose.yaml` → `RunConfig { name: "compose", command: "docker compose up", port: 0, disabled_reason: Some("Docker is not available inside the sandbox") }`; `Cargo.toml` with `[[bin]]` or deps `axum|actix-web|rocket|warp` → `cargo run`, 8080 guessed; `manage.py` → `python manage.py runserver 0.0.0.0:8000`, 8000; `pyproject.toml` with `fastapi` → `uvicorn main:app --host 0.0.0.0 --port 8000`, `flask` → `flask run --host 0.0.0.0 --port 5000`).
- Consumes: proto `RunConfig`, `RunConfigSource`, `DetectRunConfigsResult`, `RepoPathParams`.

- [ ] **Step 1: Write the failing tests** (unit, in each module):

```rust
// net/allowlist.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_and_wildcard_patterns_match_case_insensitively() {
        let p = HostPattern::parse("Api.Anthropic.com").unwrap();
        assert!(p.matches("api.anthropic.com"));
        assert!(!p.matches("anthropic.com"));
        let w = HostPattern::parse("*.npmjs.org").unwrap();
        assert!(w.matches("registry.npmjs.org"));
        assert!(w.matches("a.b.npmjs.org"));
        assert!(!w.matches("npmjs.org"));
        assert!(!w.matches("evilnpmjs.org"));
    }
    #[test]
    fn invalid_patterns_are_rejected() {
        for bad in ["", " ", "*", "foo.*", "a/b", "host:80", "*.", "*bar.com"] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?}");
        }
    }
    #[test]
    fn default_list_allows_the_registries_and_nothing_else() {
        let a = Allowlist::from_strings(&DEFAULT_ALLOW.map(String::from));
        for ok in ["api.anthropic.com", "registry.npmjs.org", "index.crates.io", "raw.githubusercontent.com", "github.com"] {
            assert!(a.allows(ok), "{ok}");
        }
        for no in ["example.com", "evil-github.com", "githubusercontent.com"] {
            assert!(!a.allows(no), "{no}");
        }
    }
    #[test]
    fn effective_list_extends_defaults_without_duplicates() {
        let cfg = crate::runs::config::RepoConfig { network: crate::runs::config::NetworkSection { allow: vec!["*.mycompany.com".into(), "github.com".into()] }, ..Default::default() };
        let v = effective(Some(&cfg));
        assert_eq!(v.len(), DEFAULT_ALLOW.len() + 1);
        assert_eq!(v.last().unwrap(), "*.mycompany.com");
        assert_eq!(effective(None).len(), DEFAULT_ALLOW.len());
    }
}
```

```rust
// runs/config.rs
#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::RunConfigSource;
    fn repo(name: &str) -> std::path::PathBuf { std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/repos").join(name) }

    #[test]
    fn toml_runs_win_over_detection() {
        let v = configs_for(&repo("with-toml"));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "web");
        assert_eq!(v[0].command, "npm run dev -- --port 3000");
        assert_eq!(v[0].port, 3000);
        assert_eq!(v[0].source, RunConfigSource::ConfigFile);
        assert!(!v[0].port_guessed);
        assert_eq!(v[0].env.get("NODE_ENV").map(String::as_str), Some("development"));
        assert_eq!(v[0].ready_regex.as_deref(), Some("Local:.*http"));
        assert_eq!(v[1].name, "api");
    }
    #[test]
    fn toml_network_and_claude_sections_parse() {
        let c = load_repo_config(&repo("with-toml")).unwrap().unwrap();
        assert_eq!(c.network.allow, vec!["*.mycompany.com".to_string()]);
        assert_eq!(c.claude.settings.as_deref(), Some(".claude/settings.json"));
        assert!(load_repo_config(&repo("vite-app")).unwrap().is_none());
    }
    #[test]
    fn a_broken_toml_is_an_error_naming_the_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bondsymphonic.toml"), "[[run]]\nname = \n").unwrap();
        let err = load_repo_config(dir.path()).unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }
    #[test]
    fn vite_with_pnpm_lock_is_detected_with_its_configured_port() {
        let v = detect(&repo("vite-app"));
        let dev = v.iter().find(|c| c.name == "dev").expect("dev script");
        assert_eq!(dev.command, "pnpm run dev");
        assert_eq!(dev.port, 5174); // vite.config.ts says server: { port: 5174 }
        assert!(!dev.port_guessed);
        assert_eq!(dev.source, RunConfigSource::Detected);
    }
    #[test]
    fn next_app_guesses_3000_and_compose_is_disabled() {
        let v = detect(&repo("next-app"));
        let dev = v.iter().find(|c| c.name == "dev").unwrap();
        assert_eq!((dev.port, dev.port_guessed), (3000, true));
        let v = detect(&repo("compose-only"));
        assert_eq!(v.len(), 1);
        assert!(v[0].disabled_reason.as_deref().unwrap().contains("Docker"));
    }
    #[test]
    fn cargo_django_and_fastapi_heuristics() {
        let v = detect(&repo("cargo-axum"));
        assert_eq!(v[0].command, "cargo run");
        assert_eq!((v[0].port, v[0].port_guessed), (8080, true));
        let v = detect(&repo("django"));
        assert_eq!(v[0].command, "python manage.py runserver 0.0.0.0:8000");
        let v = detect(&repo("fastapi"));
        assert!(v[0].command.starts_with("uvicorn"));
        assert_eq!(v[0].port, 8000);
    }
    #[test]
    fn an_empty_directory_detects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path()).is_empty());
        assert!(configs_for(dir.path()).is_empty());
    }
}
```

Fixture contents: `with-toml/bondsymphonic.toml` = the spec §10.1 example verbatim; `vite-app/package.json` = `{"name":"v","scripts":{"dev":"vite","build":"vite build"}}`, `vite-app/vite.config.ts` = `export default { server: { port: 5174 } }`, `vite-app/pnpm-lock.yaml` = `lockfileVersion: '9.0'`; `next-app/package.json` = `{"scripts":{"dev":"next dev","start":"next start"}}` (no lockfile → `npm run dev`); `cargo-axum/Cargo.toml` = `[package]\nname="svc"\nversion="0.1.0"\n[dependencies]\naxum="0.7"`; `django/manage.py` = `#!/usr/bin/env python`; `fastapi/pyproject.toml` = `[project]\nname="x"\ndependencies=["fastapi"]`; `compose-only/docker-compose.yml` = `services: {}`.

- [ ] **Step 2: Run to verify failure**: `. .\scripts\env.ps1; cargo test -p bondsymphonic-daemon --lib net::allowlist runs::config` → modules not found.

- [ ] **Step 3: Implement** both modules, the handler arm (`Request::RepoDetectRunConfigs(p) => ok(DetectRunConfigsResult { configs: tokio::task::spawn_blocking(move || runs::config::configs_for(Path::new(&p.path))).await.map_err(internal)? })`), and in `lifecycle::create` set `allowlist: allowlist::effective(load_repo_config(&repo).ok().flatten().as_ref())` (a broken toml logs a `warn!` and falls back to the defaults; it must not block creation). Keep detection pure-filesystem (no process spawns).

- [ ] **Step 4: Run** the unit tests → green; run `cargo test -p bondsymphonic-daemon` (existing suites still green; `workspace_integration` may need its `WorkspaceInfo.allowlist` expectation updated from empty to the defaults — do that).

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/daemon Cargo.lock
git commit -m "feat(daemon): host allowlist, bondsymphonic.toml, run-config detection"
```

---

### Task 2: Allowlisting proxy, in-sandbox shim, `workspace.set_allowlist`

**Files:**
- Create: `crates/daemon/src/net/proxy.rs`, `crates/daemon/src/net/shim.rs`, `crates/daemon/tests/network_integration.rs`
- Modify (proto helpers `Event::network_denied`/`denied_host` come from Task 1): `crates/daemon/src/main.rs` (`Cmd::ProxyShim { socket: PathBuf, listen: String }`), `crates/daemon/src/daemon.rs` (`pub proxies: net::proxy::ProxyRegistry`), `crates/daemon/src/workspace/lifecycle.rs` (`start_sandbox`: start the proxy listener before the sandbox, spawn the shim after; `spec_for`: proxy env vars on bwrap only; `destroy`/sandbox down: stop the listener), `crates/daemon/src/server/handlers.rs` (`WorkspaceSetAllowlist`), `crates/daemon/src/net/mod.rs`

**Interfaces:**
- Produces (`net/proxy.rs`): `pub struct ProxyRegistry { .. }` (`Default`) with `pub async fn start(&self, id: &WorkspaceId, socket: &Path, allow: Allowlist, events: EventBus) -> Result<(), RpcError>` (Unix listener; idempotent per id), `pub fn set_allowlist(&self, id: &WorkspaceId, allow: Allowlist)` (live swap via `Arc<RwLock<Allowlist>>`), `pub fn stop(&self, id: &WorkspaceId)` (abort the accept task, remove the socket file); pure helpers `pub fn parse_request_head(buf: &[u8]) -> Option<RequestHead>` with `pub struct RequestHead { pub method: String, pub target: String, pub version: String, pub headers: Vec<(String, String)>, pub head_len: usize }` (returns `None` until the blank line has arrived), `pub fn target_host_port(head: &RequestHead) -> Option<(String, u16)>` (CONNECT `host:port`; absolute URI `http://host[:port]/path` → port 80 default; anything else → `None`), `pub fn origin_form(head: &RequestHead) -> Vec<u8>` (rewrites the request line to `METHOD /path HTTP/1.1` and drops `Proxy-Connection`/`Proxy-Authorization`), `pub fn denied_response(host: &str) -> Vec<u8>` (`HTTP/1.1 403 Forbidden` + `Content-Type: text/plain` + body `"host <host> is not in this workspace's allowlist; add it to bondsymphonic.toml [network] allow or use Allow host in the IDE"`). Per connection: read the head (cap 64 KiB, 10 s), resolve target, check `allow.read().allows(host)`; denied → write the 403, publish `Event::network_denied(host)` with the workspace id, close; allowed → `TcpStream::connect((host, port))` (5 s; failure → `502`), for CONNECT reply `HTTP/1.1 200 Connection Established\r\n\r\n` then `copy_bidirectional`; for absolute-URI write `origin_form` + any body bytes already read, then `copy_bidirectional`.
- Produces (`net/shim.rs`): `pub async fn run(socket: &Path, listen: &str) -> anyhow::Result<()>` — `TcpListener::bind(listen)` (default `127.0.0.1:3128`), per connection `UnixStream::connect(socket)` and `copy_bidirectional`; Unix-only (`#[cfg(unix)]`; the subcommand errors elsewhere).
- Lifecycle: `start_sandbox` → `d.proxies.start(&ws.id, &run_dir.join("proxy.sock"), Allowlist::from_strings(&ws.allowlist), d.events.clone())` **before** `backend.start` (so the socket exists when the shim starts); after the handle is up on bwrap: `handle.spawn(SandboxCommand { argv: ["/tmp/.bs-init","proxy-shim","--socket","/run/bs/proxy.sock","--listen","127.0.0.1:3128"], env: [], cwd: None, pty: None })` and keep the child's `killer` in the sandbox watch so it dies with the sandbox. `spec_for` on bwrap adds `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` = `http://127.0.0.1:3128`, `NO_PROXY=localhost,127.0.0.1`. Noop: no shim, no env (processes already have host networking).
- `WorkspaceSetAllowlist { workspace_id, hosts }`: validate every pattern (`InvalidParams` naming the first bad one), `registry.update` to persist, `proxies.set_allowlist` live, `emit_state` (the `WorkspaceInfo.allowlist` changed). Returns `Empty`.

- [ ] **Step 1: Write the failing tests.** Unit (`proxy.rs`): `parse_request_head` incremental (returns `None` for a partial head, then the head with `head_len`), CONNECT target parsing, absolute-URI parsing with and without port, `origin_form` rewriting and header dropping, `denied_response` shape. Integration (`tests/network_integration.rs`, `#[cfg(unix)]`, runs on WSL over the noop backend and additionally over bwrap when available):

```rust
// 1. Start a tiny HTTP server on the host: tokio TcpListener on 127.0.0.1:0 answering "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".
// 2. start_daemon; create_ws; set the allowlist to ["127.0.0.1"] via workspace.set_allowlist → expect a workspace.state event whose info.allowlist == ["127.0.0.1"].
// 3. Connect to <run_dir>/proxy.sock directly (the daemon side), send "GET http://127.0.0.1:<port>/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n" → expect the 200 body "ok".
// 4. Send "CONNECT 127.0.0.1:<port> HTTP/1.1\r\n\r\n" → expect "200 Connection Established", then send a raw GET and read "ok".
// 5. set_allowlist to [] → CONNECT again → expect 403 and a daemon.log warn event for this workspace with denied_host() == Some("127.0.0.1").
// 6. Invalid pattern "a/b" → InvalidParams.
// 7. (bwrap only, gated like sandbox_integration) spawn inside the sandbox: `sh -c 'exec 3<>/dev/tcp/127.0.0.1/<port>'` must FAIL (no route to the host from the network namespace); then run `python3 -c "import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:<port>/').read())"` inside the sandbox (its env carries HTTP_PROXY, so urllib goes through the shim): expect b'ok' when allowlisted and an HTTP 403 error when not. (`python3` is on the read-only root, so it is available inside bwrap.)
```

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-daemon --test network_integration` (WSL) and the unit tests (Windows).

- [ ] **Step 3: Implement** as specified. The accept loop runs one task per connection with a 10 s head timeout and no limit on connection lifetime (WebSockets over CONNECT must live). Log denials at `warn!` too. `ProxyRegistry::stop` on destroy and when the sandbox goes down (`watch_sandbox`).

- [ ] **Step 4: Run** Windows (`cargo test -p bondsymphonic-daemon -p bondsymphonic-proto`) and WSL (`.\scripts\test-daemon.ps1`) → green, including the bwrap probe.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/daemon crates/proto
git commit -m "feat(daemon): allowlisting CONNECT proxy on a per-workspace socket; in-sandbox shim; workspace.set_allowlist"
```

---

### Task 3: Port bridge, in-sandbox forwarder, run manager, `run.*` handlers

**Files:**
- Create: `crates/daemon/src/net/bridge.rs`, `crates/daemon/src/net/forward.rs`, `crates/daemon/src/runs/manager.rs`, `crates/daemon/tests/run_integration.rs`
- Modify: `crates/daemon/src/main.rs` (`Cmd::Forward { socket: PathBuf, port: u16 }`), `crates/daemon/src/daemon.rs` (`pub runs: runs::manager::RunManager`), `crates/daemon/src/server/handlers.rs` (`RunStart`, `RunStop`, `RunList`), `crates/daemon/src/workspace/lifecycle.rs` (`destroy` stops runs first; sandbox-down stops runs; `workspace_info.runs` filled), `crates/daemon/src/runs/mod.rs`, `crates/daemon/src/net/mod.rs`, `crates/daemon/tests/sandbox_integration.rs` (bwrap bridge test). Proto: none (the `detail` field on `run.state` is Task 1's).

**Interfaces:**
- Produces (`net/forward.rs`, in-sandbox): `pub async fn run(socket: &Path, port: u16) -> anyhow::Result<()>` — `UnixListener::bind(socket)` (remove a stale file first), per accepted connection `TcpStream::connect(("127.0.0.1", port))` with a 2 s timeout; write status byte `1` and `copy_bidirectional`, or `0` and close. Unix-only.
- Produces (`net/bridge.rs`, host): `pub struct Bridge { pub host_port: u16, .. }` with `pub async fn start(socket: PathBuf) -> std::io::Result<Bridge>` (binds `127.0.0.1:0`, spawns the accept loop: per TCP connection → `UnixStream::connect(&socket)`, read one status byte; `1` → `copy_bidirectional`; `0`/error → drop), `pub async fn probe(&self) -> bool` (connect to the Unix socket, read the status byte, true on `1`), `pub fn stop(self)` (abort, remove the socket file). Windows: `Bridge` is `#[cfg(unix)]`; the manager compiles without it.
- Produces (`runs/manager.rs`): `pub struct RunManager { events, runs: Mutex<HashMap<RunId, Arc<Run>>> }`; `pub async fn start(&self, d: &Daemon, p: RunStartParams) -> Result<RunStartResult, RpcError>`: workspace must be `Ready`; config = `configs_for(worktree)` entry with that name (else `NotFound`; `disabled_reason` → `InvalidParams` with the reason); a second run of the same config in the same workspace → `Conflict`; on bwrap: `socket = run_dir.join(format!("fwd-{port}.sock"))`, `Bridge::start(socket)` → `host_port`, spawn the forwarder inside the sandbox (`/tmp/.bs-init forward --socket /run/bs/fwd-<port>.sock --port <port>`), then spawn the command via `shell-words` → `["bash","-lc", command]`? — no: run `["/bin/sh","-c", command]` so shell features in `command` (`&&`, `--`) work, with `cwd = worktree.join(config.cwd.unwrap_or("."))`, env = config env + `PORT` + `HOST`; on noop: no bridge/forwarder, `host_port = port`; `url = format!("http://localhost:{host_port}")`. Reader tasks for stdout and stderr publish `Event::RunOutput { run_id, line }` per line (cap 8 KiB per line; keep the last 2000 lines in the run for `run.list` consumers? — no, the IDE keeps its own buffer; the daemon does not store output); state task: `Starting` published at start; every 500 ms probe readiness (regex over each output line if `ready_regex` set — compile once, `InvalidParams` on a bad regex at start — else bridge probe on bwrap / direct `TcpStream::connect(("127.0.0.1", port))` on noop); first success → `Ready` with `url`; process exit before ready → `Failed` (detail: exit code + last 20 lines), after ready → `Stopped`; `pub async fn stop(&self, id: &RunId) -> Result<Empty, RpcError>`: `(signal)(SIGTERM)`, wait 5 s, `(signal)(SIGKILL)`, then bridge stop; `Stopped` published once; `pub fn list(&self, ws: &WorkspaceId) -> Vec<RunInfo>`; `pub async fn stop_all_in(&self, ws: &WorkspaceId)`; `pub fn runs_of(&self, ws) -> Vec<RunId>` for `workspace_info`.
- Consumes: Task 1 `configs_for`, `SandboxChild.signal`, `EventBus`, `Daemon::sandbox`.

- [ ] **Step 1: Write the failing tests.** `tests/run_integration.rs` (Windows noop + WSL noop): create a throwaway repo with `bondsymphonic.toml` declaring `[[run]] name="web" command="python -m http.server {port} --bind 127.0.0.1"` (on Unix `python3`) with a free port chosen by the test, plus a second config `bad` with `ready_regex = "("`; `run.start web` → `{run_id, host_port == port, url == http://localhost:<port>}`; expect `run.state starting` then `ready` within 10 s (the daemon's TCP probe) and at least one `run.output` line (http.server logs to stderr on requests — make one request from the test through `url` and expect a `"GET /"` output line); `run.list` shows it `ready`; `run.start web` again → `Conflict`; `run.start bad` → `InvalidParams` mentioning the regex; `run.stop` → `run.state stopped`, process gone (a second request to the port fails), `run.list` empty; a run whose command exits immediately (`sh -c 'exit 1'` config) → `failed` with the exit code in the event's `detail` field (added to `Event::RunStateChanged` by Task 1). `workspace.destroy` with a ready run stops it first (expect `stopped` before the destroy reply). `tests/sandbox_integration.rs` (bwrap): the same `web` run on a bwrap workspace: `host_port != port` (bridged), `Invoke-WebRequest`-equivalent from the test (`reqwest`-free: raw `TcpStream` to `127.0.0.1:host_port`, send `GET / HTTP/1.0`, expect `200`), readiness via the bridge probe, and `sh -c 'exec 3<>/dev/tcp/127.0.0.1/<host_port>'` from inside the sandbox fails (the sandbox cannot see the host port).

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-daemon --test run_integration`.

- [ ] **Step 3: Implement** as specified; register the subcommand; wire `workspace_info.runs`, `destroy` ordering (runs → agents → ptys → sandbox), sandbox-down handling.

- [ ] **Step 4: Run** Windows and WSL suites → green (bwrap bridge test included; note WSL2's localhost forwarding is a Windows-side concern the test does not need).

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/daemon crates/proto
git commit -m "feat(daemon): port bridge with in-sandbox forwarder; run manager with readiness, output streaming and stop"
```

---

### Task 4: IDE run model, run-event routing, `RunPanelModel`, controller additions

**Files:**
- Create: `crates/ide/src/model/run_config.rs`, `crates/ide/src/qobjects/run_panel.rs`, `crates/ide/tests/run_tests.rs`
- Modify: `crates/ide/src/model/mod.rs`, `crates/ide/src/model/app_state.rs` (`AgentTab.run_config: Option<String>`), `crates/ide/src/client/router.rs` (`subscribe_run`, `StreamKey::Run`), `crates/ide/src/qobjects/app_controller.rs` (`detectRunConfigs(path)` → `runConfigsDetected(path, json)`; `setAllowlist(workspaceId, hostsJson)` → `operationFailed` on error; `networkDenied(workspaceId, host)` from `drain_events` via `denied_host()`; `createWorkspace*` variants accept an optional `runConfig` name stored on the tab), `crates/ide/src/qobjects/group_model.rs` (tab JSON carries `run_config`), `crates/ide/src/qobjects/mod.rs`, `crates/ide/build.rs`, `crates/ide/tests/router_tests.rs`, `crates/ide/tests/qobject_smoke.rs`

**Interfaces:**
- Produces (`model/run_config.rs`): `#[derive(Clone, Debug, Default, PartialEq, Serialize)] pub struct RunLog { lines: VecDeque<String> }` with `pub const CAP: usize = 2000`, `push(line)`, `text() -> String`, `len()`; `#[derive(Clone, Debug, PartialEq, Serialize)] pub struct RunView { pub run_id: String, pub config_name: String, pub state: String /* starting|ready|stopped|failed */, pub host_port: u16, pub url: String, pub detail: String }`; `#[derive(Default)] pub struct WorkspaceRuns { pub configs: Vec<RunConfig>, pub selected: Option<String>, pub port_override: Option<u16>, pub runs: Vec<RunView>, pub logs: BTreeMap<String, RunLog>, pub denied_hosts: Vec<String> }` with `set_configs(Vec<RunConfig>)` (keeps `selected` if still present, else first enabled), `select(name)`, `apply_list(Vec<RunInfo>)`, `apply_state(run_id, state, url, detail) -> bool` (changed), `apply_output(run_id, line)`, `active_run(&self) -> Option<&RunView>` (the run of the selected config that is starting/ready), `note_denied(host) -> bool` (new host), `clear_denied(host)`, `selected_config_json()`, `runs_json()`, `configs_json()`.
- Produces (`client/router.rs`): `pub fn subscribe_run(&self, id: &RunId) -> EventRx`, `unsubscribe_run`; early buffer keyed by `StreamKey::Run` like PTY/agent.
- Produces (`RunPanelModel` QObject, one app-wide instance like `ChangesModel`): properties `workspaceId`, `selectedConfig`, `activeState: QString` (empty when no active run), `activeUrl`, `activeRunId`, `busy`; invokables `setWorkspace(workspaceId, worktreeRepoPath)` (detects via `repo.detect_run_configs` on the WORKSPACE's worktree path — the daemon method takes a path; use `WorkspaceInfo.worktree_path` from the tab JSON), `selectConfig(name)`, `setPort(port)`, `start()` (sends the selected `config_name`; the daemon uses the detected port. There is no per-start port override, so the spec's "editable port when guessed" is deferred to Milestone 6 with persistence; do not implement `setPort`/`portEditable`), `stop()`, `logText(runId)`, `refresh()`, `allowHost(host)` (calls `setAllowlist` with `WorkspaceInfo.allowlist` + host, then `clear_denied`), `dismissDenied(host)`; signals `configsChanged()`, `runsChanged()`, `outputAppended(runId, line)`, `stateChanged()`, `denied(host)`, `errorOccurred(message)`. Subscribes to `run.*` events for every run it knows via `subscribe_run` when a run starts / is listed, and to `networkDenied` for its workspace.
- `AppController`: as listed; `networkDenied(workspaceId, host)` emitted for `Event::DaemonLog` with `denied_host()` and the envelope's workspace id.

- [ ] **Step 1: Write the failing tests** (`tests/run_tests.rs`): `RunLog` cap and text; `WorkspaceRuns::set_configs` selection rules (disabled configs never auto-selected; selection kept across a re-detect); `apply_state` transitions and `active_run`; `apply_output` goes to the right log; `note_denied` de-dup; JSON shapes. Router: `subscribe_run` early buffer + unsubscribe. `qobject_smoke.rs`: `AgentTab.run_config` round trip; a `DaemonLog` warn with `host` maps to `(workspace, host)` via a pure helper `network_denial(ws, &Event) -> Option<(String, String)>`.

- [ ] **Step 2: Run to verify failure**: `cargo test -p bondsymphonic-ide --test run_tests`.

- [ ] **Step 3: Implement** (mirror `changes_model.rs` for the app-wide QObject with a workspace generation guard, `terminal_session.rs` for subscriptions with `Drop`). Register in `build.rs`/`mod.rs`.

- [ ] **Step 4: Build and test**: `cargo build -p bondsymphonic-ide`, `cargo test -p bondsymphonic-ide` → green.

- [ ] **Step 5: Clippy, fmt, commit**

```bash
git add crates/ide
git commit -m "feat(ide): run model and RunPanelModel; run event routing; allowlist and detection on the controller"
```

---

### Task 5: C++ Run panel, denial toast, New Agent dialog run config

**Files:**
- Create: `crates/ide/cpp/RunPanel.{h,cpp}`
- Modify: `crates/ide/cpp/NewAgentDialog.{h,cpp}`, `crates/ide/cpp/MainWindow.{h,cpp}`, `crates/ide/cpp/app.cpp`, `crates/ide/build.rs`

**Interfaces:**
- `RunPanel : QWidget` — `RunPanel(RunPanelModel* model, QWidget*)`: top row { config `QComboBox` (items from `configsJson()`: name + " (guessed :port)" when `port_guessed`, disabled items for `disabled_reason` with the reason as tooltip), Start / Stop buttons (Start enabled when a config is selected and no active run; Stop when `activeState` is starting/ready), URL `QLabel` (rich text link, shows `activeUrl` when ready, "starting…" while starting, empty otherwise), "Open" `QPushButton` (enabled when ready; `QDesktopServices::openUrl`), state glyph }, a toast strip (hidden; shows "Blocked network access to <host>" with "Allow host" and "Dismiss" buttons; one toast at a time, queue the rest in Rust — the model emits `denied(host)` one at a time and the panel calls `dismissDenied`/`allowHost`), and a read-only `QPlainTextEdit` log (monospace, `setMaximumBlockCount(2000)`), filled from `logText(activeRunId)` on run change and appended on `outputAppended` for the active run; `void setWorkspace(...)` is not on the panel — MainWindow drives `model->setWorkspace` on active-tab change and the panel repaints on `configsChanged/runsChanged/stateChanged`.
- `NewAgentDialog`: a "Run config" combo populated from `AppController::runConfigsDetected(path, json)` after `repoInspected` (call `detectRunConfigs(repoPath)` alongside `inspectRepo`), items as above plus "(none)"; accessor `runConfig() -> QString` (empty for none); the chosen name is passed to `createWorkspace*` and stored on the tab (Task 4), so the Run panel preselects it. Guessed ports are shown, not editable (deferred; tooltip says "edit bondsymphonic.toml to pin the port").
- `MainWindow`: replaces the bottom dock's "Run" placeholder with `RunPanel`; on active-tab change calls `m_runModel->setWorkspace(workspaceId, worktreePath)` (worktree path from the tab JSON — add it to `AgentTab` if missing; it is in `WorkspaceInfo`) and the tab's `run_config` → `selectConfig`; `workspaceDestroyed` → `setWorkspace("")` if it was the shown one; `app.cpp` constructs `RunPanelModel`.

- [ ] **Step 1: Implement** the panel, the dialog field, and the wiring.

- [ ] **Step 2: Verification with a temporary env-gated hook** (`BS_T5_SELFTEST=1`; no desktop input; own-window `grab()` to `<scratchpad>\m5-run.png`; removed before commit) against the real daemon in WSL: throwaway repo containing `bondsymphonic.toml` with `[[run]] name="web" command="python3 -m http.server 8000 --bind 0.0.0.0" port=8000`; create a Claude-less terminal workspace with run config `web`; log the combo items and the preselection; click Start programmatically (`QPushButton::click()` in-process); log the state transitions starting → ready, the URL (`http://localhost:<H>` with `H != 8000`), and fetch the URL from the hook with `QNetworkAccessManager`? — QtNetwork is not linked (spec §13); instead the hook writes the URL to stderr and the agent fetches it from PowerShell with `Invoke-WebRequest` (allowed), expecting 200 and an `http.server` `"GET /"` line to appear in the panel log; then in the workspace's shell PTY run `curl -sS https://example.com` (or `python3 -c "import urllib.request;urllib.request.urlopen('https://example.com')"`) and log that the toast appears with `example.com`; click "Allow host" programmatically and log that `WorkspaceInfo.allowlist` now contains `example.com` (via `workspace.get`) and the toast is gone; click Stop, log `stopped`; destroy the workspace; quit. Record every stderr line.

- [ ] **Step 3: Build, clippy, fmt, `cargo test -p bondsymphonic-ide`, zero MSVC warnings; remove the hook; commit**

```bash
git add crates/ide
git commit -m "feat(ide): run panel with start/stop, bridged URL and open-in-browser; network denial toast; run config in the New Agent dialog"
```

---

### Task 6: Smoke test, docs, footprint, end-to-end

**Files:**
- Modify: `crates/ide/src/qobjects/smoke.rs` (steps `detect`, `run_start`, `run_stop`), `crates/ide/tests/smoke.rs` (fake daemon: `repo.detect_run_configs` (one config `web`, port 3000 guessed), `run.start` → `{run_id, host_port: 41873, url}` + events `run.state starting`, `run.output "ready on 3000"`, `run.state ready`; `run.stop` → `stopped`; `run.list`; `workspace.set_allowlist` → `{}` + a `workspace.state` event; and a `daemon.log warn host=example.com` injected after `run_start` so the toast path runs — the smoke step `allow_host` calls `RunPanelModel::allowHost` through a production `AppController::requestAllowHost(workspaceId, host)` invokable+signal pair that MainWindow routes to the panel model; assertions: order includes `repo.detect_run_configs`, `run.start`, `workspace.set_allowlist`, `run.stop`; the `set_allowlist` journal entry contains `example.com` plus the defaults; no "failed" warnings for `run.*`/`set_allowlist`), `README.md` ("What works now": network allowlist + proxy, port bridge + Run panel, `bondsymphonic.toml`, detection, denial toast; "Test hooks": new smoke steps; limitations: plain-HTTP proxying is minimal (no chunked request bodies beyond what is already read), guessed ports not editable in the IDE yet, one run per config, no Docker), `docs/superpowers/specs/2026-09-08-bondsymphonic-ide-design.md` (§13 footprint row: two workspaces, one with a ready run; §14 smoke description), `docs/superpowers/specs/2026-09-08-bondsymphonic-daemon-design.md` (§7.3: the status byte; §10.3: noop has no bridge), `.github/workflows/ci.yml` only if the Linux job needs `python3` (Ubuntu runners have it)

- [ ] **Step 1: Smoke** as above; `cargo test -p bondsymphonic-ide --test smoke` green; then the full gates on Windows and the WSL suite.

- [ ] **Step 2: Footprint** (IDE spec §13): two workspaces, one with a ready run (`python3 -m http.server`) and its bridge, 60 s idle: IDE working set and daemon RSS; targets unchanged (IDE < 150 MB, daemon < 30 MB).

- [ ] **Step 3: Docs** as listed.

- [ ] **Step 4: End-to-end** with `.\launch.ps1` and a temporary hook (removed before commit): the Task 5 scenario on a throwaway repo, plus: from the workspace shell, `python3 -c "import urllib.request;print(urllib.request.urlopen('https://api.anthropic.com/').status)"` reaches the proxy and is allowed by the default list (any HTTP status is fine; what matters is that the connection went through the proxy — check the daemon log for no denial), while `https://example.com` is denied; `Invoke-WebRequest http://localhost:<H>` returns 200 from Windows; screenshot `<scratchpad>\m5-final.png`; destroy the throwaway workspace; the user's workspace untouched (registry before/after).

- [ ] **Step 5: Commit**

```bash
git add crates/ide README.md docs
git commit -m "test(ide): smoke covers detection, a bridged run and an allow-host reply; docs and footprint for milestone 5"
```

---

## Milestone 5 exit criteria

- Inside a bwrap workspace, direct network access fails; `HTTP_PROXY`-honouring tools reach allowlisted hosts through the proxy and get a `403` naming the host for others; a denial shows a toast in the IDE with "Allow host" that updates the workspace allowlist immediately and persistently.
- `bondsymphonic.toml` run entries and auto-detected configs appear in the New Agent dialog and the Run panel; Start spawns the command inside the sandbox with `PORT`/`HOST`, the panel shows starting → ready with `http://localhost:<H>`, Open launches the browser, output streams into the log, Stop ends it; a ready run's URL answers from Windows through the bridge.
- `workspace.destroy` stops runs and the bridge; the daemon suite (Windows noop + WSL bwrap) covers the proxy, the bridge, readiness and stop; smoke test covers detection, a run and an allow-host reply; clippy/fmt clean; footprint recorded.
