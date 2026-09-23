#include "ExplorerDock.h"
#include "ChangesToolbar.h"
#include "Theme.h"
#include "WorkspaceLabel.h"
#include "bondsymphonic-ide/src/qobjects/changes_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include <QChar>
#include <QColor>
#include <QEvent>
#include <QFont>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QList>
#include <QPalette>
#include <QResizeEvent>
#include <QSizePolicy>
#include <QStandardItem>
#include <QStandardItemModel>
#include <QStringList>
#include <QStyle>
#include <QTabWidget>
#include <QTimer>
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

/// The narrowest the worktree path is elided to. Below this the elision eats
/// the whole path and leaves an ellipsis, which says less than a clipped path.
constexpr int kMinPathWidth = 40;

/// How long a `workspace.changes` request may be out before the Changes tab
/// says it is loading.
///
/// The list refreshes on every `fs.changed`, and an agent editing files sends
/// those constantly. On an ordinary worktree on the WSL side the answer takes
/// about 300 ms, so a row put up the moment the request left would blink the
/// list to "Loading…" on every file the agent saved. Well over twice that
/// measured figure, so the row never shows on the common path; and well under
/// the 4.5 s a warm `/mnt/c` checkout takes, let alone the minutes a cold one
/// does, so the wait that needs announcing is announced long before anyone
/// wonders whether the panel is broken.
constexpr int kChangesGraceMs = 800;

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
    // Named for the offscreen check below, which reads the rows through it.
    m_changesView->setObjectName(QStringLiteral("ExplorerChangesView"));
    m_changesView->setModel(m_changeItems);
    m_changesView->setRootIsDecorated(false);
    m_changesView->setUniformRowHeights(true);
    m_changesView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_changesView->header()->setSectionResizeMode(0, QHeaderView::Stretch);
    for (int column = 1; column < m_changeItems->columnCount(); ++column) {
        m_changesView->header()->setSectionResizeMode(column, QHeaderView::ResizeToContents);
    }
    m_changesGrace = new QTimer(this);
    // Named for the offscreen check below, which drives it rather than waits.
    m_changesGrace->setObjectName(QStringLiteral("ExplorerChangesGrace"));
    m_changesGrace->setSingleShot(true);
    m_changesGrace->setInterval(kChangesGraceMs);
    QObject::connect(m_changesGrace, &QTimer::timeout, this, &ExplorerDock::onChangesLoading);
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
    // worktree without anyone pressing Refresh -- which is also why the model
    // has to say when a refresh starts: the dock does not see most of them.
    QObject::connect(m_changes, &ChangesModel::refreshStarted, this,
                     &ExplorerDock::onChangesRequested);
    QObject::connect(m_changes, &ChangesModel::changesLoaded, this,
                     &ExplorerDock::onChangesLoaded);
    QObject::connect(m_changes, &ChangesModel::loadFailed, this, &ExplorerDock::onChangesFailed);

    // The empty state, so the strip is never blank before the first tab.
    setWorkspaceHeader(QString(), QString(), QString(), QString(), QString());
}

ChangesToolbar* ExplorerDock::changesToolbar() const {
    return m_changesToolbar;
}

QWidget* ExplorerDock::buildHeader() {
    auto* header = new QWidget(this);
    m_header = header;
    header->setObjectName(QStringLiteral("ExplorerHeader"));
    // Its own fill, so the strip reads as a band naming what is below it rather
    // than as the first row of the Files tab.
    header->setAutoFillBackground(true);

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
    m_headerDetail->setTextInteractionFlags(Qt::TextSelectableByMouse);
    m_headerDetail->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    lines->addWidget(m_headerDetail);

    m_headerPath = new QLabel(header);
    m_headerPath->setObjectName(QStringLiteral("ExplorerHeaderPath"));
    // The daemon wrote this path and an agent chose part of it; it is shown as
    // the text it is, and it can be selected because a path is something people
    // paste into a shell.
    m_headerPath->setTextFormat(Qt::PlainText);
    m_headerPath->setTextInteractionFlags(Qt::TextSelectableByMouse);
    m_headerPath->setMinimumWidth(1);
    m_headerPath->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    lines->addWidget(m_headerPath);
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
    applyHeaderColours();
    return header;
}

