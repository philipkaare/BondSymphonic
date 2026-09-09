#include "MainWindow.h"
#include "AgentArea.h"
#include "Branding.h"
#include "CodeView.h"
#include "EditorArea.h"
#include "EditorWidget.h"
#include "ExplorerDock.h"
#include "GroupBar.h"
#include "NewAgentDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include <QAction>
#include <QApplication>
#include <QCheckBox>
#include <QDockWidget>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QMenu>
#include <QMenuBar>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QSplitter>
#include <QStatusBar>
#include <QStyle>
#include <QTabWidget>
#include <QToolBar>
#include <QVBoxLayout>
#include <QWidget>

MainWindow::MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
                       QWidget* parent)
    : QMainWindow(parent), m_controller(controller), m_groupModel(groupModel), m_fileTreeModel(fileTreeModel) {
    setWindowTitle("BondSymphonic");
    resize(1400, 900);
    // Central first: the File, Edit and View items act on the editor area, so
    // it has to exist before the menus that reach into it are built.
    buildCentral();
    buildMenus();
    buildDocks();
    buildToolBar();
    buildStatusBar();
    connectController();
    onConnectionStateChanged();
    updateWorkspaceStatus();
    onActiveTabChanged();
    updateEditActions();
}

void MainWindow::buildMenus() {
    auto* file = menuBar()->addMenu("&File");
    file->addAction("&New Agent…", this, &MainWindow::onNewAgent);
    file->addSeparator();
    auto* save = file->addAction("&Save", this, [this] {
        if (EditorWidget* editor = m_editorArea->currentEditor()) {
            editor->save();
        }
    });
    save->setShortcut(QKeySequence::Save);
    // The working Ctrl+S is the one the focused editor pane owns. A second,
    // window-wide shortcut on the same keys would make both ambiguous and
    // neither would fire, so this one is scoped to the menu bar, where it can
    // never match: it is here to print "Ctrl+S" beside the item.
    save->setShortcutContext(Qt::WidgetShortcut);
    auto* saveAll = file->addAction("Save A&ll", this, [this] { m_editorArea->saveAll(); });
    saveAll->setShortcut(QKeySequence("Ctrl+Shift+S"));
    file->addSeparator();
    file->addAction("E&xit", this, &QWidget::close);

    auto* edit = menuBar()->addMenu("&Edit");
    addEditAction(edit, "&Undo", QKeySequence::Undo, &QPlainTextEdit::undo);
    addEditAction(edit, "&Redo", QKeySequence::Redo, &QPlainTextEdit::redo);
    edit->addSeparator();
    addEditAction(edit, "Cu&t", QKeySequence::Cut, &QPlainTextEdit::cut);
    addEditAction(edit, "&Copy", QKeySequence::Copy, &QPlainTextEdit::copy);
    addEditAction(edit, "&Paste", QKeySequence::Paste, &QPlainTextEdit::paste);
    edit->addSeparator();
    addEditAction(edit, "Select &All", QKeySequence::SelectAll, &QPlainTextEdit::selectAll);

    auto* view = menuBar()->addMenu("&View");
    view->addAction("&Swap editor and agent", this,
                    [this] { m_centerSplitter->insertWidget(0, m_centerSplitter->widget(1)); });
    menuBar()->addMenu("&Workspace");
    menuBar()->addMenu("&Run");
    auto* help = menuBar()->addMenu("&Help");
    help->addAction("&About BondSymphonic…", this, &MainWindow::onAbout);
    help->addAction("About &Qt", qApp, &QApplication::aboutQt);
}

