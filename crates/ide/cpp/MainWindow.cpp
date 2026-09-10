#include "MainWindow.h"
#include "AgentArea.h"
#include "Branding.h"
#include "CodeView.h"
#include "EditorArea.h"
#include "EditorWidget.h"
#include "ExplorerDock.h"
#include "GroupBar.h"
#include "NewAgentDialog.h"
#include "RunPanel.h"
#include "SettingsDialog.h"
#include "SetupPage.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/changes_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/run_panel.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/transcript_model.cxxqt.h"
#include <QAction>
#include <QApplication>
#include <QChar>
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
#include <QTabWidget>
#include <QVBoxLayout>
#include <QWidget>

MainWindow::MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
                       ChangesModel* changesModel, RunPanelModel* runModel, QWidget* parent)
    : QMainWindow(parent), m_controller(controller), m_groupModel(groupModel),
      m_fileTreeModel(fileTreeModel), m_changesModel(changesModel), m_runModel(runModel) {
    setWindowTitle("BondSymphonic");
    resize(1400, 900);
    // Central first: the File, Edit and View items act on the editor area, so
    // it has to exist before the menus that reach into it are built.
    buildCentral();
    buildMenus();
    buildDocks();
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
    m_runPanel = new RunPanel(m_runModel, m_bottomTabs);
    m_bottomTabs->addTab(m_runPanel, "Run");
    m_shellArea = new AgentArea(m_bottomTabs);
    m_shellArea->setPlaceholderText("No workspace selected");
    m_bottomTabs->addTab(m_shellArea, "Terminal");
    bottom->setWidget(m_bottomTabs);
    addDockWidget(Qt::BottomDockWidgetArea, bottom);
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
                     [this](const QString& info, const QString& group, const QString& adapter,
                            const QString& command, const QString& options,
                            const QString& runConfig) {
                         m_groupModel->addTab(info, group, adapter, command, options);
                         const QString workspaceId =
                             QJsonDocument::fromJson(info.toUtf8()).object().value("id").toString();
                         // After `addTab`, which is what creates the tab this
                         // writes on. The Run panel reads it back off the tab
                         // on every activation, so recording it here is what
                         // makes the dialog's choice survive a tab switch and a
                         // restored session.
                         if (!runConfig.isEmpty()) {
                             m_groupModel->setTabRunConfig(workspaceId, runConfig);
                         }
                         // A Claude workspace is created with an agent start
                         // already on its way, and the tab is shown before that
                         // answers. The pane has to know, or it offers a Start
                         // button for a start that is already running.
                         if (adapter == QLatin1String("claude")) {
                             m_agentArea->setStarting(workspaceId, true);
                         }
                     });
    QObject::connect(m_controller, &AppController::workspaceChanged, this,
                     [this](const QString& info) { m_groupModel->applyWorkspaceInfo(info); });
    // Before `onWorkspaceDestroyed`, which takes the tab out and so provokes
    // the active-tab change that points the Run panel at whatever survived: the
    // dead workspace's runs, logs and blocked hosts have to be gone by then.
    QObject::connect(m_controller, &AppController::workspaceDestroyed, m_runModel,
                     &RunPanelModel::forgetWorkspace);
    QObject::connect(m_controller, &AppController::workspaceDestroyed, this, &MainWindow::onWorkspaceDestroyed);
    // A host the workspace's proxy refused. The queue is per workspace and
    // lives in the model, so one blocked while another tab is in front waits
    // there rather than being shown over the wrong workspace.
    QObject::connect(m_controller, &AppController::networkDenied, m_runModel,
                     &RunPanelModel::noteDenied);
    // Answering that toast from outside the panel. Only the workspace the panel
    // is actually showing may be answered: an allow routed anywhere else would
    // extend the allowlist of a workspace the user is not looking at, which is
    // the one thing a network decision must never do behind their back.
    QObject::connect(m_controller, &AppController::allowHostRequested, this,
                     [this](const QString& workspaceId, const QString& host) {
                         if (m_runModel->getWorkspaceId() != workspaceId) {
                             qWarning("allow host not routed: the run panel is showing %s, not %s",
                                      qUtf8Printable(m_runModel->getWorkspaceId()),
                                      qUtf8Printable(workspaceId));
                             return;
                         }
                         m_runModel->allowHost(host);
                     });
    // `setWorkspace` publishes what it already knows and only then re-detects,
    // so the tab's own configuration is offered again each time the list
    // changes, until the model accepts it.
    QObject::connect(m_runModel, &RunPanelModel::configsChanged, this, [this] {
        if (m_pendingRunConfig.isEmpty()) {
            return;
        }
        m_runModel->selectConfig(m_pendingRunConfig);
        if (m_runModel->getSelectedConfig() == m_pendingRunConfig) {
            m_pendingRunConfig.clear();
        }
    });
    // The tab records the agent so a restored session finds it again, and the
    // pane attaches to it. Both, in that order: `onActiveTabChanged` reads the
    // id back out of the tab.
    QObject::connect(m_controller, &AppController::agentStarted, this,
                     [this](const QString& workspaceId, const QString& agentId) {
                         m_agentArea->setStarting(workspaceId, false);
                         m_groupModel->setAgent(workspaceId, agentId);
                         m_agentArea->setAgent(workspaceId, agentId);
                         rebindCost();
                     });
    // A transcript pane with no agent -- a restored session, or one whose agent
    // exited -- offers to start one. The options are the tab's own, so a
    // restarted agent gets the model and permission mode the user chose.
    QObject::connect(m_agentArea, &AgentArea::startAgentRequested, this,
                     &MainWindow::onStartAgentRequested);
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
    // Sending and stopping from outside the pane, through the same model calls
    // the prompt box and the Stop button use. Only the pane attached to that
    // agent may be driven: a prompt routed elsewhere would land in a
    // conversation the caller never named.
    QObject::connect(m_controller, &AppController::agentSendRequested, this,
                     [this](const QString& agentId, const QString& text) {
                         if (TranscriptModel* model = activeAgentModel(agentId, "send")) {
                             model->send(text);
                         }
                     });
    QObject::connect(m_controller, &AppController::agentStopRequested, this,
                     [this](const QString& agentId) {
                         if (TranscriptModel* model = activeAgentModel(agentId, "stop")) {
                             model->stop();
                         }
                     });
    // Answering a permission request from outside the transcript view. Only the
    // view that is actually showing `requestId` may answer it: an answer routed
    // to any other pane would allow a tool the user never saw.
    QObject::connect(m_controller, &AppController::permissionReplyRequested, this,
                     [this](const QString& agentId, const QString& requestId, bool allow) {
                         const QString workspaceId = activeWorkspaceId();
                         TranscriptModel* model =
                             workspaceId.isEmpty() ? nullptr : m_agentArea->transcriptModel(workspaceId);
                         const QString pending =
                             model == nullptr ? QString() : model->getPendingJson();
                         const QString shown = QJsonDocument::fromJson(pending.toUtf8())
                                                   .object()
                                                   .value("request_id")
                                                   .toString();
                         if (model == nullptr || model->getAgentId() != agentId || shown != requestId) {
                             qWarning("permission reply not routed: no visible transcript is showing "
                                      "request %s for agent %s",
                                      qUtf8Printable(requestId), qUtf8Printable(agentId));
                             return;
                         }
                         model->reply(requestId, allow, false, QString());
                     });

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
        m_controller->createWorkspaceWithAgentAndRun(dialog.repoPath(), dialog.baseBranch(),
                                                     dialog.name(), dialog.group(),
                                                     dialog.optionsJson(), dialog.initialPrompt(),
                                                     dialog.runConfig());
        return;
    }
    m_controller->createWorkspaceWithRun(dialog.repoPath(), dialog.baseBranch(), dialog.name(),
                                         dialog.group(), dialog.adapter(), dialog.command(),
                                         dialog.runConfig());
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

