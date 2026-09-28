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
    /// Every `pty.write` as the bytes it carried, escaped. This is where the
    /// answers to the terminal's own questions show up: a program asking where
    /// the cursor is writes nothing else and reads nothing until it is told.
    pty_writes: Journal,
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
            pty_writes: fresh(),
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
    let pty_writes = Journals::read(&journals.pty_writes);
    // Read before the assertions, so a failure prints the file the run left
    // behind rather than only the fact that it was wrong.
    let state_json = std::fs::read_to_string(&state_path).unwrap_or_default();
    let context = format!(
        "requests: {seen:?}\npermission replies: {answered:?}\nallowlists: {allowed:?}\n\
         merges: {merges:?}\npull requests: {prs:?}\n\
         pty writes: {pty_writes:?}\nstate.json: {state_json}\n\
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
    // The terminal answered the question the daemon's output asked. Every PTY
    // this daemon opens asks where the cursor is, which is what `gh auth login`
    // does before each of its yes/no prompts -- and until this, the answer went
    // nowhere: the emulator parsed the query, handed the report to a listener
    // that dropped it, and the program waited for a terminal that was never
    // going to speak. On screen that was a question refusing every keystroke.
    let report = pty_writes
        .iter()
        .find(|w| w.starts_with("\\u{1b}[") && w.ends_with('R'));
    assert!(
        report.is_some(),
        "the IDE never reported the cursor position the daemon asked for
{context}"
    );

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
        kind: bondsymphonic_proto::WorkspaceKind::Worktree,
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
            pty_writes: recorded_pty_writes,
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
                                backends: vec![],
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
                            items: vec![
                                PrereqStatus {
                                    name: "git".into(),
                                    ok: true,
                                    detail: "git version 2.43".into(),
                                    fix_hint: None,
                                },
                                // Part of the fixture, not decoration: it is
                                // what opens the transcript composer, so the
                                // scripted `send` runs against a pane in the
                                // state a logged-in user sees rather than
                                // behind the login button.
                                PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                },
                            ],
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
                        // A prompt, and then the question `gh auth login` asks
                        // before each of its yes/no prompts: park the cursor
                        // past the far corner and ask where it ended up, which
                        // is how a program measures a screen. It reads nothing
                        // until the answer comes back, so a terminal that does
                        // not reply is a terminal whose prompts cannot be
                        // answered -- and the reply has to come from the IDE,
                        // which is what `pty_writes` below records.
                        follow_ups.push(ServerMessage::event(
                            Some(p.workspace_id.clone()),
                            Event::PtyOutput {
                                pty_id: pty_id.clone(),
                                data_b64: BASE64.encode("prompt$ \x1b[999;999f\x1b[6n"),
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
                            PermissionDecision::Allow | PermissionDecision::AllowForSession => {
                                "allow"
                            }
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
                    Request::PtyWrite(p) => {
                        let bytes = BASE64.decode(p.data_b64.as_bytes()).unwrap_or_default();
                        Journals::push(
                            &recorded_pty_writes,
                            String::from_utf8_lossy(&bytes).escape_debug().to_string(),
                        );
                        Some(ServerMessage::ok(id, &Empty {}))
                    }
                    Request::PtyResize(_) => Some(ServerMessage::ok(id, &Empty {})),
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

/// Task 12: the two context menus in the agent bar, against a model that moves
/// while they are open.
///
/// A menu is a nested event loop. Everything a daemon can say arrives during
/// it, so a tab index or a group index read back *after* `QMenu::exec` returns
/// may no longer be the one the user pointed at -- and the two items behind
/// these two indices destroy a workspace and close a group. The bar therefore
/// resolves its target before the menu runs and carries it in the signal; this
/// is the proof, driven through the `BS_MENU_TEST` seam, which opens a menu
/// over a named tab or group, rearranges the model while it is up and chooses
/// the item, exactly as the daemon and a user between them would.
///
/// The window prints what it resolved instead of raising its confirmation --
/// an offscreen run has nobody to answer a modal -- and those lines are what is
/// asserted on here. The third step is the same seam reading the New Agent
/// dialog's status label: the daemon writes that sentence, so it is shown as
/// text and never as markup.
mod menu_targets {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "menu-token";
    /// Nothing but the quit: the menus are driven by the seam, not by a script
    /// step, and every tab in the run comes back out of `workspace.list`.
    const SCRIPT: &str = "quit";
    /// The seam steps this run arms. Not an order: `widgets` and
    /// `signal-counts` are set up from `buildCentral` and fire as the window
    /// builds, while `destroy` and `close-group` wait for the first
    /// `workspace.list` to have filled the bar, so the widget report comes out
    /// first however this string is spelled.
    const MENU_TEST: &str = "destroy,close-group,widgets,signal-counts";

    /// Two groups: the first holds the two tabs the agent menu is opened over,
    /// the second is the one the group menu is opened over.
    const GROUP_A: &str = "alpha";
    const GROUP_B: &str = "beta";
    /// The tab that is *clicked*: second in `alpha`, which the seam then makes
    /// first by reversing that group's tabs while the menu is up.
    const CLICKED_ID: &str = "ws_menu2";
    const CLICKED_NAME: &str = "alpha-two";
    /// The tab index 1 names once the model has moved. A run that destroys this
    /// one destroyed a workspace the user never pointed at.
    const DISPLACED_ID: &str = "ws_menu1";
    const DISPLACED_NAME: &str = "alpha-one";
    const OTHER_ID: &str = "ws_menu3";
    const OTHER_NAME: &str = "beta-one";
    /// `theme::removed()`, the IDE's one red, as `QColor::name()` spells it.
    /// The literal `#eb5757` both call sites used to carry is deliberately not
    /// this, which is what makes the two colour assertions below discriminate.
    const THEME_RED: &str = "#d03933";
    /// What `#eb5757` was, so a revert is named in the failure rather than
    /// merely not matching.
    const OLD_RED: &str = "#eb5757";
    /// How many `workspace.state` events the fake daemon pushes once the list
    /// has answered. Each moves a tab's status and nothing else, so each fires
    /// `changed` and must not fire `arrangementChanged`. Three rather than one
    /// because an event that overtakes the restore has no tab to land on yet;
    /// the assertion only needs one of them to arrive.
    const STATUS_EVENTS: usize = 3;
    /// What the daemon says is wrong with the workspace it reports in error.
    /// The tab that carries it is the one whose text colour CI6 is about.
    const ERROR_DETAIL: &str = "worktree is gone";
    /// The quit step's two seconds, three panes, and a cold Qt start.
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn a_context_menu_acts_on_the_tab_and_group_that_were_clicked() {
        if bondsymphonic_ide::testing::skip_without_qt("menu targets") {
            return;
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        // Never the developer's real `%APPDATA%\BondSymphonic`.
        let config = std::env::temp_dir().join(format!("bs-menu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("menu config dir");
        let state_path = config.join("state.json");
        // The arrangement the tabs come back in. Two groups, so the group menu
        // has a second one to be opened over, and two tabs in the first, so the
        // agent menu has a tab that is not the one the model settles on.
        let state = StateFile {
            version: STATE_VERSION,
            groups: vec![
                PersistedGroup {
                    name: GROUP_A.to_owned(),
                    workspace_ids: vec![DISPLACED_ID.to_owned(), CLICKED_ID.to_owned()],
                    ..PersistedGroup::default()
                },
                PersistedGroup {
                    name: GROUP_B.to_owned(),
                    workspace_ids: vec![OTHER_ID.to_owned()],
                    ..PersistedGroup::default()
                },
            ],
            active_workspace: Some(DISPLACED_ID.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&state).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");

        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !format!("{out}{err}").contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );

        // CP2. The workspace destroyed is the one whose tab was under the
        // pointer, and the question names it: the whole point of resolving both
        // before the menu runs rather than after.
        let destroy = line(&out, "destroy")
            .unwrap_or_else(|| panic!("the agent menu never reported a destroy target\n{context}"));
        assert!(
            destroy.contains(&format!("target={CLICKED_ID}")),
            "the destroy went to the wrong workspace: {destroy}\n{context}"
        );
        assert!(
            !destroy.contains(&format!("target={DISPLACED_ID}")),
            "the destroy followed the tab index instead of the tab: {destroy}\n{context}"
        );
        assert!(
            destroy.contains(CLICKED_NAME) && !destroy.contains(DISPLACED_NAME),
            "the confirmation did not name the clicked workspace: {destroy}\n{context}"
        );

        // CP6. Same for the group menu, whose item closes every workspace in a
        // group.
        let close = line(&out, "close-group")
            .unwrap_or_else(|| panic!("the group menu never reported a close target\n{context}"));
        assert!(
            close.contains(&format!("target={GROUP_B}")),
            "the close went to the wrong group: {close}\n{context}"
        );

        // CP5. The daemon writes what lands in that label -- a repository's
        // name, a git error -- so it is shown as text.
        let status_format = line(&out, "new-agent-status").unwrap_or_else(|| {
            panic!("the New Agent dialog never reported its status format\n{context}")
        });
        assert!(
            status_format.contains("target=PlainText"),
            "the New Agent status label renders markup: {status_format}\n{context}"
        );

        // CP2, the other half of it. The seam prints what the window resolved
        // and returns before the modal, so the destroy must never reach the
        // daemon: a run that asked for one asked about a workspace nobody
        // confirmed.
        assert!(
            !seen.iter().any(|m| m.starts_with("workspace.destroy")),
            "a destroy reached the daemon without a confirmation\n{context}"
        );

        // CI6. Both former `#eb5757` sites now read the IDE's one red out of
        // `theme`. Which hex that is depends on the palette, because
        // `theme::ink` lifts an accent for a dark one, so the palette is
        // reported rather than assumed.
        let palette = line(&out, "palette")
            .unwrap_or_else(|| panic!("the seam never reported the palette\n{context}"));
        assert!(
            palette.contains("target=light"),
            "the offscreen run is no longer on a light palette, so the two colour assertions below want the lifted red instead: {palette}\n{context}"
        );

        let tab_colour = line(&out, "error-tab-colour")
            .unwrap_or_else(|| panic!("the seam never reported a tab colour\n{context}"));
        assert!(
            tab_colour.contains(&format!("target={THEME_RED}")),
            "an agent tab in error is not painted the theme's red; a revert to {OLD_RED} looks like this: {tab_colour}\n{context}"
        );

        let hint_style = line(&out, "name-hint-style")
            .unwrap_or_else(|| panic!("the seam never reported the name hint\n{context}"));
        assert!(
            hint_style.contains(&format!("color:{THEME_RED}")),
            "the New Agent name hint is not painted the theme's red; a revert to {OLD_RED} looks like this: {hint_style}\n{context}"
        );

        // CI1. The layout is recorded from `arrangementChanged`, which fires
        // only when the groups themselves moved. The daemon pushed
        // `STATUS_EVENTS` workspace-state events that move a tab's status and
        // nothing else, so `changed` must have fired at least that many times
        // more than the layout was recorded. Wired back to `changed`, as it was
        // before this task, the two counts are equal and this fails.
        let changed = count(&out, "model-changed");
        let recorded = count(&out, "layout-recorded");
        assert!(
            recorded >= 1,
            "the layout was never recorded, so this run proves nothing about what does not record it\n{context}"
        );
        assert!(
            changed > recorded,
            "every `changed` rewrote the layout: it fired {changed} times and the layout was recorded {recorded} times. The run pushed {STATUS_EVENTS} status-only events, so with the recording on `arrangementChanged` these two differ; wired back to `changed` they are equal, which is exactly this\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);
    }

    /// How many lines the seam printed for `what`.
    fn count(out: &str, what: &str) -> usize {
        let prefix = format!("BS_MENU_TEST {what} ");
        out.lines()
            .filter(|l| l.trim_start().starts_with(&prefix))
            .count()
    }

    /// The one line the seam printed for `what`, or `None`.
    fn line(out: &str, what: &str) -> Option<String> {
        let prefix = format!("BS_MENU_TEST {what} ");
        out.lines()
            .find(|l| l.trim_start().starts_with(&prefix))
            .map(|l| l.trim().to_owned())
    }

    /// A daemon with the three workspaces the arrangement above files into two
    /// groups. Terminal adapters throughout: a Claude pane would replay a
    /// transcript, and nothing here is about transcripts.
    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut ptys = 0_u32;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let trimmed = line.trim_end();
                    let ClientMessage::Request { id, request } =
                        codec::decode(trimmed).expect("decode");
                    // The id travels with the method for a destroy: the point of
                    // the run is *which* workspace one would have named.
                    let method = match &request {
                        Request::WorkspaceDestroy(p) => {
                            format!("workspace.destroy:{}", p.workspace_id.0)
                        }
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let listed = matches!(request, Request::WorkspaceList {});
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![
                                        AgentAdapterKind::Terminal,
                                        AgentAdapterKind::Claude,
                                    ],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "git".into(),
                                    ok: true,
                                    detail: "git version 2.43".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => ServerMessage::ok(
                            id,
                            &WorkspaceListResult {
                                workspaces: vec![
                                    broken(DISPLACED_ID, DISPLACED_NAME),
                                    workspace(CLICKED_ID, CLICKED_NAME),
                                    workspace(OTHER_ID, OTHER_NAME),
                                ],
                            },
                        ),
                        Request::WorkspaceGet(p) => {
                            ServerMessage::ok(id, &workspace(&p.workspace_id.0, "unknown"))
                        }
                        Request::PtyOpen(_) => {
                            ptys += 1;
                            ServerMessage::ok(
                                id,
                                &PtyOpenResult {
                                    pty_id: PtyId(format!("pty_menu{ptys}")),
                                },
                            )
                        }
                        Request::PtyResize(_) | Request::PtyWrite(_) | Request::PtyClose(_) => {
                            ServerMessage::ok(id, &Empty {})
                        }
                        Request::FsListDir(_) => ServerMessage::ok(
                            id,
                            &ListDirResult {
                                entries: vec![entry("src", true), entry("README.md", false)],
                            },
                        ),
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        // Answered so a run that got this far would still end
                        // cleanly; the assertions require it never to be asked.
                        Request::WorkspaceDestroy(_) => ServerMessage::ok(id, &Empty {}),
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                    // Behind the list, so the tabs they name exist. Each moves
                    // one tab's status and touches no group, no membership and
                    // no order, which is the whole of what CI1 is about: these
                    // must reach `changed` and must not reach
                    // `arrangementChanged`. The workspace is the one in the
                    // group that is *not* on show, so the error tab the colour
                    // assertion looks for is left alone.
                    if listed {
                        let mut pushed = false;
                        for i in 0..STATUS_EVENTS {
                            let state = if i % 2 == 0 {
                                WorkspaceState::SandboxDown
                            } else {
                                WorkspaceState::Ready
                            };
                            let info = Box::new(super::workspace(OTHER_ID, OTHER_NAME, state, &[]));
                            let event = ServerMessage::event(
                                Some(WorkspaceId(OTHER_ID.to_owned())),
                                Event::WorkspaceStateChanged { info },
                            );
                            if w.write_all(codec::encode(&event).as_bytes()).await.is_err() {
                                pushed = true;
                                break;
                            }
                        }
                        if pushed {
                            break;
                        }
                    }
                }
            }
        });

        (addr, journal)
    }

    /// A ready workspace with no allowlist of its own, which is every tab in
    /// this run bar the one the daemon puts into error.
    fn workspace(id: &str, name: &str) -> WorkspaceInfo {
        super::workspace(id, name, WorkspaceState::Ready, &[])
    }

    /// The same tab, reported in error, so one agent tab in the bar is painted
    /// the status colour CI6 is about.
    fn broken(id: &str, name: &str) -> WorkspaceInfo {
        super::workspace(
            id,
            name,
            WorkspaceState::Error(ERROR_DETAIL.to_owned()),
            &[],
        )
    }

    /// One entry of the fake file listing. Size is never read here.
    fn entry(name: &str, is_dir: bool) -> FileEntry {
        super::entry(name, is_dir, 0)
    }
}

/// Task 16: a destroy whose workspace went while the menu was up.
///
/// The window returns without a word when the workspace a destroy names is no
/// longer in the model -- destroyed by another IDE, or gone with its own agent
/// -- the way closing a group already answers a group that has gone. The guard
/// runs *before* the seam prints, so a run that arms `destroy-gone` must print
/// no destroy line at all and must put no `workspace.destroy` on the wire.
///
/// An absence proves nothing on its own: a run whose menus never opened would
/// print no destroy line either. The `close-group` step is the control. It
/// opens the other menu from the same pass of the same loop, so its line says
/// the seam ran, and only then does the missing destroy line mean the guard
/// fired rather than the run falling short.
mod task_16 {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "gone-token";
    const SCRIPT: &str = "quit";
    /// The destroy whose workspace vanishes under the menu, and the close-group
    /// that says the seam ran at all.
    const MENU_TEST: &str = "destroy-gone,close-group";

    const GROUP_A: &str = "alpha";
    const GROUP_B: &str = "beta";
    /// The tab the agent menu is opened over, which the fixture then takes out
    /// of the model while the menu is up.
    const CLICKED_ID: &str = "ws_gone2";
    const CLICKED_NAME: &str = "alpha-two";
    const OTHER_TAB_ID: &str = "ws_gone1";
    const OTHER_TAB_NAME: &str = "alpha-one";
    const GROUP_B_ID: &str = "ws_gone3";
    const GROUP_B_NAME: &str = "beta-one";

    /// A cold Qt start, three panes and the script's settle.
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn a_destroy_for_a_workspace_that_has_gone_says_nothing_at_all() {
        if bondsymphonic_ide::testing::skip_without_qt("task 16 gone destroy") {
            return;
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        // Never the developer's real `%APPDATA%\BondSymphonic`.
        let config = std::env::temp_dir().join(format!("bs-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let state = StateFile {
            version: STATE_VERSION,
            groups: vec![
                PersistedGroup {
                    name: GROUP_A.to_owned(),
                    workspace_ids: vec![OTHER_TAB_ID.to_owned(), CLICKED_ID.to_owned()],
                    ..PersistedGroup::default()
                },
                PersistedGroup {
                    name: GROUP_B.to_owned(),
                    workspace_ids: vec![GROUP_B_ID.to_owned()],
                    ..PersistedGroup::default()
                },
            ],
            active_workspace: Some(OTHER_TAB_ID.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&state).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");

        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !format!("{out}{err}").contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );

        // The control. Both steps run in one pass of the seam's loop, so this
        // line says the menus opened at all -- without it the missing destroy
        // line below would be the silence of a run that fell short rather than
        // of a guard that fired.
        assert!(
            line(&out, "close-group").is_some(),
            "the seam never opened a menu, so this run proves nothing about the destroy below\n{context}"
        );

        // The guard itself. The bar emitted the id and the name it resolved
        // before the menu; the model no longer has that workspace, so there is
        // nothing to ask about and nothing to say.
        assert!(
            line(&out, "destroy").is_none(),
            "the window went on to ask about a workspace that is no longer in the model\n{context}"
        );
        assert!(
            !seen.iter().any(|m| m.starts_with("workspace.destroy")),
            "a destroy reached the daemon for a workspace that had gone\n{context}"
        );

        let _ = std::fs::remove_dir_all(&config);
    }

    /// The one line the seam printed for `what`, or `None`.
    fn line(out: &str, what: &str) -> Option<String> {
        let prefix = format!("BS_MENU_TEST {what} ");
        out.lines()
            .find(|l| l.trim_start().starts_with(&prefix))
            .map(|l| l.trim().to_owned())
    }

    /// Three ready workspaces in two groups, with the list held back.
    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut ptys = 0_u32;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let trimmed = line.trim_end();
                    let ClientMessage::Request { id, request } =
                        codec::decode(trimmed).expect("decode");
                    let method = match &request {
                        Request::WorkspaceDestroy(p) => {
                            format!("workspace.destroy:{}", p.workspace_id.0)
                        }
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Terminal],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "git".into(),
                                    ok: true,
                                    detail: "git version 2.43".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => ServerMessage::ok(
                            id,
                            &WorkspaceListResult {
                                workspaces: vec![
                                    workspace(OTHER_TAB_ID, OTHER_TAB_NAME),
                                    workspace(CLICKED_ID, CLICKED_NAME),
                                    workspace(GROUP_B_ID, GROUP_B_NAME),
                                ],
                            },
                        ),
                        Request::WorkspaceGet(p) => {
                            ServerMessage::ok(id, &workspace(&p.workspace_id.0, "unknown"))
                        }
                        Request::PtyOpen(_) => {
                            ptys += 1;
                            ServerMessage::ok(
                                id,
                                &PtyOpenResult {
                                    pty_id: PtyId(format!("pty_gone{ptys}")),
                                },
                            )
                        }
                        Request::PtyResize(_) | Request::PtyWrite(_) | Request::PtyClose(_) => {
                            ServerMessage::ok(id, &Empty {})
                        }
                        Request::FsListDir(_) => ServerMessage::ok(
                            id,
                            &ListDirResult {
                                entries: vec![super::entry("README.md", false, 0)],
                            },
                        ),
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        // Answered so a run that got this far would still end
                        // cleanly; the assertions require it never to be asked.
                        Request::WorkspaceDestroy(_) => ServerMessage::ok(id, &Empty {}),
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });

        (addr, journal)
    }

    fn workspace(id: &str, name: &str) -> WorkspaceInfo {
        super::workspace(id, name, WorkspaceState::Ready, &[])
    }
}

/// A workspace that cannot run says why on its own pane, and the pane's Retry
/// and Remove do what they say.
///
/// The daemon reports a restore that failed as `Error(reason)` and a sandbox
/// that died as a bare `SandboxDown`, and answers `workspace.restart` with the
/// workspace back up or with a readable refusal. Before this the tab read
/// "sandbox down" with nothing to press. The seam prints every change to a
/// workspace's banner (`sandbox-banner`) and presses the banner's own Retry or
/// Remove once it is up (`sandbox-retry`, `sandbox-remove`), on the one
/// throwaway workspace this fake daemon reports -- never anything window-wide.
mod sandbox_retry {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "retry-token";
    /// A pause long enough for the seam's click and the restart behind it to
    /// have landed, then the quit.
    const SCRIPT: &str = "wait,quit";
    const WORKSPACE: &str = "ws_retry1";
    const NAME: &str = "retry-one";
    /// The reason the restore failed with, as the daemon words it.
    const RESTORE_REASON: &str =
        "The worktree's git registration was missing and could not be restored.";
    /// The reason a refused Retry comes back with.
    const RETRY_REASON: &str = "The sandbox could not start: bwrap: permission denied.";
    /// The agent the daemon restored from its records: ended, with a session to
    /// resume. A tab bound to it is what every workspace looks like after a
    /// daemon restart, and nothing starts it again unless Retry does.
    const OLD_AGENT: &str = "ag_retry_old";
    const NEW_AGENT: &str = "ag_retry_new";
    const SESSION: &str = "sess-retry-1";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    /// How the fake answers `workspace.restart`.
    #[derive(Clone, Copy)]
    enum Restart {
        Works,
        Refused,
    }

    #[test]
    fn a_failed_workspace_says_why_and_retry_brings_it_and_its_agent_back() {
        if bondsymphonic_ide::testing::skip_without_qt("sandbox retry") {
            return;
        }
        let run = run_ide(
            WorkspaceState::Error(RESTORE_REASON.to_owned()),
            Restart::Works,
            "sandbox-banner,sandbox-retry=ws_retry1",
        );
        let reason = run
            .banner_line(RESTORE_REASON)
            .unwrap_or_else(|| panic!("no banner with the restore reason\n{}", run.context));
        assert!(
            run.out.contains("This workspace could not be started"),
            "the banner has no title\n{}",
            run.context
        );
        let retried = run
            .line_index("BS_MENU_TEST sandbox-retry target=ws_retry1")
            .unwrap_or_else(|| panic!("Retry was never pressed\n{}", run.context));
        let cleared = run
            .line_index("BS_MENU_TEST sandbox-banner target=ws_retry1 question=cleared")
            .unwrap_or_else(|| panic!("the banner never came down\n{}", run.context));
        assert!(reason < retried && retried < cleared, "{}", run.context);

        // Retry is the restart on the wire, and the agent comes back behind it,
        // resuming the conversation the restored record carried.
        let seen = &run.journal;
        let restart = seen
            .iter()
            .position(|m| m == "workspace.restart:ws_retry1")
            .unwrap_or_else(|| panic!("no workspace.restart\n{}", run.context));
        let start = seen
            .iter()
            .position(|m| m.starts_with("agent.start:ws_retry1"))
            .unwrap_or_else(|| panic!("the agent was not started again\n{}", run.context));
        assert!(restart < start, "{}", run.context);
        assert_eq!(
            seen[start],
            format!("agent.start:ws_retry1:{SESSION}"),
            "{}",
            run.context
        );
        assert_eq!(
            seen.iter().filter(|m| m.starts_with("agent.start")).count(),
            1,
            "one start, not one per pass\n{}",
            run.context
        );
        // The agents the restart stopped are the Retry at work, not a crash:
        // no "The agent stopped." banner over a workspace that is coming back.
        assert!(
            run.line_index("BS_MENU_TEST agent-stopped").is_none(),
            "the restart's own agent stop was reported as a failure\n{}",
            run.context
        );
    }

    #[test]
    fn a_retry_the_daemon_refuses_shows_the_new_reason() {
        if bondsymphonic_ide::testing::skip_without_qt("sandbox retry refused") {
            return;
        }
        let run = run_ide(
            WorkspaceState::Error(RESTORE_REASON.to_owned()),
            Restart::Refused,
            "sandbox-banner,sandbox-retry=ws_retry1",
        );
        let first = run
            .banner_line(RESTORE_REASON)
            .unwrap_or_else(|| panic!("no banner with the restore reason\n{}", run.context));
        let second = run
            .banner_line(RETRY_REASON)
            .unwrap_or_else(|| panic!("the refusal never reached the banner\n{}", run.context));
        assert!(first < second, "{}", run.context);
        assert!(
            run.journal
                .iter()
                .any(|m| m == "workspace.restart:ws_retry1"),
            "{}",
            run.context
        );
        assert!(
            !run.journal.iter().any(|m| m.starts_with("agent.start")),
            "an agent was started in a workspace that is still down\n{}",
            run.context
        );
        assert!(
            run.line_index("question=cleared").is_none(),
            "the banner came down over a workspace that is still down\n{}",
            run.context
        );
    }

    #[test]
    fn a_down_sandbox_says_it_stopped_and_remove_asks_to_destroy_it() {
        if bondsymphonic_ide::testing::skip_without_qt("sandbox remove") {
            return;
        }
        let run = run_ide(
            WorkspaceState::SandboxDown,
            Restart::Works,
            "sandbox-banner,sandbox-remove=ws_retry1",
        );
        assert!(
            run.banner_line("stopped unexpectedly").is_some(),
            "{}",
            run.context
        );
        assert!(
            run.out
                .contains("The sandbox for this workspace is not running"),
            "{}",
            run.context
        );
        // The seam stops the destroy at its confirmation, which is the question
        // the tab's own menu asks, naming this workspace.
        assert!(
            run.line_index(&format!(
                "BS_MENU_TEST destroy target={WORKSPACE} question=Destroy workspace \"{NAME}\"?"
            ))
            .is_some(),
            "Remove did not ask to destroy this workspace\n{}",
            run.context
        );
        assert!(
            !run.journal
                .iter()
                .any(|m| m.starts_with("workspace.restart")),
            "{}",
            run.context
        );
        assert!(
            !run.journal.iter().any(|m| m.starts_with("agent.start")),
            "an agent was started in a workspace whose sandbox is down\n{}",
            run.context
        );
    }

    struct Run {
        out: String,
        journal: Vec<String>,
        context: String,
    }

    impl Run {
        /// The index of the first stdout line containing `needle`.
        fn line_index(&self, needle: &str) -> Option<usize> {
            self.out.lines().position(|l| l.contains(needle))
        }

        /// The index of the first banner line for the workspace carrying
        /// `detail`.
        fn banner_line(&self, detail: &str) -> Option<usize> {
            let prefix = format!("BS_MENU_TEST sandbox-banner target={WORKSPACE} ");
            self.out
                .lines()
                .position(|l| l.starts_with(&prefix) && l.contains(detail))
        }
    }

    fn run_ide(state: WorkspaceState, restart: Restart, menu_test: &str) -> Run {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon(state, restart));

        // Never the developer's real `%APPDATA%\BondSymphonic`, and one
        // directory per test: they run in parallel.
        let config = std::env::temp_dir().join(format!(
            "bs-retry-{}-{}",
            std::process::id(),
            menu_test.replace(',', "-")
        ));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "retry".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", menu_test)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let journal = journal.lock().expect("journal mutex").clone();
        let context =
            format!("requests: {journal:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !format!("{out}{err}").contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);
        Run {
            out,
            journal,
            context,
        }
    }

    /// One Claude workspace in `state`, bound to a restored agent that ended.
    async fn fake_daemon(
        state: WorkspaceState,
        restart: Restart,
    ) -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut current = state;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    let method = match &request {
                        Request::WorkspaceRestart(p) => {
                            format!("workspace.restart:{}", p.workspace_id.0)
                        }
                        Request::AgentStart(p) => format!(
                            "agent.start:{}:{}",
                            p.workspace_id.0,
                            p.options.resume_session.clone().unwrap_or_default()
                        ),
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => ServerMessage::ok(
                            id,
                            &WorkspaceListResult {
                                workspaces: vec![workspace(current.clone())],
                            },
                        ),
                        Request::WorkspaceGet(_) => {
                            ServerMessage::ok(id, &workspace(current.clone()))
                        }
                        Request::WorkspaceRestart(_) => match restart {
                            Restart::Works => {
                                current = WorkspaceState::Ready;
                                // What the daemon does on the way: every agent
                                // in the workspace is stopped, and says so,
                                // before the sandbox comes back and the answer
                                // goes out.
                                let ws = WorkspaceId(WORKSPACE.to_owned());
                                let exited = ServerMessage::event(
                                    Some(ws.clone()),
                                    Event::AgentStateChanged {
                                        agent_id: AgentId(OLD_AGENT.to_owned()),
                                        state: AgentState::Exited,
                                        detail: Some("stopped for a restart".to_owned()),
                                    },
                                );
                                let ready = ServerMessage::event(
                                    Some(ws),
                                    Event::WorkspaceStateChanged {
                                        info: Box::new(workspace(current.clone())),
                                    },
                                );
                                for event in [exited, ready] {
                                    let _ = w.write_all(codec::encode(&event).as_bytes()).await;
                                }
                                ServerMessage::ok(id, &workspace(current.clone()))
                            }
                            Restart::Refused => {
                                current = WorkspaceState::Error(RETRY_REASON.to_owned());
                                ServerMessage::err(
                                    id,
                                    RpcError::new(ErrorCode::SandboxError, RETRY_REASON),
                                )
                            }
                        },
                        Request::AgentStart(_) => ServerMessage::ok(
                            id,
                            &AgentStartResult {
                                agent_id: AgentId(NEW_AGENT.to_owned()),
                            },
                        ),
                        Request::AgentHistory(_) => ServerMessage::ok(
                            id,
                            &HistoryResult {
                                messages: vec![],
                                state: AgentState::Exited,
                                detail: None,
                            },
                        ),
                        Request::FsListDir(_) => ServerMessage::ok(
                            id,
                            &ListDirResult {
                                entries: vec![super::entry("README.md", false, 0)],
                            },
                        ),
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        // Answered so a run that got this far would still end
                        // cleanly; the seam stops every destroy at its question.
                        Request::WorkspaceDestroy(_) => ServerMessage::ok(id, &Empty {}),
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });

        (addr, journal)
    }

    fn workspace(state: WorkspaceState) -> WorkspaceInfo {
        let mut info = super::workspace(WORKSPACE, NAME, state, &[]);
        let record = AgentSummary {
            id: AgentId(OLD_AGENT.to_owned()),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Exited,
            session_id: Some(SESSION.to_owned()),
            command: None,
            model: None,
            permission_mode: None,
        };
        info.agents = vec![record.id.clone()];
        info.agent_records = vec![record];
        info
    }
}