void MainWindow::onAbout() {
    QMessageBox box(this);
    box.setWindowTitle("About BondSymphonic");
    box.setIconPixmap(branding::logo(96));
    box.setTextFormat(Qt::RichText);
    box.setText(QStringLiteral("<h2 style=\"margin-bottom:0\">BondSymphonic</h2>"
                               "<p style=\"margin-top:2px\">Version %1</p>"
                               "<p>An IDE for orchestrating coding agents: every agent works in its own "
                               "git worktree inside a sandbox, and you conduct from here.</p>"
                               "<p><a href=\"https://github.com/philipkaare/BondSymphonic\">"
                               "github.com/philipkaare/BondSymphonic</a></p>"
                               "<p style=\"color:gray\">Rust + Qt %2</p>")
                    .arg(branding::version(), QString::fromLatin1(qVersion())));
    box.setStandardButtons(QMessageBox::Ok);
    box.exec();
}

void MainWindow::buildCentral() {
    auto* central = new QWidget(this);
    auto* layout = new QVBoxLayout(central);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    m_groupBar = new GroupBar(m_groupModel, central);
    layout->addWidget(m_groupBar);

    m_centerSplitter = new QSplitter(Qt::Horizontal, central);
    m_editorArea = new EditorArea(m_centerSplitter);
    m_agentArea = new AgentArea(m_centerSplitter);
    m_agentArea->setPlaceholderText("No agent selected");
    m_centerSplitter->addWidget(m_editorArea);
    m_centerSplitter->addWidget(m_agentArea);
    m_centerSplitter->setStretchFactor(0, 3);
    m_centerSplitter->setStretchFactor(1, 2);
    // The stretch factors only govern space handed out on a later resize. The
    // first division comes from the children's size hints, and the agent area's
    // is the placeholder label's until a terminal is created in it, which
    // leaves the pane a couple of dozen columns wide for the life of the
    // window. Seeding the same 3:2 explicitly gives the terminal a usable width
    // from the start; the splitter rescales the pair to the width it has.
    m_centerSplitter->setSizes({ 900, 600 });
    layout->addWidget(m_centerSplitter, 1);
    setCentralWidget(central);

    QObject::connect(m_groupBar, &GroupBar::newAgentRequested, this, &MainWindow::onNewAgent);
    QObject::connect(m_groupBar, &GroupBar::destroyRequested, this, &MainWindow::onDestroyRequested);
}

void MainWindow::buildDocks() {
    m_explorer = new ExplorerDock(m_fileTreeModel, this);
    addDockWidget(Qt::LeftDockWidgetArea, m_explorer);
    QObject::connect(m_explorer, &ExplorerDock::fileActivated, this, [this](const QString& path) {
        m_editorArea->openFile(activeWorkspaceId(), path);
    });

    auto* bottom = new QDockWidget("Output", this);
    bottom->setObjectName("BottomDock");
    m_bottomTabs = new QTabWidget(bottom);
    m_bottomTabs->addTab(new QPlainTextEdit(m_bottomTabs), "Run");
    m_shellArea = new AgentArea(m_bottomTabs);
    m_shellArea->setPlaceholderText("No workspace selected");
    m_bottomTabs->addTab(m_shellArea, "Terminal");
    bottom->setWidget(m_bottomTabs);
    addDockWidget(Qt::BottomDockWidgetArea, bottom);
}

void MainWindow::buildToolBar() {
    auto* toolBar = addToolBar("Main");
    toolBar->setObjectName("MainToolBar");
    toolBar->setMovable(false);
    toolBar->setToolButtonStyle(Qt::ToolButtonTextBesideIcon);
    auto* refresh = toolBar->addAction(style()->standardIcon(QStyle::SP_BrowserReload), "Refresh");
    refresh->setToolTip("Reload the Explorer file tree");
    QObject::connect(refresh, &QAction::triggered, this, [this] { m_explorer->refresh(); });
}

void MainWindow::buildStatusBar() {
    m_sandboxIdleText = "sandbox: -";
    m_daemonLabel = new QLabel(this);
    m_sandboxLabel = new QLabel(m_sandboxIdleText, this);
    m_branchLabel = new QLabel("branch: -", this);
    m_costLabel = new QLabel("$0.00", this);
    statusBar()->addWidget(m_daemonLabel);
    statusBar()->addWidget(m_sandboxLabel);
    statusBar()->addWidget(m_branchLabel);
    statusBar()->addPermanentWidget(m_costLabel);
}

