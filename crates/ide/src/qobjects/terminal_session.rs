//! One PTY plus the terminal screen it drives.
//!
//! The session owns a [`TerminalGrid`] on the Qt side and a pump task on the
//! tokio side. The pump decodes `pty.output` events, collects the bytes for up
//! to [`BATCH_WINDOW`], and then queues a single closure onto the Qt thread
//! that feeds the grid, refreshes the paint properties and emits `frame`. At
//! most one such closure is ever outstanding (the `frame_pending` flag), so a
//! chatty PTY costs the UI one repaint per window rather than one per event.
//!
//! Everything the widget paints is a stored property: `rows_json` is
//! regenerated inside that closure, never on a property read.

use crate::client::router::{EventRouter, EventRx, Release};
use crate::model::terminal_grid::{
    key_to_bytes, parse_hex_rgb, paste_bytes, Appearance, TerminalGrid,
};
use crate::qobjects::app_controller::{on_reconnect, require_connection, runtime, shared};
use base64::Engine as _;
use bondsymphonic_proto::{
    Event, PtyId, PtyIdParams, PtyOpenParams, PtyOpenResult, PtyResizeParams, PtyWriteParams,
    Request, WorkspaceId,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `rows_json` is exposed as `getRowsJson`/`rowsJsonChanged` and
    // `scroll_to_bottom` as `scrollToBottom`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        // `pty_id` is empty until `pty.open` has answered; `rows_json` holds
        // the visible rows (an array of `Row`) and is regenerated once per
        // frame; `title` is what the application set with OSC 0/2; `error` is
        // the last failure, set together with a `frame`.
        #[qobject]
        #[qproperty(QString, pty_id)]
        #[qproperty(QString, workspace_id)]
        #[qproperty(QString, rows_json)]
        #[qproperty(i32, cursor_col)]
        #[qproperty(i32, cursor_row)]
        #[qproperty(bool, cursor_visible)]
        #[qproperty(i32, cols)]
        #[qproperty(i32, rows)]
        #[qproperty(bool, exited)]
        #[qproperty(i32, exit_code)]
        #[qproperty(QString, title)]
        #[qproperty(QString, error)]
        type TerminalSession = super::TerminalSessionRust;

        /// A batch of output has been applied: repaint.
        #[qsignal]
        fn frame(self: Pin<&mut TerminalSession>);

        /// `pty.open` succeeded and `pty_id` is now set.
        #[qsignal]
        fn opened(self: Pin<&mut TerminalSession>);

        /// The PTY exited; `exited` and `exit_code` are set.
        #[qsignal]
        fn exited_signal(self: Pin<&mut TerminalSession>);

        /// The output stream carried a Claude or GitHub login URL. Emitted at
        /// most once per distinct URL for the life of the session, so a link
        /// the shell echoes back does not open a second browser tab.
        #[qsignal]
        fn link_detected(self: Pin<&mut TerminalSession>, url: QString);

        /// Opens a PTY of `cols` x `rows` in `workspace_id`. An empty
        /// `command` runs the daemon default shell. Answers with `opened`, or
        /// sets `error` and emits `frame` on failure.
        #[qinvokable]
        fn open(
            self: Pin<&mut TerminalSession>,
            workspace_id: QString,
            cols: i32,
            rows: i32,
            command: QString,
        );

        /// Adopts a PTY somebody else opened -- the host terminal
        /// `system.setup_pty` answers with -- and drives it exactly as `open`
        /// does from that point on: subscribes to its events, resizes it to
        /// `cols` x `rows`, and answers with `opened`.
        ///
        /// A host terminal belongs to no workspace, so `workspace_id` is left
        /// empty: its events carry no workspace and are routed by PTY id.
        #[qinvokable]
        fn attach(self: Pin<&mut TerminalSession>, pty_id: QString, cols: i32, rows: i32);

        /// Pastes `text` into the PTY: the clipboard, or any other block of
        /// text the user did not type a key at a time.
        ///
        /// Not the keys that would have produced it. `paste_bytes` is what
        /// turns the text into what a program reading the terminal expects a
        /// paste to look like.
        #[qinvokable]
        fn paste(self: Pin<&mut TerminalSession>, text: QString);

        /// Sends one key press: `qt_key` is a `Qt::Key`, `modifiers` the
        /// `Qt::KeyboardModifiers` bits, `text` the event text.
        #[qinvokable]
        fn write_key(self: Pin<&mut TerminalSession>, qt_key: i32, modifiers: i32, text: QString);

        /// Tells the terminal what the widget paints with: the default
        /// foreground and background as `#rrggbb`, and the pixel size of one
        /// cell. Unparseable colours and zero sizes leave the current answer
        /// alone.
        ///
        /// A program is allowed to ask any of this -- a CLI asks the
        /// background colour to decide whether it is on a light or a dark
        /// terminal -- and the grid has no other way to know: the pane's
        /// colours come from the Qt palette, which follows the desktop.
        /// Remembered across a `reopen`, because the widget is the same one.
        #[qinvokable]
        fn set_appearance(
            self: Pin<&mut TerminalSession>,
            fg: QString,
            bg: QString,
            cell_width: i32,
            cell_height: i32,
        );

        /// Starts a selection at the cell `col`, `row` of the visible screen.
        /// `right_half` says which side of that cell the pointer is on, and
        /// `word` picks out the word under it -- what a double-click does.
        #[qinvokable]
        fn begin_selection(
            self: Pin<&mut TerminalSession>,
            col: i32,
            row: i32,
            right_half: bool,
            word: bool,
        );

        /// Drags the loose end of the selection to `col`, `row`.
        #[qinvokable]
        fn extend_selection(self: Pin<&mut TerminalSession>, col: i32, row: i32, right_half: bool);

        /// Drops the selection.
        #[qinvokable]
        fn clear_selection(self: Pin<&mut TerminalSession>);

        /// The selected text, empty when nothing is selected. What Copy puts
        /// on the clipboard.
        #[qinvokable]
        fn selection_text(self: &TerminalSession) -> QString;

        /// Whether anything is selected, which is what a Copy that is about to
        /// be offered needs to know.
        #[qinvokable]
        fn has_selection(self: &TerminalSession) -> bool;

        /// Resizes the screen now and tells the daemon. After the process has
        /// exited only the screen is resized.
        #[qinvokable]
        fn resize(self: Pin<&mut TerminalSession>, cols: i32, rows: i32);

        /// Scrolls the viewport; positive `delta` moves towards history.
        #[qinvokable]
        fn scroll(self: Pin<&mut TerminalSession>, delta: i32);

        #[qinvokable]
        fn scroll_to_bottom(self: Pin<&mut TerminalSession>);

        /// Asks the daemon to close the PTY. The session tears itself down
        /// when the resulting `pty.exit` arrives. A no-op once the process has
        /// exited, beyond releasing the router subscription: there is no PTY
        /// left to close.
        #[qinvokable]
        fn close(self: Pin<&mut TerminalSession>);

        /// Writes the "output dropped" marker into the screen, for when the
        /// daemon reports that it discarded events.
        #[qinvokable]
        fn note_output_dropped(self: Pin<&mut TerminalSession>);

        /// Opens a new PTY with the workspace, command and size the last
        /// `open` used.
        ///
        /// This is the answer to the `[daemon restarted]` state: a PTY does
        /// not survive a daemon restart -- the process inside it was in a
        /// sandbox that is gone -- so the session cannot resume, only start
        /// again. The scrollback of the old one is deliberately dropped with
        /// it: it describes a shell that no longer exists.
        ///
        /// Refused, with the reason on `error`, for a session that was
        /// `attach`ed rather than opened: a setup terminal on the host belongs
        /// to a `system.setup_pty` call, and re-running that is the setup
        /// page's decision, not this object's.
        #[qinvokable]
        fn reopen(self: Pin<&mut TerminalSession>);

        /// The exit line a PTY lost to a daemon restart leaves on the screen.
        ///
        /// Exposed so the pane can recognise that state -- and offer Reopen for
        /// it, rather than for an ordinary `exit` the user typed -- without
        /// spelling the marker a second time in C++, where it could drift from
        /// the one written here.
        #[qinvokable]
        fn restart_marker(self: &TerminalSession) -> QString;
    }

    impl cxx_qt::Threading for TerminalSession {}
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt::Threading;
use cxx_qt_lib::QString;