/// Remove on the banner of a workspace git has lost track of.
///
/// The daemon cannot tell whether such a worktree is clean, so a destroy
/// without force is refused with `Conflict` and `data.dirty`. The banner's
/// advice for that workspace is to remove it, and the destroy dialog's Force
/// box starts unticked, so the first answer is always that refusal. The
/// window turns it into one plain question -- remove it anyway, discarding
/// its changes and its branch -- and a yes is the forced destroy.
///
/// `destroy-yes` answers both questions yes: the seam prints each one instead
/// of raising a modal box and carries on as a user who pressed Yes would.
mod sandbox_remove_dirty {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "remove-dirty-token";
    const SCRIPT: &str = "wait,quit";
    const MENU_TEST: &str = "sandbox-banner,sandbox-remove=ws_dirty1,destroy-yes=ws_dirty1";
    const WORKSPACE: &str = "ws_dirty1";
    const NAME: &str = "dirty-one";
    const REASON: &str = "Git no longer lists this workspace's worktree. Remove the workspace.";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn remove_asks_again_when_the_daemon_cannot_tell_the_worktree_is_clean() {
        if bondsymphonic_ide::testing::skip_without_qt("sandbox remove dirty") {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        let config = std::env::temp_dir().join(format!("bs-remove-dirty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "dirty".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);

        let line = |needle: &str| out.lines().position(|l| l.contains(needle));

        // The first question is the tab menu's, unticked.
        let asked = line(&format!("BS_MENU_TEST destroy target={WORKSPACE} "))
            .unwrap_or_else(|| panic!("Remove did not ask to destroy\n{context}"));
        // The refusal is asked about, in words that say what a yes costs.
        let refused = out
            .lines()
            .find(|l| l.starts_with(&format!("BS_MENU_TEST destroy-refused target={WORKSPACE} ")))
            .unwrap_or_else(|| panic!("the refusal was not turned into a question\n{context}"));
        assert!(refused.contains(NAME), "{refused}");
        assert!(refused.contains("uncommitted changes"), "{refused}");
        assert!(refused.contains("branch"), "{refused}");
        assert!(
            asked < line("BS_MENU_TEST destroy-refused").unwrap_or(0),
            "{context}"
        );

        // Unforced first, forced on the yes, and nothing else.
        let destroys: Vec<&String> = seen
            .iter()
            .filter(|m| m.starts_with("workspace.destroy"))
            .collect();
        assert_eq!(
            destroys,
            [
                &format!("workspace.destroy:{WORKSPACE}:false"),
                &format!("workspace.destroy:{WORKSPACE}:true"),
            ],
            "{context}"
        );
        // A refusal that was asked about is not also a box.
        assert!(
            !err.contains("workspace.destroy failed"),
            "the refusal was reported as a failure too\n{context}"
        );
    }

    /// One workspace git has lost track of: an unforced destroy is refused as
    /// the daemon refuses it, a forced one works.
    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut destroyed = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    let method = match &request {
                        Request::WorkspaceDestroy(p) => {
                            format!("workspace.destroy:{}:{}", p.workspace_id.0, p.force)
                        }
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let listed = if destroyed { vec![] } else { vec![workspace()] };
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => {
                            ServerMessage::ok(id, &WorkspaceListResult { workspaces: listed })
                        }
                        Request::WorkspaceGet(_) => ServerMessage::ok(id, &workspace()),
                        Request::WorkspaceDestroy(p) if !p.force => ServerMessage::err(
                            id,
                            RpcError::new(
                                ErrorCode::Conflict,
                                "workspace has uncommitted changes or unmerged commits; use \
                                 force to discard",
                            )
                            .with_data(serde_json::json!({ "dirty": true, "unmerged": false })),
                        ),
                        Request::WorkspaceDestroy(_) => {
                            destroyed = true;
                            ServerMessage::ok(id, &Empty {})
                        }
                        Request::FsListDir(_) => {
                            ServerMessage::ok(id, &ListDirResult { entries: vec![] })
                        }
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });

        (addr, journal)
    }

    /// No agent: the question is about the worktree, and an agent record
    /// would only add requests this fixture has nothing to say about.
    fn workspace() -> WorkspaceInfo {
        super::workspace(
            WORKSPACE,
            NAME,
            WorkspaceState::Error(REASON.to_owned()),
            &[],
        )
    }
}

/// Review follow-ups to `sandbox_retry`: a Retry after the sandbox died under a
/// running agent, and a Retry whose answer never arrived.
///
/// * A sandbox that dies takes its agent with it, and the agent's `exited` can
///   arrive before the workspace's `SandboxDown`. That exit is the sandbox's
///   news, not a crash: once Retry has the workspace and its agent back, the
///   pane must not still say "The agent stopped." over a tab painted red.
/// * A Retry that the daemon carried out but whose answer was lost to a
///   dropped connection is not the workspace failing. The banner must not show
///   the connection's error as the workspace's reason, and the agent must come
///   back once the workspace is seen running.
///
/// The seam steps name the one workspace they may press on.
mod sandbox_retry_followups {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "followup-token";
    /// Long enough for a dropped connection to be rebuilt and re-listed.
    const SCRIPT: &str = "wait,wait,wait,quit";
    const WORKSPACE: &str = "ws_follow1";
    const NAME: &str = "follow-one";
    const AGENT: &str = "ag_follow_old";
    const NEW_AGENT: &str = "ag_follow_new";
    const SESSION: &str = "sess-follow-1";
    const RUN_LIMIT: Duration = Duration::from_secs(120);
    /// `TabStatus::Error` as `GroupModel::tabStatus` codes it.
    const ERROR_CODE: &str = "status=3";

