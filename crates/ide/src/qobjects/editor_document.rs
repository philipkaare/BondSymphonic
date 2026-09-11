//! One open file: text and highlight spans live in an [`EditorBuffer`]; the
//! daemon is the file system. Edits arrive from the view in UTF-16 units and
//! are applied in char units; spans go back out per line as JSON.
//!
//! Two things the daemon's bytes are put through before they reach the buffer:
//!
//! * **Line separators.** `QTextDocument` starts a new block at U+2029 and
//!   U+2028, while [`EditorBuffer`] breaks only on LF and CRLF. A file
//!   containing either character would leave the view and the buffer counting
//!   different lines, so [`normalise_line_separators`] rewrites both to `\n`
//!   on load. The rewrite is visible: saving such a file writes newlines back.
//! * **Size.** Highlighting re-parses the whole file after every keystroke, so
//!   a document over [`HIGHLIGHT_MAX_BYTES`] opens with it switched off.
//!   `language` still reports what the file is; `spansForLine` returns `[]`.
//!
//! Line endings are the buffer's business ([`EditorBuffer::text_for_save`]):
//! the rope holds LF, the file's own ending is put back on save, and `save`
//! therefore writes `text_for_save()` rather than `text()`.
//!
//! The document also watches its own file. [`watch_file`] subscribes to the
//! workspace's `fs.changed` stream, enables `fs.watch` on the daemon, and
//! reports what happens as a [`WatchNotice`]. It follows the *connection*
//! rather than one router: every connect builds a fresh [`EventRouter`], so a
//! subscription taken on the previous one is dead, and the task re-subscribes
//! on the router that is live after a reconnect, asks the restarted daemon for
//! `fs.watch` again, and re-reads the file.
//!
//! Every change is answered by re-reading the file and then deciding, in
//! [`disk_verdict`], what the bytes that came back mean. The document remembers
//! by hash what it last knew to be on disk -- what it loaded, what it reloaded,
//! what it last wrote -- so bytes it already knows are its own save landing, or
//! a touch, and change nothing. That is what lets a keystroke typed between the
//! save and the event survive: the document is dirty again, but the file is not
//! news. Bytes it does not know reload a clean document silently and raise
//! `externalChange` on a dirty one, which waits for `acceptExternal` or
//! `keepLocal`.
//!
//! Writes and reads overlap with typing, so both are decided twice: once when
//! they are issued and again when they land. Every edit bumps
//! `content_generation`, and each request captures it. A write clears `dirty`
//! only if the value is unchanged when it returns, so a keystroke typed during
//! a save leaves the document dirty rather than looking saved. A silent reload
//! installs disk text only if [`may_install_disk_text`] still allows it, so it
//! can neither overwrite fresh edits nor undo a save that overtook it. The one
//! exception is `acceptExternal`, which is the user asking for exactly that
//! overwrite.
//!
//! [`EventRouter`]: crate::client::router::EventRouter