type QtHandle = cxx_qt::CxxQtThread<qobject::TerminalSession>;

/// Ends this session's router subscription. Held so that closing the widget,
/// re-opening the session, or dropping the object all release it.
type Unsubscribe = Box<dyn FnOnce() + Send>;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// How long the pump collects output before painting it. One frame at 60 Hz:
/// long enough to coalesce a burst, short enough to feel immediate.
const BATCH_WINDOW: Duration = Duration::from_millis(16);

/// The banner written into the screen when the daemon drops events.
const DROP_MARKER: &str = "[output dropped]";

/// The exit line written into the screen when the daemon the PTY lived in was
/// restarted. A PTY cannot survive that -- the process was in a sandbox the
/// new daemon has not got -- so this is the session's last word, and `reopen`
/// is the only way forward from it.
pub const RESTART_MARKER: &str = "[daemon restarted]";

const DEFAULT_COLS: i32 = 80;
const DEFAULT_ROWS: i32 = 24;

/// The only URLs the IDE will open a browser for, all of them printed by a
/// setup terminal that is waiting for a login to happen elsewhere.
///
/// A deliberately closed list. The output of a terminal is whatever the program
/// inside it chose to print, so anything wider would let a repository's build
/// script open a page on the developer's desktop by writing a link.
///
/// Both Claude hosts are here because the CLI prints `claude.com` and the
/// documentation says `claude.ai`. Knowing only the second is what left the
/// first real login with a browser that never opened. The trailing slash is
/// part of each prefix, so a look-alike host -- `claude.com.evil.example` --
/// is not a match.
const LINK_PREFIXES: [&str; 3] = [
    "https://claude.ai/",
    "https://claude.com/",
    "https://github.com/login/device",
];

/// How much of the output stream a [`LinkScanner`] keeps, so that a URL split
/// across two `pty.output` events is still seen whole. One kibibyte is several
/// times the longest login URL either CLI prints.
pub const TAIL_BYTES: usize = 1024;

