#include "ExplorerDock.h"
#include "ChangesToolbar.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/changes_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include <QChar>
#include <QColor>
#include <QFont>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QList>
#include <QPalette>
#include <QSizePolicy>
#include <QStandardItem>
#include <QStandardItemModel>
#include <QStringList>
#include <QStyle>
#include <QTabWidget>
#include <QToolButton>
#include <QTreeView>
#include <QVBoxLayout>
#include <QWidget>

namespace {

/// Joins a directory path and a child name the way the daemon addresses it.
QString childPath(const QString& dir, const QString& name) {
    return dir.isEmpty() ? name : dir + QLatin1Char('/') + name;
}

/// The colour a git status is drawn in, or an invalid colour for a status that
/// is not a change, `unchanged` among them. The accents are the diff's, so a
/// file listed as modified here and a rewritten row over in the diff view are
/// the same amber.
QColor statusColour(const QString& status, bool dark) {
    QColor colour;
    if (status == QLatin1String("added") || status == QLatin1String("untracked")) {
        colour = theme::added();
    } else if (status == QLatin1String("modified")) {
        colour = theme::changed();
    } else if (status == QLatin1String("deleted")) {
        colour = theme::removed();
    } else if (status == QLatin1String("renamed")) {
        colour = theme::renamed();
    }
    return colour.isValid() ? theme::ink(colour, dark) : colour;
}

/// The tail of a repository path -- its last two components -- which is what
/// tells two checkouts apart without spending the strip's width on the prefix.
/// Both separators, because the daemon reports POSIX paths and the New Agent
/// dialog takes Windows ones. The whole path stays in the tooltip.
QString pathTail(const QString& repoPath) {
    QString normalised = repoPath;
    normalised.replace(QLatin1Char('\\'), QLatin1Char('/'));
    const QStringList parts = normalised.split(QLatin1Char('/'), Qt::SkipEmptyParts);
    if (parts.isEmpty()) {
        return repoPath;
    }
    const int from = parts.size() > 2 ? parts.size() - 2 : 0;
    return QStringList(parts.mid(from)).join(QLatin1Char('/'));
}

/// One right-aligned count for the `+` and `−` columns.
QStandardItem* makeCount(int n) {
    auto* item = new QStandardItem(QString::number(n));
    item->setTextAlignment(Qt::AlignRight | Qt::AlignVCenter);
    return item;
}

} // namespace

ExplorerDock::ExplorerDock(FileTreeModel* model, ChangesModel* changes, AppController* controller,
                           QWidget* parent)
    : QDockWidget(QStringLiteral("Explorer"), parent), m_model(model), m_changes(changes) {
    setObjectName(QStringLiteral("ExplorerDock"));
    m_dirIcon = style()->standardIcon(QStyle::SP_DirIcon);
    m_fileIcon = style()->standardIcon(QStyle::SP_FileIcon);

    auto* body = new QWidget(this);
    auto* bodyLayout = new QVBoxLayout(body);
    bodyLayout->setContentsMargins(0, 0, 0, 0);
    bodyLayout->setSpacing(0);
    bodyLayout->addWidget(buildHeader());

    auto* tabs = new QTabWidget(body);
    m_items = new QStandardItemModel(this);
    m_items->setColumnCount(1);
    m_files = new QTreeView(tabs);
    m_files->setModel(m_items);
    m_files->setHeaderHidden(true);
    m_files->setUniformRowHeights(true);
    m_files->setEditTriggers(QAbstractItemView::NoEditTriggers);
    tabs->addTab(m_files, QStringLiteral("Files"));

    m_changeItems = new QStandardItemModel(this);
    // U+2212, the minus sign, written as a code point: the file is compiled
    // without a byte order mark and MSVC would read a literal as the ANSI code
    // page.
    m_changeItems->setHorizontalHeaderLabels({ QStringLiteral("Path"), QStringLiteral("Status"),
                                               QStringLiteral("+"), QString(QChar(0x2212)) });
    m_changesView = new QTreeView(tabs);
    m_changesView->setModel(m_changeItems);
    m_changesView->setRootIsDecorated(false);
    m_changesView->setUniformRowHeights(true);
    m_changesView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_changesView->header()->setSectionResizeMode(0, QHeaderView::Stretch);
    for (int column = 1; column < m_changeItems->columnCount(); ++column) {
        m_changesView->header()->setSectionResizeMode(column, QHeaderView::ResizeToContents);
    }
    // The toolbar goes above the list rather than in the window's menus: what
    // Merge and Discard act on is the thing the list is showing, and the two
    // belong next to each other.
    auto* changesTab = new QWidget(tabs);
    auto* changesLayout = new QVBoxLayout(changesTab);
    changesLayout->setContentsMargins(0, 0, 0, 0);
    changesLayout->setSpacing(0);
    m_changesToolbar = new ChangesToolbar(controller, changesTab);
    changesLayout->addWidget(m_changesToolbar);
    changesLayout->addWidget(m_changesView, 1);
    tabs->addTab(changesTab, QStringLiteral("Changes"));
    bodyLayout->addWidget(tabs, 1);
    setWidget(body);

    QObject::connect(m_files, &QTreeView::expanded, this, &ExplorerDock::onExpanded);
    QObject::connect(m_files, &QTreeView::doubleClicked, this, &ExplorerDock::onDoubleClicked);
    QObject::connect(m_model, &FileTreeModel::entriesLoaded, this, &ExplorerDock::onEntriesLoaded);
    QObject::connect(m_model, &FileTreeModel::loadFailed, this, &ExplorerDock::onLoadFailed);
    QObject::connect(m_changesView, &QTreeView::doubleClicked, this,
                     &ExplorerDock::onChangeActivated);
    // The model refreshes itself from `fs.changed`, so the tab follows the
    // worktree without anyone pressing Refresh.
    QObject::connect(m_changes, &ChangesModel::changesLoaded, this,
                     &ExplorerDock::onChangesLoaded);
    QObject::connect(m_changes, &ChangesModel::loadFailed, this, &ExplorerDock::onChangesFailed);

    // The empty state, so the strip is never blank before the first tab.
    setWorkspaceHeader(QString(), QString(), QString(), QString());
}