use crate::highlight::languages::Language;
use crate::highlight::theme::Theme;
use crate::model::editor_buffer::EditorBuffer;
use crate::qobjects::app_controller::{
    generation_watch, require_connection, runtime, shared, Shared,
};
use crate::qobjects::changes_model::enable_watch;
use bondsymphonic_proto::{
    Event, FsPathParams, FsWriteParams, ReadFileResult, Request, WorkspaceId,
};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `read_only_reason` is exposed as `getReadOnlyReason`/`readOnlyReasonChanged`
    // and `apply_edit` as `applyEdit`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        // `read_only_reason` is empty while the file is editable and otherwise
        // the sentence the header shows; `language` is the detected language's
        // display name, or empty; `error` is the last failure text.
        #[qobject]
        #[qproperty(QString, workspace_id)]
        #[qproperty(QString, path)]
        #[qproperty(bool, dirty)]
        #[qproperty(QString, read_only_reason)]
        #[qproperty(QString, language)]
        #[qproperty(QString, error)]
        #[qproperty(bool, dark_theme)]
        type EditorDocument = super::EditorDocumentRust;

        /// The text is ready or was replaced wholesale: the view must reset
        /// its document from `text()`.
        #[qsignal]
        fn loaded(self: Pin<&mut EditorDocument>);

        /// Spans changed on the inclusive line range `from_line..=to_line`.
        #[qsignal]
        fn highlight_changed(self: Pin<&mut EditorDocument>, from_line: i32, to_line: i32);

        /// `fs.write_file` succeeded; `dirty` is now false.
        #[qsignal]
        fn saved(self: Pin<&mut EditorDocument>);

        #[qsignal]
        fn save_failed(self: Pin<&mut EditorDocument>, message: QString);

        /// The file changed on disk while this document had unsaved edits.
        /// Answer with `acceptExternal` or `keepLocal`.
        #[qsignal]
        fn external_change(self: Pin<&mut EditorDocument>);

        #[qsignal]
        fn load_failed(self: Pin<&mut EditorDocument>, message: QString);

        /// Reads `path` from `workspace_id` and starts watching it. Answers
        /// with `loaded` or `loadFailed`; re-opening drops the previous file's
        /// watch and any reply still in flight for it.
        #[qinvokable]
        fn open(self: Pin<&mut EditorDocument>, workspace_id: QString, path: QString);

        /// Applies one edit from the view. Positions are UTF-16 code units, as
        /// `QTextDocument` reports them. A no-op on a read-only document.
        #[qinvokable]
        fn apply_edit(
            self: Pin<&mut EditorDocument>,
            utf16_pos: i32,
            utf16_removed: i32,
            inserted: QString,
        );

        #[qinvokable]
        fn text(self: &EditorDocument) -> QString;

        #[qinvokable]
        fn line_count(self: &EditorDocument) -> i32;

        /// The spans on line `n` as a JSON array, or `[]` for a line that does
        /// not exist and for a document that is not highlighted.
        #[qinvokable]
        fn spans_for_line(self: Pin<&mut EditorDocument>, n: i32) -> QString;

        /// Writes the buffer back. Answers with `saved` or `saveFailed`.
        #[qinvokable]
        fn save(self: Pin<&mut EditorDocument>);

        /// Reloads from disk, dropping local edits.
        #[qinvokable]
        fn accept_external(self: Pin<&mut EditorDocument>);

        /// Dismisses the external-change state; the next save overwrites.
        #[qinvokable]
        fn keep_local(self: Pin<&mut EditorDocument>);
    }

    impl cxx_qt::Threading for EditorDocument {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

type QtHandle = cxx_qt::CxxQtThread<qobject::EditorDocument>;

/// Above this many bytes a document opens with highlighting switched off. A
/// full tree-sitter pass runs after every keystroke, so half a megabyte is
/// about where re-parsing stops being free on a keypress.
pub const HIGHLIGHT_MAX_BYTES: usize = 512 * 1024;

/// Why `save` refuses a document that has never finished loading. Reported
/// through `saveFailed` rather than swallowed, so a save-all can tell the two
/// apart.
pub const NOT_LOADED: &str = "no file is open";

pub struct EditorDocumentRust {
    workspace_id: QString,
    path: QString,
    dirty: bool,
    read_only_reason: QString,
    language: QString,
    error: QString,
    dark_theme: bool,
    buffer: Option<EditorBuffer>,
    /// Background subscription to `fs.changed`, aborted on re-open and Drop.
    watch_task: Option<tokio::task::JoinHandle<()>>,
    /// Every decision about the file on disk, and the state behind them.
    disk: DiskState,
    /// Bumped on every `open` so a late read for an earlier file is dropped.
    generation: u64,
    /// Bumped on every edit that changes the text. A write and a read each
    /// capture it when they are issued and compare it when they land, so
    /// neither can act on a buffer the user has changed in the meantime.
    /// Monotonic across `open`s, so a value captured for an earlier file can
    /// never match by accident.
    content_generation: u64,
}

impl Default for EditorDocumentRust {
    fn default() -> Self {
        Self {
            workspace_id: QString::from(""),
            path: QString::from(""),
            dirty: false,
            read_only_reason: QString::from(""),
            language: QString::from(""),
            error: QString::from(""),
            dark_theme: false,
            buffer: None,
            watch_task: None,
            disk: DiskState {
                known: None,
                external_pending: false,
            },
            generation: 0,
            content_generation: 0,
        }
    }
}

impl Drop for EditorDocumentRust {
    fn drop(&mut self) {
        if let Some(task) = self.watch_task.take() {
            task.abort();
        }
    }
}

/// Whether a read issued when the content stood at `at_read` may still install
/// its text, given the document's state now.
///
/// Both halves are load-bearing, and neither implies the other:
///
/// * A document that has become dirty has edits the disk text would erase.
/// * A document that is clean again may have been made clean by a *save* of
///   newer text than the read is carrying, and installing it would undo the
///   save. The content generation is what catches that one.
///
/// Not consulted by `acceptExternal`, where discarding local edits is the
/// whole point of the call.
pub fn may_install_disk_text(dirty: bool, at_read: u64, now: u64) -> bool {
    !dirty && at_read == now
}

/// Identifies the bytes of a file without keeping a second copy of them.
///
/// A document may hold four megabytes, and the only question ever asked of the
/// remembered copy is whether the bytes just read are the same ones. Compared
/// only against hashes taken in the same process run and never persisted, so
/// the hasher only has to be consistent with itself.
///
/// A collision would mean a real external change read as the document's own
/// bytes and silently ignored, which is the user's edit or the agent's write
/// lost rather than a wrong pixel. At 64 bits that is not a practical risk for
/// the handful of versions one open file goes through, and it is the reason
/// this is a hash of the whole content rather than a cheaper summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentHash(u64);

pub fn content_hash(text: &str) -> ContentHash {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    ContentHash(hasher.finish())
}

/// What a completed re-read of the watched file means for the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskVerdict {
    /// The file holds the bytes this document already knew were there: its own
    /// save landing, or a touch that rewrote the same content. Nothing to do,
    /// and in particular nothing to prompt about -- which is what keeps a
    /// keystroke typed between the save and the event.
    Ignore,
    /// Something else wrote the file and this document has edits that
    /// installing it would erase. Ask.
    ExternalChange,
    /// Something else wrote the file and there is nothing to lose. Install it.
    Install,
    /// The read is out of date: the buffer moved on while it was in flight, so
    /// its bytes describe neither what is on disk now nor what the user has.
    Drop,
}

