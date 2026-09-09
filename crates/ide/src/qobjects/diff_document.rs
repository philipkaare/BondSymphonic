//! One file's working copy against its base, as side-by-side rows.
//!
//! The rows and the stat counts come from [`crate::model::diff`]; the two
//! sides also get an [`EditorBuffer`] each so the view can ask for highlight
//! spans by line number, exactly as it does for an open file.
//!
//! Alignment is Myers' algorithm, which is O(n·d): two large files that share
//! almost nothing can run for tens of seconds. So it runs on the tokio side,
//! never on the Qt thread, and under [`DIFF_BUDGET`]. When the budget runs out
//! the rows are still a valid alignment, only a coarser one, and `truncated`
//! is set so the header can say so.
//!
//! Both texts are put through
//! [`normalise_line_separators`](crate::qobjects::editor_document::normalise_line_separators)
//! first, for the reason the editor does it: `QTextDocument` breaks blocks at
//! U+2029 and U+2028 and the buffer does not.

use crate::highlight::theme::Theme;
use crate::model::diff::{align_with_deadline, counts, rows_json};
use crate::model::editor_buffer::EditorBuffer;
use crate::qobjects::app_controller::{require_connection, runtime};
use crate::qobjects::editor_document::build_buffer;
use bondsymphonic_proto::{DiffResult, Request, WorkspaceDiffParams, WorkspaceId};
use std::time::Duration;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `rows_json` is exposed as `rowsJson` and `rows_loaded` as `rowsLoaded`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        // `additions`/`deletions` are the diff stat, counting a replaced line
        // as both; `truncated` says the alignment gave up on being minimal.
        #[qobject]
        #[qproperty(QString, workspace_id)]
        #[qproperty(QString, path)]
        #[qproperty(bool, dark_theme)]
        #[qproperty(i32, additions)]
        #[qproperty(i32, deletions)]
        #[qproperty(bool, truncated)]
        #[qproperty(QString, error)]
        type DiffDocument = super::DiffDocumentRust;

        /// The rows are ready: read them with `rowsJson`.
        #[qsignal]
        fn rows_loaded(self: Pin<&mut DiffDocument>);

        #[qsignal]
        fn load_failed(self: Pin<&mut DiffDocument>, message: QString);

        /// Diffs `path` in `workspace_id` against its base. Answers with
        /// `rowsLoaded` or `loadFailed`; a second call supersedes the first.
        #[qinvokable]
        fn load(self: Pin<&mut DiffDocument>, workspace_id: QString, path: QString);

        /// The rows as a JSON array of `DiffRow`, or `[]` before the first
        /// successful load.
        #[qinvokable]
        fn rows_json(self: &DiffDocument) -> QString;

        /// Spans on the base side, by the 1-based `left_no` the rows carry.
        #[qinvokable]
        fn spans_for_left_line(self: Pin<&mut DiffDocument>, n: i32) -> QString;

        /// Spans on the working side, by the 1-based `right_no`.
        #[qinvokable]
        fn spans_for_right_line(self: Pin<&mut DiffDocument>, n: i32) -> QString;
    }

    impl cxx_qt::Threading for DiffDocument {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

/// How long the alignment may run before it approximates the rest. Two seconds
/// is past what any ordinary file needs and short enough that a pathological
/// one does not look like a hang.
pub const DIFF_BUDGET: Duration = Duration::from_secs(2);

pub struct DiffDocumentRust {
    workspace_id: QString,
    path: QString,
    dark_theme: bool,
    additions: i32,
    deletions: i32,
    truncated: bool,
    error: QString,
    rows_json: String,
    /// The base text, for spans on the left side.
    left: Option<EditorBuffer>,
    /// The working text, for spans on the right side.
    right: Option<EditorBuffer>,
    /// Bumped on every `load` so a late reply for an earlier file is dropped.
    generation: u64,
}

impl Default for DiffDocumentRust {
    fn default() -> Self {
        Self {
            workspace_id: QString::from(""),
            path: QString::from(""),
            dark_theme: false,
            additions: 0,
            deletions: 0,
            truncated: false,
            error: QString::from(""),
            rows_json: "[]".to_owned(),
            left: None,
            right: None,
            generation: 0,
        }
    }
}

/// Everything one alignment produces, computed off the Qt thread.
struct Aligned {
    rows_json: String,
    additions: i32,
    deletions: i32,
    truncated: bool,
    left: EditorBuffer,
    right: EditorBuffer,
}

/// Aligns one `workspace.diff` result. Runs on the tokio side: both the
/// alignment and building the two ropes are proportional to the file, and
/// neither may happen while the Qt thread is trying to paint.
fn build(path: &str, res: DiffResult) -> Aligned {
    let (base, _) = crate::qobjects::editor_document::normalise_line_separators(&res.base_text);
    let (work, _) = crate::qobjects::editor_document::normalise_line_separators(&res.work_text);
    let (rows, truncated) = align_with_deadline(&base, &work, DIFF_BUDGET);
    if truncated {
        tracing::warn!("workspace.diff: {path} exceeded the alignment budget; rows approximated");
    }
    let (additions, deletions) = counts(&rows);
    let (left, _) = build_buffer(path, &base);
    let (right, _) = build_buffer(path, &work);
    Aligned {
        rows_json: rows_json(&rows),
        additions: clamp(additions),
        deletions: clamp(deletions),
        truncated,
        left,
        right,
    }
}

impl qobject::DiffDocument {
    pub fn load(mut self: Pin<&mut Self>, workspace_id: QString, path: QString) {
        let generation = {
            let mut rust = self.as_mut().rust_mut();
            rust.generation += 1;
            rust.rows_json = "[]".to_owned();
            rust.left = None;
            rust.right = None;
            rust.generation
        };
        let workspace = workspace_id.to_string();
        let file = path.to_string();
        self.as_mut().set_workspace_id(workspace_id);
        self.as_mut().set_path(path);
        self.as_mut().set_additions(0);
        self.as_mut().set_deletions(0);
        self.as_mut().set_truncated(false);
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
        runtime().spawn(async move {
            let params = WorkspaceDiffParams {
                workspace_id: WorkspaceId(workspace),
                path: file.clone(),
            };
            match shared
                .client
                .request::<DiffResult>(Request::WorkspaceDiff(params))
                .await
            {
                Ok(res) => {
                    let aligned = build(&file, res);
                    let _ = qt.queue(move |mut q| {
                        if q.as_ref().rust().generation != generation {
                            tracing::debug!("workspace.diff: dropping stale rows for {file}");
                            return;
                        }
                        q.as_mut().set_additions(aligned.additions);
                        q.as_mut().set_deletions(aligned.deletions);
                        q.as_mut().set_truncated(aligned.truncated);
                        {
                            let mut rust = q.as_mut().rust_mut();
                            rust.rows_json = aligned.rows_json;
                            rust.left = Some(aligned.left);
                            rust.right = Some(aligned.right);
                        }
                        q.rows_loaded();
                    });
                }
                Err(e) => {
                    let message = format!("workspace.diff failed: {e}");
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
        });
    }

    pub fn rows_json(&self) -> QString {
        QString::from(&self.rust().rows_json)
    }

    pub fn spans_for_left_line(mut self: Pin<&mut Self>, n: i32) -> QString {
        let theme = Theme::for_dark(*self.as_ref().dark_theme());
        spans(self.as_mut().rust_mut().left.as_mut(), n, theme)
    }

    pub fn spans_for_right_line(mut self: Pin<&mut Self>, n: i32) -> QString {
        let theme = Theme::for_dark(*self.as_ref().dark_theme());
        spans(self.as_mut().rust_mut().right.as_mut(), n, theme)
    }
}

/// Spans for one 1-based row line number. Row numbers are `None` on the side a
/// row does not occupy and reach the invokable as 0 or -1, which is no line.
fn spans(buffer: Option<&mut EditorBuffer>, n: i32, theme: &Theme) -> QString {
    if n <= 0 {
        return QString::from("[]");
    }
    match buffer {
        Some(buffer) => QString::from(&buffer.spans_json(n as usize - 1, theme)),
        None => QString::from("[]"),
    }
}

/// Counts cross into Qt as `i32`; the daemon's read cap puts real files many
/// orders of magnitude below the saturation point.
fn clamp(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}