/// Punctuation that ends a sentence rather than a URL.
const TRAILING_PUNCTUATION: [char; 5] = ['.', ',', ')', '\'', '"'];

/// How many distinct URLs a [`LinkScanner`] remembers before it starts over.
///
/// The scanner runs on every terminal, not only the setup one, and a program
/// inside a workspace shell chooses what it prints. A login flow shows one or
/// two links, so this is far beyond any honest use and only bounds what a
/// dishonest one can make the IDE hold on to.
const SEEN_LIMIT: usize = 256;

/// Where a URL stops: whitespace, a quote, or any control character.
///
/// The control characters matter as much as the spaces. Both CLIs wrap their
/// link in an OSC 8 hyperlink, whose terminator is an ESC, so a scanner that
/// only stopped at whitespace would hand the browser a URL with an escape
/// sequence glued to the end of it.
fn is_url_boundary(c: char) -> bool {
    c.is_whitespace() || c.is_control() || c == '"' || c == '\''
}

/// Every login URL in `tail`, in the order they appear, without repeats.
///
/// A URL runs from one of [`LINK_PREFIXES`] to the next boundary, minus any
/// trailing sentence punctuation. The end of the string counts as a boundary,
/// which is why this is the *complete-buffer* scan: [`LinkScanner`] decides
/// which part of a growing stream is complete before calling it.
pub fn find_links(tail: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut from = 0usize;
    while from < tail.len() {
        let Some(start) = LINK_PREFIXES
            .iter()
            .filter_map(|prefix| tail[from..].find(prefix).map(|off| from + off))
            .min()
        else {
            break;
        };
        let end = tail[start..]
            .find(is_url_boundary)
            .map_or(tail.len(), |off| start + off);
        let url = tail[start..end].trim_end_matches(TRAILING_PUNCTUATION);
        if !url.is_empty() && !found.iter().any(|seen| seen == url) {
            found.push(url.to_owned());
        }
        // `end` is past `start` because a prefix begins with a non-boundary
        // character, so the walk always advances.
        from = end;
    }
    found
}

/// Finds login URLs in a stream that arrives in arbitrary chunks.
///
/// Two things make this more than a call to [`find_links`] per chunk. The
/// scanner keeps the last [`TAIL_BYTES`] of the stream, so a URL split down the
/// middle by the daemon's batching is still seen whole; and it remembers what
/// it has already reported, so the tail it re-scans, and any echo of the link
/// later in the session, do not open a second browser tab.
///
/// A URL that reaches the end of the buffer with nothing after it is held back
/// rather than reported: the rest of it may still be arriving, and half a URL
/// is worse than a moment's delay. [`LinkScanner::flush`] releases whatever is
/// left when the terminal exits and no more can arrive.
pub struct LinkScanner {
    tail: Vec<u8>,
    seen: std::collections::HashSet<String>,
}

impl Default for LinkScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkScanner {
    pub fn new() -> Self {
        Self {
            tail: Vec::new(),
            seen: std::collections::HashSet::new(),
        }
    }

    /// Feeds one decoded `pty.output` chunk; answers with the URLs that are
    /// new. Bytes rather than text: a chunk boundary can fall inside a
    /// multi-byte character, and joining the pieces here is what keeps the
    /// stream intact. URLs are ASCII, so the lossy decode used to scan cannot
    /// damage one.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.tail.extend_from_slice(chunk);
        let candidates = {
            let text = String::from_utf8_lossy(&self.tail);
            match text.rfind(is_url_boundary) {
                // Everything up to and including the last boundary is settled;
                // whatever follows it may still be growing.
                Some(idx) => {
                    let width = text[idx..].chars().next().map_or(1, char::len_utf8);
                    find_links(&text[..idx + width])
                }
                None => Vec::new(),
            }
        };
        let fresh = self.take_unseen(candidates);
        if self.tail.len() > TAIL_BYTES {
            let excess = self.tail.len() - TAIL_BYTES;
            self.tail.drain(..excess);
        }
        fresh
    }

    /// Reports whatever is left in the tail, terminated or not. Called when the
    /// PTY exits: nothing more is coming, so a URL still sitting at the end of
    /// the buffer is as complete as it will ever be.
    pub fn flush(&mut self) -> Vec<String> {
        let candidates = find_links(&String::from_utf8_lossy(&self.tail));
        self.take_unseen(candidates)
    }

    /// How much of the stream is being kept. Exists for the test that pins the
    /// bound; nothing in the IDE reads it.
    pub fn tail_len(&self) -> usize {
        self.tail.len()
    }

    fn take_unseen(&mut self, candidates: Vec<String>) -> Vec<String> {
        if self.seen.len() >= SEEN_LIMIT {
            // Starting over rather than refusing to report: forgetting can
            // cost one repeated link, refusing would cost a real login.
            self.seen.clear();
        }
        candidates
            .into_iter()
            .filter(|url| self.seen.insert(url.clone()))
            .collect()
    }
}

