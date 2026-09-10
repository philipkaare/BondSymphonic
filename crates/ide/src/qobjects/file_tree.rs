//! Lazily-loaded directory listings for the file tree view.
//!
//! The view asks for one directory at a time; the request goes out on the
//! tokio runtime and the answer comes back as a signal, so the Qt thread never
//! waits on the daemon. Listings are cached in a [`FileTree`] keyed by
//! repo-relative path, and the cache belongs to one workspace: pointing the
//! model at another workspace drops it.

use crate::model::file_tree::FileTree;
use crate::qobjects::app_controller::{on_reconnect, require_connection, runtime};
use bondsymphonic_proto::{FsPathParams, ListDirResult, Request, WorkspaceId};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `load_dir` is exposed as `loadDir` and `entries_loaded` as `entriesLoaded`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        type FileTreeModel = super::FileTreeModelRust;

        /// The listing for `path` arrived and is now cached. `entries_json` is
        /// a JSON array of `FileEntry`.
        #[qsignal]
        fn entries_loaded(self: Pin<&mut FileTreeModel>, path: QString, entries_json: QString);

        /// Listing `path` failed; `message` is the daemon error text.
        #[qsignal]
        fn load_failed(self: Pin<&mut FileTreeModel>, path: QString, message: QString);

        /// Lists one directory of `workspace_id`; the empty path is the repo
        /// root. Answers with `entriesLoaded` or `loadFailed`. Naming a
        /// different workspace than the last call drops the cache first, so a
        /// caller never has to sequence `setWorkspace` before this.
        #[qinvokable]
        fn load_dir(self: Pin<&mut FileTreeModel>, workspace_id: QString, path: QString);

        /// Points the model at a workspace, dropping the cache if it changed.
        #[qinvokable]
        fn set_workspace(self: Pin<&mut FileTreeModel>, workspace_id: QString);

        /// The workspace the cache belongs to, or empty before the first load.
        #[qinvokable]
        fn workspace_id(self: &FileTreeModel) -> QString;

        /// Drops the cached listing for `path` and every directory under it,
        /// so the next `loadDir` goes back to the daemon.
        #[qinvokable]
        fn invalidate(self: Pin<&mut FileTreeModel>, path: QString);

        #[qinvokable]
        fn is_loaded(self: &FileTreeModel, path: QString) -> bool;

        /// The cached listing for `path` as a JSON array, or an empty array
        /// when nothing is cached. Use `isLoaded` to tell the two apart.
        #[qinvokable]
        fn cached_entries(self: &FileTreeModel, path: QString) -> QString;
    }

    impl cxx_qt::Threading for FileTreeModel {}
}

use core::pin::Pin;
use cxx_qt::CxxQtType;
use cxx_qt::Threading;
use cxx_qt_lib::QString;