/// Decides what to do with a file that has just been re-read, given what the
/// document last knew to be on disk (`known`), whether it has unsaved edits,
/// and the content generation the read was issued at against the one now.
///
/// The order of the three questions is the whole of it. Bytes already known
/// come first, because a file that has not actually changed is not a conflict
/// however dirty the buffer is. Unsaved edits come next, because they are the
/// only thing a prompt could protect. The generation comes last, and catches
/// the clean document that was made clean by a *save* newer than this read: see
/// [`may_install_disk_text`].
pub fn disk_verdict(
    disk_text: &str,
    known: Option<ContentHash>,
    dirty: bool,
    at_read: u64,
    now: u64,
) -> DiskVerdict {
    if known == Some(content_hash(disk_text)) {
        return DiskVerdict::Ignore;
    }
    if dirty {
        return DiskVerdict::ExternalChange;
    }
    if !may_install_disk_text(dirty, at_read, now) {
        return DiskVerdict::Drop;
    }
    DiskVerdict::Install
}

/// The document's disk bookkeeping, with no Qt in it.
///
/// Everything the document decides about a file on disk is decided here: what
/// a watch notice should do, what a completed read means, and what is on disk
/// now. The QObject around it is left with the two things only Qt can do --
/// replacing the buffer and emitting a signal -- so the decisions can be tested
/// without a running Qt application. See [`DiskState::on_notice`] and
/// [`DiskState::on_read`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskState {
    /// What the document last knew to be on disk, by hash: the bytes it
    /// loaded, reloaded, wrote, or was told about by a change it prompted
    /// over. `None` until the first read lands.
    pub known: Option<ContentHash>,
    /// A conflict is on screen, waiting for `acceptExternal` or `keepLocal`.
    pub external_pending: bool,
}