    type Journal = Arc<Mutex<Vec<String>>>;

    #[derive(Clone, Copy, PartialEq)]
    enum Scenario {
        /// Listed running; the agent exits and then the sandbox goes down.
        /// Retry works.
        DiedAtRuntime,
        /// Listed down. Retry is carried out -- the agent's stop and the
        /// `Ready` go out -- and then the connection drops before the answer.
        AnswerLost,
    }

    #[test]
    fn a_retry_after_the_sandbox_died_leaves_no_agent_stopped_banner() {
        if bondsymphonic_ide::testing::skip_without_qt("retry after runtime death") {
            return;
        }
        let run = run_ide(Scenario::DiedAtRuntime);
        let problem = run
            .line_index("BS_MENU_TEST sandbox-banner target=ws_follow1 ")
            .unwrap_or_else(|| panic!("the sandbox going down was never shown\n{}", run.context));
        assert!(
            run.out
                .lines()
                .nth(problem)
                .unwrap_or("")
                .contains("stopped unexpectedly"),
            "{}",
            run.context
        );
        let started = run
            .out
            .lines()
            .find(|l| l.starts_with("BS_MENU_TEST agent-started target=ws_follow1 "))
            .unwrap_or_else(|| panic!("the agent was not started again\n{}", run.context));
        assert!(
            started.contains("banner=hidden"),
            "the pane still shows a banner over a running agent: {started}\n{}",
            run.context
        );
        assert!(
            !started.contains(ERROR_CODE),
            "the tab is still red over a running agent: {started}\n{}",
            run.context
        );
        assert_eq!(
            run.journal
                .iter()
                .filter(|m| m.starts_with("agent.start"))
                .collect::<Vec<_>>(),
            [&format!("agent.start:{WORKSPACE}:{SESSION}")],
            "{}",
            run.context
        );
    }

    #[test]
    fn a_retry_whose_answer_was_lost_still_brings_the_agent_back() {
        if bondsymphonic_ide::testing::skip_without_qt("retry answer lost") {
            return;
        }
        let run = run_ide(Scenario::AnswerLost);
        // The connection's failure is not the workspace's reason.
        for line in run
            .out
            .lines()
            .filter(|l| l.starts_with("BS_MENU_TEST sandbox-banner target=ws_follow1 "))
        {
            assert!(
                line.contains("stopped unexpectedly") || line.ends_with("question=cleared"),
                "a banner line that is not the workspace's own news: {line}\n{}",
                run.context
            );
        }
        assert_eq!(
            run.journal
                .iter()
                .filter(|m| m.starts_with("workspace.restart"))
                .count(),
            1,
            "{}",
            run.context
        );
        // The daemon did restart it, so the agent comes back: once.
        assert_eq!(
            run.journal
                .iter()
                .filter(|m| m.starts_with("agent.start"))
                .collect::<Vec<_>>(),
            [&format!("agent.start:{WORKSPACE}:{SESSION}")],
            "{}",
            run.context
        );
        let started = run
            .out
            .lines()
            .find(|l| l.starts_with("BS_MENU_TEST agent-started target=ws_follow1 "))
            .unwrap_or_else(|| panic!("the agent never reported started\n{}", run.context));
        assert!(
            started.contains("banner=hidden"),
            "{started}\n{}",
            run.context
        );
        assert!(!started.contains(ERROR_CODE), "{started}\n{}", run.context);
    }

    struct Run {
        out: String,
        journal: Vec<String>,
        context: String,
    }

    impl Run {
        fn line_index(&self, needle: &str) -> Option<usize> {
            self.out.lines().position(|l| l.contains(needle))
        }
    }

    fn run_ide(scenario: Scenario) -> Run {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon(scenario));

