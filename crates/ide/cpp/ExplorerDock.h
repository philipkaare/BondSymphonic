#pragma once
#include <QDockWidget>
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
    explicit ExplorerDock(FileTreeModel* model, QWidget* parent = nullptr);

    /// Shows `workspaceId`'s worktree: the tree is emptied and its root asked
    /// for. An empty id leaves the tree empty.
    void setWorkspace(const QString& workspaceId);

    /// Drops every cached listing and loads the root again. Directories that
    /// were expanded are expanded again as their parent's listing arrives,
    /// which asks for them in turn, so the shape of the tree survives.
    void refresh();

signals:
    /// A file was double-clicked. Unused until the editor exists.
    void fileActivated(const QString& path);

private:
    void onExpanded(const QModelIndex& index);
    void onDoubleClicked(const QModelIndex& index);
    void onEntriesLoaded(const QString& path, const QString& entriesJson);
    void onLoadFailed(const QString& path, const QString& message);

    /// Asks the model for `path` unless a request for it is already out.
    void requestDir(const QString& path);
    /// The item for a repo-relative path, or the invisible root for the empty
    /// path; null when that path is not in the tree.
    QStandardItem* itemForPath(const QString& path) const;
    /// The repo-relative paths of `parent`'s directory children that are
    /// currently expanded.
    QSet<QString> expandedChildren(QStandardItem* parent) const;
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
    /// Directories asked for whose answer has not arrived, so collapsing and
    /// expanding one again does not ask twice.
    QSet<QString> m_pending;
};