/// What a [`WatchNotice`] asks the document to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchAction {
    /// Read the file and install it whatever the document has done since: the
    /// open's own first read.
    ReadAndInstall,
    /// Read the file and let [`DiskState::on_read`] decide, pinned to the
    /// content generation the read is issued at.
    ReadAndJudge(u64),
    /// Nothing at all.
    Nothing,
}

/// What is left for the QObject to do once a completed read has been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Put the text that was read into the buffer and emit `loaded`.
    InstallDiskText,
    /// Emit `externalChange` and wait for the user.
    RaiseExternalChange,
    /// Nothing: the file is not news, or the read is out of date.
    Nothing,
}

impl DiskState {
    /// Records the bytes that are on disk now: a write that landed, or a read
    /// about to be installed. Clears any conflict, which those bytes settle.
    pub fn record(&mut self, on_disk: ContentHash) {
        self.known = Some(on_disk);
        self.external_pending = false;
    }

    /// Forgets a conflict without resolving it: `keepLocal`, the user saying
    /// their edits win. The next change is a fresh question.
    pub fn keep_local(&mut self) {
        self.external_pending = false;
    }

    /// Whether `notice` should issue a read, and on what terms.
    ///
    /// A conflict already on screen swallows further notices: a `git checkout`
    /// touching the file repeatedly must not stack one prompt per event, and
    /// the read it would issue could not do anything the pending answer will
    /// not do better.
    pub fn on_notice(&self, notice: WatchNotice, content_generation: u64) -> WatchAction {
        match notice {
            WatchNotice::Subscribed => WatchAction::ReadAndInstall,
            WatchNotice::Changed | WatchNotice::Reconnected => {
                if self.external_pending {
                    WatchAction::Nothing
                } else {
                    WatchAction::ReadAndJudge(content_generation)
                }
            }
        }
    }

    /// Applies a completed re-read, issued when the content stood at `at_read`,
    /// and says what is left for the document to do.
    pub fn on_read(&mut self, disk_text: &str, dirty: bool, at_read: u64, now: u64) -> ReadOutcome {
        match disk_verdict(disk_text, self.known, dirty, at_read, now) {
            DiskVerdict::Ignore => {
                tracing::debug!("fs.read_file: the file holds what this document already knew");
                ReadOutcome::Nothing
            }
            DiskVerdict::Drop => {
                // The user typed, or saved newer text, while this read was in
                // flight. Installing now would erase either one, and the bytes
                // it carries are not what is on disk any more either, so they
                // are not recorded as known.
                tracing::debug!("fs.read_file: the document moved on; dropping the reload");
                ReadOutcome::Nothing
            }
            DiskVerdict::ExternalChange => {
                // Recorded even though nothing is installed: this *is* what is
                // on disk, so a second event carrying the same bytes is not a
                // second conflict once the user has answered.
                self.known = Some(content_hash(disk_text));
                self.external_pending = true;
                ReadOutcome::RaiseExternalChange
            }
            DiskVerdict::Install => {
                self.record(content_hash(disk_text));
                ReadOutcome::InstallDiskText
            }
        }
    }
}

/// What the watch task has to say about the file it is watching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchNotice {
    /// Attached to the live connection's `fs.changed` stream. The document
    /// reads the file for the first time here rather than before subscribing,
    /// so a change landing between the two cannot be missed.
    Subscribed,
    /// The watched path was named in an `fs.changed`.
    Changed,
    /// Attached again, on the connection that is live after a reconnect. The
    /// daemon was restarted and has forgotten everything it knew, so the file
    /// is re-read as if it had changed -- it may well have, while the IDE had
    /// nobody to hear about it.
    Reconnected,
}