#[derive(Default)]
pub struct FileTreeModelRust {
    workspace_id: String,
    tree: FileTree,
    /// Waits for the connection generation to move and then re-reads the root
    /// from the daemon that came back. Replaced whenever the workspace
    /// changes, aborted on Drop.
    reconnect_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for FileTreeModelRust {
    fn drop(&mut self) {
        if let Some(task) = self.reconnect_task.take() {
            task.abort();
        }
    }
}

impl qobject::FileTreeModel {
    pub fn load_dir(mut self: Pin<&mut Self>, workspace_id: QString, path: QString) {
        let workspace = workspace_id.to_string();
        let path = path.to_string();
        self.as_mut().adopt_workspace(&workspace);

        let shared = match require_connection() {
            Ok(shared) => shared,
            Err(message) => {
                // Already on the Qt thread, so the failure is reported directly.
                self.load_failed(QString::from(&path), QString::from(message));
                return;
            }
        };
        let qt = self.qt_thread();
        runtime().spawn(async move {
            let params = FsPathParams {
                workspace_id: WorkspaceId(workspace.clone()),
                path: path.clone(),
            };
            match shared
                .client
                .request::<ListDirResult>(Request::FsListDir(params))
                .await
            {
                Ok(res) => {
                    let json = FileTree::entries_json(&res.entries);
                    let _ = qt.queue(move |mut q| {
                        // The model may have moved to another workspace while
                        // this was in flight. The listing is then stale: it
                        // must neither poison the new cache nor reach the view,
                        // which would show one workspace's files under another.
                        if q.as_ref().rust().workspace_id != workspace {
                            tracing::debug!("fs.list_dir: dropping listing for {workspace}");
                            return;
                        }
                        q.as_mut().rust_mut().tree.set_dir(&path, res.entries);
                        q.entries_loaded(QString::from(&path), QString::from(&json));
                    });
                }
                Err(e) => {
                    let message = e.to_string();
                    tracing::warn!("fs.list_dir failed: {message}");
                    let _ = qt.queue(move |q| {
                        // Stale for the same reason, and an error banner for a
                        // workspace nobody is looking at is worse than silence.
                        if q.as_ref().rust().workspace_id != workspace {
                            tracing::debug!("fs.list_dir: dropping failure for {workspace}");
                            return;
                        }
                        q.load_failed(QString::from(&path), QString::from(&message))
                    });
                }
            }
        });
    }

    pub fn set_workspace(self: Pin<&mut Self>, workspace_id: QString) {
        self.adopt_workspace(&workspace_id.to_string());
    }

    pub fn workspace_id(&self) -> QString {
        QString::from(&self.rust().workspace_id)
    }

    pub fn invalidate(mut self: Pin<&mut Self>, path: QString) {
        self.as_mut().rust_mut().tree.invalidate(&path.to_string());
    }

    pub fn is_loaded(&self, path: QString) -> bool {
        self.rust().tree.is_loaded(&path.to_string())
    }

    pub fn cached_entries(&self, path: QString) -> QString {
        match self.rust().tree.get_dir(&path.to_string()) {
            Some(entries) => QString::from(&FileTree::entries_json(entries)),
            None => QString::from("[]"),
        }
    }

    /// Records `workspace` as the one the cache describes, emptying the cache
    /// when it is a different workspace than before.
    fn adopt_workspace(mut self: Pin<&mut Self>, workspace: &str) {
        if self.as_ref().rust().workspace_id == workspace {
            return;
        }
        {
            let mut rust = self.as_mut().rust_mut();
            rust.workspace_id = workspace.to_owned();
            rust.tree = FileTree::new();
            if let Some(previous) = rust.reconnect_task.take() {
                previous.abort();
            }
        }
        if workspace.is_empty() {
            return;
        }
        let qt = self.as_ref().qt_thread();
        let watch = on_reconnect(qt, qobject::FileTreeModel::reload_after_reconnect);
        self.as_mut().rust_mut().reconnect_task = Some(watch);
    }

    /// Drops the cache and re-reads the root from the daemon that came back.
    ///
    /// The root, not every expanded directory: this model holds no subscription
    /// and cannot repaint the view by itself, so what it can honestly do is
    /// invalidate what it cached against a daemon that is gone and answer the
    /// view's `entriesLoaded("")` with the new listing. The view rebuilds from
    /// the root and asks again for whatever the user re-opens.
    fn reload_after_reconnect(mut self: Pin<&mut Self>) {
        let workspace = self.as_ref().rust().workspace_id.clone();
        if workspace.is_empty() {
            return;
        }
        tracing::info!("file tree reloading {workspace} after a reconnect");
        self.as_mut().rust_mut().tree = FileTree::new();
        // Re-armed here rather than inside `load_dir`, which `adopt_workspace`
        // would refuse to do for the workspace it is already showing.
        {
            let qt = self.as_ref().qt_thread();
            let watch = on_reconnect(qt, qobject::FileTreeModel::reload_after_reconnect);
            self.as_mut().rust_mut().reconnect_task = Some(watch);
        }
        self.load_dir(QString::from(&workspace), QString::from(""));
    }
}
