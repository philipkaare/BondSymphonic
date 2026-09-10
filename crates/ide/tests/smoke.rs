//! Offscreen end-to-end smoke test.
//!
//! Runs the real `bondsymphonic-ide` binary with `QT_QPA_PLATFORM=offscreen`
//! against an in-process fake daemon, and drives it with the two test hooks the
//! IDE carries for exactly this purpose: `BS_DAEMON_ADDR`/`BS_DAEMON_TOKEN`
//! (connect here instead of starting a daemon in WSL) and `BS_SMOKE_SCRIPT`
//! (perform these steps once connected). Both are inert when unset.
//!
//! What it proves: the window builds, the daemon connection comes up, the
//! workspace/PTY/file-tree requests reach the daemon in the right order, a
//! Claude agent's tab attaches a transcript of its own accord and carries one
//! whole turn — prompt, permission request, allow, tool call, result — with the
//! answer routed back through the window to the pane that was showing the
//! request, an editor tab and a diff tab open through the same controller
//! signals the Explorer emits and fetch their contents (`fs.read_file`,
//! `workspace.diff`) and their live-update watches (`fs.watch`,
//! `workspace.changes`), panes whose process has exited are torn down without
//! talking to the daemon about the PTYs it has already reaped, a run
//! configuration is detected and a run started and stopped on a bridged host
//! port, the network denial the daemon reports behind it becomes a toast on the
//! workspace that raised it and answering that toast sends the workspace's own
//! allowlist back with the blocked host added, a workspace is merged and then
//! squashed into a conflict and a pull request opened for it, the daemon drops
//! the connection and the IDE builds a new one and re-syncs on its own, the
//! layout is written to `state.json` with the workspace's group and its open
//! editor in it, the Qt event loop is still responsive at the end (the `quit`
//! step runs on it), and the process ends with status 0 well inside the time
//! limit.

use base64::Engine as _;
use bondsymphonic_ide::model::persistence::{StateFile, STATE_VERSION};
use bondsymphonic_proto::*;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

const TOKEN: &str = "smoke-token";
/// The Claude half comes first, on its own workspace, and the terminal half
/// second on another: a Claude tab has a transcript pane and no PTY, so the
/// `close`/`destroy` pair only means something over a workspace whose pane is a
/// terminal.
///
/// The two `merge` steps and the `pr` land on that terminal workspace, and the
/// `reconnect` comes after them, so everything before it was done on the first
/// connection and the `destroy` after it on the second.
const SCRIPT: &str = "create_claude,open_agent,send,allow,tree,open_file,open_diff,stop,create,\
                      detect,run_start,allow_host,run_stop,merge,merge,pr,open,close,reconnect,\
                      destroy,quit";