ChangesToolbar* ExplorerDock::changesToolbar() const {
    return m_changesToolbar;
}

QWidget* ExplorerDock::buildHeader() {
    auto* header = new QWidget(this);
    header->setObjectName(QStringLiteral("ExplorerHeader"));
    // Its own fill, so the strip reads as a band naming what is below it rather
    // than as the first row of the Files tab.
    header->setAutoFillBackground(true);
    QPalette headerPalette = header->palette();
    headerPalette.setColor(QPalette::Window, theme::band(palette()));
    header->setPalette(headerPalette);

    auto* layout = new QHBoxLayout(header);
    layout->setContentsMargins(8, 6, 4, 6);
    layout->setSpacing(6);

    auto* lines = new QVBoxLayout();
    lines->setContentsMargins(0, 0, 0, 0);
    lines->setSpacing(0);
    m_headerName = new QLabel(header);
    m_headerName->setObjectName(QStringLiteral("ExplorerHeaderName"));
    QFont nameFont = m_headerName->font();
    nameFont.setBold(true);
    m_headerName->setFont(nameFont);
    // The dock is narrow and a workspace name is not; the tooltip carries the
    // whole of it either way.
    m_headerName->setTextInteractionFlags(Qt::TextSelectableByMouse);
    // Ignored, not Preferred: a long workspace name must not push the dock
    // wider than the user sized it. What does not fit is clipped, and the
    // tooltip has all of it.
    m_headerName->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    lines->addWidget(m_headerName);

    m_headerDetail = new QLabel(header);
    m_headerDetail->setObjectName(QStringLiteral("ExplorerHeaderDetail"));
    QPalette detailPalette = m_headerDetail->palette();
    detailPalette.setColor(QPalette::WindowText, theme::muted(palette()));
    m_headerDetail->setPalette(detailPalette);
    m_headerDetail->setTextInteractionFlags(Qt::TextSelectableByMouse);
    m_headerDetail->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    lines->addWidget(m_headerDetail);
    layout->addLayout(lines, 1);

    m_refreshButton = new QToolButton(header);
    m_refreshButton->setIcon(style()->standardIcon(QStyle::SP_BrowserReload));
    m_refreshButton->setToolTip(QStringLiteral("Reload the file tree and changes"));
    m_refreshButton->setAutoRaise(true);
    // Named so the verification hook, and anyone reading the widget tree, can
    // find the one button that reloads the dock.
    m_refreshButton->setObjectName(QStringLiteral("ExplorerRefreshButton"));
    QObject::connect(m_refreshButton, &QToolButton::clicked, this, &ExplorerDock::refresh);
    layout->addWidget(m_refreshButton, 0, Qt::AlignTop);
    return header;
}