        let tag = match scenario {
            Scenario::DiedAtRuntime => "died",
            Scenario::AnswerLost => "lost",
        };
        let config =
            std::env::temp_dir().join(format!("bs-retry-follow-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "follow".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env(
                "BS_MENU_TEST",
                format!("sandbox-banner,sandbox-retry={WORKSPACE}"),
            )
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let journal = journal.lock().expect("journal mutex").clone();
        let context =
            format!("requests: {journal:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !format!("{out}{err}").contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);
        Run {
            out,
            journal,
            context,
        }
    }

    fn event(body: Event) -> ServerMessage {
        ServerMessage::event(Some(WorkspaceId(WORKSPACE.to_owned())), body)
    }

    fn exited() -> ServerMessage {
        event(Event::AgentStateChanged {
            agent_id: AgentId(AGENT.to_owned()),
            state: AgentState::Exited,
            detail: Some("the sandbox went away".to_owned()),
        })
    }

    fn state_event(state: WorkspaceState, agent: AgentState) -> ServerMessage {
        event(Event::WorkspaceStateChanged {
            info: Box::new(workspace(state, agent)),
        })
    }

    async fn fake_daemon(scenario: Scenario) -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let (mut current, mut agent) = match scenario {
                Scenario::DiedAtRuntime => (WorkspaceState::Ready, AgentState::Idle),
                Scenario::AnswerLost => (WorkspaceState::SandboxDown, AgentState::Exited),
            };
            let mut died = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    let method = match &request {
                        Request::WorkspaceRestart(p) => {
                            format!("workspace.restart:{}", p.workspace_id.0)
                        }
                        Request::AgentStart(p) => format!(
                            "agent.start:{}:{}",
                            p.workspace_id.0,
                            p.options.resume_session.clone().unwrap_or_default()
                        ),
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let mut after: Vec<ServerMessage> = Vec::new();
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => Some(ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        )),
                        Request::Hello(_) => Some(ServerMessage::err(id, RpcError::unauthorized())),
                        Request::SystemCheckPrereqs {} => Some(ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        )),
                        Request::WorkspaceList {} => {
                            // The death, once the tab exists to hear of it:
                            // the agent first, as the adapter notices its
                            // process gone before the daemon marks the sandbox.
                            if scenario == Scenario::DiedAtRuntime && !died {
                                died = true;
                                after.push(exited());
                                after.push(state_event(
                                    WorkspaceState::SandboxDown,
                                    AgentState::Exited,
                                ));
                            }
                            let listed = workspace(current.clone(), agent);
                            if died {
                                current = WorkspaceState::SandboxDown;
                                agent = AgentState::Exited;
                            }
                            Some(ServerMessage::ok(
                                id,
                                &WorkspaceListResult {
                                    workspaces: vec![listed],
                                },
                            ))
                        }
                        Request::WorkspaceGet(_) => {
                            Some(ServerMessage::ok(id, &workspace(current.clone(), agent)))
                        }
                        Request::WorkspaceRestart(_) => {
                            current = WorkspaceState::Ready;
                            agent = AgentState::Exited;
                            let stop = exited();
                            let ready = state_event(WorkspaceState::Ready, AgentState::Exited);
                            for message in [stop, ready] {
                                let _ = w.write_all(codec::encode(&message).as_bytes()).await;
                            }
                            match scenario {
                                Scenario::DiedAtRuntime => {
                                    Some(ServerMessage::ok(id, &workspace(current.clone(), agent)))
                                }
                                // Carried out, never answered.
                                Scenario::AnswerLost => None,
                            }
                        }
                        Request::AgentStart(_) => Some(ServerMessage::ok(
                            id,
                            &AgentStartResult {
                                agent_id: AgentId(NEW_AGENT.to_owned()),
                            },
                        )),
                        Request::AgentHistory(_) => Some(ServerMessage::ok(
                            id,
                            &HistoryResult {
                                messages: vec![],
                                state: AgentState::Exited,
                                detail: None,
                            },
                        )),
                        Request::FsListDir(_) => {
                            Some(ServerMessage::ok(id, &ListDirResult { entries: vec![] }))
                        }
                        Request::FsWatch(_) => Some(ServerMessage::ok(id, &Empty {})),
                        Request::WorkspaceChanges(_) => {
                            Some(ServerMessage::ok(id, &ChangesResult { files: vec![] }))
                        }
                        Request::WorkspaceStatus(_) => Some(ServerMessage::ok(
                            id,
                            &WorkspaceStatusResult { entries: vec![] },
                        )),
                        Request::RepoDetectRunConfigs(_) => Some(ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        )),
                        Request::RunList(_) => {
                            Some(ServerMessage::ok(id, &RunListResult { runs: vec![] }))
                        }
                        other => Some(ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        )),
                    };
                    let Some(reply) = reply else {
                        // Drop the connection with the request unanswered.
                        break;
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                    if !after.is_empty() {
                        // After the list has been applied: events travel on
                        // their own channel and would otherwise reach a model
                        // that has no tab for them yet.
                        tokio::time::sleep(Duration::from_millis(1_500)).await;
                    }
                    for message in after {
                        let _ = w.write_all(codec::encode(&message).as_bytes()).await;
                    }
                }
            }
        });

        (addr, journal)
    }

    fn workspace(state: WorkspaceState, agent: AgentState) -> WorkspaceInfo {
        let mut info = super::workspace(WORKSPACE, NAME, state, &[]);
        let record = AgentSummary {
            id: AgentId(AGENT.to_owned()),
            adapter: AgentAdapterKind::Claude,
            state: agent,
            session_id: Some(SESSION.to_owned()),
            command: None,
            model: None,
            permission_mode: None,
        };
        info.agents = vec![record.id.clone()];
        info.agent_records = vec![record];
        info
    }
}

/// The banner seam acts on the workspace its steps name and on nothing else.
///
/// `sandbox-retry`, `sandbox-remove` and `destroy-yes` send real requests --
/// a restart stops agents, a forced destroy deletes a branch -- so a step that
/// named no workspace, or another one, must leave every workspace alone even
/// when its banner is up and its buttons are pressable. The workspace here has
/// a problem and its pane is on screen; the steps name a different id.
mod seam_targets {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "seam-target-token";
    const WORKSPACE: &str = "ws_bystander1";
    const MENU_TEST: &str = "sandbox-banner,sandbox-retry=ws_nobody,sandbox-remove=ws_nobody,\
                             destroy-yes=ws_nobody,sandbox-retry,sandbox-remove,destroy-yes";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    #[test]
    fn a_banner_step_never_acts_on_a_workspace_it_does_not_name() {
        if bondsymphonic_ide::testing::skip_without_qt("seam targets") {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let journal: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let addr = rt.block_on(fake_daemon(journal.clone()));

        let config = std::env::temp_dir().join(format!("bs-seam-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "bystander".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", "wait,quit")
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");
        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(status.success(), "{context}");
        let _ = std::fs::remove_dir_all(&config);

        // The control: the banner is up, so the seam had something to press.
        assert!(
            out.lines().any(|l| l
                .starts_with(&format!("BS_MENU_TEST sandbox-banner target={WORKSPACE} "))),
            "the banner never came up, so this run proves nothing\n{context}"
        );
        assert!(
            !out.contains("BS_MENU_TEST sandbox-retry ") && !out.contains("BS_MENU_TEST destroy "),
            "a button was pressed on a workspace no step named\n{context}"
        );
        assert!(
            !seen
                .iter()
                .any(|m| m.starts_with("workspace.restart") || m.starts_with("workspace.destroy")),
            "a request went out for a workspace no step named\n{context}"
        );
    }

    async fn fake_daemon(recorded: Arc<Mutex<Vec<String>>>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    recorded
                        .lock()
                        .expect("journal mutex")
                        .push(request.method_name().to_owned());
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => {
                            ServerMessage::ok(id, &CheckPrereqsResult { items: vec![] })
                        }
                        Request::WorkspaceList {} => ServerMessage::ok(
                            id,
                            &WorkspaceListResult {
                                workspaces: vec![super::workspace(
                                    WORKSPACE,
                                    "bystander",
                                    WorkspaceState::Error("left alone".to_owned()),
                                    &[],
                                )],
                            },
                        ),
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::FsListDir(_) => {
                            ServerMessage::ok(id, &ListDirResult { entries: vec![] })
                        }
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        // Answered as a daemon would, so a run that did send
                        // one would not stall before the assertions catch it.
                        Request::WorkspaceRestart(_) | Request::WorkspaceDestroy(_) => {
                            ServerMessage::err(id, RpcError::internal("must not be asked"))
                        }
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });
        addr
    }
}

/// A Retry whose answer was lost resumes the agent only into a workspace that
/// is actually running. One the daemon reports `Destroying` is on its way out:
/// no agent is started in it, and the pending resume is dropped, so a later
/// `Ready` for it does not start one either.
mod sandbox_retry_destroying {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "retry-destroying-token";
    const WORKSPACE: &str = "ws_going1";
    const AGENT: &str = "ag_going_old";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn a_lost_retry_starts_no_agent_in_a_workspace_being_destroyed() {
        if bondsymphonic_ide::testing::skip_without_qt("retry into destroying") {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let addr = rt.block_on(fake_daemon(journal.clone()));

        let config = std::env::temp_dir().join(format!("bs-retry-going-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "going".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", "wait,wait,wait,quit")
            .env(
                "BS_MENU_TEST",
                format!("sandbox-banner,sandbox-retry={WORKSPACE}"),
            )
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");
        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(status.success(), "{context}");
        let _ = std::fs::remove_dir_all(&config);

        // The controls: the Retry went out, its connection was rebuilt and
        // the workspaces listed again, and the `Ready` behind that was sent.
        assert_eq!(
            seen.iter()
                .filter(|m| m.starts_with("workspace.restart"))
                .count(),
            1,
            "{context}"
        );
        assert!(
            seen.iter().filter(|m| *m == "workspace.list").count() >= 2,
            "the connection was never rebuilt, so this run proves nothing\n{context}"
        );
        assert!(
            seen.iter().any(|m| m == "sent:ready"),
            "the late Ready was never sent\n{context}"
        );
        assert!(
            !seen.iter().any(|m| m.starts_with("agent.start")),
            "an agent was started in a workspace being destroyed\n{context}"
        );
    }

    fn workspace(state: WorkspaceState) -> WorkspaceInfo {
        let mut info = super::workspace(WORKSPACE, "going", state, &[]);
        let record = AgentSummary {
            id: AgentId(AGENT.to_owned()),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Exited,
            session_id: Some("sess-going".to_owned()),
            command: None,
            model: None,
            permission_mode: None,
        };
        info.agents = vec![record.id.clone()];
        info.agent_records = vec![record];
        info
    }

    /// Listed down; the Retry is dropped unanswered; the reconnect lists the
    /// workspace `Destroying`, and a moment later a `Ready` event arrives for
    /// it -- which is what would start the agent if the pending resume had
    /// been kept.
    async fn fake_daemon(recorded: Journal) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let mut current = WorkspaceState::SandboxDown;
            let mut retried = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    recorded
                        .lock()
                        .expect("journal mutex")
                        .push(request.method_name().to_owned());
                    let mut late_ready = false;
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => Some(ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        )),
                        Request::Hello(_) => Some(ServerMessage::err(id, RpcError::unauthorized())),
                        Request::SystemCheckPrereqs {} => {
                            Some(ServerMessage::ok(id, &CheckPrereqsResult { items: vec![] }))
                        }
                        Request::WorkspaceList {} => {
                            late_ready = retried;
                            Some(ServerMessage::ok(
                                id,
                                &WorkspaceListResult {
                                    workspaces: vec![workspace(current.clone())],
                                },
                            ))
                        }
                        Request::WorkspaceRestart(_) => {
                            retried = true;
                            current = WorkspaceState::Destroying;
                            None
                        }
                        Request::AgentStart(_) => Some(ServerMessage::ok(
                            id,
                            &AgentStartResult {
                                agent_id: AgentId("ag_going_new".to_owned()),
                            },
                        )),
                        Request::AgentHistory(_) => Some(ServerMessage::ok(
                            id,
                            &HistoryResult {
                                messages: vec![],
                                state: AgentState::Exited,
                                detail: None,
                            },
                        )),
                        Request::FsWatch(_) => Some(ServerMessage::ok(id, &Empty {})),
                        Request::FsListDir(_) => {
                            Some(ServerMessage::ok(id, &ListDirResult { entries: vec![] }))
                        }
                        Request::WorkspaceChanges(_) => {
                            Some(ServerMessage::ok(id, &ChangesResult { files: vec![] }))
                        }
                        Request::WorkspaceStatus(_) => Some(ServerMessage::ok(
                            id,
                            &WorkspaceStatusResult { entries: vec![] },
                        )),
                        Request::RunList(_) => {
                            Some(ServerMessage::ok(id, &RunListResult { runs: vec![] }))
                        }
                        other => Some(ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        )),
                    };
                    let Some(reply) = reply else {
                        break;
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                    if late_ready {
                        // After the list has been applied; events travel on
                        // their own channel.
                        tokio::time::sleep(Duration::from_millis(1_500)).await;
                        let ready = ServerMessage::event(
                            Some(WorkspaceId(WORKSPACE.to_owned())),
                            Event::WorkspaceStateChanged {
                                info: Box::new(workspace(WorkspaceState::Ready)),
                            },
                        );
                        if w.write_all(codec::encode(&ready).as_bytes()).await.is_ok() {
                            recorded
                                .lock()
                                .expect("journal mutex")
                                .push("sent:ready".to_owned());
                        }
                    }
                }
            }
        });
        addr
    }
}

