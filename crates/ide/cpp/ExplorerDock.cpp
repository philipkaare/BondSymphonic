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
    m_expandedToRestore.clear();
    m_items->clear();
    m_items->setColumnCount(1);
    // Listings still in flight are dropped by the model, which checks the
    // workspace an answer belongs to before it emits.
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
    m_expandedToRestore.clear();
    collectExpanded(m_items->invisibleRootItem());
    // Forced: a root listing already in flight describes the tree before the
    // refresh, so it is not the answer this asks for.
    requestDir(QString(), true);
}

void ExplorerDock::onExpanded(const QModelIndex& index) {
    if (!index.data(kIsDirRole).toBool() || index.data(kLoadedRole).toBool()) {
        return;
    }
    requestDir(index.data(kPathRole).toString());
}

void ExplorerDock::onDoubleClicked(const QModelIndex& index) {
    const QString path = index.data(kPathRole).toString();
    if (path.isEmpty() || index.data(kIsDirRole).toBool()) {
        return;
    }
    emit fileActivated(path);
}

void ExplorerDock::onEntriesLoaded(const QString& path, const QString& entriesJson) {
    QStandardItem* dir = itemForPath(path);
    if (dir == nullptr) {
        // The tree was emptied while the listing was in flight.
        noteAnswered(path);
        return;
    }
    // Whatever is expanded under here is about to be destroyed; remember it
    // alongside anything a refresh already recorded.
    collectExpanded(dir);
    dir->removeRows(0, dir->rowCount());
    if (!path.isEmpty()) {
        dir->setData(true, kLoadedRole);
    }

    const QJsonArray entries = QJsonDocument::fromJson(entriesJson.toUtf8()).array();
    for (const QJsonValue value : entries) {
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
        // Expanding asks for the directory in turn, which is what carries a
        // reload down the tree.
        if (isDir && m_expandedToRestore.contains(entryPath)) {
            m_files->expand(m_items->indexFromItem(item));
        }
    }
    // Last, so the requests the re-expansion just made keep the reload alive.
    noteAnswered(path);
}

void ExplorerDock::onLoadFailed(const QString& path, const QString& message) {
    QStandardItem* dir = itemForPath(path);
    if (dir == nullptr) {
        noteAnswered(path);
        return;
    }
    // The error takes the placeholder's place, so the reason is visible where
    // the contents would have been. The root's whole listing is the error.
    dir->removeRows(0, dir->rowCount());
    if (!path.isEmpty()) {
        dir->setData(false, kLoadedRole);
    }
    dir->appendRow(makeError(message));
    noteAnswered(path);
}

void ExplorerDock::requestDir(const QString& path, bool force) {
    if (m_workspaceId.isEmpty() || (!force && m_pending.value(path) > 0)) {
        return;
    }
    // Counted first: a load with no daemon behind it fails synchronously.
    ++m_pending[path];
    m_model->loadDir(m_workspaceId, path);
}

void ExplorerDock::noteAnswered(const QString& path) {
    const auto it = m_pending.find(path);
    if (it != m_pending.end() && --it.value() <= 0) {
        m_pending.erase(it);
    }
    if (m_pending.isEmpty()) {
        m_expandedToRestore.clear();
    }
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

void ExplorerDock::collectExpanded(QStandardItem* parent) {
    for (int row = 0; row < parent->rowCount(); ++row) {
        QStandardItem* child = parent->child(row);
        if (!child->data(kIsDirRole).toBool()) {
            continue;
        }
        if (m_files->isExpanded(m_items->indexFromItem(child))) {
            m_expandedToRestore.insert(child->data(kPathRole).toString());
        }
        collectExpanded(child);
    }
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
