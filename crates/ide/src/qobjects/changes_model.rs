//! The changed-files list for one workspace.
//!
//! One instance, app-wide, like `FileTreeModel`. Pointing it at a workspace
//! turns on the daemon's `fs.watch` for that workspace and fetches
//! `workspace.changes`; from then on a background task refreshes the list
//! whenever the worktree changes. A burst of writes (a `git checkout`, a
//! formatter sweeping the tree) is coalesced into one refresh
//! [`COALESCE_WINDOW`] after the first event, so a rebuild costs one request
//! per burst rather than one per file.

use crate::client::router::EventRx;
use crate::qobjects::app_controller::{require_connection, runtime, Shared};
use bondsymphonic_proto::{
    ChangesResult, Event, FsWatchParams, Request, WorkspaceId, WorkspaceIdParams,
};
use std::time::Duration;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `set_workspace` is exposed as `setWorkspace` and `changes_loaded` as
    // `changesLoaded`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        type ChangesModel = super::ChangesModelRust;

        /// The current list of changed files, as a JSON array of `ChangedFile`.
        /// Emitted after every refresh, including the empty list.
        #[qsignal]
        fn changes_loaded(self: Pin<&mut ChangesModel>, json: QString);

        /// `workspace.changes` failed; `message` is the daemon error text.
        #[qsignal]
        fn load_failed(self: Pin<&mut ChangesModel>, message: QString);

        /// Points the model at a workspace: enables `fs.watch` for it, fetches
        /// the list, and keeps it current. The empty id detaches the model and
        /// reports an empty list. Naming the workspace it already holds does
        /// nothing, so this is safe to call on every tab switch.
        #[qinvokable]
        fn set_workspace(self: Pin<&mut ChangesModel>, workspace_id: QString);

        /// Re-fetches the list now. Answers with `changesLoaded` or
        /// `loadFailed`.
        #[qinvokable]
        fn refresh(self: Pin<&mut ChangesModel>);

        /// The workspace the list describes, or empty before `setWorkspace`.
        #[qinvokable]
        fn workspace_id(self: &ChangesModel) -> QString;
    }

    impl cxx_qt::Threading for ChangesModel {}
}

use core::pin::Pin;
use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;

type QtHandle = cxx_qt::CxxQtThread<qobject::ChangesModel>;

/// How long a burst of `fs.changed` events is collected before one refresh.
/// Long enough to swallow a checkout or a formatter run, short enough that a
/// single save shows up as soon as the user looks away from the editor.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct ChangesModelRust {
    workspace_id: String,
    /// Background subscription to `fs.changed`, aborted on every
    /// `set_workspace` and on Drop.
    watch_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ChangesModelRust {
    fn drop(&mut self) {
        if let Some(task) = self.watch_task.take() {
            task.abort();
        }
    }
}

/// Whether `ev` is a worktree change belonging to `workspace_id`.
///
/// The router carries every event from the connection, so both this model and
/// each open editor filter the same stream down to the workspace they are
/// showing. An event with no workspace is the daemon talking about itself and
/// never matches.
pub fn touches_workspace(ws: &Option<WorkspaceId>, ev: &Event, workspace_id: &str) -> bool {
    matches!(ev, Event::FsChanged { .. }) && ws.as_ref().is_some_and(|w| w.0 == workspace_id)
}

/// Refreshes the list once per burst of worktree changes, for as long as the
/// model is pointed at `workspace`.
async fn watch(mut rx: EventRx, workspace: String, qt: QtHandle) {
    while let Some((ws, ev)) = rx.recv().await {
        if !touches_workspace(&ws, &ev, &workspace) {
            continue;
        }
        // The first event opens the window; everything inside it joins the
        // same refresh, matching the daemon's own debounce one level up.
        let mut ended = false;
        let window = tokio::time::sleep(COALESCE_WINDOW);
        tokio::pin!(window);
        loop {
            tokio::select! {
                _ = &mut window => break,
                more = rx.recv() => {
                    if more.is_none() {
                        // The router dropped us. Refresh what we know about,
                        // then stop.
                        ended = true;
                        break;
                    }
                }
            }
        }
        if qt.queue(|q| q.refresh()).is_err() || ended {
            return;
        }
    }
}

impl qobject::ChangesModel {
    pub fn set_workspace(mut self: Pin<&mut Self>, workspace_id: QString) {
        let workspace = workspace_id.to_string();
        if self.as_ref().rust().workspace_id == workspace {
            return;
        }
        {
            let mut rust = self.as_mut().rust_mut();
            rust.workspace_id = workspace.clone();
            // The previous workspace's watch stops here, so its events can
            // never refresh a list that is now describing another worktree.
            if let Some(task) = rust.watch_task.take() {
                task.abort();
            }
        }
        if workspace.is_empty() {
            self.changes_loaded(QString::from("[]"));
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.load_failed(QString::from(message));
                return;
            }
        };
        let rx = shared.router.subscribe_all();
        let qt = self.as_ref().qt_thread();
        let task = runtime().spawn(watch(rx, workspace.clone(), qt));
        self.as_mut().rust_mut().watch_task = Some(task);
        // Enabling the watch is a round trip the first listing need not wait
        // for; a change arriving before the daemon has the watcher up is
        // covered by this refresh anyway.
        runtime().spawn(async move { enable_watch(&shared, &workspace).await });
        self.refresh();
    }

    pub fn refresh(self: Pin<&mut Self>) {
        let workspace = self.as_ref().rust().workspace_id.clone();
        if workspace.is_empty() {
            self.changes_loaded(QString::from("[]"));
            return;
        }
        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                self.load_failed(QString::from(message));
                return;
            }
        };
        let qt = self.as_ref().qt_thread();
        runtime().spawn(async move {
            let params = WorkspaceIdParams {
                workspace_id: WorkspaceId(workspace.clone()),
            };
            match shared
                .client
                .request::<ChangesResult>(Request::WorkspaceChanges(params))
                .await
            {
                Ok(res) => {
                    let json =
                        serde_json::to_string(&res.files).unwrap_or_else(|_| "[]".to_owned());
                    let _ = qt.queue(move |q| {
                        // The model may have moved to another workspace while
                        // this was in flight; one workspace's changed files
                        // must not be listed under another.
                        if q.as_ref().rust().workspace_id != workspace {
                            tracing::debug!("workspace.changes: dropping list for {workspace}");
                            return;
                        }
                        q.changes_loaded(QString::from(&json));
                    });
                }
                Err(e) => {
                    let message = format!("workspace.changes failed: {e}");
                    tracing::warn!("{message}");
                    let _ = qt.queue(move |q| {
                        if q.as_ref().rust().workspace_id != workspace {
                            return;
                        }
                        q.load_failed(QString::from(&message));
                    });
                }
            }
        });
    }

    pub fn workspace_id(&self) -> QString {
        QString::from(&self.rust().workspace_id)
    }
}

/// Turns on `fs.watch` for a workspace, which is what makes the daemon send
/// `fs.changed` at all. Idempotent, so every consumer that needs the events
/// asks for them rather than assuming someone else did. A failure only costs
/// live updates, so it is logged rather than surfaced.
pub(crate) async fn enable_watch(shared: &Shared, workspace: &str) {
    let params = FsWatchParams {
        workspace_id: WorkspaceId(workspace.to_owned()),
        enable: true,
    };
    if let Err(e) = shared.client.request_raw(Request::FsWatch(params)).await {
        tracing::warn!("fs.watch enable for {workspace} failed: {e}");
    }
}