pub struct TerminalSessionRust {
    pty_id: QString,
    workspace_id: QString,
    rows_json: QString,
    cursor_col: i32,
    cursor_row: i32,
    cursor_visible: bool,
    cols: i32,
    rows: i32,
    exited: bool,
    exit_code: i32,
    title: QString,
    error: QString,
    /// The screen. Absent until `open` creates it at the requested size.
    grid: Option<TerminalGrid>,
    /// Set when `close` runs before `pty.open` has answered: the reply then
    /// closes the PTY it just learned about instead of adopting it.
    close_requested: bool,
    /// True while a frame closure is queued but not yet applied. Owned here
    /// and cloned into the pump so the two agree on how many frames are in
    /// flight; replaced on every `open` so an older pump cannot gate the new
    /// session's frames.
    frame_pending: Arc<AtomicBool>,
    unsubscribe: Option<Unsubscribe>,
    /// What `open` was last called with, or `None` for a session that was
    /// `attach`ed to somebody else's PTY. This is what `reopen` re-runs, and
    /// its absence is what makes `reopen` refuse.
    last_open: Option<OpenRequest>,
    /// Waits for the connection generation to move, and marks the session
    /// restarted when it does. Replaced on every `open`/`attach`, aborted on
    /// Drop.
    reconnect_task: Option<tokio::task::JoinHandle<()>>,
    /// What the widget paints with, for the programs that ask. Kept here as
    /// well as on the grid because `begin` builds a new grid for every PTY
    /// while the widget in front of it has not changed.
    appearance: Appearance,
}

/// The arguments of an `open`, kept so `reopen` can repeat it.
#[derive(Clone)]
struct OpenRequest {
    workspace_id: String,
    command: String,
}

impl Default for TerminalSessionRust {
    fn default() -> Self {
        Self {
            pty_id: QString::from(""),
            workspace_id: QString::from(""),
            rows_json: QString::from("[]"),
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: false,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            exited: false,
            exit_code: 0,
            title: QString::from(""),
            error: QString::from(""),
            grid: None,
            close_requested: false,
            frame_pending: Arc::new(AtomicBool::new(false)),
            unsubscribe: None,
            last_open: None,
            reconnect_task: None,
            appearance: Appearance::default(),
        }
    }
}

impl Drop for TerminalSessionRust {
    fn drop(&mut self) {
        // The widget went away. `Drop` runs on the Rust struct, not the
        // QObject, so it repeats what `teardown` does rather than calling it.
        if let Some(task) = self.reconnect_task.take() {
            task.abort();
        }
        if let Some(unsubscribe) = self.unsubscribe.take() {
            unsubscribe();
        }
        if !self.exited {
            close_pty(self.pty_id.to_string());
        }
    }
}

/// Asks the daemon to close a PTY and forgets about it. Used when a session
/// goes away without anyone calling `close`: a destroyed pane, or a session
/// re-opened onto a new PTY. Without this the shell keeps running in the
/// sandbox with nothing reading it.
fn close_pty(pty_id: String) {
    if pty_id.is_empty() {
        return;
    }
    let Some(shared) = shared() else {
        return;
    };
    runtime().spawn(async move {
        let params = PtyIdParams {
            pty_id: PtyId(pty_id.clone()),
        };
        if let Err(e) = shared.client.request_raw(Request::PtyClose(params)).await {
            tracing::warn!("pty.close for {pty_id} during teardown failed: {e}");
        }
    });
}

/// A widget's cell coordinates as the grid takes them. Negative is what a
/// drag above or left of the pane reports, and it means the first cell.
fn cell_at(col: i32, row: i32) -> (u16, u16) {
    (
        col.clamp(0, u16::MAX as i32) as u16,
        row.clamp(0, u16::MAX as i32) as u16,
    )
}

/// Clamps a requested size into what a terminal grid accepts.
fn clamp_size(cols: i32, rows: i32) -> (u16, u16) {
    (
        cols.clamp(2, u16::MAX as i32) as u16,
        rows.clamp(1, u16::MAX as i32) as u16,
    )
}

/// What happened to a batch of output the pump tried to hand to the Qt thread.
enum Flush {
    /// Queued; the batch is now the Qt thread's problem.
    Applied,
    /// A frame is still pending, so the bytes stayed in the batch.
    Deferred,
    /// The QObject is gone; nothing more can be delivered.
    Gone,
}

/// Queues one frame closure carrying `batch`, unless the previous one has not
/// been applied yet. Never blocks: deferring costs one more [`BATCH_WINDOW`],
/// and the bytes stay in order because the batch is only ever appended to.
fn flush(qt: &QtHandle, pending: &Arc<AtomicBool>, id: &str, batch: &mut Vec<u8>) -> Flush {
    if batch.is_empty() {
        return Flush::Applied;
    }
    if pending
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Flush::Deferred;
    }
    let bytes = std::mem::take(batch);
    let flag = pending.clone();
    let id = id.to_owned();
    let queued = qt.queue(move |mut q| {
        flag.store(false, Ordering::Release);
        // A session that has since been re-opened must not be fed the old
        // PTY's output.
        if q.pty_id().to_string() != id {
            return;
        }
        q.as_mut().feed(&bytes);
        q.apply_frame();
    });
    match queued {
        Ok(()) => Flush::Applied,
        Err(_) => {
            pending.store(false, Ordering::Release);
            Flush::Gone
        }
    }
}