void ExplorerDock::applyHeaderColours() {
    if (m_header == nullptr) {
        return;
    }
    // A palette of one role each, rather than the widget's own with that role
    // overwritten: everything else these three paint with is the dock's, and
    // must stay the dock's when the dock changes.
    QPalette headerPalette;
    headerPalette.setColor(QPalette::Window, theme::band(palette()));
    m_header->setPalette(headerPalette);

    QPalette linePalette;
    linePalette.setColor(QPalette::WindowText, theme::muted(palette()));
    m_headerDetail->setPalette(linePalette);
    m_headerPath->setPalette(linePalette);
}

void ExplorerDock::changeEvent(QEvent* event) {
    QDockWidget::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        applyHeaderColours();
    }
}

void ExplorerDock::setWorkspaceHeader(const QString& name, const QString& branch,
                                      const QString& repoPath, const QString& baseBranch,
                                      const QString& worktreePath, bool inPlace) {
    m_changesToolbar->setInPlace(inPlace);
    m_worktreePath = worktreePath;
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
        m_worktreePath.clear();
        updatePathElide();
        m_refreshButton->setEnabled(false);
        return;
    }
    m_headerName->setText(name);
    m_headerName->setToolTip(name);
    m_headerDetail->setText(workspacelabel::origin(repoPath, baseBranch) +
                            (inPlace ? workspacelabel::inPlaceSuffix() : QString()));
    m_headerDetail->setToolTip(
        inPlace ? workspacelabel::inPlaceDetail(repoPath, baseBranch, worktreePath)
                : workspacelabel::detail(repoPath, baseBranch, branch, worktreePath));
    updatePathElide();
    m_refreshButton->setEnabled(true);
}

void ExplorerDock::updatePathElide() {
    if (m_worktreePath.isEmpty()) {
        m_headerPath->clear();
        m_headerPath->setToolTip(QString());
        m_headerPath->setVisible(false);
        return;
    }
    m_headerPath->setVisible(true);
    // From the left: every worktree of every agent shares a prefix, and what
    // tells them apart is at the end.
    const int room = qMax(m_headerPath->width(), kMinPathWidth);
    m_headerPath->setText(
        m_headerPath->fontMetrics().elidedText(m_worktreePath, Qt::ElideLeft, room));
    m_headerPath->setToolTip(m_worktreePath);
}

