#include "MainWindow.h"
#include "AgentArea.h"
#include "Branding.h"
#include "CodeView.h"
#include "EditorArea.h"
#include "EditorWidget.h"
#include "ExplorerDock.h"
#include "GroupBar.h"
#include "NewAgentDialog.h"
#include "SettingsDialog.h"
#include "SetupPage.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/changes_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/transcript_model.cxxqt.h"
#include <QAction>
#include <QApplication>
#include <QCheckBox>
#include <QCloseEvent>
#include <QDockWidget>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLatin1Char>
#include <QList>
#include <QMenu>
#include <QMenuBar>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QSplitter>
#include <QStackedWidget>
#include <QStatusBar>
#include <QStyle>
#include <QTabWidget>
#include <QToolBar>
#include <QVBoxLayout>
#include <QWidget>

MainWindow::MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
                       ChangesModel* changesModel, QWidget* parent)
    : QMainWindow(parent), m_controller(controller), m_groupModel(groupModel),
      m_fileTreeModel(fileTreeModel), m_changesModel(changesModel) {
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

void MainWindow::closeEvent(QCloseEvent* event) {
    // Already answered, or there is nothing to lose. `hasUnsavedEditors` is the
    // whole test: a clean editor and a diff pane have nothing on them that the
    // file on disk does not.
    if (m_closeState == CloseState::Confirmed || !m_editorArea->hasUnsavedEditors()) {
        QMainWindow::closeEvent(event);
        return;
    }
    if (m_closeState == CloseState::WaitingForSaves) {
        // The writes this close already started are still out. Asking again
        // would put a second box over a question that has been answered.
        event->ignore();
        return;
    }
    switch (m_editorArea->askUnsavedAll()) {
    case EditorArea::Unsaved::Discard:
        m_closeState = CloseState::Confirmed;
        QMainWindow::closeEvent(event);
        return;
    case EditorArea::Unsaved::Save:
        // Armed before the writes go out, not after: a save that lands while
        // this function is still running would otherwise be missed and the
        // window would sit open waiting for something that had happened.
        armCloseAfterSaves();
        m_editorArea->saveAll();
        event->ignore();
        return;
    case EditorArea::Unsaved::Cancel:
    default:
        event->ignore();
        return;
    }
}

void MainWindow::armCloseAfterSaves() {
    m_closeState = CloseState::WaitingForSaves;
    m_closeWatch.append(
        QObject::connect(m_editorArea, &EditorArea::unsavedStateChanged, this, [this] {
            if (m_closeState != CloseState::WaitingForSaves || m_editorArea->hasUnsavedEditors()) {
                return;
            }
            // Every write has landed and nothing has been typed since, so the
            // second `closeEvent` this provokes has nothing left to ask about.
            disarmCloseAfterSaves();
            m_closeState = CloseState::Confirmed;
            close();
        }));
    m_closeWatch.append(QObject::connect(m_editorArea, &EditorArea::saveFailed, this,
                                         [this](const QString&) {
                                             // The edit is still only in the pane and the pane has
                                             // said why. Closing now would throw it away, which is
                                             // exactly what the prompt was for.
                                             disarmCloseAfterSaves();
                                         }));
}

void MainWindow::disarmCloseAfterSaves() {
    for (const QMetaObject::Connection& c : m_closeWatch) {
        QObject::disconnect(c);
    }
    m_closeWatch.clear();
    m_closeState = CloseState::Idle;
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
    // The window's, at the default window scope, and the only Ctrl+S there is.
    // One action for every pane means the shortcut works with the focus in the
    // Explorer, in a terminal or in the text, and nothing competes with it.
    save->setShortcut(QKeySequence::Save);
    auto* saveAll = file->addAction("Save A&ll", this, [this] { m_editorArea->saveAll(); });
    saveAll->setShortcut(QKeySequence("Ctrl+Shift+S"));
    file->addSeparator();
    file->addAction("Se&ttings…", this, &MainWindow::onSettings);
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
    view->addAction("&Swap editor and agent", this, [this] {
        // `insertWidget` re-parents, and the splitter then re-derives its
        // division from size hints -- losing the 3:2 `buildCentral` seeded
        // precisely because the agent pane's hint is wrong.
        const QList<int> sizes = m_centerSplitter->sizes();
        m_centerSplitter->insertWidget(0, m_centerSplitter->widget(1));
        if (sizes.size() == 2) {
            m_centerSplitter->setSizes({ sizes.at(1), sizes.at(0) });
        }
    });
    menuBar()->addMenu("&Workspace");
    menuBar()->addMenu("&Run");
    auto* help = menuBar()->addMenu("&Help");
    // Available whatever the prerequisites say: the page is also how a user
    // logs in to Claude Code again after a token has expired.
    help->addAction("&Setup…", this, &MainWindow::showSetupPage);
    help->addSeparator();
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
    // The window shows one of two things: the workbench, or the setup page in
    // front of it. A stack rather than a hidden workbench, so the panes, tabs
    // and terminals behind the page keep their state while it is up.
    m_stack = new QStackedWidget(this);
    auto* central = new QWidget(m_stack);
    m_workbench = central;
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

    m_stack->addWidget(central);
    m_setupPage = new SetupPage(m_controller, m_stack);
    m_stack->addWidget(m_setupPage);
    m_stack->setCurrentWidget(central);
    setCentralWidget(m_stack);

    QObject::connect(m_setupPage, &SetupPage::completed, this, &MainWindow::showWorkbench);
    QObject::connect(m_groupBar, &GroupBar::newAgentRequested, this, &MainWindow::onNewAgent);
    QObject::connect(m_groupBar, &GroupBar::destroyRequested, this, &MainWindow::onDestroyRequested);
}

void MainWindow::showSetupPage() {
    m_stack->setCurrentWidget(m_setupPage);
}

void MainWindow::showWorkbench() {
    m_stack->setCurrentWidget(m_workbench);
}

void MainWindow::onSettings() {
    SettingsDialog dialog(m_controller, this);
    dialog.exec();
}

void MainWindow::onPrereqsChecked(const QString& json) {
    const QJsonArray items = QJsonDocument::fromJson(json.toUtf8()).array();
    bool anyFailed = false;
    for (const QJsonValue& value : items) {
        anyFailed = anyFailed || !value.toObject().value("ok").toBool();
    }
    // Which of the failures they are is the controller's judgement, not this
    // window's: it owns the list of prerequisites there is no working around.
    const bool blocked = m_controller->prereqsBlock(json);

    m_sandboxIdleText = anyFailed ? "sandbox: prerequisites missing" : "sandbox: -";
    if (!anyFailed) {
        m_sandboxLabel->setToolTip(QString());
    }
    // Only worth offering when the workbench is what is on screen. While the
    // page is up, the link would point at the page the user is already on.
    m_setupLabel->setVisible(anyFailed && !blocked);
    updateWorkspaceStatus();

    if (blocked) {
        showSetupPage();
    }
}

void MainWindow::buildDocks() {
    m_explorer = new ExplorerDock(m_fileTreeModel, m_changesModel, this);
    addDockWidget(Qt::LeftDockWidgetArea, m_explorer);
    QObject::connect(m_explorer, &ExplorerDock::fileActivated, this, [this](const QString& path) {
        m_editorArea->openFile(activeWorkspaceId(), path);
    });
    QObject::connect(m_explorer, &ExplorerDock::diffActivated, this, [this](const QString& path) {
        m_editorArea->openDiff(activeWorkspaceId(), path);
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
    m_costLabel = new QLabel(this);
    // Rich text so the offer is a link rather than an instruction to go and
    // find a menu item. The href is never followed by Qt itself.
    m_setupLabel = new QLabel("<a href=\"#setup\">Set up…</a>", this);
    m_setupLabel->setTextFormat(Qt::RichText);
    m_setupLabel->setOpenExternalLinks(false);
    m_setupLabel->setVisible(false);
    QObject::connect(m_setupLabel, &QLabel::linkActivated, this,
                     [this](const QString&) { showSetupPage(); });
    statusBar()->addWidget(m_daemonLabel);
    statusBar()->addWidget(m_sandboxLabel);
    statusBar()->addWidget(m_setupLabel);
    statusBar()->addWidget(m_branchLabel);
    statusBar()->addPermanentWidget(m_costLabel);
    updateCostLabel();
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
    // Emitted just before the warning above, and after every re-check, so this
    // is what decides which of the two views the window is in.
    QObject::connect(m_controller, &AppController::prereqsChecked, this,
                     &MainWindow::onPrereqsChecked);

    QObject::connect(m_controller, &AppController::workspacesListed, this,
                     [this](const QString& json) { m_groupModel->reconcile(json); });
    QObject::connect(m_controller, &AppController::workspaceCreated, this,
                     [this](const QString& info, const QString& group, const QString& adapter, const QString& command) {
                         m_groupModel->addTab(info, group, adapter, command);
                     });
    QObject::connect(m_controller, &AppController::workspaceChanged, this,
                     [this](const QString& info) { m_groupModel->applyWorkspaceInfo(info); });
    QObject::connect(m_controller, &AppController::workspaceDestroyed, this, &MainWindow::onWorkspaceDestroyed);
    // The tab records the agent so a restored session finds it again, and the
    // pane attaches to it. Both, in that order: `onActiveTabChanged` reads the
    // id back out of the tab.
    QObject::connect(m_controller, &AppController::agentStarted, this,
                     [this](const QString& workspaceId, const QString& agentId) {
                         m_groupModel->setAgent(workspaceId, agentId);
                         m_agentArea->setAgent(workspaceId, agentId);
                         rebindCost();
                     });
    QObject::connect(m_controller, &AppController::agentStateChanged, this,
                     [this](const QString& agentId, const QString& state, const QString& detail) {
                         m_groupModel->setAgentStatus(agentId, state, detail);
                     });
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
}

void MainWindow::addEditAction(QMenu* menu, const QString& text,
                               QKeySequence::StandardKey shortcut,
                               void (QPlainTextEdit::*slot)()) {
    // The sequence goes in the label after a tab, which the menu draws
    // right-aligned exactly where a registered shortcut would appear. Setting
    // it as the action's shortcut instead would register it window-wide.
    QAction* action = menu->addAction(
        text + QLatin1Char('\t') + QKeySequence(shortcut).toString(QKeySequence::NativeText), this,
        [this, slot] { forwardToEditor(slot); });
    m_editActions.append(action);
}

void MainWindow::forwardToEditor(void (QPlainTextEdit::*slot)()) {
    EditorWidget* editor = m_editorArea->currentEditor();
    if (editor != nullptr) {
        (editor->view()->*slot)();
    }
}

void MainWindow::updateEditActions() {
    const bool live = m_editorArea->currentEditor() != nullptr;
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
    if (dialog.adapter() == "claude") {
        // One call: the workspace, the agent in it and its opening prompt. The
        // controller emits `workspaceCreated` as soon as the workspace exists,
        // so a slow `agent.start` happens in front of the user.
        m_controller->createWorkspaceWithAgent(dialog.repoPath(), dialog.baseBranch(), dialog.name(),
                                               dialog.group(), dialog.optionsJson(),
                                               dialog.initialPrompt());
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
        rebindCost();
        return;
    }
    const QString workspaceId = active.value("workspace_id").toString();
    m_explorer->setWorkspace(workspaceId);
    // Both areas create their terminal on the workspace's first activation and
    // keep it afterwards, so this runs on every model change and is a no-op
    // once the pane exists.
    // The agent id comes from the tab, so a workspace listed at start-up with
    // an agent already running attaches without waiting for `agentStarted`.
    m_agentArea->showWorkspace(workspaceId, active.value("adapter").toString(),
                               active.value("command").toString(),
                               active.value("agent_id").toString());
    // The shell tab is a plain login shell in the same sandbox, whatever the
    // tab's adapter is.
    m_shellArea->showWorkspace(workspaceId, "terminal", QString());
    rebindCost();
}

void MainWindow::rebindCost() {
    QObject::disconnect(m_costWatch);
    const QString workspaceId = activeWorkspaceId();
    TranscriptModel* model =
        workspaceId.isEmpty() ? nullptr : m_agentArea->transcriptModel(workspaceId);
    if (model != nullptr) {
        m_costWatch = QObject::connect(model, &TranscriptModel::costUsdChanged, this,
                                       &MainWindow::updateCostLabel);
    }
    updateCostLabel();
}

void MainWindow::updateCostLabel() {
    const QString workspaceId = activeWorkspaceId();
    TranscriptModel* model =
        workspaceId.isEmpty() ? nullptr : m_agentArea->transcriptModel(workspaceId);
    // A terminal tab has no transcript and so no cost; the label still shows a
    // number, because a blank one reads as "unknown" rather than as "nothing".
    const double cost = model == nullptr ? 0.0 : model->getCostUsd();
    m_costLabel->setText(QString("$%1").arg(cost, 0, 'f', 4));
}

void MainWindow::onWorkspaceDestroyed(const QString& workspaceId) {
    // Panes first: the model change that follows re-selects a surviving tab,
    // and the areas must no longer hold the dead one when it does. The editor
    // tabs go with them: there is nothing left to save the file to.
    m_agentArea->removeWorkspace(workspaceId);
    m_shellArea->removeWorkspace(workspaceId);
    m_editorArea->closeWorkspace(workspaceId);
    m_groupModel->removeWorkspace(workspaceId);
    // After the model change, so the tab this rebinds to is the one that
    // survived rather than the one that has just gone.
    rebindCost();
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