/// Drains one PTY's events until it exits or the subscription ends, batching
/// output into at most one queued frame at a time.
async fn pump(
    mut rx: EventRx,
    pty_id: PtyId,
    release: Release,
    qt: QtHandle,
    pending: Arc<AtomicBool>,
) {
    let id = pty_id.to_string();
    let mut batch: Vec<u8> = Vec::new();
    let mut batch_started: Option<Instant> = None;
    let mut exit: Option<i32> = None;
    let mut gone = false;
    // Scanned here rather than in the frame closure: a login URL is news the
    // moment it arrives, and the Qt thread has enough to do painting.
    let mut links = LinkScanner::new();

    loop {
        let received = match batch_started {
            Some(started) => {
                let elapsed = started.elapsed();
                if elapsed >= BATCH_WINDOW {
                    match flush(&qt, &pending, &id, &mut batch) {
                        Flush::Applied => batch_started = None,
                        // Try again a window later rather than spinning on the
                        // flag: the Qt thread is behind, not stuck.
                        Flush::Deferred => batch_started = Some(Instant::now()),
                        Flush::Gone => {
                            gone = true;
                            break;
                        }
                    }
                    continue;
                }
                match tokio::time::timeout(BATCH_WINDOW - elapsed, rx.recv()).await {
                    Ok(received) => received,
                    // Window elapsed; the branch above flushes on the next lap.
                    Err(_) => continue,
                }
            }
            None => rx.recv().await,
        };
        // `None` means the subscription was replaced or ended.
        let Some((_, event)) = received else { break };
        match event {
            Event::PtyOutput { data_b64, .. } => {
                match BASE64.decode(data_b64.as_bytes()) {
                    Ok(bytes) => {
                        for url in links.push(&bytes) {
                            tracing::info!("login link in terminal output: {url}");
                            let _ = qt.queue(move |q| q.link_detected(QString::from(&url)));
                        }
                        batch.extend_from_slice(&bytes);
                    }
                    Err(e) => tracing::warn!("pty.output: undecodable base64: {e}"),
                }
                if batch_started.is_none() {
                    batch_started = Some(Instant::now());
                }
            }
            Event::PtyExit { code, .. } => {
                exit = Some(code);
                break;
            }
            _ => {}
        }
    }

    // This session's own subscription and no other: the loop above may have
    // ended precisely because another session took this PTY's key, and by key
    // this line would then close the live one.
    release.release();
    if gone {
        return;
    }
    // Nothing more can arrive, so a URL still sitting unterminated at the end
    // of the tail is complete. Queued before the closure below, so the link is
    // acted on before the pane reports the process gone.
    for url in links.flush() {
        tracing::info!("login link in terminal output: {url}");
        let _ = qt.queue(move |q| q.link_detected(QString::from(&url)));
    }
    // The last frame carries whatever is left plus the exit state, so the
    // final output is painted before the pane is marked dead. Queued directly
    // rather than through `flush`: the Qt thread applies closures in order, so
    // this still lands after any frame already queued.
    let _ = qt.queue(move |mut q| {
        if q.pty_id().to_string() != id {
            return;
        }
        if !batch.is_empty() {
            q.as_mut().feed(&batch);
        }
        // The subscription is already gone; forget the handle so `close` and
        // `Drop` do not run it a second time.
        q.as_mut().rust_mut().unsubscribe = None;
        if let Some(code) = exit {
            q.as_mut().set_exited(true);
            q.as_mut().set_exit_code(code);
            // The daemon has reaped this PTY, so its id no longer names
            // anything. Clearing it is what makes every later `resize`,
            // `write` and `close` a local no-op instead of a request the
            // daemon answers with `NotFound`, which would raise an error
            // banner over a pane whose only news is that the process ended.
            q.as_mut().set_pty_id(QString::from(""));
        }
        q.as_mut().apply_frame();
        if exit.is_some() {
            q.exited_signal();
        }
    });
}

/// Publishes a PTY the caller has already subscribed to, then pumps it until
/// it exits. The half of `open` and `attach` that is the same terminal either
/// way: from the moment an id exists, where it came from stops mattering.
async fn adopt(
    router: EventRouter,
    qt: QtHandle,
    pty_id: PtyId,
    rx: EventRx,
    release: Release,
    pending: Arc<AtomicBool>,
) {
    let unsubscribe = {
        let release = release.clone();
        move || release.release()
    };
    let id_text = pty_id.to_string();
    let queued = qt.queue(move |mut q| {
        // `close` ran while the id was in flight, so the session never learned
        // an id it could close. It exists now: close it here, rather than
        // leaving a shell running with nothing reading it.
        if q.as_ref().rust().close_requested {
            q.as_mut().rust_mut().close_requested = false;
            close_pty(id_text);
            return;
        }
        q.as_mut().set_pty_id(QString::from(&id_text));
        q.as_mut().rust_mut().unsubscribe = Some(Box::new(unsubscribe));
        q.opened();
    });
    if queued.is_err() {
        // The QObject went away before the id reached it: the same orphan,
        // from the other direction. The PTY itself is closed here, so the id is
        // being retired and taking everything on it -- parked output included
        // -- is what is wanted.
        router.unsubscribe_pty(&pty_id);
        close_pty(pty_id.to_string());
        return;
    }
    pump(rx, pty_id, release, qt, pending).await;
}