void MainWindow::connectController() {
    QObject::connect(m_controller, &AppController::connectionStateChanged, this, &MainWindow::onConnectionStateChanged);
    QObject::connect(m_controller, &AppController::statusMessageChanged, this, &MainWindow::onConnectionStateChanged);
    QObject::connect(m_controller, &AppController::daemonVersionChanged, this, &MainWindow::onConnectionStateChanged);
    // No showMessage() here: a temporary status bar message hides every addWidget()
    // widget while it is shown, including the sandbox label the details hang off.
    QObject::connect(m_controller, &AppController::prereqWarning, this, [this](const QString& msg) {
        m_sandboxIdleText = "sandbox: prerequisites missing";
        m_sandboxLabel->setToolTip(msg);
        updateWorkspaceStatus();
    });

    QObject::connect(m_controller, &AppController::workspacesListed, this,
                     [this](const QString& json) { m_groupModel->reconcile(json); });
    QObject::connect(m_controller, &AppController::workspaceCreated, this,
                     [this](const QString& info, const QString& group, const QString& adapter, const QString& command) {
                         m_groupModel->addTab(info, group, adapter, command);
                     });
    QObject::connect(m_controller, &AppController::workspaceChanged, this,
                     [this](const QString& info) { m_groupModel->applyWorkspaceInfo(info); });
    QObject::connect(m_controller, &AppController::workspaceDestroyed, this, &MainWindow::onWorkspaceDestroyed);
    // The daemon discarded events, so every terminal has a hole in it and says so.
    QObject::connect(m_controller, &AppController::outputDropped, this, [this](::std::int64_t) {
        for (TerminalSession* session : m_agentArea->sessions()) {
            session->noteOutputDropped();
        }
        for (TerminalSession* session : m_shellArea->sessions()) {
            session->noteOutputDropped();
        }
    });
    QObject::connect(m_controller, &AppController::operationFailed, this, &MainWindow::onOperationFailed);
    QObject::connect(m_groupModel, &GroupModel::changed, this, &MainWindow::updateWorkspaceStatus);
    QObject::connect(m_groupModel, &GroupModel::changed, this, &MainWindow::onActiveTabChanged);

    // These three are the only way anything on the Rust side reaches the
    // editor area.
    QObject::connect(m_controller, &AppController::openFileRequested, this,
                     [this](const QString& workspaceId, const QString& path) {
                         m_editorArea->openFile(workspaceId, path);
                     });
    QObject::connect(m_controller, &AppController::openDiffRequested, this,
                     [this](const QString& workspaceId, const QString& path) {
                         m_editorArea->openDiff(workspaceId, path);
                     });
    QObject::connect(m_controller, &AppController::saveAllRequested, this,
                     [this] { m_editorArea->saveAll(); });

    QObject::connect(m_editorArea, &EditorArea::currentEditorChanged, this,
                     [this](EditorWidget*) { updateEditActions(); });
    QObject::connect(qApp, &QApplication::focusChanged, this,
                     [this](QWidget*, QWidget*) { updateEditActions(); });
}

void MainWindow::addEditAction(QMenu* menu, const QString& text,
                               QKeySequence::StandardKey shortcut,
                               void (QPlainTextEdit::*slot)()) {
    QAction* action = menu->addAction(text, this, [this, slot] { forwardToEditor(slot); });
    action->setShortcut(shortcut);
    m_editActions.append(action);
}

void MainWindow::forwardToEditor(void (QPlainTextEdit::*slot)()) {
    EditorWidget* editor = m_editorArea->currentEditor();
    if (editor != nullptr) {
        (editor->view()->*slot)();
    }
}

void MainWindow::updateEditActions() {
    EditorWidget* editor = m_editorArea->currentEditor();
    const QWidget* focus = QApplication::focusWidget();
    const bool live = editor != nullptr && focus != nullptr &&
                      (focus == editor->view() || editor->view()->isAncestorOf(focus));
    for (QAction* action : m_editActions) {
        action->setEnabled(live);
    }
}

