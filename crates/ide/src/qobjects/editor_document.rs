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
//! The document also watches its own file. `fs.watch` is enabled for the
//! workspace on open, and a background task filters `fs.changed` down to this
//! path. A change while the document is clean reloads it silently; a change
//! while it is dirty raises `externalChange` and waits for `acceptExternal` or
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

use crate::highlight::languages::Language;
use crate::highlight::theme::Theme;
use crate::model::editor_buffer::EditorBuffer;
use crate::qobjects::app_controller::{require_connection, runtime, Shared};
use crate::qobjects::changes_model::{enable_watch, touches_workspace};
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
    /// Set while a disk change is pending the user's reload/keep decision.
    external_pending: bool,
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
            external_pending: false,
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
    /// The watch task's silent reload, carrying the content generation the
    /// read was issued at. Installs only while [`may_install_disk_text`]
    /// allows it, and then only if the file actually differs.
    IfUnchangedSince(u64),
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
                replace_from_disk(q, res, install);
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

/// Loads the file, then watches it for the life of this generation.
async fn open_and_watch(
    shared: Shared,
    qt: QtHandle,
    workspace: String,
    path: String,
    generation: u64,
) {
    // Subscribed before the read goes out, so a change landing between the
    // read and the subscription is not missed.
    let mut rx = shared.router.subscribe_all();
    read_into(
        shared.clone(),
        qt.clone(),
        workspace.clone(),
        path.clone(),
        generation,
        Install::Always,
    )
    .await;
    // Enabled even when the read failed: the file may be about to appear, and
    // the watch is what will pick it up.
    enable_watch(&shared, &workspace).await;

    while let Some((ws, ev)) = rx.recv().await {
        if !touches_workspace(&ws, &ev, &workspace) {
            continue;
        }
        let Event::FsChanged { paths } = &ev else {
            continue;
        };
        if !paths.iter().any(|p| p == &path) {
            continue;
        }
        let queued = qt.queue(move |mut q| {
            if q.as_ref().rust().generation != generation {
                return;
            }
            if *q.as_ref().dirty() {
                // Local edits win until the user says otherwise. Raised once
                // per unanswered conflict: a `git checkout` touching the file
                // repeatedly must not stack one prompt per event.
                if q.as_ref().rust().external_pending {
                    return;
                }
                q.as_mut().rust_mut().external_pending = true;
                q.external_change();
                return;
            }
            // Captured here, on the Qt thread, so the read that goes out is
            // pinned to the buffer as it stands at this instant.
            let at_read = q.as_ref().rust().content_generation;
            q.reload(Install::IfUnchangedSince(at_read));
        });
        if queued.is_err() {
            return;
        }
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
            rust.external_pending = false;
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

        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.as_mut().set_error(QString::from(message));
                self.load_failed(QString::from(message));
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        let task = runtime().spawn(open_and_watch(shared, qt, workspace, file, generation));
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
        let Some(content) = self.as_ref().rust().buffer.as_ref().map(EditorBuffer::text) else {
            self.save_failed(QString::from(NOT_LOADED));
            return;
        };
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
                        // The write itself produces an `fs.changed`; the watch
                        // then re-reads, finds identical content and does
                        // nothing. That is how our own writes are ignored,
                        // without any bookkeeping to get wrong.
                        q.as_mut().rust_mut().external_pending = false;
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
        self.as_mut().rust_mut().external_pending = false;
    }

    /// Re-reads the file and offers what is on disk to `replace_from_disk`.
    ///
    /// [`Install::Always`] is the user answering `externalChange`: the text is
    /// replaced and `loaded` emitted whatever the file now holds. The watch
    /// task passes [`Install::IfUnchangedSince`] instead, so an unchanged file
    /// is left alone (which is what makes our own `save` a no-op here) and a
    /// document the user has moved on from is not overwritten.
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

/// Installs a freshly read file over the buffer, if this read is still allowed
/// to and the file actually differs.
fn replace_from_disk(
    mut q: Pin<&mut qobject::EditorDocument>,
    res: ReadFileResult,
    install: Install,
) {
    let force = match install {
        Install::Always => true,
        Install::IfUnchangedSince(at_read) => {
            let now = q.as_ref().rust().content_generation;
            if !may_install_disk_text(*q.as_ref().dirty(), at_read, now) {
                // The user typed, or saved newer text, while this read was in
                // flight. Installing now would erase either one.
                tracing::debug!("fs.read_file: the document moved on; dropping the reload");
                return;
            }
            false
        }
    };
    let path = q.as_ref().path().to_string();
    let (text, _) = normalise_line_separators(&res.content);
    let unchanged = q
        .as_ref()
        .rust()
        .buffer
        .as_ref()
        .is_some_and(|buffer| buffer.text() == text);
    if unchanged && !force {
        return;
    }
    // Rebuilt rather than `replace_all`ed so the size rule is re-applied: a
    // file that grew past the limit while open must not start highlighting.
    let (buffer, _) = build_buffer(&path, &res.content);
    q.as_mut().rust_mut().buffer = Some(buffer);
    q.as_mut()
        .set_read_only_reason(QString::from(read_only_reason(&res)));
    q.as_mut().set_dirty(false);
    q.as_mut().rust_mut().external_pending = false;
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