/// In-place workspaces: the banner of an in-place workspace that cannot run
/// asks to *close* it, in words that promise the checkout is untouched, and a
/// yes sends exactly one destroy, unforced. The fake daemon reports the
/// workspace with `kind: in_place`, which is the only way the IDE learns it.
///
/// The seam steps name the one workspace they may press on.
mod in_place_close {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "in-place-close-token";
    const SCRIPT: &str = "wait,quit";
    const MENU_TEST: &str = "sandbox-banner,sandbox-remove=ws_inplace1,destroy-yes=ws_inplace1";
    const WORKSPACE: &str = "ws_inplace1";
    const NAME: &str = "checkout-one";
    const REASON: &str = "The repository /smoke/repo is missing or is no longer a git \
                          repository. Close the workspace, or restore the folder and press Retry.";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn an_in_place_workspace_is_closed_with_one_plain_destroy() {
        if bondsymphonic_ide::testing::skip_without_qt("in-place close") {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        // Never the developer's real `%APPDATA%\BondSymphonic`.
        let config = std::env::temp_dir().join(format!("bs-in-place-close-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "here".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);

        let question = out
            .lines()
            .find(|l| l.starts_with(&format!("BS_MENU_TEST destroy target={WORKSPACE} ")))
            .unwrap_or_else(|| panic!("Close did not ask\n{context}"));
        assert!(
            question.ends_with(&format!(
                "question=Close workspace \"{NAME}\"? The agent and its sandbox stop. Your \
                 files, branches and git history are not touched."
            )),
            "{question}"
        );
        assert!(!out.contains("BS_MENU_TEST destroy-refused"), "{context}");
        let destroys: Vec<&String> = seen
            .iter()
            .filter(|m| m.starts_with("workspace.destroy"))
            .collect();
        assert_eq!(
            destroys,
            [&format!("workspace.destroy:{WORKSPACE}:false")],
            "{context}"
        );
    }

    /// One in-place workspace that cannot run. Any destroy succeeds: the daemon
    /// ignores `force` for this kind, and a second, forced one would be the bug.
    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut destroyed = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    let method = match &request {
                        Request::WorkspaceDestroy(p) => {
                            format!("workspace.destroy:{}:{}", p.workspace_id.0, p.force)
                        }
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let listed = if destroyed { vec![] } else { vec![workspace()] };
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => {
                            ServerMessage::ok(id, &WorkspaceListResult { workspaces: listed })
                        }
                        Request::WorkspaceGet(_) => ServerMessage::ok(id, &workspace()),
                        Request::WorkspaceDestroy(_) => {
                            destroyed = true;
                            ServerMessage::ok(id, &Empty {})
                        }
                        Request::FsListDir(_) => {
                            ServerMessage::ok(id, &ListDirResult { entries: vec![] })
                        }
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });

        (addr, journal)
    }

    /// No agent, for the reason `sandbox_remove_dirty` gives: the question is
    /// about the workspace.
    fn workspace() -> WorkspaceInfo {
        WorkspaceInfo {
            kind: WorkspaceKind::InPlace,
            worktree_path: "/smoke/repo".to_owned(),
            branch: "main".to_owned(),
            ..super::workspace(
                WORKSPACE,
                NAME,
                WorkspaceState::Error(REASON.to_owned()),
                &[],
            )
        }
    }
}

/// The breach detail (final wave, I1): the daemon stops an in-place sandbox
/// whose protected git files were replaced, and says so in two events -- a
/// warning carrying the sentence and a diff of what changed, then the
/// workspace's own `Error` state carrying the sentence alone. The sentence
/// tells the user to check `.git/config`; this is the run that proves the diff
/// is one click away in the banner rather than only in a log they cannot see.
mod in_place_breach_detail {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "in-place-breach-token";
    const SCRIPT: &str = "wait,wait,quit";
    const MENU_TEST: &str = "sandbox-banner,sandbox-what-changed=ws_breach1";
    const WORKSPACE: &str = "ws_breach1";
    const NAME: &str = "checkout-one";
    const SENTENCE: &str = "Git files this workspace protects were replaced while the agent was \
                            running (.git/config), so its sandbox was stopped. Check .git/config \
                            for settings you did not make.";
    const DIFF: &str = "--- .git/config\n+++ .git/config\n+\tfsmonitor = ./planted.sh\n";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn the_diff_behind_a_stopped_sandbox_is_one_click_from_the_banner() {
        if bondsymphonic_ide::testing::skip_without_qt("in-place breach detail") {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        // Never the developer's real `%APPDATA%\BondSymphonic`.
        let config =
            std::env::temp_dir().join(format!("bs-in-place-breach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "here".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);

        // The IDE's stdout is a Windows text stream, so every line it wrote
        // arrives with a carriage return in front of the newline. The diff
        // below is matched whole, across lines, which is the one assertion
        // here that cares.
        let out = out.replace("\r\n", "\n");
        // The sentence is in the strip, as any workspace problem's is.
        assert!(
            out.contains(&format!(
                "BS_MENU_TEST sandbox-banner target={WORKSPACE} question=This workspace could \
                 not be started | {SENTENCE}"
            )),
            "the banner did not say why the workspace stopped\n{context}"
        );
        // And the diff the sentence sends the user to check is behind the
        // banner's own disclosure, verbatim.
        assert!(
            out.contains(&format!(
                "BS_MENU_TEST sandbox-what-changed target={WORKSPACE} question={DIFF}"
            )),
            "the diff never reached the banner\n{context}"
        );
    }

    /// One in-place workspace that is running, until the list is answered: then
    /// the daemon says what it found and what it did about it, in that order.
    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut stopped = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    recorded
                        .lock()
                        .expect("journal mutex")
                        .push(request.method_name().to_owned());
                    let mut after: Vec<ServerMessage> = Vec::new();
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => {
                            let listed = workspace(if stopped {
                                WorkspaceState::Error(SENTENCE.to_owned())
                            } else {
                                WorkspaceState::Ready
                            });
                            if !stopped {
                                stopped = true;
                                after.push(ServerMessage::event(
                                    Some(WorkspaceId(WORKSPACE.to_owned())),
                                    Event::DaemonLog {
                                        level: LogLevel::Warn,
                                        message: format!("{SENTENCE}\n{DIFF}"),
                                        host: None,
                                    },
                                ));
                                after.push(ServerMessage::event(
                                    Some(WorkspaceId(WORKSPACE.to_owned())),
                                    Event::WorkspaceStateChanged {
                                        info: Box::new(workspace(WorkspaceState::Error(
                                            SENTENCE.to_owned(),
                                        ))),
                                    },
                                ));
                            }
                            ServerMessage::ok(
                                id,
                                &WorkspaceListResult {
                                    workspaces: vec![listed],
                                },
                            )
                        }
                        Request::WorkspaceGet(_) => {
                            ServerMessage::ok(id, &workspace(WorkspaceState::Ready))
                        }
                        Request::FsListDir(_) => {
                            ServerMessage::ok(id, &ListDirResult { entries: vec![] })
                        }
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                    if !after.is_empty() {
                        // After the list has been applied: the events would
                        // otherwise reach a model that has no tab for them yet,
                        // which is a case the model tests cover and this run is
                        // not about.
                        tokio::time::sleep(Duration::from_millis(1_500)).await;
                    }
                    for message in after {
                        let _ = w.write_all(codec::encode(&message).as_bytes()).await;
                    }
                }
            }
        });

        (addr, journal)
    }

    /// No agent: the banner this run is about belongs to the workspace.
    fn workspace(state: WorkspaceState) -> WorkspaceInfo {
        WorkspaceInfo {
            kind: WorkspaceKind::InPlace,
            worktree_path: "/smoke/repo".to_owned(),
            branch: "main".to_owned(),
            ..super::workspace(WORKSPACE, NAME, state, &[])
        }
    }
}

/// Selecting a tab whose agent has ended starts it again -- once, resuming the
/// conversation -- and an agent that dies of that start bars the workspace from
/// any more of them. And an agent exit asks the daemon about the prerequisites
/// again, once however many exits arrive together.
///
/// Two workspaces, because a selection has to come *from* somewhere: the first
/// carries a live agent and is where the run starts, the second carries an
/// agent the daemon reports as ended with a session to resume. Nothing else
/// starts that one -- the automatic start at launch skips an agent that exited,
/// deliberately -- so every `agent.start` these runs see is the selection's
/// doing. The `select-tab` seam alternates the active tab between the two, so
/// the run visits the ended one twice, and the second visit is what the loop
/// guard has to refuse.
///
/// The two exit scenarios arm no seam at all, which is what makes them the
/// control for the first two: no selection, no `agent.start`.
mod agent_auto_restart {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "auto-restart-token";
    /// Three waits and the quit: the seam makes four selections a second and a
    /// half apart, and the last of them has to have been answered -- or refused
    /// -- before the window goes.
    const SCRIPT: &str = "wait,wait,wait,quit";
    /// The workspace whose agent is alive. Its session id is its own, so an
    /// assertion naming the other one's cannot be satisfied by a start sent
    /// here.
    const LIVE_WORKSPACE: &str = "ws_auto1";
    const LIVE_NAME: &str = "auto-live";
    const LIVE_AGENT: &str = "ag_auto_live";
    const LIVE_SESSION: &str = "sess-auto-1";
    /// The workspace under test: ready, Claude, with an ended agent behind it.
    const DEAD_WORKSPACE: &str = "ws_auto2";
    const DEAD_NAME: &str = "auto-dead";
    const DEAD_AGENT: &str = "ag_auto_old";
    const SESSION: &str = "sess-auto-2";
    /// What the fake daemon hands back for an `agent.start`.
    const NEW_AGENT: &str = "ag_auto_new";
    /// How the daemon words an agent that died as it started, which is what an
    /// expired Claude login looks like from the IDE's side.
    const DIED: &str = "claude exited with code 1: the session has expired";
    /// The line the window prints when an automatic start is what sent the
    /// request. The journal shows a start was sent; this shows which path sent
    /// it.
    const AUTO_STARTED: &str = "BS_MENU_TEST agent-auto-started target=ws_auto2 ";
    /// Every arrival of the seam at the tab under test.
    const SELECTED_DEAD: &str = "BS_MENU_TEST select-tab target=ws_auto2 ";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[derive(Clone, Copy, PartialEq)]
    enum Scenario {
        /// The started agent lives, and reports itself idle the way a real one
        /// does.
        StartWorks,
        /// The started agent dies at once. The seam comes back to the tab after
        /// that, which is the visit the loop guard has to refuse.
        StartDies,
        /// No selections: one agent exits, and that alone has to reach the
        /// daemon as a prerequisite check.
        OneExit,
        /// No selections: three exits inside a few milliseconds, which is what
        /// a sandbox backend going down looks like.
        ExitBurst,
        /// The window comes up *on* the ended agent's tab, and no seam is armed
        /// at all: the only trigger left is the tab the window opened on, which
        /// is what a daemon restart leaves the user looking at.
        FrontTab,
        /// The opening tab again, but the daemon lists its workspace `ready`
        /// from the first request and only announces it by event two seconds
        /// later -- which is what a daemon that is still restoring the
        /// workspace does, and what put a box titled `agent.start` over a
        /// user's window.
        StaleReady,
        /// The opening tab again: the first `agent.start` is refused with a
        /// `SandboxError`, the way a start against a sandbox that is not there
        /// is, and the daemon then announces the workspace ready.
        StartRefused,
    }
    /// The journal line the fake daemon writes just before it sends the
    /// `workspace.state ready` event, so a run can say which side of that
    /// event a start was on.
    const SENT_READY: &str = "sent:ready";

    #[test]
    fn selecting_a_tab_whose_agent_ended_starts_one_agent_resuming_its_session() {
        if bondsymphonic_ide::testing::skip_without_qt("auto restart on selection") {
            return;
        }
        let run = run_ide(Scenario::StartWorks);
        // The control. An absence proves nothing about a run whose selections
        // never happened, and the assertion below is that the *second* visit
        // asked for nothing more.
        let visits = run
            .out
            .lines()
            .filter(|l| l.starts_with(SELECTED_DEAD))
            .count();
        assert!(
            visits >= 2,
            "the seam reached {DEAD_WORKSPACE} {visits} times, so this run proves nothing\n{}",
            run.context
        );
        assert!(
            run.out.lines().any(|l| l.starts_with(AUTO_STARTED)),
            "the start was not the selection's doing\n{}",
            run.context
        );
        // One start, naming the workspace that was selected and the session the
        // daemon's own record carried: the conversation is resumed, not
        // replaced by an empty one.
        assert_eq!(
            run.journal
                .iter()
                .filter(|m| m.starts_with("agent.start"))
                .collect::<Vec<_>>(),
            [&format!("agent.start:{DEAD_WORKSPACE}:{SESSION}")],
            "{}",
            run.context
        );
        // And the other half of the prerequisite claim: no agent exited in this
        // run, so the only check is the one the controller makes on connect.
        assert_eq!(
            run.journal
                .iter()
                .filter(|m| *m == "system.check_prereqs")
                .count(),
            1,
            "something other than an agent exit asked for a prerequisite check\n{}",
            run.context
        );
    }

    #[test]
    fn an_agent_that_dies_of_an_automatic_start_gets_no_second_one() {
        if bondsymphonic_ide::testing::skip_without_qt("auto restart loop guard") {
            return;
        }
        let run = run_ide(Scenario::StartDies);
        let visits = run
            .out
            .lines()
            .filter(|l| l.starts_with(SELECTED_DEAD))
            .count();
        assert!(
            visits >= 2,
            "the seam reached {DEAD_WORKSPACE} {visits} times, so the tab was never selected a \
             second time and this run proves nothing\n{}",
            run.context
        );
        assert!(
            run.out.lines().any(|l| l.starts_with(AUTO_STARTED)),
            "the first start never happened, so there is no loop to guard\n{}",
            run.context
        );
        assert_eq!(
            run.journal
                .iter()
                .filter(|m| m.starts_with("agent.start"))
                .collect::<Vec<_>>(),
            [&format!("agent.start:{DEAD_WORKSPACE}:{SESSION}")],
            "an agent that died of being started was started again\n{}",
            run.context
        );
    }

