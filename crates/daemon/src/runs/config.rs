//! Where a workspace's run commands come from: the repo's `bondsymphonic.toml`
//! if it has one, and otherwise a set of ordered filesystem heuristics.
//!
//! Detection only ever *reads files*. It never runs `npm`, `cargo metadata` or
//! anything else, because it is called on a path the user has just typed into
//! the New Agent dialog — a repo nobody has vetted yet, outside any sandbox.
//! Guessing a port wrongly is a small annoyance the IDE lets the user correct;
//! executing a stranger's build script to find one is not.

use bondsymphonic_proto::{RunConfig, RunConfigSource};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

/// The per-repo configuration file, read from the repo root (and, for a
/// workspace, from its worktree, which is a checkout of the same tree).
pub const CONFIG_FILE: &str = "bondsymphonic.toml";

/// The reason `docker compose up` is listed but cannot be started: there is no
/// Docker daemon inside the sandbox, and reaching the host's would hand the
/// workspace a way straight out of it.
pub const COMPOSE_DISABLED: &str = "Docker is not available inside the sandbox";

/// A repo's `bondsymphonic.toml`.
///
/// Every section is optional, so a repo can use the file for just one of them —
/// a `[network] allow` with no runs is a perfectly ordinary configuration.
/// Unknown keys are accepted rather than rejected: an older daemon must keep
/// reading a file written for a newer one instead of failing the workspace it
/// belongs to.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RepoConfig {
    #[serde(default)]
    pub run: Vec<RunEntry>,
    #[serde(default)]
    pub network: NetworkSection,
    #[serde(default)]
    pub claude: ClaudeSection,
}

/// One `[[run]]` block: a command the Run panel can start, and the port it
/// serves on so the daemon can bridge it back to the host browser.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RunEntry {
    pub name: String,
    pub command: String,
    pub port: u16,
    /// Relative to the worktree root. `None` means the root itself.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Output line pattern that marks the run `ready`, instead of waiting for
    /// the port to accept a connection.
    #[serde(default)]
    pub ready_regex: Option<String>,
}

/// `[network]`: hosts this repo needs on top of [the defaults].
///
/// [the defaults]: crate::net::allowlist::DEFAULT_ALLOW
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct NetworkSection {
    #[serde(default)]
    pub allow: Vec<String>,
}

/// `[claude]`: the repo's own Claude Code settings file, copied into a
/// workspace's sandbox home instead of the user's.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ClaudeSection {
    #[serde(default)]
    pub settings: Option<String>,
}

/// Reads `<root>/bondsymphonic.toml`.
///
/// A repo without the file is the normal case, not an error, so it answers
/// `Ok(None)`. A file that will not parse *is* an error, carrying the `toml`
/// crate's message with the offending line and a caret, because the only way a
/// user can fix it is by being told where it is wrong. Callers decide how loud
/// that is: `repo.detect_run_configs` falls back to detection and workspace
/// creation falls back to the default allowlist, but neither pretends the file
/// said nothing.
pub fn load_repo_config(root: &Path) -> Result<Option<RepoConfig>, String> {
    let path = root.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{CONFIG_FILE}: {e}")),
    };
    match toml::from_str::<RepoConfig>(&text) {
        Ok(c) => Ok(Some(c)),
        Err(e) => Err(format!("{CONFIG_FILE}: {e}")),
    }
}

/// The run configurations for a repo: what its `bondsymphonic.toml` declares,
/// or what detection can find.
///
/// The file wins whenever it declares any run, because a repo that took the
/// trouble to write one down does not want a guessed `npm run dev` next to it.
/// A file that fails to parse falls back to detection with a warning rather
/// than answering nothing, so a stray comma in the config cannot make the Run
/// panel look like the repo has nothing to run.
pub fn configs_for(root: &Path) -> Vec<RunConfig> {
    match load_repo_config(root) {
        Ok(Some(cfg)) if !cfg.run.is_empty() => cfg.run.iter().map(from_entry).collect(),
        Ok(_) => detect(root),
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "unreadable repo config, detecting runs instead");
            detect(root)
        }
    }
}

/// A configured run, as the protocol carries it. Nothing here is a guess: the
/// port is the one the user wrote.
fn from_entry(e: &RunEntry) -> RunConfig {
    RunConfig {
        name: e.name.clone(),
        command: e.command.clone(),
        port: e.port,
        cwd: e.cwd.clone(),
        env: e.env.clone(),
        ready_regex: e.ready_regex.clone(),
        source: RunConfigSource::ConfigFile,
        port_guessed: false,
        disabled_reason: None,
    }
}