void MainWindow::onConnectionStateChanged() {
    // AppController composes the full text, including the version suffix.
    m_daemonLabel->setText(m_controller->getStatusMessage());
}

void MainWindow::onNewAgent() {
    NewAgentDialog dialog(m_controller, m_groupModel, this);
    dialog.setGroup(m_groupBar->currentGroupName());
    m_newAgentDialog = &dialog;
    const int result = dialog.exec();
    m_newAgentDialog = nullptr;
    if (result != QDialog::Accepted) {
        return;
    }
    m_controller->createWorkspace(dialog.repoPath(), dialog.baseBranch(), dialog.name(), dialog.group(),
                                  dialog.adapter(), dialog.command());
}

void MainWindow::onDestroyRequested(const QString& workspaceId) {
    QMessageBox box(this);
    box.setIcon(QMessageBox::Question);
    box.setWindowTitle("Destroy workspace");
    box.setText("Destroy this workspace? Its sandbox and worktree are removed.");
    box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    auto* force = new QCheckBox("Force (discard changes)", &box);
    box.setCheckBox(force);
    if (box.exec() != QMessageBox::Yes) {
        return;
    }
    m_controller->destroyWorkspace(workspaceId, force->isChecked());
}

void MainWindow::onOperationFailed(const QString& op, const QString& message) {
    // The New Agent dialog reports its own inspection failures inline, and while
    // it is up it is modal, so a box parented to this window could not be closed.
    if (!m_newAgentDialog.isNull()) {
        if (op == "repo.inspect") {
            return;
        }
        QMessageBox::warning(m_newAgentDialog, op, message);
        return;
    }
    QMessageBox::warning(this, op, message);
}

QJsonObject MainWindow::activeTab() const {
    const QString tabJson = m_groupModel->activeTabJson();
    if (tabJson.isEmpty()) {
        return QJsonObject();
    }
    return QJsonDocument::fromJson(tabJson.toUtf8()).object();
}

QString MainWindow::activeWorkspaceId() const {
    return activeTab().value("workspace_id").toString();
}

void MainWindow::onActiveTabChanged() {
    const QJsonObject active = activeTab();
    if (active.isEmpty()) {
        m_agentArea->showPlaceholder();
        m_shellArea->showPlaceholder();
        m_explorer->setWorkspace(QString());
        return;
    }
    const QString workspaceId = active.value("workspace_id").toString();
    m_explorer->setWorkspace(workspaceId);
    // Both areas create their terminal on the workspace's first activation and
    // keep it afterwards, so this runs on every model change and is a no-op
    // once the pane exists.
    m_agentArea->showWorkspace(workspaceId, active.value("adapter").toString(),
                               active.value("command").toString());
    // The shell tab is a plain login shell in the same sandbox, whatever the
    // tab's adapter is.
    m_shellArea->showWorkspace(workspaceId, "terminal", QString());
}

void MainWindow::onWorkspaceDestroyed(const QString& workspaceId) {
    // Panes first: the model change that follows re-selects a surviving tab,
    // and the areas must no longer hold the dead one when it does. The editor
    // tabs go with them: there is nothing left to save the file to.
    m_agentArea->removeWorkspace(workspaceId);
    m_shellArea->removeWorkspace(workspaceId);
    m_editorArea->closeWorkspace(workspaceId);
    m_groupModel->removeWorkspace(workspaceId);
}

void MainWindow::updateWorkspaceStatus() {
    const int group = m_groupModel->activeGroupIndex();
    const int tab = m_groupModel->activeTabIndex();
    const QJsonObject active = activeTab();
    if (active.isEmpty()) {
        m_branchLabel->setText("branch: -");
        m_sandboxLabel->setText(m_sandboxIdleText);
        return;
    }
    m_branchLabel->setText(QString("branch: %1").arg(active.value("branch").toString()));
    // The word itself comes from the model; this only frames it.
    m_sandboxLabel->setText(QString("sandbox: %1").arg(m_groupModel->statusWord(group, tab)));
}