/// Watches one file for as long as `notify` keeps returning true, across any
/// number of daemon connections.
///
/// Free of Qt on purpose: the document's own use of it queues each notice onto
/// the Qt thread, and a test can hand it a channel instead.
///
/// The generation receiver is taken *before* the connection is read. Taken
/// after, a connection published in between would be one this task had already
/// marked as seen, and it would wait for a change that had already happened --
/// watching a router nothing dispatches into for the rest of the session.
pub async fn watch_file<F>(workspace: String, path: String, notify: F)
where
    F: Fn(WatchNotice) -> bool,
{
    let mut first = true;
    loop {
        let mut generations = generation_watch();
        let _ = *generations.borrow_and_update();
        let Some(shared) = shared() else {
            // Nothing to subscribe to yet. The watch is armed all the same, so
            // a document opened while the daemon was down still follows its
            // file once one is connected.
            if generations.changed().await.is_err() {
                return;
            }
            continue;
        };
        let mut rx = shared.router.subscribe_fs(&WorkspaceId(workspace.clone()));
        let notice = if first {
            WatchNotice::Subscribed
        } else {
            tracing::info!("{path}: re-attaching the file watch after a reconnect");
            WatchNotice::Reconnected
        };
        first = false;
        if !notify(notice) {
            return;
        }
        // Enabled even when the read failed, and again after every reconnect:
        // the file may be about to appear, and a restarted daemon has been
        // told about no watches at all.
        enable_watch(&shared, &workspace).await;

        loop {
            tokio::select! {
                received = rx.recv() => {
                    let Some((_, ev)) = received else {
                        // The router this subscription lived in is gone, which
                        // only a reconnect does. Waiting for the new
                        // connection to be published beats re-subscribing into
                        // the one on its way out.
                        if generations.changed().await.is_err() {
                            return;
                        }
                        break;
                    };
                    let Event::FsChanged { paths } = &ev else { continue };
                    if paths.iter().any(|p| p == &path) && !notify(WatchNotice::Changed) {
                        return;
                    }
                }
                changed = generations.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
            }
        }
    }
}

/// Why a file opens read-only, or empty when it is editable.
pub fn read_only_reason(res: &ReadFileResult) -> &'static str {
    if res.encoding == "binary" {
        "binary file"
    } else if res.truncated {
        "file larger than 4 MiB (truncated)"
    } else {
        ""
    }
}

/// Rewrites every line break Qt recognises but the buffer does not into `\n`,
/// reporting whether anything was rewritten.
///
/// Three of them: U+2029 (paragraph separator), U+2028 (line separator), and a
/// lone `\r`. `QTextCursor::insertText` starts a new block at each, while
/// [`EditorBuffer`] and the diff splitter break only on `\n` and `\r\n`. A file
/// carrying any of the three would leave the view and the buffer disagreeing
/// about which line is which, so the disagreement is removed on load instead.
/// A `\r\n` is a break both sides already agree on and is left alone.
///
/// The rewrite is real: saving such a file writes newlines back.
pub fn normalise_line_separators(text: &str) -> (String, bool) {
    if !text.contains(['\u{2029}', '\u{2028}', '\r']) {
        return (text.to_owned(), false);
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{2029}' | '\u{2028}' => {
                out.push('\n');
                changed = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push_str("\r\n");
            }
            '\r' => {
                out.push('\n');
                changed = true;
            }
            _ => out.push(c),
        }
    }
    (out, changed)
}

/// Builds the buffer for one loaded file: separators normalised, highlighting
/// switched off past [`HIGHLIGHT_MAX_BYTES`]. The flag reports whether the
/// text on screen differs from the bytes on disk.
pub fn build_buffer(path: &str, content: &str) -> (EditorBuffer, bool) {
    let (text, normalised) = normalise_line_separators(content);
    if normalised {
        tracing::info!("{path}: Unicode line separators rewritten to newlines");
    }
    let mut buffer = EditorBuffer::new(path, &text);
    if text.len() > HIGHLIGHT_MAX_BYTES {
        tracing::debug!(
            bytes = text.len(),
            "{path}: over {HIGHLIGHT_MAX_BYTES} bytes, opening without highlighting"
        );
        buffer.set_highlighting(false);
    }
    (buffer, normalised)
}