impl qobject::TerminalSession {
    pub fn open(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        cols: i32,
        rows: i32,
        command: QString,
    ) {
        let (cols, rows) = clamp_size(cols, rows);
        let pending = self.as_mut().begin(cols, rows);
        self.as_mut().set_workspace_id(workspace_id.clone());
        // Recorded before the request goes out, so a `reopen` works even for a
        // session whose first `pty.open` never answered.
        self.as_mut().rust_mut().last_open = Some(OpenRequest {
            workspace_id: workspace_id.to_string(),
            command: command.to_string(),
        });

        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.qt_thread();
        let workspace = workspace_id.to_string();
        let command = command.to_string();
        runtime().spawn(async move {
            let params = PtyOpenParams {
                workspace_id: WorkspaceId(workspace),
                cols,
                rows,
                command: (!command.is_empty()).then_some(command),
            };
            let pty_id = match shared
                .client
                .request::<PtyOpenResult>(Request::PtyOpen(params))
                .await
            {
                Ok(res) => res.pty_id,
                Err(e) => {
                    let message = format!("pty.open failed: {e}");
                    tracing::warn!("{message}");
                    let _ = qt.queue(move |q| q.fail(&message));
                    return;
                }
            };
            // Subscribing before the id reaches the Qt thread: the router
            // replays output that arrived while `pty.open` was in flight.
            let (rx, release) = shared.router.subscribe_pty(&pty_id);
            adopt(shared.router.clone(), qt, pty_id, rx, release, pending).await;
        });
    }

    pub fn attach(mut self: Pin<&mut Self>, pty_id: QString, cols: i32, rows: i32) {
        let id = pty_id.to_string();
        if id.is_empty() {
            self.fail("no terminal to attach to");
            return;
        }
        let (cols, rows) = clamp_size(cols, rows);
        let pending = self.as_mut().begin(cols, rows);
        // A host terminal belongs to no workspace. Left empty rather than
        // guessed at: its events carry no workspace id either, which is why
        // the router has to key this one by PTY id alone.
        self.as_mut().set_workspace_id(QString::from(""));
        // Nothing to reopen: the PTY was opened by whoever called `attach`,
        // and only they can decide to ask for another one.
        self.as_mut().rust_mut().last_open = None;

        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.qt_thread();
        runtime().spawn(async move {
            let pty_id = PtyId(id);
            // Subscribed before anything else, exactly as in `open`: the
            // daemon starts writing the moment the terminal opens, and the
            // router replays what arrived before this session existed.
            let (rx, release) = shared.router.subscribe_pty(&pty_id);
            // The terminal was opened at whatever size the request asked for.
            // This is where it becomes the size of the pane showing it.
            let params = PtyResizeParams {
                pty_id: pty_id.clone(),
                cols,
                rows,
            };
            if let Err(e) = shared.client.request_raw(Request::PtyResize(params)).await {
                report(&qt, format!("pty.resize failed: {e}"));
            }
            adopt(shared.router.clone(), qt, pty_id, rx, release, pending).await;
        });
    }

    pub fn paste(self: Pin<&mut Self>, text: QString) {
        // Whether the program wants the paste bracketed is the grid's to
        // answer: it is the half of the session that heard the program ask.
        let bracketed = self
            .rust()
            .grid
            .as_ref()
            .is_some_and(|grid| grid.bracketed_paste());
        let bytes = paste_bytes(&text.to_string(), bracketed);
        self.send(bytes);
    }

