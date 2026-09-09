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

use crate::client::router::{EventRouter, EventRx};
use crate::model::terminal_grid::{key_to_bytes, TerminalGrid};
use crate::qobjects::app_controller::{require_connection, runtime, shared};
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

        /// Sends text (a paste, or composed input) to the PTY.
        #[qinvokable]
        fn write_text(self: Pin<&mut TerminalSession>, text: QString);

        /// Sends one key press: `qt_key` is a `Qt::Key`, `modifiers` the
        /// `Qt::KeyboardModifiers` bits, `text` the event text.
        #[qinvokable]
        fn write_key(self: Pin<&mut TerminalSession>, qt_key: i32, modifiers: i32, text: QString);

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

const DEFAULT_COLS: i32 = 80;
const DEFAULT_ROWS: i32 = 24;

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
        }
    }
}

impl Drop for TerminalSessionRust {
    fn drop(&mut self) {
        // The widget went away. `Drop` runs on the Rust struct, not the
        // QObject, so it repeats what `teardown` does rather than calling it.
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
    router: EventRouter,
    qt: QtHandle,
    pending: Arc<AtomicBool>,
) {
    let id = pty_id.to_string();
    let mut batch: Vec<u8> = Vec::new();
    let mut batch_started: Option<Instant> = None;
    let mut exit: Option<i32> = None;
    let mut gone = false;

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
                    Ok(bytes) => batch.extend_from_slice(&bytes),
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

    router.unsubscribe_pty(&pty_id);
    if gone {
        return;
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

impl qobject::TerminalSession {
    pub fn open(
        mut self: Pin<&mut Self>,
        workspace_id: QString,
        cols: i32,
        rows: i32,
        command: QString,
    ) {
        let (cols, rows) = clamp_size(cols, rows);
        // Re-opening: drop the previous subscription first so its pump ends.
        self.as_mut().teardown();
        self.as_mut().set_pty_id(QString::from(""));
        self.as_mut().set_workspace_id(workspace_id.clone());
        self.as_mut().set_cols(i32::from(cols));
        self.as_mut().set_rows(i32::from(rows));
        self.as_mut().set_exited(false);
        self.as_mut().set_exit_code(0);
        self.as_mut().set_error(QString::from(""));
        {
            let mut rust = self.as_mut().rust_mut();
            rust.close_requested = false;
            rust.grid = Some(TerminalGrid::new(cols, rows));
            // A fresh flag: any closure still queued by an older pump clears
            // that pump's flag, not this session's.
            rust.frame_pending = Arc::new(AtomicBool::new(false));
        }
        let pending = self.as_ref().rust().frame_pending.clone();
        self.as_mut().apply_frame();

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
            let rx = shared.router.subscribe_pty(&pty_id);
            let router = shared.router.clone();
            let unsubscribe = {
                let router = router.clone();
                let pty_id = pty_id.clone();
                move || router.unsubscribe_pty(&pty_id)
            };
            let id_text = pty_id.to_string();
            let queued = qt.queue(move |mut q| {
                // `close` ran while this `pty.open` was in flight, so the
                // session never learned an id it could close. It exists now:
                // close it here, rather than leaving a shell running in the
                // sandbox with nothing reading it.
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
                // The QObject went away before the id reached it: the same
                // orphan, from the other direction.
                router.unsubscribe_pty(&pty_id);
                close_pty(pty_id.to_string());
                return;
            }
            pump(rx, pty_id, router, qt, pending).await;
        });
    }

    pub fn write_text(self: Pin<&mut Self>, text: QString) {
        let bytes = text.to_string().into_bytes();
        self.send(bytes);
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

    /// Feeds PTY bytes into the screen. No-op before `open`.
    fn feed(mut self: Pin<&mut Self>, bytes: &[u8]) {
        if let Some(grid) = self.as_mut().rust_mut().grid.as_mut() {
            grid.feed(bytes);
        }
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