/// What a completed read may do with the text it brings back.
#[derive(Clone, Copy)]
enum Install {
    /// An `open` or an `acceptExternal`: replace the text and emit `loaded`
    /// whatever the document has done while the read was in flight, because
    /// discarding local state is what the caller asked for.
    Always,
    /// The watch task's re-read, carrying the content generation the read was
    /// issued at. What happens to it is [`disk_verdict`]'s answer.
    ByVerdict(u64),
}

/// Reads one file and installs it over the buffer on the Qt thread, dropping
/// the reply if the document has been re-opened since.
async fn read_into(
    shared: Shared,
    qt: QtHandle,
    workspace: String,
    path: String,
    generation: u64,
    install: Install,
) {
    let params = FsPathParams {
        workspace_id: WorkspaceId(workspace),
        path: path.clone(),
    };
    match shared
        .client
        .request::<ReadFileResult>(Request::FsReadFile(params))
        .await
    {
        Ok(res) => {
            let _ = qt.queue(move |q| {
                if q.as_ref().rust().generation != generation {
                    tracing::debug!("fs.read_file: dropping stale read of {path}");
                    return;
                }
                apply_disk_read(q, res, install);
            });
        }
        Err(e) => {
            let message = format!("fs.read_file failed: {e}");
            tracing::warn!("{message}");
            let _ = qt.queue(move |mut q| {
                if q.as_ref().rust().generation != generation {
                    return;
                }
                q.as_mut().set_error(QString::from(&message));
                q.load_failed(QString::from(&message));
            });
        }
    }
}

/// Watches the file for the life of this generation, reading it whenever the
/// watch has something to say -- including the first time.
async fn open_and_watch(qt: QtHandle, workspace: String, path: String, generation: u64) {
    watch_file(workspace, path, move |notice| {
        qt.queue(move |q| on_watch_notice(q, notice, generation))
            .is_ok()
    })
    .await;
}

/// One notice from the watch task, on the Qt thread.
fn on_watch_notice(
    q: core::pin::Pin<&mut qobject::EditorDocument>,
    notice: WatchNotice,
    generation: u64,
) {
    if q.as_ref().rust().generation != generation {
        return;
    }
    // Read here, on the Qt thread, so the request that goes out is pinned to
    // the buffer as it stands at this instant.
    let now = q.as_ref().rust().content_generation;
    match q.as_ref().rust().disk.on_notice(notice, now) {
        // The open's own read. Issued from here so that it cannot outrun the
        // subscription that would have told us about a change made meanwhile.
        WatchAction::ReadAndInstall => q.reload(Install::Always),
        WatchAction::ReadAndJudge(at_read) => q.reload(Install::ByVerdict(at_read)),
        WatchAction::Nothing => {}
    }
}

impl qobject::EditorDocument {
    pub fn open(mut self: Pin<&mut Self>, workspace_id: QString, path: QString) {
        // The previous file's watch must stop before the new one starts, or
        // two subscriptions would race to reload the same document.
        if let Some(task) = self.as_mut().rust_mut().watch_task.take() {
            task.abort();
        }
        let generation = {
            let mut rust = self.as_mut().rust_mut();
            rust.generation += 1;
            rust.buffer = None;
            // Nothing is known about the new file's bytes until its first read
            // lands; the previous file's hash must not answer for it.
            rust.disk = DiskState::default();
            rust.generation
        };
        let workspace = workspace_id.to_string();
        let file = path.to_string();
        let language = Language::from_path(&file).map_or("", Language::name);
        self.as_mut().set_workspace_id(workspace_id);
        self.as_mut().set_path(path);
        self.as_mut().set_language(QString::from(language));
        self.as_mut().set_dirty(false);
        self.as_mut().set_read_only_reason(QString::from(""));
        self.as_mut().set_error(QString::from(""));

        // Reported here rather than left to the watch task: a file opened
        // while the daemon is down must say so, and the task itself simply
        // waits for a connection.
        if let Err(message) = require_connection() {
            self.as_mut().set_error(QString::from(message));
            self.as_mut().load_failed(QString::from(message));
        }
        let qt = self.as_ref().qt_thread();
        let task = runtime().spawn(open_and_watch(qt, workspace, file, generation));
        self.rust_mut().watch_task = Some(task);
    }