/// What a repo looks like it can run, from its marker files alone.
///
/// The heuristics run in the daemon spec's order (§10.2) and the result keeps
/// that order, because the Run panel preselects the first entry: a `dev` script
/// is a better first offer than the `docker compose up` that cannot even start.
pub fn detect(root: &Path) -> Vec<RunConfig> {
    let mut out = Vec::new();
    detect_node(root, &mut out);
    detect_compose(root, &mut out);
    detect_cargo(root, &mut out);
    detect_python(root, &mut out);
    out
}

/// A detected run. `port_guessed` says whether the IDE should offer the port
/// for editing: it is set when the port is this module's assumption about a
/// framework's default, and cleared when the port was read from the project's
/// own config or is spelled out in the command itself, where editing the number
/// alone would not change what the process binds.
fn detected(name: &str, command: String, port: u16, port_guessed: bool) -> RunConfig {
    RunConfig {
        name: name.into(),
        command,
        port,
        cwd: None,
        env: BTreeMap::new(),
        ready_regex: None,
        source: RunConfigSource::Detected,
        port_guessed,
        disabled_reason: None,
    }
}

fn read(path: std::path::PathBuf) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The `package.json` scripts worth offering, in the order a project is most
/// likely to mean them. A `build` or `test` script is not a long-running server
/// and has no port, so neither is listed.
const NODE_SCRIPTS: [&str; 3] = ["dev", "start", "serve"];

fn detect_node(root: &Path, out: &mut Vec<RunConfig>) {
    let Some(text) = read(root.join("package.json")) else {
        return;
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "package.json is not valid JSON");
            return;
        }
    };
    let Some(scripts) = json.get("scripts").and_then(|v| v.as_object()) else {
        return;
    };
    let pm = package_manager(root);
    for name in NODE_SCRIPTS {
        let Some(script) = scripts.get(name).and_then(|v| v.as_str()) else {
            continue;
        };
        let (port, guessed) = node_port(root, script);
        out.push(detected(name, format!("{pm} run {name}"), port, guessed));
    }
}

/// Which package manager to invoke, decided by the lockfile that is checked in.
/// Guessing wrong is worse than it looks: `npm run dev` in a pnpm workspace
/// resolves a different dependency tree, or none at all.
fn package_manager(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if root.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    }
}

/// The port a JS dev server will listen on, and whether that is a guess.
///
/// Vite is asked first and by name, because it is the one bundler that reliably
/// writes its port down somewhere this code can read; everything else falls
/// back to the framework default.
fn node_port(root: &Path, script: &str) -> (u16, bool) {
    if mentions(script, "vite") || vite_config(root).is_some() {
        return match vite_config(root).as_deref().and_then(vite_port) {
            Some(p) => (p, false),
            None => (5173, true),
        };
    }
    if mentions(script, "next") {
        return (3000, true);
    }
    if root.join("angular.json").exists() {
        return (4200, true);
    }
    (3000, true)
}

/// Whether a script invokes `tool`, as a whole word.
///
/// Splitting on everything that is not part of a package name matches the tool
/// however it is reached — `vite`, `node_modules/.bin/vite`, `cross-env FOO=1
/// vite` — without `next` also matching a script called `next-thing`.
fn mentions(script: &str, tool: &str) -> bool {
    script
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .any(|w| w == tool)
}

const VITE_CONFIG_NAMES: [&str; 6] = [
    "vite.config.ts",
    "vite.config.js",
    "vite.config.mts",
    "vite.config.mjs",
    "vite.config.cts",
    "vite.config.cjs",
];

fn vite_config(root: &Path) -> Option<String> {
    VITE_CONFIG_NAMES
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.exists())
        .and_then(read)
}

/// The `server.port` a `vite.config.*` sets, if it sets one literally.
///
/// This is a regex over the source and not an evaluation of it: the file is
/// TypeScript, and running it to ask would mean executing the repo. A config
/// that computes its port from an env var simply has no literal to find, and
/// the caller falls back to Vite's 5173 with `port_guessed` set — which is what
/// that flag is for. The match is anchored inside the `server` block so a
/// `port` under `preview` or `test` is not mistaken for it.
fn vite_port(text: &str) -> Option<u16> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?s)\bserver\s*:\s*\{[^}]*?\bport\s*:\s*(\d+)").expect("static regex")
    });
    re.captures(text)?.get(1)?.as_str().parse().ok()
}

/// Compose files are found and listed, never started: the Run panel shows the
/// entry with [`COMPOSE_DISABLED`] so a user who expected their stack to come
/// up learns why it did not, instead of finding nothing at all.
fn detect_compose(root: &Path, out: &mut Vec<RunConfig>) {
    let found = [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ]
    .iter()
    .any(|n| root.join(n).exists());
    if !found {
        return;
    }
    let mut cfg = detected("compose", "docker compose up".into(), 0, false);
    cfg.disabled_reason = Some(COMPOSE_DISABLED.into());
    out.push(cfg);
}