void ExplorerDock::setWorkspaceHeader(const QString& name, const QString& branch,
                                      const QString& repoPath, const QString& baseBranch) {
    m_name = name;
    m_branch = branch;
    m_baseBranch = baseBranch;
    // The toolbar acts on the workspace the tree is showing, and is told about
    // it from here because this is where the name and the two branch names
    // arrive. An empty name is the no-workspace state for it too.
    m_changesToolbar->setWorkspace(m_workspaceId, name, branch, baseBranch);
    if (name.isEmpty()) {
        m_headerName->setText(QStringLiteral("No workspace selected"));
        m_headerName->setToolTip(QString());
        m_headerDetail->clear();
        m_headerDetail->setToolTip(QString());
        m_refreshButton->setEnabled(false);
        return;
    }
    m_headerName->setText(name);
    m_headerName->setToolTip(name);
    // U+00B7, the middle dot, as a code point rather than as a character in a
    // literal, so no compiler's idea of this file's source encoding can change
    // what it means.
    const QString separator = QStringLiteral("  ") + QString(QChar(0x00B7)) + QStringLiteral("  ");
    m_headerDetail->setText(branch + separator + pathTail(repoPath));
    m_headerDetail->setToolTip(repoPath);
    m_refreshButton->setEnabled(true);
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
    // Answers with the new list, the empty one included, so the tab needs no
    // clearing of its own.
    m_changes->setWorkspace(workspaceId);
    if (workspaceId.isEmpty()) {
        setWorkspaceHeader(QString(), QString(), QString(), QString());
        return;
    }
    // The window calls `setWorkspaceHeader` right after this, but the id has to
    // reach the toolbar even if it did not: acting on the workspace before it
    // is switched is the one mistake this widget must not make.
    m_changesToolbar->setWorkspace(workspaceId, m_name, m_branch, m_baseBranch);
    requestDir(QString());
}

void ExplorerDock::refresh() {
    if (m_workspaceId.isEmpty()) {
        return;
    }
    m_changes->refresh();
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
        QStandardItem* item = makeEntry(entryPath, name, isDir,
                                        entry.value(QStringLiteral("size")).toInteger(),
                                        entry.value(QStringLiteral("status")).toString());
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

void ExplorerDock::onChangesLoaded(const QString& json) {
    // The list moved, so what a Discard would cost has moved with it. The
    // toolbar asks the daemon rather than counting these rows: "dirty" is the
    // daemon's judgement and the rows are only its changed files.
    m_changesToolbar->refreshSummary();
    m_changeItems->removeRows(0, m_changeItems->rowCount());
    const bool dark = theme::isDark(palette());
    const QJsonArray files = QJsonDocument::fromJson(json.toUtf8()).array();
    for (const QJsonValue value : files) {
        const QJsonObject file = value.toObject();
        const QString path = file.value(QStringLiteral("path")).toString();
        if (path.isEmpty()) {
            continue;
        }
        auto* pathItem = new QStandardItem(path);
        pathItem->setData(path, kPathRole);
        pathItem->setToolTip(path);
        // The daemon's own word for the status, lower case as it sends it.
        const QString status = file.value(QStringLiteral("status")).toString();
        auto* statusItem = new QStandardItem(status);
        const QColor colour = statusColour(status, dark);
        if (colour.isValid()) {
            statusItem->setForeground(colour);
        }
        m_changeItems->appendRow({ pathItem, statusItem,
                                   makeCount(file.value(QStringLiteral("additions")).toInt()),
                                   makeCount(file.value(QStringLiteral("deletions")).toInt()) });
    }
}

void ExplorerDock::onChangesFailed(const QString& message) {
    m_changeItems->removeRows(0, m_changeItems->rowCount());
    m_changeItems->appendRow(makeError(message));
}

void ExplorerDock::onChangeActivated(const QModelIndex& index) {
    // Any column of the row: the path is on the first one.
    const QString path = index.siblingAtColumn(0).data(kPathRole).toString();
    if (!path.isEmpty()) {
        emit diffActivated(path);
    }
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
                                       qint64 size, const QString& status) const {
    auto* item = new QStandardItem(isDir ? m_dirIcon : m_fileIcon, name);
    item->setData(path, kPathRole);
    item->setData(isDir, kIsDirRole);
    item->setToolTip(isDir ? path : QStringLiteral("%1\n%2 bytes").arg(path).arg(size));
    // `fs.list_dir` reports every entry as `unchanged` today, so this lights up
    // the day the daemon fills the field in; nothing here goes looking for it.
    const QColor colour = statusColour(status, theme::isDark(palette()));
    if (colour.isValid()) {
        item->setForeground(colour);
    }
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