    #[test]
    fn the_tab_the_window_opened_on_has_its_ended_agent_started() {
        if bondsymphonic_ide::testing::skip_without_qt("auto restart on the opening tab") {
            return;
        }
        let run = run_ide(Scenario::FrontTab);
        // Nobody selected anything -- the selection seam is not armed in this
        // run -- so the tab the window came up on is the only thing that can
        // have asked. This is the daemon-restart case: every restored Claude tab
        // is bound to an ended agent record, and the one in front used to be the
        // only tab in the window that needed a switch away and back first.
        assert!(
            !run.out.contains("BS_MENU_TEST select-tab"),
            "a selection was driven after all, so this run proves nothing about the opening              tab
{}",
            run.context
        );
        assert!(
            run.out.lines().any(|l| l.starts_with(AUTO_STARTED)),
            "the tab the window opened on never had its agent started
{}",
            run.context
        );
        assert_eq!(
            run.journal
                .iter()
                .filter(|m| m.starts_with("agent.start"))
                .collect::<Vec<_>>(),
            [&format!("agent.start:{DEAD_WORKSPACE}:{SESSION}")],
            "{}",
            run.context
        );
    }

    #[test]
    fn an_agent_exit_asks_the_daemon_about_the_prerequisites_again() {
        if bondsymphonic_ide::testing::skip_without_qt("exit rechecks prerequisites") {
            return;
        }
        let run = run_ide(Scenario::OneExit);
        run.assert_one_check_after_the_exits(1);
        // Nothing was selected, so nothing was started: the exit is news about
        // the login, not a reason to restart an agent where it stopped.
        assert!(
            !run.journal.iter().any(|m| m.starts_with("agent.start")),
            "an agent exit started an agent by itself\n{}",
            run.context
        );
    }

    #[test]
    fn a_burst_of_exits_asks_about_the_prerequisites_once() {
        if bondsymphonic_ide::testing::skip_without_qt("exit burst rechecks once") {
            return;
        }
        let run = run_ide(Scenario::ExitBurst);
        run.assert_one_check_after_the_exits(3);
    }

    #[test]
    fn a_workspace_listed_ready_is_not_started_until_the_daemon_announces_it() {
        if bondsymphonic_ide::testing::skip_without_qt("stale ready at launch") {
            return;
        }
        let run = run_ide(Scenario::StaleReady);
        assert!(
            !run.out.contains("BS_MENU_TEST select-tab"),
            "a selection was driven after all, so this run proves nothing about the opening tab\n{}",
            run.context
        );
        // The daemon listed the workspace `ready` and said nothing more for two
        // seconds. A daemon that is still restoring the workspace looks exactly
        // like that, and a start sent into that silence goes against a sandbox
        // that does not exist: on a user's machine it hung until the request
        // timed out and the window put up a box. So nothing before the event,
        // and the one start after it, resuming the session the record carried.
        let starts_and_event: Vec<String> = run
            .journal
            .iter()
            .filter(|m| m.starts_with("agent.start") || *m == SENT_READY)
            .cloned()
            .collect();
        assert_eq!(
            starts_and_event,
            [
                SENT_READY.to_owned(),
                format!("agent.start:{DEAD_WORKSPACE}:{SESSION}"),
            ],
            "the opening tab was started on the list's word, or not on the event's\n{}",
            run.context
        );
        assert!(
            run.out.lines().any(|l| l.starts_with(AUTO_STARTED)),
            "the start was not the opening tab's doing\n{}",
            run.context
        );
    }

    #[test]
    fn an_automatic_start_the_daemon_refuses_is_tried_again_when_the_workspace_is_ready() {
        if bondsymphonic_ide::testing::skip_without_qt("refused automatic start retries") {
            return;
        }
        let run = run_ide(Scenario::StartRefused);
        assert!(
            !run.out.contains("BS_MENU_TEST select-tab"),
            "a selection was driven after all, so this run proves nothing about the opening tab\n{}",
            run.context
        );
        // The first start was refused -- no agent came of it, so nothing can
        // have crashed -- and the daemon then said the workspace was ready. That
        // is the launch-time shape once more, and it has to be answered the
        // same way: with a start, not with a tab that stays dead for the rest
        // of the session because its one automatic start has been spent.
        let start = format!("agent.start:{DEAD_WORKSPACE}:{SESSION}");
        let starts_and_events: Vec<String> = run
            .journal
            .iter()
            .filter(|m| m.starts_with("agent.start") || *m == SENT_READY)
            .cloned()
            .collect();
        assert_eq!(
            starts_and_events,
            [
                SENT_READY.to_owned(),
                start.clone(),
                SENT_READY.to_owned(),
                start
            ],
            "a refused automatic start was not tried again once the workspace was ready\n{}",
            run.context
        );
    }

    struct Run {
        out: String,
        journal: Vec<String>,
        context: String,
    }

    impl Run {
        /// That `sent` exits reached the window and exactly one prerequisite
        /// check followed them. Counted from the daemon's own marker rather
        /// than from the start of the journal, because the controller checks
        /// once on connect and that one is not what these runs are about.
        fn assert_one_check_after_the_exits(&self, sent: usize) {
            let after: Vec<&String> = self
                .journal
                .iter()
                .skip_while(|m| *m != "sent:exits")
                .collect();
            assert!(
                !after.is_empty(),
                "the fake daemon never sent the exits\n{}",
                self.context
            );
            assert_eq!(
                after.iter().filter(|m| **m == "sent:exit").count(),
                sent,
                "the fake daemon did not send {sent} exits, so this run proves nothing\n{}",
                self.context
            );
            assert_eq!(
                after
                    .iter()
                    .filter(|m| **m == "system.check_prereqs")
                    .count(),
                1,
                "the exits did not produce exactly one prerequisite check\n{}",
                self.context
            );
            // The connect-time check is the control: a run with none of those
            // has a broken controller rather than a working trigger.
            assert!(
                self.journal
                    .iter()
                    .take(self.journal.len() - after.len())
                    .any(|m| m == "system.check_prereqs"),
                "the controller never checked the prerequisites on connect\n{}",
                self.context
            );
        }
    }

    fn run_ide(scenario: Scenario) -> Run {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon(scenario));

        let tag = match scenario {
            Scenario::StartWorks => "works",
            Scenario::StartDies => "dies",
            Scenario::OneExit => "one",
            Scenario::ExitBurst => "burst",
            Scenario::FrontTab => "front",
            Scenario::StaleReady => "stale",
            Scenario::StartRefused => "refused",
        };
        let config = std::env::temp_dir().join(format!("bs-auto-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        // Both tabs in one group, the live one in front: the seam steps between
        // them, and a group with one tab has nowhere to step.
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "auto".to_owned(),
                workspace_ids: vec![LIVE_WORKSPACE.to_owned(), DEAD_WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            // Which tab the window comes up on. The ended agent's own tab for
            // the opening-tab trigger, and the live one for every scenario that
            // is about a selection: a trigger that only fires on the tab in
            // front proves nothing from the tab in front.
            active_workspace: Some(match scenario {
                Scenario::FrontTab | Scenario::StaleReady | Scenario::StartRefused => {
                    DEAD_WORKSPACE.to_owned()
                }
                _ => LIVE_WORKSPACE.to_owned(),
            }),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        // The exit scenarios arm nothing at all: an exit must start no agent,
        // and a seam that was not armed cannot be what proves it.
        let menu_test = match scenario {
            Scenario::StartWorks | Scenario::StartDies => format!("select-tab={DEAD_WORKSPACE}"),
            Scenario::OneExit | Scenario::ExitBurst => String::new(),
            // A step that reports and presses nothing: the window has to be in
            // an announcing mood for the automatic start to say so, and no seam
            // may touch the selection.
            Scenario::FrontTab | Scenario::StaleReady | Scenario::StartRefused => {
                "sandbox-banner".to_owned()
            }
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", menu_test)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let journal = journal.lock().expect("journal mutex").clone();
        let context =
            format!("requests: {journal:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !format!("{out}{err}").contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);
        Run {
            out,
            journal,
            context,
        }
    }

    /// An `agent.state` event for one of the two workspaces' agents.
    fn state_event(
        workspace: &str,
        agent: &str,
        state: AgentState,
        detail: Option<&str>,
    ) -> ServerMessage {
        ServerMessage::event(
            Some(WorkspaceId(workspace.to_owned())),
            Event::AgentStateChanged {
                agent_id: AgentId(agent.to_owned()),
                state,
                detail: detail.map(str::to_owned),
            },
        )
    }

    /// The `workspace.state` event a finished restore of the dead workspace
    /// emits: the workspace `ready`, its agent record still ended.
    fn ready_event() -> ServerMessage {
        ServerMessage::event(
            Some(WorkspaceId(DEAD_WORKSPACE.to_owned())),
            Event::WorkspaceStateChanged {
                info: Box::new(dead_in(WorkspaceState::Ready)),
            },
        )
    }

    async fn fake_daemon(scenario: Scenario) -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut exits_sent = false;
            // How many `agent.start`s have been answered, for the scenario that
            // refuses the first and takes the second.
            let mut starts_answered = 0usize;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, w) = stream.into_split();
                // Shared with the tasks that send the delayed events below, so
                // the read loop never sleeps: a request the window sends during
                // a delay is read -- and journaled -- when it arrives, not after
                // the delay, and the journal's order is the wire's. The markers
                // the runs assert against are only worth something on those
                // terms; read in line with the sleeps, an `agent.start` sent
                // half a second after the list was journaled after a marker
                // written two seconds after it.
                let w = Arc::new(tokio::sync::Mutex::new(w));
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    // The workspace and the session an `agent.start` asked to
                    // resume, because both are the claim: a start naming the
                    // other workspace, or naming no session, would satisfy a
                    // journal that only counted methods.
                    let method = match &request {
                        Request::AgentStart(p) => format!(
                            "agent.start:{}:{}",
                            p.workspace_id.0,
                            p.options.resume_session.clone().unwrap_or_default()
                        ),
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let mut after: Vec<ServerMessage> = Vec::new();
                    // Long enough for the reply to have been applied: the state
                    // event below is about the agent the start's own answer
                    // records, and events travel on their own channel.
                    let mut delay = Duration::from_millis(200);
                    let mut marked = false;
                    // A journal line to write just before the messages below go
                    // out, when a run needs to know which side of them a
                    // request was on.
                    let mut announce: Option<&'static str> = None;
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => Some(ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        )),
                        Request::Hello(_) => Some(ServerMessage::err(id, RpcError::unauthorized())),
                        // Everything passes, every time. A failing prerequisite
                        // would open Settings over the run, and what is
                        // asserted here is that the question was asked at all.
                        Request::SystemCheckPrereqs {} => Some(ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        )),
                        Request::WorkspaceList {} => {
                            // The exits, once the tabs exist to hear of them.
                            if matches!(scenario, Scenario::OneExit | Scenario::ExitBurst)
                                && !exits_sent
                            {
                                exits_sent = true;
                                marked = true;
                                delay = Duration::from_millis(1_500);
                                after.push(state_event(
                                    LIVE_WORKSPACE,
                                    LIVE_AGENT,
                                    AgentState::Exited,
                                    Some(DIED),
                                ));
                                if scenario == Scenario::ExitBurst {
                                    // Every agent in the window at once, which
                                    // is what a sandbox backend going down
                                    // looks like from here.
                                    after.push(state_event(
                                        DEAD_WORKSPACE,
                                        DEAD_AGENT,
                                        AgentState::Exited,
                                        Some(DIED),
                                    ));
                                    after.push(state_event(
                                        LIVE_WORKSPACE,
                                        LIVE_AGENT,
                                        AgentState::Exited,
                                        Some(DIED),
                                    ));
                                }
                            }
                            // The daemon-restart shape: the sandbox is still
                            // coming up when the window opens, so the first
                            // thing the front tab is is ineligible. What starts
                            // its agent is the `Ready` that follows, which is
                            // why the opening-tab trigger has to outlive the
                            // pass it was first asked in.
                            //
                            // Or the older daemon's shape: the workspace is
                            // listed `ready` while its restore is still running,
                            // and only the event two seconds on says it is
                            // there. Two seconds is four times the automatic
                            // start's own delay, so a start sent on the list's
                            // word lands well before the marker.
                            let listed = match scenario {
                                Scenario::FrontTab | Scenario::StartRefused => {
                                    delay = Duration::from_millis(1_500);
                                    announce = Some(SENT_READY);
                                    after.push(ready_event());
                                    dead_in(WorkspaceState::Creating)
                                }
                                Scenario::StaleReady => {
                                    delay = Duration::from_millis(2_000);
                                    announce = Some(SENT_READY);
                                    after.push(ready_event());
                                    dead()
                                }
                                _ => dead(),
                            };
                            Some(ServerMessage::ok(
                                id,
                                &WorkspaceListResult {
                                    workspaces: vec![live(), listed],
                                },
                            ))
                        }
                        Request::WorkspaceGet(p) => Some(ServerMessage::ok(
                            id,
                            &if p.workspace_id.0 == LIVE_WORKSPACE {
                                live()
                            } else {
                                dead()
                            },
                        )),
                        // The first start refused the way the real daemon
                        // refuses one against a sandbox it has not brought up,
                        // and the workspace then announced ready -- the same
                        // event that follows a finished restore.
                        Request::AgentStart(_)
                            if scenario == Scenario::StartRefused && starts_answered == 0 =>
                        {
                            starts_answered += 1;
                            announce = Some(SENT_READY);
                            after.push(ready_event());
                            Some(ServerMessage::err(
                                id,
                                RpcError::new(
                                    ErrorCode::SandboxError,
                                    format!("sandbox for {DEAD_WORKSPACE} is not running"),
                                ),
                            ))
                        }
                        Request::AgentStart(_) => {
                            starts_answered += 1;
                            after.push(state_event(
                                DEAD_WORKSPACE,
                                NEW_AGENT,
                                match scenario {
                                    Scenario::StartDies => AgentState::Exited,
                                    _ => AgentState::Idle,
                                },
                                match scenario {
                                    Scenario::StartDies => Some(DIED),
                                    _ => None,
                                },
                            ));
                            Some(ServerMessage::ok(
                                id,
                                &AgentStartResult {
                                    agent_id: AgentId(NEW_AGENT.to_owned()),
                                },
                            ))
                        }
                        // The live agent's history says it is live. The state a
                        // history carries is the pane's reading of the
                        // conversation, and answering `Exited` for every agent
                        // would describe a workspace this run says is working.
                        Request::AgentHistory(p) => Some(ServerMessage::ok(
                            id,
                            &HistoryResult {
                                messages: vec![],
                                state: if p.agent_id.0 == LIVE_AGENT {
                                    AgentState::Idle
                                } else {
                                    AgentState::Exited
                                },
                                detail: None,
                            },
                        )),
                        Request::FsListDir(_) => {
                            Some(ServerMessage::ok(id, &ListDirResult { entries: vec![] }))
                        }
                        Request::FsWatch(_) => Some(ServerMessage::ok(id, &Empty {})),
                        Request::WorkspaceChanges(_) => {
                            Some(ServerMessage::ok(id, &ChangesResult { files: vec![] }))
                        }
                        Request::WorkspaceStatus(_) => Some(ServerMessage::ok(
                            id,
                            &WorkspaceStatusResult { entries: vec![] },
                        )),
                        Request::RepoDetectRunConfigs(_) => Some(ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        )),
                        Request::RunList(_) => {
                            Some(ServerMessage::ok(id, &RunListResult { runs: vec![] }))
                        }
                        other => Some(ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        )),
                    };
                    let Some(reply) = reply else {
                        break;
                    };
                    if w.lock()
                        .await
                        .write_all(codec::encode(&reply).as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if after.is_empty() {
                        continue;
                    }
                    let w = w.clone();
                    let recorded = recorded.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        if marked {
                            // Where the prerequisite assertions count from.
                            // Written before the exits, so a check that raced
                            // them is still counted as theirs.
                            recorded
                                .lock()
                                .expect("journal mutex")
                                .push("sent:exits".to_owned());
                        }
                        // Likewise before the event it names: a start that
                        // raced the event is on the wrong side of the marker,
                        // which is the side the assertion is about.
                        if let Some(mark) = announce {
                            recorded
                                .lock()
                                .expect("journal mutex")
                                .push(mark.to_owned());
                        }
                        let mut w = w.lock().await;
                        for message in after {
                            if w.write_all(codec::encode(&message).as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if marked {
                                recorded
                                    .lock()
                                    .expect("journal mutex")
                                    .push("sent:exit".to_owned());
                            }
                        }
                    });
                }
            }
        });

        (addr, journal)
    }

    /// The workspace whose agent is running.
    fn live() -> WorkspaceInfo {
        workspace(
            LIVE_WORKSPACE,
            LIVE_NAME,
            LIVE_AGENT,
            AgentState::Idle,
            LIVE_SESSION,
        )
    }

    /// The workspace whose agent has ended, with the session a restart resumes.
    /// Ready, so nothing about the workspace itself is what keeps an agent from
    /// starting in it: these runs are about the agent alone.
    fn dead() -> WorkspaceInfo {
        dead_in(WorkspaceState::Ready)
    }

    /// The same workspace in a state of the caller's choosing. A daemon that has
    /// just restarted reports every workspace `Creating` and brings them up over
    /// the next few seconds, and the opening-tab run comes up in the middle of
    /// that: the tab is in front, its agent record is dead, and its sandbox is
    /// not there yet.
    fn dead_in(state: WorkspaceState) -> WorkspaceInfo {
        let mut info = workspace(
            DEAD_WORKSPACE,
            DEAD_NAME,
            DEAD_AGENT,
            AgentState::Exited,
            SESSION,
        );
        info.state = state;
        info
    }

    fn workspace(
        id: &str,
        name: &str,
        agent: &str,
        state: AgentState,
        session: &str,
    ) -> WorkspaceInfo {
        let mut info = super::workspace(id, name, WorkspaceState::Ready, &[]);
        let record = AgentSummary {
            id: AgentId(agent.to_owned()),
            adapter: AgentAdapterKind::Claude,
            state,
            session_id: Some(session.to_owned()),
            command: None,
            model: None,
            permission_mode: None,
        };
        info.agents = vec![record.id.clone()];
        info.agent_records = vec![record];
        info
    }
}