/// Rust web frameworks whose presence in `[dependencies]` means the crate is a
/// server rather than a library.
const RUST_WEB_DEPS: [&str; 5] = ["axum", "actix-web", "actix", "rocket", "warp"];

fn detect_cargo(root: &Path, out: &mut Vec<RunConfig>) {
    let Some(text) = read(root.join("Cargo.toml")) else {
        return;
    };
    let manifest: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "Cargo.toml did not parse");
            return;
        }
    };
    let has_bin = manifest
        .get("bin")
        .and_then(|b| b.as_array())
        .is_some_and(|a| !a.is_empty());
    let has_web_dep = manifest
        .get("dependencies")
        .and_then(|d| d.as_table())
        .is_some_and(|t| RUST_WEB_DEPS.iter().any(|d| t.contains_key(*d)));
    if !has_bin && !has_web_dep {
        return;
    }
    // 8080 is a pure assumption: nothing in a Cargo manifest says which port
    // the binary binds, so the IDE has to let the user correct it.
    out.push(detected("cargo", "cargo run".into(), 8080, true));
}

fn detect_python(root: &Path, out: &mut Vec<RunConfig>) {
    if root.join("manage.py").exists() {
        out.push(detected(
            "django",
            "python manage.py runserver 0.0.0.0:8000".into(),
            8000,
            false,
        ));
    }
    let Some(text) = read(root.join("pyproject.toml")) else {
        return;
    };
    let deps = python_deps(&text);
    let has = |name: &str| deps.iter().any(|d| d == name);
    if has("fastapi") {
        out.push(detected(
            "fastapi",
            "uvicorn main:app --host 0.0.0.0 --port 8000".into(),
            8000,
            false,
        ));
    } else if has("flask") {
        out.push(detected(
            "flask",
            "flask run --host 0.0.0.0 --port 5000".into(),
            5000,
            false,
        ));
    }
}

/// The distribution names a `pyproject.toml` depends on, lowercased.
///
/// Both layouts in the wild are read — PEP 621's `[project] dependencies` array
/// of requirement strings and Poetry's `[tool.poetry.dependencies]` table — so
/// a Poetry-managed FastAPI service is not mistaken for a plain library. A
/// substring search over the file would have been shorter and would also have
/// matched the word in a comment or in the project's own name.
fn python_deps(text: &str) -> Vec<String> {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let project = v.get("project");
    if let Some(arr) = project
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        names.extend(arr.iter().filter_map(|x| x.as_str()).map(requirement_name));
    }
    if let Some(groups) = project
        .and_then(|p| p.get("optional-dependencies"))
        .and_then(|d| d.as_table())
    {
        for arr in groups.values().filter_map(|g| g.as_array()) {
            names.extend(arr.iter().filter_map(|x| x.as_str()).map(requirement_name));
        }
    }
    if let Some(table) = v
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        names.extend(table.keys().map(|k| k.to_ascii_lowercase()));
    }
    names
}

