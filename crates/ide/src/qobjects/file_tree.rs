//! Lazily-loaded directory listings for the file tree view.
//!
//! The view asks for one directory at a time; the request goes out on the
//! tokio runtime and the answer comes back as a signal, so the Qt thread never
//! waits on the daemon. Listings are cached in a [`FileTree`] keyed by
//! repo-relative path, and the cache belongs to one workspace: pointing the
//! model at another workspace drops it.

use crate::model::file_tree::FileTree;
use crate::qobjects::app_controller::{runtime, shared};
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

/// Reported when a listing is asked for before the daemon connection exists.
const NOT_CONNECTED: &str = "not connected to the daemon";

#[derive(Default)]
pub struct FileTreeModelRust {
    workspace_id: String,
    tree: FileTree,
}

impl qobject::FileTreeModel {
    pub fn load_dir(mut self: Pin<&mut Self>, workspace_id: QString, path: QString) {
        let workspace = workspace_id.to_string();
        let path = path.to_string();
        self.as_mut().adopt_workspace(&workspace);

        let Some(shared) = shared() else {
            // Already on the Qt thread, so the failure is reported directly.
            self.load_failed(QString::from(&path), QString::from(NOT_CONNECTED));
            return;
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
                        // this was in flight; the listing is then stale and
                        // must not poison the new cache.
                        if q.as_ref().rust().workspace_id == workspace {
                            q.as_mut().rust_mut().tree.set_dir(&path, res.entries);
                        }
                        q.entries_loaded(QString::from(&path), QString::from(&json));
                    });
                }
                Err(e) => {
                    let message = e.to_string();
                    tracing::warn!("fs.list_dir failed: {message}");
                    let _ = qt.queue(move |q| {
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
        let mut rust = self.as_mut().rust_mut();
        rust.workspace_id = workspace.to_owned();
        rust.tree = FileTree::new();
    }
}