/// The file `open_file` and `open_diff` act on, and the one entry of the fake
/// `fs.list_dir` listing that is not a directory.
const OPEN_PATH: &str = "README.md";
/// The group the smoke script files its tabs under, spelled the same way
/// `qobjects::smoke` spells it, and the id the fake daemon gives the first
/// workspace — the Claude one, the only one still alive when the process ends.
const GROUP: &str = "Default";
const CLAUDE_WORKSPACE: &str = "ws_smoke1";
/// The second, the terminal one, which the `destroy` step takes away again.
const TERMINAL_WORKSPACE: &str = "ws_smoke2";
/// The agent the fake daemon hands out, the tool it asks about and the id of
/// the request the `allow` step answers. The script agrees the request id with
/// the daemon rather than reading it off the transcript, and the window refuses
/// to route an answer to a pane that is not showing exactly that request.
const AGENT_ID: &str = "ag_smoke1";
const REQUEST_ID: &str = "req-1";
const TOOL_NAME: &str = "Bash";
/// What the turn cost, so the assertion on the result frame has a number.
const TURN_COST_USD: f64 = 0.002;
/// The one run configuration this daemon reports, and the run it starts from
/// it. The port is the one the run listens on inside the sandbox.
const RUN_CONFIG: &str = "web";
const RUN_ID: &str = "run_smoke1";
const RUN_PORT: u16 = 3000;
/// The bridged port the daemon hands back. Deliberately not [`RUN_PORT`]: the
/// URL the panel shows has to come from the daemon's reply, never be rebuilt
/// from the configuration's own port.
const HOST_PORT: u16 = 41873;
const RUN_URL: &str = "http://localhost:41873";
/// The one line the run prints before it reports itself ready.
const RUN_OUTPUT_LINE: &str = "ready on 3000";
/// The host the fake proxy refuses just after the run comes up. Not in
/// [`DEFAULT_ALLOW`], which is what makes answering the toast a change.
const DENIED_HOST: &str = "example.com";
/// The allowlist a freshly created workspace carries, in the daemon spec's
/// order (§7.1). Spelled out here rather than imported from the daemon crate:
/// it is a fixture of what a daemon reports, and the assertion below is that
/// the IDE sent this list *back* with one host added rather than replacing it
/// with a stale or empty copy.
const DEFAULT_ALLOW: [&str; 12] = [
    "api.anthropic.com",
    "*.anthropic.com",
    "registry.npmjs.org",
    "*.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
];
/// The file the second merge reports a conflict in. Deliberately not
/// [`OPEN_PATH`]: a conflict list is the daemon's own reading of the failed
/// merge and has nothing to do with what the Explorer happens to be showing.
const CONFLICT_PATH: &str = "a.txt";
/// The summary the squashing merge carries. The first merge sends none, so the
/// pair shows both halves of the optional field crossing the wire.
const MERGE_SUMMARY: &str = "smoke: squashed";
/// What `pr` sends and what the fake daemon answers with. `draft` is true so
/// the flag is carried as something other than its default.
const PR_TITLE: &str = "Smoke PR";
const PR_BODY: &str = "Opened by the smoke run.";
const PR_DRAFT: bool = true;
const PR_URL: &str = "https://github.com/example/repo/pull/42";
/// The `quit` step alone waits 2 s, each of `open_agent`, `send`, `allow`,
/// `run_start` and `allow_host` another 1.5 s, `reconnect` a second of backoff
/// and 1.5 s of settling, and `create`, `create_claude`, `open_file`,
/// `open_diff`, `stop`, `detect`, `merge`, `pr` and `close` another 0.75 s each
/// — about 22 s of deliberate waiting; the rest is a Qt startup on a cold cache.
const RUN_LIMIT: Duration = Duration::from_secs(120);
/// The methods the script must produce, in this order. `agent.history` is the
/// window's own doing — only `TranscriptModel::attach` sends it, and the model
/// only attaches because the window reacted to `agentStarted` — so its place
/// between `agent.start` and `agent.send` shows the pane was built and wired up
/// before the turn began. `fs.read_file` is the editor tab loading its file and
/// `workspace.diff` the diff tab loading its alignment, so their place in the
/// sequence is what shows the two tabs opened in the order the script asked for
/// them.
/// `repo.detect_run_configs` and `run.list` between `agent.stop` and
/// `run.start` are the Run panel's own, issued when the second workspace's tab
/// appears and the panel is pointed at its worktree; `workspace.get` and
/// `workspace.set_allowlist` are `RunPanelModel::allowHost` answering the
/// toast, in that order, because it reads the daemon's list before it sends one
/// back.
/// The second `hello` and the `workspace.list` after it are the reconnect: no
/// step connects to anything, so a second handshake can only be
/// `AppController`'s own loop noticing the socket had gone and building a new
/// connection, and the `workspace.list` behind it is the re-sync that finds the
/// tabs the restarted daemon still knows. The `workspace.destroy` after both is
/// the proof that the script's next step reached the daemon on the *new*
/// connection.
const EXPECTED: [&str; 24] = [
    "hello",
    "workspace.create",
    "agent.start",
    "agent.history",
    "agent.send",
    "agent.permission_reply",
    "fs.list_dir",
    "fs.read_file",
    "workspace.diff",
    "agent.stop",
    "repo.detect_run_configs",
    "run.list",
    "run.start",
    "workspace.get",
    "workspace.set_allowlist",
    "run.stop",
    "workspace.merge",
    "workspace.merge",
    "workspace.create_pr",
    "pty.open",
    "pty.close",
    "hello",
    "workspace.list",
    "workspace.destroy",
];
/// Requests the window must have made after `workspace.create` on its own
/// account: the Changes tab lists the new workspace and turns on the file
/// watch. Neither is in the script, so seeing them proves the window wired the
/// workspace up rather than the script standing in for it.
const AFTER_CREATE: [&str; 2] = ["fs.watch", "workspace.changes"];
/// What `fs.read_file` returns and the base side of the diff.
const BASE_TEXT: &str = "hello\n";
/// The work side of the diff: one line added to [`BASE_TEXT`].
const WORK_TEXT: &str = "hello\nworld\n";
/// The warnings `EditorDocument`, `DiffDocument` and `ChangesModel` log when
/// one of their requests fails, matched as their exact prefixes. A run in which
/// the fake daemon answered but the reply was rejected — a body the IDE could
/// not deserialise, say — still reaches the journal, so the journal alone
/// cannot tell a served request from a served-and-refused one. These can.
/// `fs.write_file` is not among them: no step saves, so asserting on its
/// warning would assert nothing. The fake daemon answers it anyway, so a future
/// step that does save needs no change on the daemon side.
const NO_WARNINGS: [&str; 21] = [
    "fs.read_file failed",
    "workspace.diff failed",
    "workspace.changes failed",
    "fs.watch enable",
    // The transcript's own four. Each is logged by the `TranscriptModel` call
    // that issued the request, and the script drives all four of those calls
    // through the window, so every one of these is reachable: it means the IDE
    // refused a reply it was given -- a body it could not deserialise, say --
    // which the journal alone cannot show.
    "agent.history failed",
    "agent.send failed",
    "agent.permission_reply failed",
    "agent.stop failed",
    // The window's three, when a request arrives for an agent no visible
    // transcript is attached to. These are what make `send`, `allow` and `stop`
    // assertions rather than wishes: each reaches the daemon only through
    // `TranscriptModel`, and the window calls that only after it has found the
    // pane attached to the named agent -- with the request on its bar, for the
    // permission reply.
    "permission reply not routed",
    "agent send not routed",
    "agent stop not routed",
    // The Run panel's own, and the controller's. Each is logged by the call
    // that issued the request: the panel detects and lists when the tab
    // appears, the `detect` step goes through `AppController::detectRunConfigs`,
    // and `allowHost` makes both of the last two. `run.start failed` and
    // `run.stop failed` are deliberately *not* here: the script makes those two
    // on its own client, where a failure ends the step and stops the script
    // without quitting, which the exit-status assertion catches instead.
    "repo.detect_run_configs failed",
    "run.list failed",
    "workspace.get failed",
    "workspace.set_allowlist failed",
    // The window refusing to answer a denial for a workspace the Run panel is
    // not showing. This is what makes `allow_host` an assertion rather than a
    // wish: `workspace.set_allowlist` reaches the daemon only after the window
    // has matched the request against the panel's own workspace.
    "allow host not routed",
    // The Changes toolbar's three. A merge that conflicts is not one of these:
    // it comes back as `mergeFinished(ok: false)`, an answer rather than a
    // failure, and only a refusal or a `GitError` is logged here. So a run in
    // which either of the first two appears is a run where the request never
    // reached the daemon or its reply could not be read -- which the journal
    // alone cannot tell apart from a served one.
    "workspace.merge failed",
    "workspace.create_pr failed",
    // The pair the toolbar issues behind every `mergeFinished` to refresh its
    // "N changed files": logged together under this prefix when either half
    // fails, which would leave the Discard confirmation with no count.
    "workspace summary for",
    // The re-sync after the reconnect. Both are `AppController`'s own, and both
    // go out on the connection it has just built, so a warning here is a
    // reconnect that produced a status bar saying "connected" over a client
    // that could not be used.
    "workspace.list failed",
    "system.check_prereqs failed",
];

/// The fake daemon's own control method: it answers by closing the connection,
/// so the IDE's reconnect loop has something to reconnect from.
///
/// **Test-only, and fake-daemon-only.** It is not a `Request` variant, so it
/// can only be built by hand; to the real daemon's decoder it is an unknown
/// enum variant, answered `invalid_params` with nothing dropped.
/// `crates/ide/tests/reconnect_tests.rs` is where it is exercised end to end.
const TEST_DROP: &str = "system.test_drop";

/// A recorder the fake daemon appends to.
type Journal = Arc<Mutex<Vec<String>>>;

/// Everything the fake daemon writes down, so a request that arrived can be
/// told apart from a request that arrived carrying the right thing. The method
/// list alone cannot do that: it says a `workspace.merge` was sent, not which
/// mode it asked for.
#[derive(Clone)]
struct Journals {
    /// Every request method answered, in arrival order.
    methods: Journal,
    /// Every `agent.permission_reply` as `<request id>:<decision>`.
    replies: Journal,
    /// Every `workspace.set_allowlist` as its comma-joined host list.
    allowlists: Journal,
    /// Every `workspace.merge` as `<mode>|<summary>`, with `-` for a merge that
    /// sent none.
    merges: Journal,
    /// Every `workspace.create_pr` as `<title>|<body>|<draft>`.
    prs: Journal,
}

impl Journals {
    fn new() -> Self {
        let fresh = || -> Journal { Arc::new(Mutex::new(Vec::new())) };
        Self {
            methods: fresh(),
            replies: fresh(),
            allowlists: fresh(),
            merges: fresh(),
            prs: fresh(),
        }
    }

    fn push(journal: &Journal, entry: String) {
        journal.lock().expect("journal mutex").push(entry);
    }

    fn read(journal: &Journal) -> Vec<String> {
        journal.lock().expect("journal mutex").clone()
    }
}