/// The bare name in front of any extras, version specifier or environment
/// marker: `fastapi[all]>=0.110 ; python_version >= "3.9"` is `fastapi`.
fn requirement_name(req: &str) -> String {
    req.trim()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bondsymphonic_proto::RunConfigSource;
    fn repo(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/repos")
            .join(name)
    }

    #[test]
    fn toml_runs_win_over_detection() {
        let v = configs_for(&repo("with-toml"));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "web");
        assert_eq!(v[0].command, "npm run dev -- --port 3000");
        assert_eq!(v[0].port, 3000);
        assert_eq!(v[0].source, RunConfigSource::ConfigFile);
        assert!(!v[0].port_guessed);
        assert_eq!(
            v[0].env.get("NODE_ENV").map(String::as_str),
            Some("development")
        );
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

    /// A config file the daemon cannot read must not make the repo look empty:
    /// the runs it can still work out from the tree are the ones the user will
    /// be reaching for while they fix the file.
    #[test]
    fn a_broken_config_falls_back_to_detection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bondsymphonic.toml"), "[[run]\nname = 1").unwrap();
        std::fs::write(dir.path().join("manage.py"), "#!/usr/bin/env python\n").unwrap();
        let v = configs_for(dir.path());
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "django");
        assert_eq!(v[0].source, RunConfigSource::Detected);

        // A file that parses but declares no run is the same fallback, without
        // the warning: `[network]`-only configs are an ordinary thing to write.
        std::fs::write(
            dir.path().join("bondsymphonic.toml"),
            "[network]\nallow = [\"a.example\"]\n",
        )
        .unwrap();
        let cfg = load_repo_config(dir.path()).unwrap().unwrap();
        assert!(cfg.run.is_empty());
        assert_eq!(cfg.network.allow, vec!["a.example".to_string()]);
        assert_eq!(configs_for(dir.path())[0].name, "django");
    }

    /// The detection order is the order the spec lists the heuristics in, and
    /// the Run panel preselects the first entry, so a repo that trips several
    /// of them has to come back with the runnable one in front.
    #[test]
    fn heuristics_keep_the_specs_order_and_the_package_manager_follows_the_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(
            p.join("package.json"),
            r#"{"scripts":{"start":"node server.js","dev":"node dev.js","build":"tsc"}}"#,
        )
        .unwrap();
        std::fs::write(p.join("yarn.lock"), "").unwrap();
        std::fs::write(p.join("compose.yaml"), "services: {}").unwrap();
        std::fs::write(
            p.join("Cargo.toml"),
            "[package]\nname=\"x\"\n[[bin]]\nname=\"x\"\n",
        )
        .unwrap();
        let found = detect(p);
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["dev", "start", "compose", "cargo"]);
        // `dev` before `start` is the listed order, not the file's; `build` is
        // not a server and is never offered.
        assert_eq!(found[0].command, "yarn run dev");
        // Nothing here is vite or next, so the fallback 3000 applies and is
        // flagged as the guess it is.
        assert_eq!((found[0].port, found[0].port_guessed), (3000, true));
    }

    /// Vite's port is read out of the config source, so the ways that source
    /// can be written have to be pinned: a computed port is a guess, and a
    /// `port` belonging to another block is not the server's.
    #[test]
    fn the_vite_port_is_only_taken_from_the_server_block() {
        assert_eq!(
            vite_port("export default { server: { port: 5174 } }"),
            Some(5174)
        );
        assert_eq!(
            vite_port("export default {\n  server: {\n    host: true,\n    port: 4000,\n  },\n}"),
            Some(4000)
        );
        // A port under another section is not the dev server's.
        assert_eq!(
            vite_port("export default { preview: { port: 4173 } }"),
            None
        );
        // Computed, so there is no literal to read.
        assert_eq!(
            vite_port("export default { server: { port: Number(env.PORT) } }"),
            None
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("vite.config.js"), "export default {}").unwrap();
        let v = detect(dir.path());
        assert_eq!((v[0].port, v[0].port_guessed), (5173, true));
    }

    /// Python projects declare dependencies in two shapes and dress the names
    /// up with extras and version specifiers, none of which changes which
    /// framework is being asked for.
    #[test]
    fn python_dependencies_are_read_from_both_layouts() {
        assert_eq!(
            requirement_name("fastapi[all]>=0.110 ; python_version >= \"3.9\""),
            "fastapi"
        );
        let poetry = "[tool.poetry.dependencies]\npython = \"^3.12\"\nFlask = \"^3.0\"\n";
        assert!(python_deps(poetry).contains(&"flask".to_string()));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), poetry).unwrap();
        let v = detect(dir.path());
        assert_eq!(v[0].name, "flask");
        assert_eq!((v[0].port, v[0].port_guessed), (5000, false));
        // A library with neither framework is not a run.
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname=\"lib\"\ndependencies=[\"requests\"]\n",
        )
        .unwrap();
        assert!(detect(dir.path()).is_empty());
    }

    /// A Cargo manifest only counts as a server when it says so; the workspace
    /// manifests and library crates a monorepo is full of must not each turn
    /// into a `cargo run` nobody can use.
    #[test]
    fn only_binary_or_web_cargo_manifests_are_runnable() {
        let dir = tempfile::tempdir().unwrap();
        let write = |body: &str| std::fs::write(dir.path().join("Cargo.toml"), body).unwrap();
        write("[workspace]\nmembers = [\"a\"]\n");
        assert!(detect(dir.path()).is_empty());
        write("[package]\nname=\"lib\"\n[dependencies]\nserde=\"1\"\n");
        assert!(detect(dir.path()).is_empty());
        write("[package]\nname=\"svc\"\n[dependencies]\nactix-web=\"4\"\n");
        assert_eq!(detect(dir.path()).len(), 1);
        write("[package]\nname=\"cli\"\n[[bin]]\nname=\"cli\"\npath=\"src/main.rs\"\n");
        assert_eq!(detect(dir.path())[0].command, "cargo run");
        // Unparseable manifests are skipped, not propagated as a failure.
        write("[package\nname=");
        assert!(detect(dir.path()).is_empty());
    }
}
