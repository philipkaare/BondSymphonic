#pragma once
#include <QDockWidget>
#include <QHash>
#include <QIcon>
#include <QSet>
#include <QString>

class ChangesModel;
class FileTreeModel;
class QLabel;
class QModelIndex;
class QStandardItem;
class QStandardItemModel;
class QToolButton;
class QTreeView;
class QWidget;

/// The left dock: the workspace's worktree as a lazily-loaded tree, beside the
/// list of files that differ from the base.
///
/// The widget holds no listing of its own. A directory is asked for when it is
/// expanded and the answer arrives as `entriesLoaded`, which replaces that
/// directory's children; order and `is_dir` come from the daemon through
/// `FileTreeModel`, so nothing here sorts, filters or stats a path. Each
/// directory item carries a placeholder child so it can be expanded before its
/// contents are known.
///
/// The Changes tab holds no listing of its own either: `ChangesModel` keeps
/// itself current from the daemon's `fs.changed` events, and every list it
/// publishes replaces the tab's rows wholesale.
class ExplorerDock : public QDockWidget {
    Q_OBJECT
public:
    /// The repo-relative path of an entry, for anything reading a selected
    /// index out of the Files tree. Empty on the placeholder and error rows.
    static constexpr int kPathRole = Qt::UserRole;
    /// Whether the entry is a directory, as the daemon reported it.
    static constexpr int kIsDirRole = Qt::UserRole + 1;
    /// The dock's own bookkeeping: whether this directory item's children are
    /// the ones the daemon listed, rather than the placeholder. Loaded-ness has
    /// to be a property of the item, because the model's cache can be refilled
    /// by a listing that was in flight across an `invalidate` and would then
    /// claim a directory is loaded whose children are still the placeholder.
    static constexpr int kLoadedRole = Qt::UserRole + 2;

    ExplorerDock(FileTreeModel* model, ChangesModel* changes, QWidget* parent = nullptr);

    /// Shows `workspaceId`'s worktree and changed files: the tree is emptied
    /// and its root asked for, and the changes model is pointed at the same
    /// workspace. An empty id leaves both empty.
    void setWorkspace(const QString& workspaceId);

    /// Names the workspace the two tabs are showing, in the strip above them:
    /// `name` on its own line, then the branch and the tail of `repoPath`, with
    /// the whole path in the tooltip. An empty `name` is the no-workspace
    /// state: the strip says so and the Refresh button is disabled.
    ///
    /// The three strings are the active tab's, passed straight through from the
    /// window; nothing here looks a workspace up or shortens a branch.
    void setWorkspaceHeader(const QString& name, const QString& branch, const QString& repoPath);

    /// Drops every cached listing and loads the root again, and re-fetches the
    /// changed files. Directories that were expanded, at any depth, are
    /// expanded again as their parent's listing arrives, which asks for them in
    /// turn, so the shape of the tree survives the reload.
    void refresh();

signals:
    /// A file was double-clicked in the Files tree.
    void fileActivated(const QString& path);

    /// A changed file was double-clicked. The window answers by opening the
    /// file's diff.
    void diffActivated(const QString& path);

private:
    void onExpanded(const QModelIndex& index);
    void onDoubleClicked(const QModelIndex& index);
    void onEntriesLoaded(const QString& path, const QString& entriesJson);
    void onLoadFailed(const QString& path, const QString& message);
    /// Replaces every row of the Changes tab from one `workspace.changes`
    /// answer. The list is always complete, never an increment.
    void onChangesLoaded(const QString& json);
    void onChangesFailed(const QString& message);
    void onChangeActivated(const QModelIndex& index);

    /// Asks the model for `path`. Unless `force` is set, a request already out
    /// for that directory suppresses this one.
    void requestDir(const QString& path, bool force = false);
    /// Books one answer in for `path`, and forgets the expansion to restore
    /// once nothing is in flight.
    void noteAnswered(const QString& path);
    /// The item for a repo-relative path, or the invisible root for the empty
    /// path; null when that path is not in the tree.
    QStandardItem* itemForPath(const QString& path) const;
    /// Records every expanded directory under `parent` in `m_expandedToRestore`.
    void collectExpanded(QStandardItem* parent);
    QStandardItem* makeEntry(const QString& path, const QString& name, bool isDir, qint64 size,
                             const QString& status) const;
    /// The "Loading…" child that makes an unread directory expandable.
    static QStandardItem* makePlaceholder();
    static QStandardItem* makeError(const QString& message);

    /// Builds the header strip -- the two labels and the Refresh button -- and
    /// returns it for the dock's layout.
    QWidget* buildHeader();

    FileTreeModel* m_model;
    ChangesModel* m_changes;
    QTreeView* m_files = nullptr;
    QStandardItemModel* m_items = nullptr;
    QTreeView* m_changesView = nullptr;
    QStandardItemModel* m_changeItems = nullptr;
    /// The header strip's first line: the workspace name, or the empty state.
    QLabel* m_headerName = nullptr;
    /// Its second line: branch and the tail of the repository path.
    QLabel* m_headerDetail = nullptr;
    /// Reload, beside the two labels. Disabled while no workspace is shown.
    QToolButton* m_refreshButton = nullptr;
    QIcon m_dirIcon;
    QIcon m_fileIcon;
    /// The workspace the tree shows, empty when none is selected.
    QString m_workspaceId;
    /// How many listings are out for each directory. Usually one; a refresh
    /// asks again for a directory that already has a request out, and both
    /// answers have to be booked in before the directory counts as settled.
    QHash<QString, int> m_pending;
    /// Directories to expand again as their parent's listing arrives, so a
    /// reload does not flatten the tree. Filled from the live tree and emptied
    /// once nothing is in flight.
    QSet<QString> m_expandedToRestore;
};
