#include "ExplorerDock.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include <QFont>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QStandardItem>
#include <QStandardItemModel>
#include <QStyle>
#include <QTabWidget>
#include <QTreeView>

namespace {

/// The repo-relative path of an entry; empty on the placeholder and error rows.
constexpr int kPathRole = Qt::UserRole;
/// Whether the entry is a directory, as the daemon reported it.
constexpr int kIsDirRole = Qt::UserRole + 1;

/// Joins a directory path and a child name the way the daemon addresses it.
QString childPath(const QString& dir, const QString& name) {
    return dir.isEmpty() ? name : dir + QLatin1Char('/') + name;
}

} // namespace

ExplorerDock::ExplorerDock(FileTreeModel* model, QWidget* parent)
    : QDockWidget(QStringLiteral("Explorer"), parent), m_model(model) {
    setObjectName(QStringLiteral("ExplorerDock"));
    m_dirIcon = style()->standardIcon(QStyle::SP_DirIcon);
    m_fileIcon = style()->standardIcon(QStyle::SP_FileIcon);

    auto* tabs = new QTabWidget(this);
    m_items = new QStandardItemModel(this);
    m_items->setColumnCount(1);
    m_files = new QTreeView(tabs);
    m_files->setModel(m_items);
    m_files->setHeaderHidden(true);
    m_files->setUniformRowHeights(true);
    m_files->setEditTriggers(QAbstractItemView::NoEditTriggers);
    tabs->addTab(m_files, QStringLiteral("Files"));
    // Milestone 3 fills this in from the daemon's diff.
    tabs->addTab(new QTreeView(tabs), QStringLiteral("Changes"));
    setWidget(tabs);

    QObject::connect(m_files, &QTreeView::expanded, this, &ExplorerDock::onExpanded);
    QObject::connect(m_files, &QTreeView::doubleClicked, this, &ExplorerDock::onDoubleClicked);
    QObject::connect(m_model, &FileTreeModel::entriesLoaded, this, &ExplorerDock::onEntriesLoaded);
    QObject::connect(m_model, &FileTreeModel::loadFailed, this, &ExplorerDock::onLoadFailed);
}

void ExplorerDock::setWorkspace(const QString& workspaceId) {
    // The group model publishes on every status change, not only on a tab
    // switch, so the same workspace arrives here over and over.
    if (workspaceId == m_workspaceId) {
        return;
    }
    m_workspaceId = workspaceId;
    m_pending.clear();
    m_items->clear();
    m_items->setColumnCount(1);
    m_model->setWorkspace(workspaceId);
    if (workspaceId.isEmpty()) {
        return;
    }
    requestDir(QString());
}

void ExplorerDock::refresh() {
    if (m_workspaceId.isEmpty()) {
        return;
    }
    m_model->invalidate(QString());
    // Every listing still in flight describes the tree before the refresh.
    m_pending.clear();
    requestDir(QString());
}

void ExplorerDock::onExpanded(const QModelIndex& index) {
    if (!index.data(kIsDirRole).toBool()) {
        return;
    }
    const QString path = index.data(kPathRole).toString();
    // A directory whose listing is cached already shows its real children.
    if (m_model->isLoaded(path)) {
        return;
    }
    requestDir(path);
}

void ExplorerDock::onDoubleClicked(const QModelIndex& index) {
    const QString path = index.data(kPathRole).toString();
    if (path.isEmpty() || index.data(kIsDirRole).toBool()) {
        return;
    }
    emit fileActivated(path);
}

void ExplorerDock::onEntriesLoaded(const QString& path, const QString& entriesJson) {
    m_pending.remove(path);
    QStandardItem* dir = itemForPath(path);
    if (dir == nullptr) {
        // The tree was emptied while the listing was in flight.
        return;
    }
    // Re-expanding after the rebuild asks for each of those directories in
    // turn, which is what carries a refresh down the tree.
    const QSet<QString> expanded = expandedChildren(dir);
    dir->removeRows(0, dir->rowCount());

    const QJsonArray entries = QJsonDocument::fromJson(entriesJson.toUtf8()).array();
    for (const QJsonValue& value : entries) {
        const QJsonObject entry = value.toObject();
        const QString name = entry.value(QStringLiteral("name")).toString();
        if (name.isEmpty()) {
            continue;
        }
        const bool isDir = entry.value(QStringLiteral("is_dir")).toBool();
        const QString entryPath = childPath(path, name);
        QStandardItem* item =
            makeEntry(entryPath, name, isDir, entry.value(QStringLiteral("size")).toInteger());
        if (isDir) {
            item->appendRow(makePlaceholder());
        }
        dir->appendRow(item);
        if (isDir && expanded.contains(entryPath)) {
            m_files->expand(m_items->indexFromItem(item));
        }
    }
}

void ExplorerDock::onLoadFailed(const QString& path, const QString& message) {
    m_pending.remove(path);
    QStandardItem* dir = itemForPath(path);
    if (dir == nullptr) {
        return;
    }
    // The error takes the placeholder's place, so the reason is visible where
    // the contents would have been. The root's whole listing is the error.
    dir->removeRows(0, dir->rowCount());
    dir->appendRow(makeError(message));
}

void ExplorerDock::requestDir(const QString& path) {
    if (m_workspaceId.isEmpty() || m_pending.contains(path)) {
        return;
    }
    // Inserted first: a load with no daemon behind it fails synchronously.
    m_pending.insert(path);
    m_model->loadDir(m_workspaceId, path);
}

QStandardItem* ExplorerDock::itemForPath(const QString& path) const {
    QStandardItem* item = m_items->invisibleRootItem();
    if (path.isEmpty()) {
        return item;
    }
    QString prefix;
    for (const QString& segment : path.split(QLatin1Char('/'))) {
        prefix = childPath(prefix, segment);
        QStandardItem* child = nullptr;
        for (int row = 0; row < item->rowCount(); ++row) {
            QStandardItem* candidate = item->child(row);
            if (candidate->data(kPathRole).toString() == prefix) {
                child = candidate;
                break;
            }
        }
        if (child == nullptr) {
            return nullptr;
        }
        item = child;
    }
    return item;
}

QSet<QString> ExplorerDock::expandedChildren(QStandardItem* parent) const {
    QSet<QString> expanded;
    for (int row = 0; row < parent->rowCount(); ++row) {
        QStandardItem* child = parent->child(row);
        if (child->data(kIsDirRole).toBool() && m_files->isExpanded(m_items->indexFromItem(child))) {
            expanded.insert(child->data(kPathRole).toString());
        }
    }
    return expanded;
}

QStandardItem* ExplorerDock::makeEntry(const QString& path, const QString& name, bool isDir,
                                       qint64 size) const {
    auto* item = new QStandardItem(isDir ? m_dirIcon : m_fileIcon, name);
    item->setData(path, kPathRole);
    item->setData(isDir, kIsDirRole);
    item->setToolTip(isDir ? path : QStringLiteral("%1\n%2 bytes").arg(path).arg(size));
    return item;
}

QStandardItem* ExplorerDock::makePlaceholder() {
    auto* item = new QStandardItem(QStringLiteral("Loading…"));
    item->setFlags(Qt::NoItemFlags);
    return item;
}

QStandardItem* ExplorerDock::makeError(const QString& message) {
    auto* item = new QStandardItem(message);
    item->setFlags(Qt::NoItemFlags);
    QFont font = item->font();
    font.setItalic(true);
    item->setFont(font);
    item->setToolTip(message);
    return item;
}