TranscriptModel* MainWindow::activeAgentModel(const QString& agentId, const char* what) {
    const QString workspaceId = activeWorkspaceId();
    TranscriptModel* model =
        workspaceId.isEmpty() ? nullptr : m_agentArea->transcriptModel(workspaceId);
    if (model == nullptr || model->getAgentId() != agentId) {
        qWarning("agent %s not routed: no visible transcript is attached to agent %s", what,
                 qUtf8Printable(agentId));
        return nullptr;
    }
    return model;
}

void MainWindow::onStartAgentRequested(const QString& workspaceId) {
    const QJsonObject tab = activeTab();
    // The Start button lives on the visible pane, so the active tab is the one
    // that asked; anything else means the model moved under the click.
    if (workspaceId.isEmpty() || tab.value("workspace_id").toString() != workspaceId) {
        m_agentArea->setStarting(workspaceId, false);
        return;
    }
    m_agentArea->setStarting(workspaceId, true);
    m_controller->startAgent(workspaceId, tab.value("options_json").toString());
}

void MainWindow::onOperationFailed(const QString& op, const QString& message) {
    if (op == QLatin1String("agent.start")) {
        // The pane must stop saying it is starting something. The signal names
        // the operation and not the workspace, and only one start is ever in
        // flight, so every mark comes down.
        m_agentArea->clearStarting();
    }
    // The New Agent dialog reports its own inspection failures inline, and while
    // it is up it is modal, so a box parented to this window could not be closed.
    if (!m_newAgentDialog.isNull()) {
        // Both of the dialog's own lookups: it reports them in place, and a
        // repository with no detectable run configuration is a normal answer
        // rather than something to put a box over.
        if (op == "repo.inspect" || op == "repo.detect_run_configs") {
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
    updateWindowTitle(active);
    if (active.isEmpty()) {
        m_agentArea->showPlaceholder();
        m_shellArea->showPlaceholder();
        m_explorer->setWorkspace(QString());
        m_pendingRunConfig.clear();
        m_runModel->setWorkspace(QString(), QString());
        rebindCost();
        return;
    }
    const QString workspaceId = active.value("workspace_id").toString();
    m_explorer->setWorkspace(workspaceId);
    // After `setWorkspace`, which clears the header when it is handed an empty
    // id, and on every model change rather than only on a switch, so a branch
    // the daemon renamed reaches the strip.
    m_explorer->setWorkspaceHeader(active.value("name").toString(),
                                   active.value("branch").toString(),
                                   active.value("repo_path").toString());
    // The tab's choice first, because `setWorkspace` publishes the list it
    // already has synchronously and the `configsChanged` slot above applies
    // this to it; the worktree path is the daemon's, from `WorkspaceInfo`.
    m_pendingRunConfig = active.value("run_config").toString();
    m_runModel->setWorkspace(workspaceId, active.value("worktree_path").toString());
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

void MainWindow::updateWindowTitle(const QJsonObject& active) {
    const QString name = active.value("name").toString();
    if (name.isEmpty()) {
        setWindowTitle(QStringLiteral("BondSymphonic"));
        return;
    }
    // U+2014, the em dash, as a code point rather than as a character in a
    // literal, so no compiler's idea of this file's source encoding can change
    // what it means.
    const QString dash = QStringLiteral(" ") + QString(QChar(0x2014)) + QStringLiteral(" ");
    setWindowTitle(name + dash + active.value("branch").toString() + dash +
                   QStringLiteral("BondSymphonic"));
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