/// A tab that comes forward because its neighbour was destroyed is not a tab
/// anybody selected, and its ended agent stays ended.
///
/// This is the hole the opening-tab trigger opens. That trigger exists because
/// nobody can select the tab a window comes up on, and it has to survive
/// republishes to outlast the seconds in which the workspace is still coming up
/// -- so on its own it would also cover a tab that arrives in front later, for a
/// reason that is the opposite of the user asking for an agent: they have just
/// destroyed the workspace next door. `m_frontTabSettled` is what draws that
/// line, and this is the run that holds it there.
///
/// Two workspaces. The first is in error, which is what gives its banner a
/// Remove button for the seam to press; the second is ready, Claude, and bound
/// to an ended agent with a session to resume -- eligible in every way except
/// that nobody asked.
mod front_tab_after_destroy {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "front-destroy-token";
    /// The Remove is pressed off the first banner, the destroy answered, and the
    /// tab behind it is then in front for the rest of the run -- several times
    /// longer than the automatic start's own delay, so an absence here is a
    /// refusal rather than a race.
    const SCRIPT: &str = "wait,wait,wait,quit";
    /// The workspace the seam destroys: in error, so its banner has the Remove.
    /// No agent record at all, so nothing about *it* can put an `agent.start` on
    /// the wire and the one this run forbids can only be the other one's.
    const GONE: &str = "ws_gone1";
    const GONE_NAME: &str = "gone-one";
    /// The workspace that is left, and comes forward because the other went.
    const NEXT: &str = "ws_after1";
    const NEXT_NAME: &str = "after-one";
    const NEXT_AGENT: &str = "ag_after_old";
    const NEXT_SESSION: &str = "sess-after-1";
    /// Why the first workspace could not be started, as the daemon words it.
    const REASON: &str = "The worktree's git registration was missing and could not be restored.";
    /// `sandbox-remove` presses the banner's Remove and `destroy-yes` answers the
    /// confirmation with Force unticked; `sandbox-banner` is what makes the
    /// window announce at all.
    const MENU_TEST: &str = "sandbox-banner,sandbox-remove=ws_gone1,destroy-yes=ws_gone1";
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn a_tab_that_comes_forward_when_its_neighbour_is_destroyed_starts_no_agent() {
        if bondsymphonic_ide::testing::skip_without_qt("no auto restart after a destroy") {
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon());

        let config = std::env::temp_dir().join(format!("bs-front-destroy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        // The failing workspace in front, so the tab that comes forward has
        // never been activated before: its first activation is the destroy's
        // doing, which is the whole case.
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "front".to_owned(),
                workspace_ids: vec![GONE.to_owned(), NEXT.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(GONE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let seen = journal.lock().expect("journal mutex").clone();
        let context = format!("requests: {seen:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !format!("{out}{err}").contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);

        // The first control: the destroy the seam pressed really went out.
        let destroyed = seen
            .iter()
            .position(|m| m == &format!("workspace.destroy:{GONE}"))
            .unwrap_or_else(|| panic!("the seam never destroyed {GONE}\n{context}"));
        // The second, and the one that makes the absence below mean anything: the
        // surviving tab really did come forward. Only an activation builds a pane
        // and attaches its transcript, and only an attach sends `agent.history`,
        // so this request *is* the tab being shown -- and its place after the
        // destroy is what says the destroy is why.
        let shown = seen
            .iter()
            .position(|m| m == &format!("agent.history:{NEXT_AGENT}"))
            .unwrap_or_else(|| {
                panic!("{NEXT} never came forward, so this run proves nothing\n{context}")
            });
        assert!(
            destroyed < shown,
            "{NEXT} was already in front before the destroy\n{context}"
        );
        // And the claim. Nobody selected this tab and nobody opened the window on
        // it: it is in front because the workspace beside it was destroyed, and
        // an agent started here would be the IDE answering a destroy with a new
        // conversation.
        assert!(
            !seen.iter().any(|m| m.starts_with("agent.start")),
            "an agent was started in a tab that came forward on its own\n{context}"
        );
    }

    async fn fake_daemon() -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            // Which workspaces a list still reports. The destroy takes the first
            // one out, so a list that arrives after it describes the world the
            // window is already showing.
            let mut gone_destroyed = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    // Three of these carry more than their method name: which
                    // workspace was destroyed, which agent a pane attached to,
                    // and which workspace and session a start asked for.
                    let method = match &request {
                        Request::WorkspaceDestroy(p) => {
                            format!("workspace.destroy:{}", p.workspace_id.0)
                        }
                        Request::AgentHistory(p) => format!("agent.history:{}", p.agent_id.0),
                        Request::AgentStart(p) => format!(
                            "agent.start:{}:{}",
                            p.workspace_id.0,
                            p.options.resume_session.clone().unwrap_or_default()
                        ),
                        other => other.method_name().to_owned(),
                    };
                    recorded.lock().expect("journal mutex").push(method);
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        ),
                        Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                        Request::SystemCheckPrereqs {} => ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in".into(),
                                    fix_hint: None,
                                }],
                            },
                        ),
                        Request::WorkspaceList {} => {
                            let mut workspaces = vec![next()];
                            if !gone_destroyed {
                                workspaces.insert(0, gone());
                            }
                            ServerMessage::ok(id, &WorkspaceListResult { workspaces })
                        }
                        Request::WorkspaceGet(p) => ServerMessage::ok(
                            id,
                            &if p.workspace_id.0 == GONE {
                                gone()
                            } else {
                                next()
                            },
                        ),
                        Request::WorkspaceDestroy(_) => {
                            gone_destroyed = true;
                            ServerMessage::ok(id, &Empty {})
                        }
                        Request::AgentHistory(_) => ServerMessage::ok(
                            id,
                            &HistoryResult {
                                messages: vec![],
                                state: AgentState::Exited,
                                detail: None,
                            },
                        ),
                        // Answered rather than refused, although the run forbids
                        // it: a refusal would put a modal box over the window and
                        // turn the assertion into a timeout that says nothing
                        // about what the IDE did.
                        Request::AgentStart(_) => ServerMessage::ok(
                            id,
                            &AgentStartResult {
                                agent_id: AgentId("ag_after_new".to_owned()),
                            },
                        ),
                        Request::FsListDir(_) => {
                            ServerMessage::ok(id, &ListDirResult { entries: vec![] })
                        }
                        Request::FsWatch(_) => ServerMessage::ok(id, &Empty {}),
                        Request::WorkspaceChanges(_) => {
                            ServerMessage::ok(id, &ChangesResult { files: vec![] })
                        }
                        Request::WorkspaceStatus(_) => {
                            ServerMessage::ok(id, &WorkspaceStatusResult { entries: vec![] })
                        }
                        Request::RepoDetectRunConfigs(_) => ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        ),
                        Request::RunList(_) => {
                            ServerMessage::ok(id, &RunListResult { runs: vec![] })
                        }
                        other => ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        ),
                    };
                    if w.write_all(codec::encode(&reply).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });

        (addr, journal)
    }

    /// The workspace the seam destroys. In error and with no agent of its own.
    fn gone() -> WorkspaceInfo {
        super::workspace(
            GONE,
            GONE_NAME,
            WorkspaceState::Error(REASON.to_owned()),
            &[],
        )
    }

    /// The workspace that is left: ready, with an ended agent and a session a
    /// restart would resume. Everything an automatic start needs except somebody
    /// asking for it.
    fn next() -> WorkspaceInfo {
        let mut info = super::workspace(NEXT, NEXT_NAME, WorkspaceState::Ready, &[]);
        let record = AgentSummary {
            id: AgentId(NEXT_AGENT.to_owned()),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Exited,
            session_id: Some(NEXT_SESSION.to_owned()),
            command: None,
            model: None,
            permission_mode: None,
        };
        info.agents = vec![record.id.clone()];
        info.agent_records = vec![record];
        info
    }
}

/// The CLI's own verdict about a dead login outranks the daemon's tick, until
/// the login has actually been redone.
///
/// The daemon's `claude_auth` check runs `claude auth status`, which reads the
/// credentials file and says logged in even when the refresh token in it has
/// been revoked. Only the CLI finds that out, at start, and it says so once --
/// in the exit detail of the agent it killed. The window re-checks on every
/// exit, and that check came back green: a tick on the setup page, an open
/// composer, and every prompt starting another agent that died the same way.
///
/// One Claude workspace with a live agent. The fake daemon lets the agent exit
/// with the CLI's sentence, answers every prerequisite check green, and answers
/// the Claude login terminal the seam opens with a PTY that exits two and a
/// half seconds later -- after the window's debounced re-check has been asked
/// and answered. What has to be visible from outside is that the gate shut on
/// the exit, stayed shut through that re-check, and reopened on the login's
/// exit.
mod claude_auth_override {
    use super::{drain, wait_for};
    use bondsymphonic_ide::model::persistence::{PersistedGroup, StateFile, STATE_VERSION};
    use bondsymphonic_proto::*;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    const TOKEN: &str = "auth-override-token";
    /// Nine seconds: the exit at one and a half, the re-check a second after
    /// it, the login's PTY exit two and a half seconds after the login was
    /// opened, and room after that for the last announcement to land.
    const SCRIPT: &str = "wait,wait,wait,quit";
    const WORKSPACE: &str = "ws_authov1";
    const NAME: &str = "auth-override";
    const AGENT: &str = "ag_authov_live";
    const SESSION: &str = "sess-authov-1";
    /// The PTY the fake hands back for the login terminal.
    const LOGIN_PTY: &str = "pty_authov_login";
    /// What the daemon put in the exit detail on the day: the CLI's sentence,
    /// with the daemon's exit code on the end of it.
    const DIED: &str =
        "Failed to authenticate: OAuth session expired and could not be refreshed (exit code 1)";
    /// The sentence the gate shows: the CLI's, without the daemon's suffix.
    const SENTENCE: &str =
        "Failed to authenticate: OAuth session expired and could not be refreshed";
    /// What the daemon puts beside a turn the server refused: the errored
    /// `result`'s own text. The CLI does not exit on a token the server
    /// refuses -- a revoked one, or a long-lived one gone bad -- it answers the
    /// turn with this and waits for the next message, so the agent's `error`
    /// state is the only place the failure shows up. Recorded from CLI 2.1.263
    /// with a well-shaped bogus `CLAUDE_CODE_OAUTH_TOKEN` on 2026-09-28.
    const REFUSED: &str = "Failed to authenticate. API Error: 401 OAuth access token is invalid.";
    /// The seam that announces the gate and opens the login; see `MainWindow`.
    const MENU_TEST: &str = "claude-gate";
    const GATE_CLOSED: &str = "BS_MENU_TEST claude-gate target=closed question=";
    const GATE_OPEN: &str = "BS_MENU_TEST claude-gate target=open question=";
    const CHECKED_CLOSED: &str = "BS_MENU_TEST claude-gate-checked target=closed question=";
    const CHECKED_OPEN: &str = "BS_MENU_TEST claude-gate-checked target=open question=";
    const LOGIN_DRIVEN: &str = "BS_MENU_TEST claude-login target=claude_login ";
    /// How long after the login terminal opens its process ends. Longer than
    /// the window's re-check debounce, so the re-check answers while the
    /// terminal is still up, which is the order the claim is about.
    const LOGIN_EXIT_DELAY: Duration = Duration::from_millis(2_500);
    const RUN_LIMIT: Duration = Duration::from_secs(120);