    pub fn set_appearance(
        mut self: Pin<&mut Self>,
        fg: QString,
        bg: QString,
        cell_width: i32,
        cell_height: i32,
    ) {
        let appearance = {
            let mut appearance = self.as_ref().rust().appearance;
            if let Some(rgb) = parse_hex_rgb(&fg.to_string()) {
                appearance.fg = rgb;
            }
            if let Some(rgb) = parse_hex_rgb(&bg.to_string()) {
                appearance.bg = rgb;
            }
            // Zero is what a widget that has not been laid out yet measures,
            // and a cell of no size is worse than the last honest answer.
            if cell_width > 0 {
                appearance.cell_width = cell_width.min(u16::MAX as i32) as u16;
            }
            if cell_height > 0 {
                appearance.cell_height = cell_height.min(u16::MAX as i32) as u16;
            }
            appearance
        };
        self.as_mut().rust_mut().appearance = appearance;
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.set_appearance(appearance);
        }
    }

    pub fn write_key(self: Pin<&mut Self>, qt_key: i32, modifiers: i32, text: QString) {
        let app_cursor = self
            .rust()
            .grid
            .as_ref()
            .is_some_and(|grid| grid.app_cursor_keys());
        let bytes = key_to_bytes(qt_key, modifiers as u32, &text.to_string(), app_cursor);
        self.send(bytes);
    }

    pub fn begin_selection(
        mut self: Pin<&mut Self>,
        col: i32,
        row: i32,
        right_half: bool,
        word: bool,
    ) {
        let (col, row) = cell_at(col, row);
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.begin_selection(col, row, right_half, word);
        }
        // The selection is part of what the widget paints, so moving it is a
        // frame like any other.
        self.apply_frame();
    }

    pub fn extend_selection(mut self: Pin<&mut Self>, col: i32, row: i32, right_half: bool) {
        let (col, row) = cell_at(col, row);
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.extend_selection(col, row, right_half);
        }
        self.apply_frame();
    }

    pub fn clear_selection(mut self: Pin<&mut Self>) {
        let had = self
            .as_ref()
            .rust()
            .grid
            .as_ref()
            .is_some_and(|grid| grid.has_selection());
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.clear_selection();
        }
        // Only when there was something to take away: every key press clears
        // the selection, and a repaint per keystroke over a terminal with
        // nothing selected is a repaint for nothing.
        if had {
            self.apply_frame();
        }
    }

    pub fn selection_text(&self) -> QString {
        QString::from(
            &self
                .rust()
                .grid
                .as_ref()
                .map(|grid| grid.selection_text())
                .unwrap_or_default(),
        )
    }

    pub fn has_selection(&self) -> bool {
        self.rust()
            .grid
            .as_ref()
            .is_some_and(|grid| grid.has_selection())
    }

    pub fn resize(mut self: Pin<&mut Self>, cols: i32, rows: i32) {
        let (cols, rows) = clamp_size(cols, rows);
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.resize(cols, rows);
        }
        self.as_mut().set_cols(i32::from(cols));
        self.as_mut().set_rows(i32::from(rows));
        // Repainted from the new geometry immediately; the daemon catches up.
        self.as_mut().apply_frame();

        // No PTY: either `pty.open` has not answered yet, or the process has
        // exited and the id was cleared. The screen still resizes; there is
        // just nothing on the daemon side left to tell.
        let pty_id = self.pty_id().to_string();
        if pty_id.is_empty() {
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.qt_thread();
        runtime().spawn(async move {
            let params = PtyResizeParams {
                pty_id: PtyId(pty_id),
                cols,
                rows,
            };
            if let Err(e) = shared.client.request_raw(Request::PtyResize(params)).await {
                report(&qt, format!("pty.resize failed: {e}"));
            }
        });
    }

    pub fn scroll(mut self: Pin<&mut Self>, delta: i32) {
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.scroll(delta);
        }
        self.apply_frame();
    }

    pub fn scroll_to_bottom(mut self: Pin<&mut Self>) {
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.scroll_to_bottom();
        }
        self.apply_frame();
    }

    pub fn close(mut self: Pin<&mut Self>) {
        let pty_id = self.pty_id().to_string();
        if pty_id.is_empty() {
            // Nothing to close: the process has already exited, or the id has
            // not arrived yet. Either way the router subscription goes, and a
            // reply still in flight is told to close the PTY it brings back.
            if let Some(unsubscribe) = self.as_mut().rust_mut().unsubscribe.take() {
                unsubscribe();
            }
            if !*self.exited() {
                self.as_mut().rust_mut().close_requested = true;
            }
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.qt_thread();
        runtime().spawn(async move {
            let params = PtyIdParams {
                pty_id: PtyId(pty_id),
            };
            // Teardown is left to the `pty.exit` this produces, so the last
            // output of the process is still painted.
            if let Err(e) = shared.client.request_raw(Request::PtyClose(params)).await {
                report(&qt, format!("pty.close failed: {e}"));
            }
        });
    }

    pub fn note_output_dropped(mut self: Pin<&mut Self>) {
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.insert_marker(DROP_MARKER);
        }
        self.apply_frame();
    }

    pub fn restart_marker(&self) -> QString {
        QString::from(RESTART_MARKER)
    }

    pub fn reopen(self: Pin<&mut Self>) {
        let Some(request) = self.as_ref().rust().last_open.clone() else {
            self.fail("this terminal was not opened by the IDE, so it cannot be reopened");
            return;
        };
        // The pane's current size, not the one the old PTY was opened at: the
        // widget may well have been resized while the daemon was away.
        let (cols, rows) = (*self.as_ref().cols(), *self.as_ref().rows());
        tracing::info!("reopening the terminal in {}", request.workspace_id);
        self.open(
            QString::from(&request.workspace_id),
            cols,
            rows,
            QString::from(&request.command),
        );
    }

    /// Reports the PTY gone because the daemon behind it was restarted.
    ///
    /// Queued by [`on_reconnect`]. Nothing is asked of the daemon: the old one
    /// is not there to answer and the new one has never heard of this PTY id,
    /// so a `pty.close` would come back `NotFound` and put an error banner over
    /// a pane whose only news is that its shell is gone. Clearing the id is
    /// what makes every later `write`, `resize` and `close` a local no-op, and
    /// marking it exited is what stops `Drop` from trying to close it.
    fn note_daemon_restarted(mut self: Pin<&mut Self>) {
        if *self.as_ref().exited() {
            // The process had already finished; there is nothing to report and
            // nothing to reopen from.
            return;
        }
        tracing::info!("terminal marked exited: the daemon was restarted");
        if let Some(unsubscribe) = self.as_mut().rust_mut().unsubscribe.take() {
            unsubscribe();
        }
        self.as_mut().set_pty_id(QString::from(""));
        self.as_mut().set_exited(true);
        self.as_mut().set_exit_code(0);
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.insert_marker(RESTART_MARKER);
        }
        self.as_mut().apply_frame();
        self.exited_signal();
    }

    /// Clears the session down to an empty `cols` x `rows` screen and hands
    /// back the frame flag the new pump is to use. Both `open` and `attach`
    /// start here, because a session that is being pointed at a second PTY has
    /// to let go of the first one first.
    fn begin(mut self: Pin<&mut Self>, cols: u16, rows: u16) -> Arc<AtomicBool> {
        // Re-opening: drop the previous subscription first so its pump ends.
        self.as_mut().teardown();
        self.as_mut().set_pty_id(QString::from(""));
        self.as_mut().set_cols(i32::from(cols));
        self.as_mut().set_rows(i32::from(rows));
        self.as_mut().set_exited(false);
        self.as_mut().set_exit_code(0);
        self.as_mut().set_error(QString::from(""));
        {
            let mut rust = self.as_mut().rust_mut();
            rust.close_requested = false;
            let mut grid = TerminalGrid::new(cols, rows);
            // The widget in front of this session has not changed, so neither
            // has what it paints with.
            grid.set_appearance(rust.appearance);
            rust.grid = Some(grid);
            // A fresh flag: any closure still queued by an older pump clears
            // that pump's flag, not this session's.
            rust.frame_pending = Arc::new(AtomicBool::new(false));
            if let Some(previous) = rust.reconnect_task.take() {
                previous.abort();
            }
        }
        // A PTY does not survive a daemon restart, so every session watches for
        // one from the moment it starts -- before `pty.open` has even answered,
        // because a restart in that window leaves the same dead pane behind.
        {
            let qt = self.as_ref().qt_thread();
            let watch = on_reconnect(qt, qobject::TerminalSession::note_daemon_restarted);
            self.as_mut().rust_mut().reconnect_task = Some(watch);
        }
        let pending = self.as_ref().rust().frame_pending.clone();
        self.apply_frame();
        pending
    }

    /// Feeds PTY bytes into the screen, and answers anything the program asked
    /// for on the way past. No-op before `open`.
    ///
    /// The answers go out here rather than on the next frame because they are
    /// not for the user to look at: a program that has asked where the cursor
    /// is, or what the terminal is, has stopped reading its input until the
    /// report arrives. `gh auth login` does exactly that before each of its
    /// yes/no prompts, and a terminal that never replies leaves the question on
    /// screen refusing every keystroke.
    fn feed(mut self: Pin<&mut Self>, bytes: &[u8]) {
        let replies = match self.as_mut().rust_mut().grid.as_mut() {
            Some(grid) => {
                grid.feed(bytes);
                grid.take_replies()
            }
            None => return,
        };
        self.send(replies);
    }

    /// Regenerates every property the widget paints from, then asks for a
    /// repaint. This is the only place `rows_json` is produced.
    fn apply_frame(mut self: Pin<&mut Self>) {
        let snapshot = self.as_ref().rust().grid.as_ref().map(|grid| {
            let (col, row, visible) = grid.cursor();
            (
                grid.rows_json(),
                i32::from(col),
                i32::from(row),
                visible,
                grid.title().unwrap_or_default(),
            )
        });
        if let Some((rows_json, col, row, visible, title)) = snapshot {
            self.as_mut().set_rows_json(QString::from(&rows_json));
            self.as_mut().set_cursor_col(col);
            self.as_mut().set_cursor_row(row);
            self.as_mut().set_cursor_visible(visible);
            self.as_mut().set_title(QString::from(&title));
        }
        self.frame();
    }

    /// Records an error and repaints, which is how every failure reaches the
    /// widget: there is no separate error signal to connect.
    fn fail(mut self: Pin<&mut Self>, message: &str) {
        self.as_mut().set_error(QString::from(message));
        self.frame();
    }

    /// Sends bytes to the PTY. Input before `pty.open` has answered, or after
    /// the process has exited, is dropped: there is nowhere to put it.
    fn send(self: Pin<&mut Self>, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let pty_id = self.pty_id().to_string();
        if pty_id.is_empty() {
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.fail(message);
                return;
            }
        };
        let qt = self.qt_thread();
        runtime().spawn(async move {
            let params = PtyWriteParams {
                pty_id: PtyId(pty_id),
                data_b64: BASE64.encode(bytes),
            };
            if let Err(e) = shared.client.request_raw(Request::PtyWrite(params)).await {
                report(&qt, format!("pty.write failed: {e}"));
            }
        });
    }

    /// Ends the router subscription and closes the PTY behind it, if this
    /// session still holds one that has not exited. Called before `open`
    /// replaces the session, and mirrored by `Drop` when the pane is
    /// destroyed, so a shell never outlives the thing that was showing it.
    fn teardown(mut self: Pin<&mut Self>) {
        if let Some(unsubscribe) = self.as_mut().rust_mut().unsubscribe.take() {
            unsubscribe();
        }
        if !*self.exited() {
            close_pty(self.pty_id().to_string());
        }
        self.set_pty_id(QString::from(""));
    }
}

/// Surfaces a tokio-side failure on the session's `error` property.
fn report(qt: &QtHandle, message: String) {
    tracing::warn!("{message}");
    let _ = qt.queue(move |q| q.fail(&message));
}