    pub fn apply_edit(
        mut self: Pin<&mut Self>,
        utf16_pos: i32,
        utf16_removed: i32,
        inserted: QString,
    ) {
        if !self.as_ref().read_only_reason().to_string().is_empty() {
            return;
        }
        let inserted = inserted.to_string();
        // `QTextDocument::contentsChange` also fires for changes that alter no
        // text at all. Marking the document dirty for one of those would make
        // a save button light up over a file nobody has touched.
        if utf16_removed <= 0 && inserted.is_empty() {
            return;
        }
        let range = {
            let mut rust = self.as_mut().rust_mut();
            let Some(buffer) = rust.buffer.as_mut() else {
                return;
            };
            let pos = utf16_pos.max(0) as usize;
            let removed = utf16_removed.max(0) as usize;
            let from = buffer.utf16_to_char(pos);
            // Converted as an absolute offset rather than a count: a surrogate
            // pair is two UTF-16 units and one char, so the two ends have to be
            // mapped separately.
            let to = buffer.utf16_to_char(pos.saturating_add(removed));
            let range = buffer.apply_edit(from, to.saturating_sub(from), &inserted);
            // Every request in flight captured this counter when it was
            // issued; moving it is what tells them the buffer is no longer
            // the one they were sent for.
            rust.content_generation = rust.content_generation.wrapping_add(1);
            range
        };
        self.as_mut().set_dirty(true);
        self.highlight_changed(clamp_line(range.0), clamp_line(range.1));
    }

    pub fn text(&self) -> QString {
        match self.rust().buffer.as_ref() {
            Some(buffer) => QString::from(&buffer.text()),
            None => QString::from(""),
        }
    }

    pub fn line_count(&self) -> i32 {
        self.rust()
            .buffer
            .as_ref()
            .map_or(0, |buffer| clamp_line(buffer.line_count()))
    }

    pub fn spans_for_line(mut self: Pin<&mut Self>, n: i32) -> QString {
        if n < 0 {
            return QString::from("[]");
        }
        let theme = Theme::for_dark(*self.as_ref().dark_theme());
        match self.as_mut().rust_mut().buffer.as_mut() {
            Some(buffer) => QString::from(&buffer.spans_json(n as usize, theme)),
            None => QString::from("[]"),
        }
    }

    pub fn save(mut self: Pin<&mut Self>) {
        // Refusals answer, rather than returning in silence: under
        // `requestSaveAll` the window has no other way to tell a document that
        // was written from one that was never going to be.
        let reason = self.as_ref().read_only_reason().to_string();
        if !reason.is_empty() {
            self.save_failed(QString::from(&reason));
            return;
        }
        // `text_for_save`, not `text`: the buffer holds LF and the file keeps
        // the ending it was loaded with.
        let Some(content) = self
            .as_ref()
            .rust()
            .buffer
            .as_ref()
            .map(EditorBuffer::text_for_save)
        else {
            self.save_failed(QString::from(NOT_LOADED));
            return;
        };
        // Taken before the content is handed to the request, so the reply can
        // record what is now on disk without keeping a copy of the file.
        let written = content_hash(&content);
        let generation = self.as_ref().rust().generation;
        // Captured with the content that is about to be written, and compared
        // when the write returns: a keystroke landing in between must not be
        // erased by a `dirty = false` describing older text.
        let content_at_write = self.as_ref().rust().content_generation;
        let params = FsWriteParams {
            workspace_id: WorkspaceId(self.as_ref().workspace_id().to_string()),
            path: self.as_ref().path().to_string(),
            content,
        };
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.as_mut().set_error(QString::from(message));
                self.save_failed(QString::from(message));
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        runtime().spawn(async move {
            match shared
                .client
                .request_raw(Request::FsWriteFile(params))
                .await
            {
                Ok(_) => {
                    let _ = qt.queue(move |mut q| {
                        if q.as_ref().rust().generation != generation {
                            return;
                        }
                        // The bytes on disk are the ones this write carried.
                        // If the user has typed since, the buffer is already
                        // past them and the document stays dirty: `saved` is
                        // still emitted, because the write did happen.
                        if q.as_ref().rust().content_generation == content_at_write {
                            q.as_mut().set_dirty(false);
                        }
                        // These are the bytes on disk now. The `fs.changed`
                        // this write produces re-reads them, `DiskState`
                        // recognises them, and nothing happens -- whether or
                        // not the user has typed since.
                        q.as_mut().rust_mut().disk.record(written);
                        q.as_mut().set_error(QString::from(""));
                        q.saved();
                    });
                }
                Err(e) => {
                    let message = format!("fs.write_file failed: {e}");
                    tracing::warn!("{message}");
                    let _ = qt.queue(move |mut q| {
                        if q.as_ref().rust().generation != generation {
                            return;
                        }
                        q.as_mut().set_error(QString::from(&message));
                        q.save_failed(QString::from(&message));
                    });
                }
            }
        });
    }