void ExplorerDock::resizeEvent(QResizeEvent* event) {
    QDockWidget::resizeEvent(event);
    updatePathElide();
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
        setWorkspaceHeader(QString(), QString(), QString(), QString(), QString());
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

void ExplorerDock::onChangesRequested() {
    // `start` on a running timer restarts it: a second request before the
    // first has answered moves the deadline, it does not add one.
    m_changesGrace->start();
}

void ExplorerDock::onChangesLoading() {
    // The previous list goes now rather than when the answer lands. The wait
    // is long enough on a Windows-drive checkout that a list left up for it
    // would be read as current, and a failure at the end of it would then
    // replace a list that was never true.
    m_changeItems->removeRows(0, m_changeItems->rowCount());
    m_changeItems->appendRow(makePlaceholder());
}

void ExplorerDock::onChangesLoaded(const QString& json) {
    m_changesGrace->stop();
    // No `refreshSummary()` here. The model reloads itself from every
    // `fs.changed` event, so an agent writing files put one `workspace.summary`
    // on the wire per keystroke's worth of output -- for a number nothing is
    // showing at the time. The toolbar asks on the two occasions the answer is
    // about to be read instead: when the active tab moves (`setWorkspace`) and
    // immediately before the Discard confirmation opens.
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
    m_changesGrace->stop();
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

// --- offscreen test entries --------------------------------------------------
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.
#if defined(BS_WIDGET_TESTS)
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QAbstractItemModel>
#include <QCoreApplication>
#include <cstdint>

namespace {

/// The Changes tab's rows, read through the view the dock names for this.
const QAbstractItemModel* changesRows(const ExplorerDock& dock) {
    const auto* view = dock.findChild<QTreeView*>(QStringLiteral("ExplorerChangesView"));
    return view == nullptr ? nullptr : view->model();
}

/// The first column of row `row`, or an empty string past the end.
QString rowText(const QAbstractItemModel& rows, int row) {
    return rows.index(row, 0).data().toString();
}

/// Whether the tab is showing the two-file list the check loads.
bool showsTheList(const QAbstractItemModel& rows) {
    return rows.rowCount() == 2 && rowText(rows, 0) == QLatin1String("src/main.rs") &&
           rowText(rows, 1) == QLatin1String("notes.md");
}

/// Whether the tab is showing the one loading row and nothing else.
bool showsLoading(const QAbstractItemModel& rows) {
    return rows.rowCount() == 1 && rowText(rows, 0).contains(QLatin1String("Loading"));
}

/// Fires `grace` now, if it is running, without waiting the grace period out.
///
/// The interval goes to zero and the pending timer event is delivered, which
/// runs the real timeout connection; only the wait is skipped. No event loop
/// runs in this suite, so a check cannot sleep the period away and let the
/// timer fire on its own -- and a check that slept would take the better part
/// of a second per assertion for a number the dock is free to change.
void fireGrace(QTimer* grace) {
    if (grace->isActive()) {
        grace->setInterval(0);
    }
    QCoreApplication::processEvents();
}

} // namespace

/// The Changes tab keeps its list while a refresh answers promptly, and says
/// it is loading once one has been out for the grace period; the answer --
/// the list or the failure -- takes the loading row's place.
///
/// Both halves are the point. The first version of the row went up on every
/// `refreshStarted`, and the model refreshes on every `fs.changed`: an agent
/// saving files would have blinked the list to "Loading…" on every save, on an
/// ordinary worktree where the answer is 300 ms away. Before that the tab
/// showed the previous list until the answer landed, and on an in-place
/// workspace on a Windows drive that is seconds warm and minutes cold -- long
/// enough for a stale list to be read as the current one. The model is driven
/// from here rather than through a daemon because the dock's part is the
/// rendering of the three signals, whichever refresh -- a tab switch, a
/// Refresh click or an `fs.changed` the dock never sees -- sent them.
extern "C" std::int32_t bs_widget_test_explorer_changes_say_they_are_loading() {
    AppController controller;
    FileTreeModel fileTree;
    ChangesModel changes;
    ExplorerDock dock(&fileTree, &changes, &controller);
    const QAbstractItemModel* rows = changesRows(dock);
    auto* grace = dock.findChild<QTimer*>(QStringLiteral("ExplorerChangesGrace"));
    if (rows == nullptr || grace == nullptr) {
        return 1;
    }
    const QString list = QStringLiteral(
        R"([{"path":"src/main.rs","status":"modified","additions":3,"deletions":1},)"
        R"({"path":"notes.md","status":"untracked","additions":0,"deletions":0}])");
    changes.refreshStarted();
    changes.changesLoaded(list);
    if (!showsTheList(*rows)) {
        return 2;
    }
    // (a) A refresh that answers inside the grace period: the list stays up
    // the whole time, and nothing is left ticking afterwards. The events are
    // pumped after the request goes out so that a grace period of nothing at
    // all -- a zero timer fires on the first pump -- is caught here.
    changes.refreshStarted();
    QCoreApplication::processEvents();
    if (!showsTheList(*rows)) {
        return 3;
    }
    if (!grace->isActive()) {
        return 4;
    }
    changes.changesLoaded(list);
    if (grace->isActive()) {
        return 5;
    }
    fireGrace(grace);
    if (!showsTheList(*rows)) {
        return 6;
    }
    // A second request before the first answers moves the deadline rather
    // than adding one: the answer stops the only timer there is.
    changes.refreshStarted();
    changes.refreshStarted();
    changes.changesLoaded(list);
    fireGrace(grace);
    if (!showsTheList(*rows) || grace->isActive()) {
        return 7;
    }
    // (b) A refresh still out when the grace period ends: the loading row
    // replaces the list, not nothing and not the list it is about to
    // supersede.
    changes.refreshStarted();
    fireGrace(grace);
    if (!showsLoading(*rows)) {
        return 8;
    }
    // (c) A failure replaces the loading row with the reason, as it always did.
    const QString failure = QStringLiteral("workspace.changes failed: request timed out");
    changes.loadFailed(failure);
    if (rows->rowCount() != 1 || rowText(*rows, 0) != failure) {
        return 9;
    }
    if (grace->isActive()) {
        return 10;
    }
    // And the list landing after the row replaces it too; the empty list is
    // an answer, with no rows and no loading row.
    changes.refreshStarted();
    fireGrace(grace);
    changes.changesLoaded(list);
    if (!showsTheList(*rows)) {
        return 11;
    }
    changes.refreshStarted();
    fireGrace(grace);
    changes.changesLoaded(QStringLiteral("[]"));
    if (rows->rowCount() != 0) {
        return 12;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