#[test]
fn the_ide_drives_a_workspace_pty_and_file_tree_then_exits_cleanly() {
    if bondsymphonic_ide::testing::skip_without_qt("smoke") {
        return;
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let (addr, journals) = rt.block_on(fake_daemon());

    // The IDE persists its settings and its layout on exit. Both are pointed
    // at a directory of this run's own: a test must never write the developer's
    // real `%APPDATA%\BondSymphonic` files, and a smoke run that inherited
    // their groups would not be reproducible either.
    let config = std::env::temp_dir().join(format!("bs-smoke-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    std::fs::create_dir_all(&config).expect("smoke config dir");
    let state_path = config.join("state.json");

    let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("BS_DAEMON_ADDR", addr.to_string())
        .env("BS_DAEMON_TOKEN", TOKEN)
        .env("BS_SMOKE_SCRIPT", SCRIPT)
        .env("BS_SETTINGS_PATH", config.join("settings.json"))
        .env("BS_STATE_PATH", &state_path)
        .env("BS_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the IDE binary starts");

    // Drained on their own threads: a full pipe would deadlock the child long
    // before the time limit and turn a fast failure into a slow one.
    let out = drain(child.stdout.take().expect("stdout is piped"));
    let err = drain(child.stderr.take().expect("stderr is piped"));

    let status = wait_for(&mut child, RUN_LIMIT);
    // `expect`, not a default: an empty string here would vacate the panic
    // assertion and all four `NO_WARNINGS` assertions, turning a dead drain
    // thread into a green run.
    let (out, err) = (
        out.recv().expect("the stdout drain thread is alive"),
        err.recv().expect("the stderr drain thread is alive"),
    );
    let seen = Journals::read(&journals.methods);
    let answered = Journals::read(&journals.replies);
    let allowed = Journals::read(&journals.allowlists);
    let merges = Journals::read(&journals.merges);
    let prs = Journals::read(&journals.prs);
    // Read before the assertions, so a failure prints the file the run left
    // behind rather than only the fact that it was wrong.
    let state_json = std::fs::read_to_string(&state_path).unwrap_or_default();
    let context = format!(
        "requests: {seen:?}\npermission replies: {answered:?}\nallowlists: {allowed:?}\n\
         merges: {merges:?}\npull requests: {prs:?}\nstate.json: {state_json}\n\
         --- stdout ---\n{out}\n--- stderr ---\n{err}"
    );
    // Both pipes together. `tracing_subscriber::fmt()` writes to *stdout* by
    // default and `main` does not override the writer, so every warning the IDE
    // logs arrives on stdout; only a panic message comes out on stderr. An
    // assertion that read stderr alone would pass whatever was logged, which is
    // what the three terminal-warning assertions below used to do.
    let logs = format!("{out}\n{err}");
    eprintln!(
        "smoke: the fake daemon answered {seen:?}, permission replies {answered:?}, allowlists \
         {allowed:?}, merges {merges:?}, pull requests {prs:?}"
    );

    let status =
        status.unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
    assert!(
        status.success(),
        "the IDE exited with {status}, expected 0\n{context}"
    );
    assert!(
        !logs.contains("panicked at"),
        "the IDE logged a panic\n{context}"
    );
    assert!(
        contains_in_order(&seen, &EXPECTED),
        "the fake daemon did not see {EXPECTED:?} in order\n{context}"
    );
    // The script issues one of each; the window issues its own for the tab the
    // script created — the Explorer dock lists the new workspace's root and the
    // agent pane opens and sizes its terminal. Without these the run would still
    // be green while covering none of the C++ widgets, which is exactly what
    // happened when the script raced the connect-time `workspace.list` and the
    // reconcile dropped the tab out from under them.
    for method in ["fs.list_dir", "pty.open"] {
        let count = seen.iter().filter(|m| *m == method).count();
        assert!(
            count >= 2,
            "expected the window to issue its own {method} as well as the script's, saw \
             {count}\n{context}"
        );
    }
    // The Changes tab and the editor's watch are the window's own doing, and
    // both only make sense once there is a workspace: `set_workspace` on the
    // changes model issues `workspace.changes` and turns `fs.watch` on, and
    // `EditorDocument::open` turns it on again for the file it just read. A
    // `fs.watch` before the create would be for a workspace that does not
    // exist yet, so the position in the journal is part of the claim.
    let after_create: Vec<&String> = seen
        .iter()
        .skip_while(|m| *m != "workspace.create")
        .collect();
    for method in AFTER_CREATE {
        assert!(
            after_create.iter().any(|m| *m == method),
            "the window never sent {method} after workspace.create\n{context}"
        );
    }
    // What the `allow` step actually did. The journal shows an answer was sent;
    // this shows it named the request the transcript was showing and said yes.
    assert_eq!(
        answered,
        vec![format!("{REQUEST_ID}:allow")],
        "the permission reply the fake daemon received was not a single allow for \
         {REQUEST_ID}\n{context}"
    );
    // What the `allow_host` step actually did. The journal shows a
    // `workspace.set_allowlist` was sent; this shows what was in it. The IDE
    // reads the daemon's own list at the moment of the click and appends to it,
    // so the twelve defaults have to come back untouched and in order with the
    // blocked host after them. A list that replaced them instead would silently
    // un-allow every registry an agent needs, and would still have satisfied the
    // journal.
    let expected_allowlist: Vec<String> = DEFAULT_ALLOW
        .iter()
        .chain(std::iter::once(&DENIED_HOST))
        .map(|h| (*h).to_owned())
        .collect();
    assert_eq!(
        allowed,
        vec![expected_allowlist.join(",")],
        "the allowlist the fake daemon received was not the defaults plus {DENIED_HOST}\n{context}"
    );
    // What the two `merge` steps actually asked for. The journal shows two
    // `workspace.merge` calls; this shows the toolbar's word reached the daemon
    // as the right mode both times, and that an empty summary crossed as "not
    // supplied" while a real one crossed intact. A run that sent `merge` twice,
    // or that turned the empty box into an empty string, would still have
    // satisfied the journal.
    assert_eq!(
        merges,
        vec!["merge|-".to_owned(), format!("squash|{MERGE_SUMMARY}"),],
        "the merges the fake daemon received were not a merge with no summary then a squash with \
         one\n{context}"
    );
    // The same for the pull request: the journal shows one was asked for, this
    // shows the title, the body and the draft flag arrived as they were sent.
    assert_eq!(
        prs,
        vec![format!("{PR_TITLE}|{PR_BODY}|{PR_DRAFT}")],
        "the pull request the fake daemon received did not carry the smoke run's title, body and \
         draft flag\n{context}"
    );
    // The reconnect. Nothing in the script connects to anything, so a second
    // handshake can only be `AppController`'s loop noticing the socket had gone
    // and building a new connection by itself.
    let hellos = seen.iter().filter(|m| *m == "hello").count();
    assert_eq!(
        hellos, 2,
        "expected exactly two handshakes, one per connection\n{context}"
    );
    assert!(
        seen.iter().any(|m| m == TEST_DROP),
        "the reconnect step never asked the fake daemon to drop the connection\n{context}"
    );
    // A login terminal on the host is the one thing this run must never open,
    // and nothing in the script asks for one: every prerequisite the fake
    // daemon reports passes, so the setup page never appears.
    assert!(
        !seen.iter().any(|m| m == "system.setup_pty"),
        "the IDE asked for a setup terminal\n{context}"
    );
    // The transcript detaches when its workspace goes, and stops nothing: the
    // daemon reaps a destroyed workspace's agents itself. An `agent.*` after
    // the destroy would be the IDE talking about an agent that is already gone,
    // which the daemon answers `NotFound`.
    // The Claude workspace is never destroyed in this script -- the destroy is
    // for the terminal workspace created later -- so this also covers a live
    // transcript sitting through another tab's teardown.
    let after_destroy: Vec<&String> = seen
        .iter()
        .skip_while(|m| *m != "workspace.destroy")
        .collect();
    assert!(
        !after_destroy.iter().any(|m| m.starts_with("agent.")),
        "the IDE sent an agent request after workspace.destroy\n{context}"
    );
    // The `close` step ended every PTY the fake daemon had open, so by the time
    // `destroy` tears the panes down their processes have exited. A pane in that
    // state must not send `pty.close` or `pty.resize`: the daemon has reaped
    // those PTYs and answers `NotFound`, which used to put an error banner over
    // a pane whose only news was that its process had finished.
    for method in ["pty.close", "pty.resize", "pty.write"] {
        assert!(
            !after_destroy.iter().any(|m| *m == method),
            "the window sent {method} for a PTY that had already exited\n{context}"
        );
    }
    // The same thing from the session's side: those failures are what set the
    // `error` property the terminal paints its banner from.
    for warning in ["pty.close failed", "pty.resize failed", "pty.write failed"] {
        assert!(
            !logs.contains(warning),
            "a terminal recorded {warning:?} after its process exited\n{context}"
        );
    }
    // The fake daemon answered every editor, diff and changes request, so none
    // of their documents may have logged a failure. This catches the case the
    // journal cannot: a request that arrived and was answered with something
    // the IDE then refused.
    for warning in NO_WARNINGS {
        assert!(
            !logs.contains(warning),
            "the IDE logged {warning:?} even though the fake daemon answered\n{context}"
        );
    }

    // The layout the run left on disk, at `BS_STATE_PATH` and nowhere near the
    // developer's own `%APPDATA%\BondSymphonic`. Nothing in the script writes
    // it: the window reports its arrangement through `noteGroups` and its
    // editor tabs through `noteEditors`, and the controller writes the file
    // debounced behind them. So a file with the Claude workspace filed under
    // its group and the editor's path against it is the whole persistence path
    // having run -- and having survived the reconnect, which happened before
    // the process ended.
    assert!(
        !state_json.is_empty(),
        "the IDE wrote no state.json at BS_STATE_PATH\n{context}"
    );
    let state: StateFile = serde_json::from_str(&state_json)
        .unwrap_or_else(|e| panic!("state.json is not a StateFile ({e})\n{context}"));
    assert_eq!(
        state.version, STATE_VERSION,
        "state.json carries the wrong format version\n{context}"
    );
    let group = state
        .groups
        .iter()
        .find(|g| g.name == GROUP)
        .unwrap_or_else(|| panic!("state.json has no {GROUP:?} group\n{context}"));
    // The group's whole membership, not just "the survivor is in there
    // somewhere". The destroyed workspace has to be gone from it, or a restart
    // would try to restore a tab for a workspace the daemon no longer has, and
    // a `contains` check would pass while it sat there.
    assert_eq!(
        group.workspace_ids,
        vec![CLAUDE_WORKSPACE.to_owned()],
        "state.json's {GROUP:?} group should hold exactly {CLAUDE_WORKSPACE}, with the destroyed \
         {TERMINAL_WORKSPACE} dropped\n{context}"
    );
    // And it is not hiding in another group either -- "Unsorted", say, if a
    // reconcile had moved it before the destroy.
    assert!(
        !state
            .groups
            .iter()
            .any(|g| g.workspace_ids.iter().any(|id| id == TERMINAL_WORKSPACE)),
        "state.json still files the destroyed {TERMINAL_WORKSPACE} under a group\n{context}"
    );
    assert_eq!(
        state.open_editors.get(CLAUDE_WORKSPACE).map(Vec::as_slice),
        Some(&[OPEN_PATH.to_owned()][..]),
        "state.json did not record {OPEN_PATH} as {CLAUDE_WORKSPACE}'s open editor\n{context}"
    );
}

/// Whether `wanted` appears in `seen` in order, other requests in between
/// allowed. The window issues its own `pty.open` and `fs.list_dir` for the tab
/// the script creates, so the script's requests are a subsequence of the whole,
/// not the whole.
fn contains_in_order(seen: &[String], wanted: &[&str]) -> bool {
    let mut rest = wanted.iter();
    let mut next = rest.next();
    for method in seen {
        if next == Some(&method.as_str()) {
            next = rest.next();
        }
    }
    next.is_none()
}

/// Reads a child pipe to end on its own thread.
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// Waits up to `limit` for `child`, killing it and returning `None` if it is
/// still running. Polling beats a wait thread here: the child has to be killed
/// on timeout, which needs the handle back.
fn wait_for(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn workspace(id: &str, name: &str, state: WorkspaceState, allowlist: &[String]) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(id.to_owned()),
        name: name.to_owned(),
        repo_path: "/smoke/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{name}/work"),
        worktree_path: format!("/wt/{id}"),
        created_at: "2026-09-09T10:00:00Z".to_owned(),
        allowlist: allowlist.to_vec(),
        state,
        agents: Vec::new(),
        agent_records: Vec::new(),
        runs: Vec::new(),
    }
}

/// The timestamp every transcript message carries. The IDE displays it and
/// never orders by it -- `seq` does that -- so one value for the run is enough.
const TS: &str = "2026-09-09T10:00:00Z";

/// Records `body` in the fake's transcript, so `agent.history` can read it
/// back, and returns the `agent.message` event a real daemon broadcasts for it.
/// `seq` is the position in that transcript, which is what makes it monotonic.
fn emit(
    transcript: &mut Vec<AgentMessage>,
    workspace_id: &WorkspaceId,
    agent_id: &AgentId,
    body: AgentMessageBody,
) -> ServerMessage {
    let message = AgentMessage {
        seq: transcript.len() as u64 + 1,
        ts: TS.to_owned(),
        body,
    };
    transcript.push(message.clone());
    ServerMessage::event(
        Some(workspace_id.clone()),
        Event::AgentMessage {
            agent_id: agent_id.clone(),
            message,
        },
    )
}

fn agent_state(
    workspace_id: &WorkspaceId,
    agent_id: &AgentId,
    state: AgentState,
    detail: Option<&str>,
) -> ServerMessage {
    ServerMessage::event(
        Some(workspace_id.clone()),
        Event::AgentStateChanged {
            agent_id: agent_id.clone(),
            state,
            detail: detail.map(str::to_owned),
        },
    )
}

/// A `run.state` event for `run_id`. The url travels on the `ready` transition
/// and nowhere else, which is how a real daemon reports it.
fn run_state(
    workspace_id: &WorkspaceId,
    run_id: &RunId,
    state: RunState,
    url: Option<String>,
) -> ServerMessage {
    ServerMessage::event(
        Some(workspace_id.clone()),
        Event::RunStateChanged {
            run_id: run_id.clone(),
            state,
            url,
            detail: None,
        },
    )
}

/// The single run configuration this daemon detects, whatever path is asked
/// about. Its port is flagged as a guess, so the Run panel renders the label and
/// the tooltip it keeps for that case.
fn run_config() -> RunConfig {
    RunConfig {
        name: RUN_CONFIG.to_owned(),
        command: "python3 -m http.server 3000".to_owned(),
        port: RUN_PORT,
        cwd: None,
        env: BTreeMap::new(),
        ready_regex: None,
        source: RunConfigSource::Detected,
        port_guessed: true,
        disabled_reason: None,
    }
}

fn entry(name: &str, is_dir: bool, size: u64) -> FileEntry {
    FileEntry {
        name: name.to_owned(),
        is_dir,
        size,
        status: FileStatus::Unchanged,
    }
}

/// A daemon just real enough for one IDE session: it answers the handshake, the
/// connect-time calls, and the ones the script makes, and it emits the events a
/// real daemon would (Creating then Ready for the new workspace, one line of
/// output for each PTY, an exit for each PTY it ends). Everything else is an
/// explicit error, so an unexpected request shows up in the journal rather than
/// hanging the IDE.
async fn fake_daemon() -> (std::net::SocketAddr, Journals) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let journals = Journals::new();
    let recording = journals.clone();

    tokio::spawn(async move {
        let Journals {
            methods: recorded,
            replies: recorded_replies,
            allowlists: recorded_allowlists,
            merges: recorded_merges,
            prs: recorded_prs,
        } = recording;
        // How many merges have been asked for. The first is answered as work
        // that landed and the ones after it as a conflict, so one script can
        // drive both shapes of `MergeResult` -- an answer either way, never an
        // error -- through the same path.
        let mut merges = 0usize;
        // One workspace id per created name, so a repeated `create` is answered
        // consistently and `pty.open` can be checked against a known workspace.
        let mut workspaces: HashMap<String, String> = HashMap::new();
        // The other direction, plus each workspace's allowlist as it stands, so
        // `workspace.get` answers with what the last `workspace.set_allowlist`
        // left behind rather than with the fixture.
        let mut names: HashMap<String, String> = HashMap::new();
        let mut allowed: HashMap<String, Vec<String>> = HashMap::new();
        // The runs this daemon has handed out and the workspace they belong to,
        // so `run.list` and the `run.state` events have somewhere to come from.
        let mut runs: Vec<RunInfo> = Vec::new();
        let mut run_workspace: Option<WorkspaceId> = None;
        let mut ptys = 0usize;
        // Every PTY handed out and the workspace it belongs to, so `pty.close`
        // can end all of them at once.
        let mut open_ptys: Vec<(WorkspaceId, PtyId)> = Vec::new();
        // The id of a `pty.close` whose reply is being held; see the arm below.
        let mut held_close: Option<u64> = None;
        // The one agent this daemon hands out, the workspace it belongs to, and
        // everything it has broadcast, which `agent.history` reads back.
        let mut agent: Option<(WorkspaceId, AgentId)> = None;
        let mut transcript: Vec<AgentMessage> = Vec::new();
        // A daemon that can be restarted. The listener stays bound for the
        // whole run, so the connection the IDE makes after a
        // `system.test_drop` lands on the same port the first one did --
        // which is what makes this a daemon restarting rather than the IDE
        // finding a different one. Everything the daemon knows is declared
        // above this loop, so it survives the drop the way a real daemon
        // reads its registry back off disk.
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = stream.into_split();
            let mut r = BufReader::new(r);
            let mut line = String::new();
            loop {
                line.clear();
                // The `quit` step ends the process, which resets this socket rather
                // than closing it politely, so a read error ends the session too.
                match r.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim_end();
                // The method is read before the body is typed. `system.test_drop`
                // is not a `Request` the proto crate knows -- it is this fake's
                // own control method, and the real daemon answers it "not
                // implemented" -- so decoding it as one would fail rather than
                // reach the arm below.
                let method = serde_json::from_str::<serde_json::Value>(trimmed)
                    .ok()
                    .and_then(|v| v.get("method")?.as_str().map(str::to_owned))
                    .unwrap_or_default();
                Journals::push(&recorded, method.clone());
                if method == TEST_DROP {
                    // No reply at all: the socket simply goes, which is what a
                    // daemon that has died looks like from the IDE's side. The
                    // accept loop above takes the reconnect on the same port.
                    break;
                }
                let ClientMessage::Request { id, request } =
                    codec::decode(trimmed).expect("decode");

                let mut follow_ups: Vec<ServerMessage> = Vec::new();
                let reply: Option<ServerMessage> = match request {
                    Request::Hello(p) if p.token == TOKEN => Some(ServerMessage::ok(
                        id,
                        &HelloResult {
                            daemon_version: "0.0.0-fake".into(),
                            capabilities: Capabilities {
                                sandbox_backend: "noop".into(),
                                git_protect: false,
                                // What a daemon with a `claude` on PATH advertises.
                                // The New Agent dialog opens on Claude only when it
                                // sees this, so the list is part of the fixture.
                                adapters: vec![
                                    AgentAdapterKind::Terminal,
                                    AgentAdapterKind::Claude,
                                ],
                            },
                            protocol_version: Some(PROTOCOL_VERSION),
                        },
                    )),
                    Request::Hello(_) => Some(ServerMessage::err(id, RpcError::unauthorized())),
                    Request::SystemCheckPrereqs {} => Some(ServerMessage::ok(
                        id,
                        &CheckPrereqsResult {
                            items: vec![PrereqStatus {
                                name: "git".into(),
                                ok: true,
                                detail: "git version 2.43".into(),
                                fix_hint: None,
                            }],
                        },
                    )),
                    // Every workspace this daemon has made, which is empty at
                    // the connect-time call and not after a `system.test_drop`:
                    // a restarted daemon reads its registry back, so the IDE's
                    // re-sync has to find the tabs it already has rather than
                    // reconciling them away.
                    Request::WorkspaceList {} => {
                        let workspaces = names
                            .iter()
                            .map(|(ws, name)| {
                                let hosts = allowed.get(ws).cloned().unwrap_or_default();
                                workspace(ws, name, WorkspaceState::Ready, &hosts)
                            })
                            .collect::<Vec<_>>();
                        Some(ServerMessage::ok(id, &WorkspaceListResult { workspaces }))
                    }
                    Request::WorkspaceCreate(p) => {
                        let ws_id = format!("ws_smoke{}", workspaces.len() + 1);
                        workspaces.insert(p.name.clone(), ws_id.clone());
                        names.insert(ws_id.clone(), p.name.clone());
                        // A real daemon gives a new workspace the default list,
                        // extended by the repository's `bondsymphonic.toml`. There
                        // is no toml here, so it is the twelve defaults exactly.
                        let hosts: Vec<String> =
                            DEFAULT_ALLOW.iter().map(|h| (*h).to_owned()).collect();
                        allowed.insert(ws_id.clone(), hosts.clone());
                        let creating = workspace(&ws_id, &p.name, WorkspaceState::Creating, &hosts);
                        let ready = workspace(&ws_id, &p.name, WorkspaceState::Ready, &hosts);
                        // A real daemon answers while still creating and reports the
                        // rest through events; the tab has to survive both.
                        for info in [creating.clone(), ready] {
                            follow_ups.push(ServerMessage::event(
                                Some(WorkspaceId(ws_id.clone())),
                                Event::WorkspaceStateChanged {
                                    info: Box::new(info),
                                },
                            ));
                        }
                        Some(ServerMessage::ok(id, &creating))
                    }
                    Request::PtyOpen(p) => {
                        ptys += 1;
                        let pty_id = PtyId(format!("pty_smoke{ptys}"));
                        open_ptys.push((p.workspace_id.clone(), pty_id.clone()));
                        follow_ups.push(ServerMessage::event(
                            Some(p.workspace_id.clone()),
                            Event::PtyOutput {
                                pty_id: pty_id.clone(),
                                data_b64: BASE64.encode("prompt$ "),
                            },
                        ));
                        Some(ServerMessage::ok(id, &PtyOpenResult { pty_id }))
                    }
                    // A real daemon would exit only the PTY named here. This one
                    // ends every PTY it has handed out, because the script closes
                    // its own and has no way to name the ones the window opened for
                    // its panes — and those are the ones the steps after this have
                    // to find already exited.
                    //
                    // The reply is held until such a pane exists: the window opens
                    // its terminal only once Qt has laid it out, which is later
                    // than the script's first steps, so answering straight away
                    // would end the script's PTY and leave the window's untouched.
                    Request::PtyClose(_) => {
                        held_close = Some(id);
                        None
                    }
                    Request::WorkspaceDestroy(_) => Some(ServerMessage::ok(id, &Empty {})),
                    // One agent, started once. A real daemon reports the process
                    // coming up as a state change rather than in the reply, so the
                    // IDE has to survive an `agent.state` that arrives before its
                    // transcript has subscribed -- which is exactly what the
                    // router's early buffer is for.
                    Request::AgentStart(p) => {
                        let agent_id = AgentId(AGENT_ID.to_owned());
                        agent = Some((p.workspace_id.clone(), agent_id.clone()));
                        follow_ups.push(agent_state(
                            &p.workspace_id,
                            &agent_id,
                            AgentState::Working,
                            None,
                        ));
                        Some(ServerMessage::ok(id, &AgentStartResult { agent_id }))
                    }
                    // A file read on a real daemon. Here it is whatever has been
                    // broadcast so far, which is what a reopened tab would replay.
                    Request::AgentHistory(_) => Some(ServerMessage::ok(
                        id,
                        &HistoryResult {
                            messages: transcript.clone(),
                            state: AgentState::Idle,
                            detail: None,
                        },
                    )),
                    // The prompt, then the turn stalling on a tool the user has to
                    // allow: the same shape as the `permission_turn.ndjson` fixture
                    // the daemon's parser tests run on.
                    Request::AgentSend(p) => match agent.clone() {
                        Some((ws, ag)) => {
                            follow_ups.push(emit(
                                &mut transcript,
                                &ws,
                                &ag,
                                AgentMessageBody::UserText { text: p.text },
                            ));
                            follow_ups.push(emit(
                                &mut transcript,
                                &ws,
                                &ag,
                                AgentMessageBody::System {
                                    subtype: "init".to_owned(),
                                    data: serde_json::json!({
                                        "session_id": "sess-3",
                                        "model": "claude-opus-5",
                                        "tools": [TOOL_NAME],
                                    }),
                                },
                            ));
                            follow_ups.push(emit(
                                &mut transcript,
                                &ws,
                                &ag,
                                AgentMessageBody::PermissionRequest {
                                    request_id: REQUEST_ID.to_owned(),
                                    tool_name: TOOL_NAME.to_owned(),
                                    input: serde_json::json!({ "command": "rm -rf build" }),
                                    suggestions: Vec::new(),
                                },
                            ));
                            follow_ups.push(agent_state(
                                &ws,
                                &ag,
                                AgentState::WaitingPermission,
                                Some(TOOL_NAME),
                            ));
                            Some(ServerMessage::ok(id, &Empty {}))
                        }
                        None => Some(ServerMessage::err(
                            id,
                            RpcError::internal("agent.send before agent.start"),
                        )),
                    },
                    // The answer, and the rest of the turn it unblocks.
                    Request::AgentPermissionReply(p) => {
                        let decision = match p.decision {
                            PermissionDecision::Allow => "allow",
                            PermissionDecision::Deny => "deny",
                        };
                        Journals::push(&recorded_replies, format!("{}:{decision}", p.request_id));
                        match agent.clone() {
                            Some((ws, ag)) => {
                                follow_ups.push(agent_state(&ws, &ag, AgentState::Working, None));
                                follow_ups.push(emit(
                                    &mut transcript,
                                    &ws,
                                    &ag,
                                    AgentMessageBody::ToolUse {
                                        id: "toolu_2".to_owned(),
                                        name: TOOL_NAME.to_owned(),
                                        input: serde_json::json!({ "command": "rm -rf build" }),
                                    },
                                ));
                                follow_ups.push(emit(
                                    &mut transcript,
                                    &ws,
                                    &ag,
                                    AgentMessageBody::ToolResult {
                                        id: "toolu_2".to_owned(),
                                        output: "removed 'build'".to_owned(),
                                        is_error: false,
                                    },
                                ));
                                follow_ups.push(emit(
                                    &mut transcript,
                                    &ws,
                                    &ag,
                                    AgentMessageBody::Result {
                                        cost_usd: TURN_COST_USD,
                                        duration_ms: 500,
                                        num_turns: 1,
                                        session_id: "sess-3".to_owned(),
                                    },
                                ));
                                follow_ups.push(agent_state(&ws, &ag, AgentState::Idle, None));
                                Some(ServerMessage::ok(id, &Empty {}))
                            }
                            None => Some(ServerMessage::err(
                                id,
                                RpcError::internal("agent.permission_reply before agent.start"),
                            )),
                        }
                    }
                    Request::AgentInterrupt(_) => Some(ServerMessage::ok(id, &Empty {})),
                    Request::AgentStop(_) => {
                        if let Some((ws, ag)) = agent.clone() {
                            follow_ups.push(agent_state(
                                &ws,
                                &ag,
                                AgentState::Exited,
                                Some("exit code 0"),
                            ));
                        }
                        Some(ServerMessage::ok(id, &Empty {}))
                    }
                    // Answered, and answered with a failure: a login terminal on
                    // the host is the one thing this run must never open, and an
                    // error here would show up in the journal rather than starting
                    // one. The assertions below require it never to be asked for.
                    Request::SystemSetupPty(_) => Some(ServerMessage::err(
                        id,
                        RpcError::internal("the smoke run never logs in"),
                    )),
                    // The terminal widget restates its size once the PTY exists.
                    Request::PtyResize(_) | Request::PtyWrite(_) => {
                        Some(ServerMessage::ok(id, &Empty {}))
                    }
                    Request::FsListDir(_) => Some(ServerMessage::ok(
                        id,
                        &ListDirResult {
                            entries: vec![entry("src", true, 0), entry(OPEN_PATH, false, 42)],
                        },
                    )),
                    // The editor tab's load. Small, valid UTF-8 and not truncated,
                    // so the document opens editable rather than as a notice.
                    Request::FsReadFile(_) => Some(ServerMessage::ok(
                        id,
                        &ReadFileResult {
                            content: BASE_TEXT.to_owned(),
                            encoding: "utf-8".to_owned(),
                            truncated: false,
                        },
                    )),
                    // Ctrl+S and the watch the editor and the Changes tab both ask
                    // for. Nothing here has to do anything: the assertions are that
                    // the requests were made and that neither was reported failed.
                    Request::FsWriteFile(_) | Request::FsWatch(_) => {
                        Some(ServerMessage::ok(id, &Empty {}))
                    }
                    // One modified file, so the Changes tab has a row to build and
                    // the counts have somewhere to land.
                    Request::WorkspaceChanges(_) => Some(ServerMessage::ok(
                        id,
                        &ChangesResult {
                            files: vec![ChangedFile {
                                path: OPEN_PATH.to_owned(),
                                status: FileStatus::Modified,
                                additions: 1,
                                deletions: 0,
                            }],
                        },
                    )),
                    // One added line, which aligns to one equal row and one insert
                    // row: enough for `DiffWidget` to build both panes, tint a row
                    // and size its gutter from two different line-number columns.
                    Request::WorkspaceDiff(_) => Some(ServerMessage::ok(
                        id,
                        &DiffResult {
                            base_text: BASE_TEXT.to_owned(),
                            work_text: WORK_TEXT.to_owned(),
                            truncated: false,
                        },
                    )),
                    // What `RunPanelModel::allowHost` reads before it writes.
                    Request::WorkspaceGet(p) => {
                        let ws = p.workspace_id.0.clone();
                        match names.get(&ws) {
                            Some(name) => {
                                let hosts = allowed.get(&ws).cloned().unwrap_or_default();
                                let info = workspace(&ws, name, WorkspaceState::Ready, &hosts);
                                Some(ServerMessage::ok(id, &info))
                            }
                            None => Some(ServerMessage::err(id, RpcError::not_found(ws))),
                        }
                    }
                    // The other half of the click. A real daemon persists the list
                    // and reports the new one as a `workspace.state` event, which is
                    // what refreshes every client; the assertion is on what arrived
                    // here.
                    Request::WorkspaceSetAllowlist(p) => {
                        let ws = p.workspace_id.0.clone();
                        Journals::push(&recorded_allowlists, p.hosts.join(","));
                        allowed.insert(ws.clone(), p.hosts.clone());
                        if let Some(name) = names.get(&ws) {
                            let info = workspace(&ws, name, WorkspaceState::Ready, &p.hosts);
                            follow_ups.push(ServerMessage::event(
                                Some(p.workspace_id.clone()),
                                Event::WorkspaceStateChanged {
                                    info: Box::new(info),
                                },
                            ));
                        }
                        Some(ServerMessage::ok(id, &Empty {}))
                    }
                    // The Changes toolbar's half of a merge. Both answers are
                    // successes at the RPC level -- a conflict is a `MergeResult`
                    // with `ok: false`, not an error -- because that is the split
                    // the toolbar branches on: `mergeFinished` for both, a banner
                    // only for the second.
                    Request::WorkspaceMerge(p) => {
                        let mode = match p.mode {
                            MergeMode::Merge => "merge",
                            MergeMode::Rebase => "rebase",
                            MergeMode::Squash => "squash",
                        };
                        // `-` for a merge that sent no summary, so the two cases
                        // are told apart in the journal rather than both reading
                        // as an empty string.
                        let summary = p.message.clone().unwrap_or_else(|| "-".to_owned());
                        Journals::push(&recorded_merges, format!("{mode}|{summary}"));
                        merges += 1;
                        let result = if merges == 1 {
                            MergeResult {
                                ok: true,
                                conflicts: Vec::new(),
                                reason: None,
                            }
                        } else {
                            MergeResult {
                                ok: false,
                                conflicts: vec![CONFLICT_PATH.to_owned()],
                                reason: Some("conflict".to_owned()),
                            }
                        };
                        Some(ServerMessage::ok(id, &result))
                    }
                    // The pull request. A real daemon pushes the branch and shells
                    // out to `gh`; what the IDE has to get right is only that the
                    // dialog's three fields reach the wire and that the URL in the
                    // reply is what it shows.
                    Request::WorkspaceCreatePr(p) => {
                        Journals::push(
                            &recorded_prs,
                            format!("{}|{}|{}", p.title, p.body, p.draft),
                        );
                        Some(ServerMessage::ok(
                            id,
                            &CreatePrResult {
                                url: PR_URL.to_owned(),
                            },
                        ))
                    }
                    // Half of what the Changes toolbar asks for behind every
                    // `mergeFinished` to refresh its changed-file count; the other
                    // half is `workspace.changes` above. One dirty file, matching
                    // that listing.
                    Request::WorkspaceStatus(_) => Some(ServerMessage::ok(
                        id,
                        &WorkspaceStatusResult {
                            entries: vec![GitStatusEntry {
                                path: OPEN_PATH.to_owned(),
                                status: FileStatus::Modified,
                                staged: false,
                            }],
                        },
                    )),
                    // One configuration, whatever path is asked about: the New Agent
                    // dialog asks about the repository and the Run panel about the
                    // worktree, and both have to get a list they can render.
                    Request::RepoDetectRunConfigs(_) => Some(ServerMessage::ok(
                        id,
                        &DetectRunConfigsResult {
                            configs: vec![run_config()],
                            network_allow: vec![],
                            warnings: vec![],
                        },
                    )),
                    // The run, on a bridged port, then the three events a real
                    // daemon reports it with -- and behind them the proxy refusing a
                    // host the run reached for, built with the same proto helper the
                    // daemon builds it with, so the IDE recognises it the same way.
                    Request::RunStart(p) => {
                        let run_id = RunId(RUN_ID.to_owned());
                        run_workspace = Some(p.workspace_id.clone());
                        runs.push(RunInfo {
                            run_id: run_id.clone(),
                            config_name: p.config_name.clone(),
                            state: RunState::Ready,
                            host_port: HOST_PORT,
                            url: RUN_URL.to_owned(),
                        });
                        follow_ups.push(run_state(
                            &p.workspace_id,
                            &run_id,
                            RunState::Starting,
                            None,
                        ));
                        follow_ups.push(ServerMessage::event(
                            Some(p.workspace_id.clone()),
                            Event::RunOutput {
                                run_id: run_id.clone(),
                                line: RUN_OUTPUT_LINE.to_owned(),
                            },
                        ));
                        follow_ups.push(run_state(
                            &p.workspace_id,
                            &run_id,
                            RunState::Ready,
                            Some(RUN_URL.to_owned()),
                        ));
                        follow_ups.push(ServerMessage::event(
                            Some(p.workspace_id.clone()),
                            Event::network_denied(DENIED_HOST),
                        ));
                        Some(ServerMessage::ok(
                            id,
                            &RunStartResult {
                                run_id,
                                host_port: HOST_PORT,
                                url: RUN_URL.to_owned(),
                            },
                        ))
                    }
                    Request::RunStop(p) => {
                        runs.retain(|r| r.run_id != p.run_id);
                        if let Some(ws) = run_workspace.clone() {
                            follow_ups.push(run_state(&ws, &p.run_id, RunState::Stopped, None));
                        }
                        Some(ServerMessage::ok(id, &Empty {}))
                    }
                    Request::RunList(_) => {
                        Some(ServerMessage::ok(id, &RunListResult { runs: runs.clone() }))
                    }
                    other => Some(ServerMessage::err(
                        id,
                        RpcError::internal(format!("not implemented: {}", other.method_name())),
                    )),
                };

                // The held `pty.close` is answered as soon as the window has a pane
                // of its own, and every PTY ends with it.
                if let Some(close_id) = held_close {
                    if open_ptys.len() >= 2 {
                        held_close = None;
                        for (workspace_id, pty_id) in open_ptys.drain(..) {
                            follow_ups.push(ServerMessage::event(
                                Some(workspace_id),
                                Event::PtyExit { pty_id, code: 0 },
                            ));
                        }
                        follow_ups.push(ServerMessage::ok(close_id, &Empty {}));
                    }
                }

                let mut batch = reply.as_ref().map(codec::encode).unwrap_or_default();
                for message in &follow_ups {
                    batch.push_str(&codec::encode(message));
                }
                if w.write_all(batch.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });

    (addr, journals)
}

#[test]
fn request_order_is_checked_as_a_subsequence() {
    /// A run the way the journal really comes out: the connect-time calls, the
    /// window's own requests for each tab, and the script's, interleaved.
    fn journal(methods: &[&str]) -> Vec<String> {
        methods.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The same run with the first `method` taken out. Used to show that a run
    /// missing one call is not a match, without respelling the other forty.
    fn drop_first(methods: &[&'static str], method: &str) -> Vec<&'static str> {
        let mut out = methods.to_vec();
        let at = out
            .iter()
            .position(|m| *m == method)
            .unwrap_or_else(|| panic!("{method} is not in the fixture"));
        out.remove(at);
        out
    }

    /// The same, for the last occurrence: which is how the reconnect's second
    /// `hello` and the second of the two merges are taken away.
    fn drop_last(methods: &[&'static str], method: &str) -> Vec<&'static str> {
        let mut out = methods.to_vec();
        let at = out
            .iter()
            .rposition(|m| *m == method)
            .unwrap_or_else(|| panic!("{method} is not in the fixture"));
        out.remove(at);
        out
    }

    /// The same run with the first `a` and the first `b` exchanged, which is
    /// how every "in the wrong order" case below is built.
    fn swap_first(methods: &[&'static str], a: &str, b: &str) -> Vec<&'static str> {
        let mut out = methods.to_vec();
        let (i, j) = (
            out.iter()
                .position(|m| *m == a)
                .unwrap_or_else(|| panic!("{a} is not in the fixture")),
            out.iter()
                .position(|m| *m == b)
                .unwrap_or_else(|| panic!("{b} is not in the fixture")),
        );
        out.swap(i, j);
        out
    }

    // One whole run, in the order the fake daemon really sees it: the
    // connect-time pair, the Claude tab and its turn, the editor and the diff,
    // the terminal tab with its Run panel, the merge pair and the pull request,
    // the PTY pair, then the drop -- and behind it the second handshake, the
    // controller's re-sync and every pane re-attaching on the new connection,
    // with the script's `workspace.destroy` last of all.
    let full: Vec<&'static str> = vec![
        "hello",
        "workspace.list",
        "system.check_prereqs",
        "workspace.create",
        "fs.list_dir",
        "fs.watch",
        "workspace.changes",
        "repo.detect_run_configs",
        "run.list",
        "agent.start",
        "agent.history",
        "agent.send",
        "agent.permission_reply",
        "fs.list_dir",
        "fs.read_file",
        "fs.watch",
        "workspace.diff",
        "agent.stop",
        "workspace.create",
        "fs.list_dir",
        "repo.detect_run_configs",
        "run.list",
        "pty.open",
        "run.start",
        "workspace.get",
        "workspace.set_allowlist",
        "run.stop",
        "workspace.merge",
        // The Changes toolbar refreshing its count behind the merge that landed.
        "workspace.status",
        "workspace.changes",
        "workspace.merge",
        "workspace.create_pr",
        "pty.open",
        "pty.close",
        "system.test_drop",
        "hello",
        "workspace.list",
        "system.check_prereqs",
        "fs.list_dir",
        "fs.watch",
        "workspace.changes",
        "repo.detect_run_configs",
        "run.list",
        "agent.history",
        "workspace.destroy",
    ];
    assert!(contains_in_order(&journal(&full), &EXPECTED));

    // Order still matters: a `pty.open` before the first create is not a match,
    // because the only PTY this script opens is the one after the pull request.
    let mut early_pty: Vec<&'static str> =
        full.iter().copied().filter(|m| *m != "pty.open").collect();
    early_pty.insert(1, "pty.open");
    assert!(!contains_in_order(&journal(&early_pty), &EXPECTED));

    // Nor does the diff count as the file's own load: a run that opened the
    // diff first would put `workspace.diff` ahead of `fs.read_file`.
    let diff_first = swap_first(&full, "fs.read_file", "workspace.diff");
    assert!(!contains_in_order(&journal(&diff_first), &EXPECTED));

    // A transcript that replayed its history only after the turn had started
    // would be a pane attached too late to have shown the permission bar.
    let history_late = swap_first(&full, "agent.history", "agent.send");
    assert!(!contains_in_order(&journal(&history_late), &EXPECTED));

    // A turn that never asked for permission, or was never answered, is not a
    // match: the whole point of the Claude half is that one reply went out.
    let unanswered = drop_first(&full, "agent.permission_reply");
    assert!(!contains_in_order(&journal(&unanswered), &EXPECTED));

    // An "Allow host" that sent a list without reading the daemon's first would
    // put `workspace.set_allowlist` ahead of `workspace.get`. The order is the
    // claim: the IDE extends the workspace's own allowlist rather than
    // replacing it with whatever it happened to be holding.
    let allowlist_unread = swap_first(&full, "workspace.get", "workspace.set_allowlist");
    assert!(!contains_in_order(&journal(&allowlist_unread), &EXPECTED));

    // One merge is not two: the pair is what covers the daemon's two answers,
    // the one that moved the base and the one that stopped on a conflict.
    let one_merge = drop_last(&full, "workspace.merge");
    assert!(!contains_in_order(&journal(&one_merge), &EXPECTED));

    // A run that never came back is not a match either. Without the second
    // handshake there is nothing to show the IDE rebuilt the connection on its
    // own, which is the whole claim of the `reconnect` step.
    let never_reconnected = drop_last(&full, "hello");
    assert!(!contains_in_order(&journal(&never_reconnected), &EXPECTED));

    // And a reconnect that handshook without re-syncing: the status bar would
    // say "connected" over tabs nobody had checked against the daemon.
    let no_resync = drop_last(&full, "workspace.list");
    assert!(!contains_in_order(&journal(&no_resync), &EXPECTED));

    // A missing step is not a match either.
    let short = journal(&["hello", "workspace.create", "agent.start", "fs.list_dir"]);
    assert!(!contains_in_order(&short, &EXPECTED));
}