    type Journal = Arc<Mutex<Vec<String>>>;

    #[test]
    fn a_cli_auth_failure_shuts_the_gate_until_the_login_terminal_exits() {
        if bondsymphonic_ide::testing::skip_without_qt("CLI auth failure overrides the tick") {
            return;
        }
        assert_gate_follows_the_cli(AgentState::Exited, DIED, SENTENCE, true);
    }

    /// The same verdict when the agent does not die of it. A refused token
    /// leaves the CLI running with an errored turn, and the gate has to shut on
    /// that turn's detail just as it does on an exit's. No re-check is asserted
    /// here: the window asks for one on an exit, and this is not one.
    #[test]
    fn a_turn_the_server_refused_shuts_the_gate_until_the_login_terminal_exits() {
        if bondsymphonic_ide::testing::skip_without_qt(
            "CLI auth failure in a turn overrides the tick",
        ) {
            return;
        }
        assert_gate_follows_the_cli(AgentState::Error, REFUSED, REFUSED, false);
    }

    /// Runs the window against a daemon whose agent reports `state` with
    /// `detail`, and checks the gate shut with `sentence` on it, stayed shut
    /// through any green re-check, and reopened on the login terminal's exit.
    /// `exit_rechecks` is whether the report is one the window re-checks the
    /// prerequisites on.
    fn assert_gate_follows_the_cli(
        state: AgentState,
        detail: &'static str,
        sentence: &str,
        exit_rechecks: bool,
    ) {
        let run = run_ide(state, detail);
        let lines: Vec<&str> = run
            .out
            .lines()
            .filter(|l| l.starts_with("BS_MENU_TEST claude-"))
            .collect();

        // The exit shut the gate, and the sentence on it is the CLI's own.
        let closed = lines
            .iter()
            .position(|l| *l == format!("{GATE_CLOSED}{sentence}"))
            .unwrap_or_else(|| {
                panic!(
                    "the agent's {state:?} did not shut the gate with the CLI's sentence\n{}",
                    run.context
                )
            });
        // The seam answered it the way the setup page's button does.
        let login = lines
            .iter()
            .position(|l| l.starts_with(LOGIN_DRIVEN))
            .unwrap_or_else(|| panic!("the seam never opened the login terminal\n{}", run.context));
        assert!(
            login > closed,
            "the login was opened before the gate shut\n{}",
            run.context
        );
        assert!(
            run.journal.iter().any(|m| m == "system.setup_pty"),
            "the login terminal never reached the daemon\n{}",
            run.context
        );

        // A prerequisite check answered green after the login was opened and
        // before its terminal exited -- and the gate stayed shut through it.
        // This is the check that used to turn the cross back into a tick.
        let after_login = &lines[login + 1..];
        assert!(
            !after_login.iter().any(|l| l.starts_with(CHECKED_OPEN)),
            "a prerequisite check on its own reopened the gate\n{}",
            run.context
        );
        if !exit_rechecks {
            assert!(
                after_login.iter().any(|l| *l == GATE_OPEN),
                "the login terminal's exit did not reopen the gate\n{}",
                run.context
            );
            return;
        }
        let rechecked = after_login
            .iter()
            .position(|l| l.starts_with(CHECKED_CLOSED))
            .unwrap_or_else(|| {
                panic!(
                    "no prerequisite answer was published with the gate still shut after the \
                     login was opened\n{}",
                    run.context
                )
            });
        assert!(
            !after_login[..rechecked]
                .iter()
                .any(|l| l.starts_with(GATE_OPEN)),
            "the gate reopened before the login terminal had exited\n{}",
            run.context
        );
        assert!(
            run.journal
                .iter()
                .filter(|m| *m == "system.check_prereqs")
                .count()
                >= 2,
            "the exit never produced a re-check, so this run proves nothing\n{}",
            run.context
        );

        // The login terminal's exit is what lifts the verdict: the gate reopens
        // on the daemon's standing answer, with no sentence on it.
        assert!(
            after_login[rechecked..].iter().any(|l| *l == GATE_OPEN),
            "the login terminal's exit did not reopen the gate\n{}",
            run.context
        );
    }

    struct Run {
        out: String,
        journal: Vec<String>,
        context: String,
    }

    fn run_ide(state: AgentState, detail: &'static str) -> Run {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (addr, journal) = rt.block_on(fake_daemon(state, detail));

        // Per state: the two tests run in parallel in one process.
        let config =
            std::env::temp_dir().join(format!("bs-authov-{}-{state:?}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).expect("config dir");
        let state_path = config.join("state.json");
        let saved = StateFile {
            version: STATE_VERSION,
            groups: vec![PersistedGroup {
                name: "auth".to_owned(),
                workspace_ids: vec![WORKSPACE.to_owned()],
                ..PersistedGroup::default()
            }],
            active_workspace: Some(WORKSPACE.to_owned()),
            ..StateFile::default()
        };
        std::fs::write(
            &state_path,
            serde_json::to_string_pretty(&saved).expect("state json"),
        )
        .expect("seed state.json");

        let mut child = Command::new(env!("CARGO_BIN_EXE_bondsymphonic-ide"))
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("BS_DAEMON_ADDR", addr.to_string())
            .env("BS_DAEMON_TOKEN", TOKEN)
            .env("BS_SMOKE_SCRIPT", SCRIPT)
            .env("BS_MENU_TEST", MENU_TEST)
            .env("BS_SETTINGS_PATH", config.join("settings.json"))
            .env("BS_STATE_PATH", &state_path)
            .env("BS_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the IDE binary starts");

        let out = drain(child.stdout.take().expect("stdout is piped"));
        let err = drain(child.stderr.take().expect("stderr is piped"));
        let status = wait_for(&mut child, RUN_LIMIT);
        let (out, err) = (
            out.recv().expect("the stdout drain thread is alive"),
            err.recv().expect("the stderr drain thread is alive"),
        );
        let journal = journal.lock().expect("journal mutex").clone();
        let context =
            format!("requests: {journal:?}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
        let status = status
            .unwrap_or_else(|| panic!("the IDE did not exit within {RUN_LIMIT:?}\n{context}"));
        assert!(
            status.success(),
            "the IDE exited with {status}, expected 0\n{context}"
        );
        assert!(
            !format!("{out}{err}").contains("panicked at"),
            "the IDE logged a panic\n{context}"
        );
        let _ = std::fs::remove_dir_all(&config);
        Run {
            out,
            journal,
            context,
        }
    }

    async fn fake_daemon(
        state: AgentState,
        detail: &'static str,
    ) -> (std::net::SocketAddr, Journal) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let recorded = journal.clone();

        tokio::spawn(async move {
            let mut exit_sent = false;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (r, w) = stream.into_split();
                // Shared with the tasks that send the delayed events, so the
                // read loop never sleeps and the journal's order is the wire's.
                let w = Arc::new(tokio::sync::Mutex::new(w));
                let mut r = BufReader::new(r);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let ClientMessage::Request { id, request } =
                        codec::decode(line.trim_end()).expect("decode");
                    recorded
                        .lock()
                        .expect("journal mutex")
                        .push(request.method_name().to_owned());
                    let mut after: Vec<ServerMessage> = Vec::new();
                    let mut delay = Duration::from_millis(200);
                    let reply = match request {
                        Request::Hello(p) if p.token == TOKEN => Some(ServerMessage::ok(
                            id,
                            &HelloResult {
                                daemon_version: "0.0.0-fake".into(),
                                capabilities: Capabilities {
                                    backends: vec![],
                                    sandbox_backend: "noop".into(),
                                    git_protect: false,
                                    adapters: vec![AgentAdapterKind::Claude],
                                },
                                protocol_version: Some(PROTOCOL_VERSION),
                            },
                        )),
                        Request::Hello(_) => Some(ServerMessage::err(id, RpcError::unauthorized())),
                        // Green, every time: this is the daemon that cannot see
                        // the dead token, and the check that lied.
                        Request::SystemCheckPrereqs {} => Some(ServerMessage::ok(
                            id,
                            &CheckPrereqsResult {
                                items: vec![PrereqStatus {
                                    name: "claude_auth".into(),
                                    ok: true,
                                    detail: "logged in to Claude".into(),
                                    fix_hint: None,
                                }],
                            },
                        )),
                        Request::WorkspaceList {} => {
                            // The exit, once the tab exists to hear of it.
                            if !exit_sent {
                                exit_sent = true;
                                delay = Duration::from_millis(1_500);
                                after.push(ServerMessage::event(
                                    Some(WorkspaceId(WORKSPACE.to_owned())),
                                    Event::AgentStateChanged {
                                        agent_id: AgentId(AGENT.to_owned()),
                                        state,
                                        detail: Some(detail.to_owned()),
                                    },
                                ));
                            }
                            Some(ServerMessage::ok(
                                id,
                                &WorkspaceListResult {
                                    workspaces: vec![workspace()],
                                },
                            ))
                        }
                        Request::WorkspaceGet(_) => Some(ServerMessage::ok(id, &workspace())),
                        // The login terminal: a PTY that ends on its own a
                        // while later, which is what a login that was typed
                        // through looks like from here.
                        Request::SystemSetupPty(p) => {
                            assert_eq!(p.action, SetupAction::ClaudeLogin);
                            delay = LOGIN_EXIT_DELAY;
                            after.push(ServerMessage::event(
                                None,
                                Event::PtyExit {
                                    pty_id: PtyId(LOGIN_PTY.to_owned()),
                                    code: 0,
                                },
                            ));
                            Some(ServerMessage::ok(
                                id,
                                &PtyOpenResult {
                                    pty_id: PtyId(LOGIN_PTY.to_owned()),
                                },
                            ))
                        }
                        // Should the window start an agent after the exit, it
                        // gets one that lives: nothing here is about restarts.
                        Request::AgentStart(_) => {
                            after.push(ServerMessage::event(
                                Some(WorkspaceId(WORKSPACE.to_owned())),
                                Event::AgentStateChanged {
                                    agent_id: AgentId("ag_authov_new".to_owned()),
                                    state: AgentState::Idle,
                                    detail: None,
                                },
                            ));
                            Some(ServerMessage::ok(
                                id,
                                &AgentStartResult {
                                    agent_id: AgentId("ag_authov_new".to_owned()),
                                },
                            ))
                        }
                        Request::AgentHistory(_) => Some(ServerMessage::ok(
                            id,
                            &HistoryResult {
                                messages: vec![],
                                state: AgentState::Idle,
                                detail: None,
                            },
                        )),
                        Request::FsListDir(_) => {
                            Some(ServerMessage::ok(id, &ListDirResult { entries: vec![] }))
                        }
                        Request::FsWatch(_) => Some(ServerMessage::ok(id, &Empty {})),
                        Request::WorkspaceChanges(_) => {
                            Some(ServerMessage::ok(id, &ChangesResult { files: vec![] }))
                        }
                        Request::WorkspaceStatus(_) => Some(ServerMessage::ok(
                            id,
                            &WorkspaceStatusResult { entries: vec![] },
                        )),
                        Request::RepoDetectRunConfigs(_) => Some(ServerMessage::ok(
                            id,
                            &DetectRunConfigsResult {
                                configs: vec![],
                                network_allow: vec![],
                                warnings: vec![],
                            },
                        )),
                        Request::RunList(_) => {
                            Some(ServerMessage::ok(id, &RunListResult { runs: vec![] }))
                        }
                        other => Some(ServerMessage::err(
                            id,
                            RpcError::internal(format!("not implemented: {}", other.method_name())),
                        )),
                    };
                    let Some(reply) = reply else {
                        break;
                    };
                    if w.lock()
                        .await
                        .write_all(codec::encode(&reply).as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if after.is_empty() {
                        continue;
                    }
                    let w = w.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let mut w = w.lock().await;
                        for message in after {
                            if w.write_all(codec::encode(&message).as_bytes())
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                }
            }
        });

        (addr, journal)
    }

    /// The one workspace: ready, Claude, with a live agent that is about to
    /// die of its login.
    fn workspace() -> WorkspaceInfo {
        let mut info = super::workspace(WORKSPACE, NAME, WorkspaceState::Ready, &[]);
        let record = AgentSummary {
            id: AgentId(AGENT.to_owned()),
            adapter: AgentAdapterKind::Claude,
            state: AgentState::Idle,
            session_id: Some(SESSION.to_owned()),
            command: None,
            model: None,
            permission_mode: None,
        };
        info.agents = vec![record.id.clone()];
        info.agent_records = vec![record];
        info
    }
}
