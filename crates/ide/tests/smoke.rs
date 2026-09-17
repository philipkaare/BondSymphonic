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
            "sandbox-banner,sandbox-retry",
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
    }

    #[test]
    fn a_retry_the_daemon_refuses_shows_the_new_reason() {
        if bondsymphonic_ide::testing::skip_without_qt("sandbox retry refused") {
            return;
        }
        let run = run_ide(
            WorkspaceState::Error(RESTORE_REASON.to_owned()),
            Restart::Refused,
            "sandbox-banner,sandbox-retry",
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
            "sandbox-banner,sandbox-remove",
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