    pub fn accept_external(self: Pin<&mut Self>) {
        self.reload(Install::Always);
    }

    pub fn keep_local(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().disk.keep_local();
    }

    /// Re-reads the file and offers what is on disk to `replace_from_disk`.
    ///
    /// [`Install::Always`] is the open, and the user answering
    /// `externalChange`: the text is replaced and `loaded` emitted whatever the
    /// file now holds. The watch task passes [`Install::ByVerdict`] instead and
    /// leaves the decision to [`disk_verdict`].
    fn reload(mut self: Pin<&mut Self>, install: Install) {
        let generation = self.as_ref().rust().generation;
        let workspace = self.as_ref().workspace_id().to_string();
        let path = self.as_ref().path().to_string();
        if path.is_empty() {
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.as_mut().set_error(QString::from(message));
                self.load_failed(QString::from(message));
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        runtime().spawn(read_into(shared, qt, workspace, path, generation, install));
    }
}

/// Acts on a freshly read file: unconditionally for an `open` or an
/// `acceptExternal`, and otherwise on [`disk_verdict`]'s answer.
fn apply_disk_read(
    mut q: Pin<&mut qobject::EditorDocument>,
    res: ReadFileResult,
    install: Install,
) {
    let at_read = match install {
        Install::Always => {
            // The open, and `acceptExternal`: these bytes go in whatever the
            // document has done since, so there is no verdict to ask for --
            // only the record of what is now on disk.
            q.as_mut()
                .rust_mut()
                .disk
                .record(content_hash(&res.content));
            return install_disk_text(q, res);
        }
        Install::ByVerdict(at_read) => at_read,
    };
    let dirty = *q.as_ref().dirty();
    let now = q.as_ref().rust().content_generation;
    let mut disk = q.as_ref().rust().disk;
    let outcome = disk.on_read(&res.content, dirty, at_read, now);
    q.as_mut().rust_mut().disk = disk;
    match outcome {
        ReadOutcome::Nothing => {}
        ReadOutcome::RaiseExternalChange => q.external_change(),
        ReadOutcome::InstallDiskText => install_disk_text(q, res),
    }
}

/// Replaces the buffer with what was just read and tells the view. The caller
/// has already recorded those bytes as what is on disk.
fn install_disk_text(mut q: Pin<&mut qobject::EditorDocument>, res: ReadFileResult) {
    let path = q.as_ref().path().to_string();
    // Rebuilt rather than `replace_all`ed so the size rule is re-applied: a
    // file that grew past the limit while open must not start highlighting.
    let (buffer, _) = build_buffer(&path, &res.content);
    q.as_mut().rust_mut().buffer = Some(buffer);
    q.as_mut()
        .set_read_only_reason(QString::from(read_only_reason(&res)));
    q.as_mut().set_dirty(false);
    // The document now matches disk, so whatever the last failure said about
    // it is history.
    q.as_mut().set_error(QString::from(""));
    q.loaded();
}

/// Line numbers cross into Qt as `i32`; a file with more lines than that would
/// have failed the daemon's 4 MiB read long before.
fn clamp_line(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}
