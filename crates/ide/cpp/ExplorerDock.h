#pragma once
#include <QDockWidget>
#include <QHash>
#include <QIcon>
#include <QSet>
#include <QString>

class FileTreeModel;
class QModelIndex;
class QStandardItem;
class QStandardItemModel;
class QTreeView;

/// The left dock: the workspace's worktree as a lazily-loaded tree, beside a
/// Changes tab that Milestone 3 fills in.
///
/// The widget holds no listing of its own. A directory is asked for when it is
/// expanded and the answer arrives as `entriesLoaded`, which replaces that
/// directory's children; order and `is_dir` come from the daemon through
/// `FileTreeModel`, so nothing here sorts, filters or stats a path. Each
/// directory item carries a placeholder child so it can be expanded before its
/// contents are known.
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

    explicit ExplorerDock(FileTreeModel* model, QWidget* parent = nullptr);

    /// Shows `workspaceId`'s worktree: the tree is emptied and its root asked
    /// for. An empty id leaves the tree empty.
    void setWorkspace(const QString& workspaceId);

    /// Drops every cached listing and loads the root again. Directories that
    /// were expanded, at any depth, are expanded again as their parent's
    /// listing arrives, which asks for them in turn, so the shape of the tree
    /// survives the reload.
    void refresh();

signals:
    /// A file was double-clicked. Unused until the editor exists.
    void fileActivated(const QString& path);

private:
    void onExpanded(const QModelIndex& index);
    void onDoubleClicked(const QModelIndex& index);
    void onEntriesLoaded(const QString& path, const QString& entriesJson);
    void onLoadFailed(const QString& path, const QString& message);

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
    QStandardItem* makeEntry(const QString& path, const QString& name, bool isDir, qint64 size) const;
    /// The "Loading…" child that makes an unread directory expandable.
    static QStandardItem* makePlaceholder();
    static QStandardItem* makeError(const QString& message);

    FileTreeModel* m_model;
    QTreeView* m_files = nullptr;
    QStandardItemModel* m_items = nullptr;
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
