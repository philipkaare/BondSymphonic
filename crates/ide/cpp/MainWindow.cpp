#include "MainWindow.h"
#include "AgentArea.h"
#include "Branding.h"
#include "ChangesToolbar.h"
#include "CloseGroupDialog.h"
#include "CodeView.h"
#include "EditorArea.h"
#include "EditorWidget.h"
#include "ExplorerDock.h"
#include "GroupBar.h"
#include "NewAgentDialog.h"
#include "RunPanel.h"
#include "SettingsDialog.h"
#include "SetupPage.h"
#include "Theme.h"
#include "WorkspaceBanner.h"
#include "WorkspaceLabel.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/changes_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/run_panel.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/transcript_model.cxxqt.h"
#include <QAction>
#include <QActionGroup>
#include <QApplication>
#include <QByteArray>
#include <QChar>
#include <QCheckBox>
#include <QCloseEvent>
#include <QDesktopServices>
#include <QDockWidget>
#include <QEvent>
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
#include <QPushButton>
#include <QTabBar>
#include <QStatusBar>
#include <QTabWidget>
#include <QTimer>
#include <QUrl>
#include <QVBoxLayout>
#include <QWidget>
#include <cstdio>

namespace {

/// The version `saveState` stamps the dock layout with, and the only one
/// `restoreState` will take back.
///
/// Version 1 was the arrangement with the agent pane in a splitter in the
/// centre, whose `splitter_sizes` are gone from `state.json` with it. A state
/// saved then describes a window with two docks where there are now three, and
/// the version number is what makes Qt discard it rather than apply the half it
/// recognises.
constexpr int kWindowStateVersion = 2;

/// The steps in `BS_MENU_TEST`, as one string. **Test-only**: empty in every
/// ordinary run, and then everything below that reads it is inert.
///
/// Gated on `BS_SMOKE_SCRIPT` as well, for the reason spelled out beside
/// `GroupBar`'s copy of this: what the seam does here is return early from a
/// destroy and a close-group, so `BS_MENU_TEST` on its own would let one stray
/// variable disable both actions. `BS_SMOKE_SCRIPT` is the IDE's existing
/// automated-run switch and no user sets it.
QString menuTest() {
    if (qEnvironmentVariableIsEmpty("BS_SMOKE_SCRIPT")) {
        return QString();
    }
    return qEnvironmentVariable("BS_MENU_TEST");
}

/// The workspace a targeted seam step names, as `step=<workspace id>`, or empty
/// when the step is not armed. **Test-only**, and the only way the banner
/// steps and `destroy-yes` choose what to act on: they act on the workspace a
/// fixture named and on nothing else, so a stray variable can never reach a
/// workspace the run did not make.
///
/// `destroy-yes` sends real destroys, so it is additionally refused unless the
/// run is pointed at a daemon of its own with `BS_DAEMON_ADDR`.
QString menuTestTarget(const char* step) {
    const QString name = QString::fromLatin1(step) + QLatin1Char('=');
    if (qstrcmp(step, "destroy-yes") == 0 && qEnvironmentVariableIsEmpty("BS_DAEMON_ADDR")) {
        return QString();
    }
    for (const QString& item : menuTest().split(QLatin1Char(','), Qt::SkipEmptyParts)) {
        const QString trimmed = item.trimmed();
        if (trimmed.startsWith(name)) {
            return trimmed.mid(name.size());
        }
    }
    return QString();
}

/// How Qt's text formats are spelled in a test line. `QLabel`'s default is
/// `AutoText`, which is what makes an unset format worth reporting at all.
const char* textFormatWord(Qt::TextFormat format) {
    switch (format) {
    case Qt::PlainText:
        return "PlainText";
    case Qt::RichText:
        return "RichText";
    case Qt::MarkdownText:
        return "MarkdownText";
    default:
        return "AutoText";
    }
}

} // namespace

MainWindow::MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
                       ChangesModel* changesModel, RunPanelModel* runModel, QWidget* parent)
    : QMainWindow(parent), m_controller(controller), m_groupModel(groupModel),
      m_fileTreeModel(fileTreeModel), m_changesModel(changesModel), m_runModel(runModel) {
    setWindowTitle("BondSymphonic");
    resize(1400, 900);
    // The widgets before the menus that act on them: the File, Edit and View
    // items reach into the editor area, and the Window menu is made of the
    // docks' own toggle actions, so both have to exist before `buildMenus`
    // runs. A menu built first could only hold hand-written stand-ins for
    // them.
    buildCentral();
    buildDocks();
    buildMenus();
    buildStatusBar();
    connectController();
    // Last, and before the controller connects: the layout is installed on a
    // window that has all its widgets and no daemon news yet, so nothing it
    // restores can be overwritten by an answer arriving mid-restore.
    onStateLoaded(m_controller->loadState());
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
        commitClose(event);
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
        commitClose(event);
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

void MainWindow::commitClose(QCloseEvent* event) {
    // Only the two branches that really close reach here, so a cancelled close
    // leaves a fully working IDE. Order matters within them: `prepareQuit`
    // first, because the connection this close is about to drop must not read
    // as a loss -- the reconnect loop would otherwise relaunch a daemon inside
    // WSL for an IDE on its way out -- and `flushState` second, because a
    // change made in the last half second is still on its debounce timer and
    // there is no half second left. Nothing between the two can refuse.
    m_controller->prepareQuit();
    m_controller->flushState();
    QMainWindow::closeEvent(event);
}

void MainWindow::moveEvent(QMoveEvent* event) {
    QMainWindow::moveEvent(event);
    noteWindowState();
}

void MainWindow::resizeEvent(QResizeEvent* event) {
    QMainWindow::resizeEvent(event);
    noteWindowState();
}

void MainWindow::changeEvent(QEvent* event) {
    QMainWindow::changeEvent(event);
    if (event->type() == QEvent::PaletteChange) {
        applySeparatorBand();
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
    // Straight to `close()`: Exit is the same close the title bar's button
    // makes, and `closeEvent` is the one place that decides whether it happens
    // and announces the quit once it has. Announcing it here as well would
    // stop the IDE reconnecting after an Exit the user then cancelled.
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
        // Which side the agent is on is now a fact about the dock, and the one
        // fact: `dockWidgetArea` is where the menu reads it and `saveState`
        // is what remembers it, so there is nothing left for the window to
        // keep a second copy of.
        const Qt::DockWidgetArea now = dockWidgetArea(m_agentDock);
        addDockWidget(now == Qt::RightDockWidgetArea ? Qt::LeftDockWidgetArea
                                                     : Qt::RightDockWidgetArea,
                      m_agentDock);
        noteWindowState();
    });
    // Nothing here is new. Five of these are the Changes tab's toolbar, which
    // is invisible unless that tab is open, and two were reachable only by
    // right-clicking a tab. The menu offers the toolbar's own `QAction`
    // objects rather than a second set: their enablement is the awkward part
    // -- no workspace, or one with a request already out -- and a parallel set
    // would have to be greyed in parallel and would drift.
    auto* workspace = menuBar()->addMenu("&Workspace");
    workspace->addAction("&New Agent…", this, &MainWindow::onNewAgent);
    m_restartAgentAction = workspace->addAction(
        "&Restart agent", this, [this] { onStartAgentRequested(activeWorkspaceId()); });
    m_restartAgentAction->setShortcut(QKeySequence("Ctrl+Shift+R"));
    workspace->addSeparator();
    m_nextAgentAction = workspace->addAction("Ne&xt agent", this, [this] { stepAgent(1); });
    m_nextAgentAction->setShortcut(QKeySequence("Ctrl+PgDown"));
    m_prevAgentAction = workspace->addAction("&Previous agent", this, [this] { stepAgent(-1); });
    m_prevAgentAction->setShortcut(QKeySequence("Ctrl+PgUp"));
    workspace->addSeparator();
    ChangesToolbar* changes = m_explorer->changesToolbar();
    workspace->addAction(changes->mergeAction());
    workspace->addAction(changes->rebaseAction());
    workspace->addAction(changes->squashAction());
    workspace->addAction(changes->prAction());
    workspace->addAction(changes->discardAction());
    workspace->addSeparator();
    // Both go through the handlers the tab's context menu uses, which read the
    // workspace once and put a named question in front of the user. The menu
    // supplies the name from the same tab it took the id from, so the question
    // and the act cannot be about different workspaces.
    m_destroyAction = workspace->addAction("&Destroy workspace…", this, [this] {
        onDestroyRequested(activeWorkspaceId(), activeTab().value("name").toString());
    });
    m_closeGroupAction =
        workspace->addAction("&Close group…", this, [this] { onCloseGroup(activeGroupName()); });

    // The panel's own actions again, for the same reason: Start and Stop were
    // buttons inside one tab of one dock, so running the thing you have just
    // built meant finding that tab first. A disabled Stop is now disabled in
    // both places because it is one action, not two that agree.
    m_runMenu = menuBar()->addMenu("&Run");
    m_runMenu->addAction(m_runPanel->runAction());
    m_runPanel->runAction()->setShortcut(QKeySequence(Qt::Key_F5));
    m_runMenu->addAction(m_runPanel->stopAction());
    m_runPanel->stopAction()->setShortcut(QKeySequence(Qt::ShiftModifier | Qt::Key_F5));
    m_runMenu->addAction(m_runPanel->restartAction());
    m_runMenu->addSeparator();
    // The detected configurations go between these two separators. Qt collapses
    // the pair while there are none, so a workspace the daemon found nothing in
    // shows one separator rather than an empty gap.
    m_runConfigMenuAnchor = m_runMenu->addSeparator();
    m_runMenu->addAction(m_runPanel->openAction());
    m_runMenu->addSeparator();
    m_runMenu->addAction(m_runPanel->clearOutputAction());
    m_runMenu->addAction(m_runPanel->copyOutputAction());
    m_runMenu->addAction(m_runPanel->allowHostAction());
    QObject::connect(m_runPanel, &RunPanel::configurationsChanged, this,
                     &MainWindow::rebuildRunConfigMenu);
    // The panel built its first list in its own constructor, before this menu
    // existed to hear about it.
    rebuildRunConfigMenu();

    // Qt's own toggle actions rather than hand-written ones: the tick and the
    // dock cannot then disagree, and a dock closed by its own X updates the
    // menu without anything here having to notice. Each entry's text is the
    // dock's window title, which is also what its title bar reads, so the way
    // back is spelled the same as the thing that went.
    //
    // `Wi&ndow` rather than the obvious `&Window`: `&Workspace` is already in
    // the bar and claims Alt+W, and it comes first, so the obvious spelling
    // would give this menu an accelerator that opens somebody else's. Alt+N is
    // free.
    auto* window = menuBar()->addMenu("Wi&ndow");
    window->addAction(m_explorer->toggleViewAction());
    window->addAction(m_agentDock->toggleViewAction());
    window->addAction(m_bottomDock->toggleViewAction());
    window->addSeparator();
    window->addAction("&Reset layout", this, &MainWindow::resetLayout);

    auto* help = menuBar()->addMenu("&Help");
    // No "Setup..." here any more. Logging in to Claude Code or GitHub is a
    // setting the user comes back to -- a token expires, an account changes --
    // and File > Settings is where they look for one; Help is where they look
    // for documentation.
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
    // One central widget. The setup page used to sit in a stack in front of
    // this one; it lives in the Settings dialog now, so the workbench is
    // always what the window shows and its panes are never torn between two
    // pages.
    auto* central = new QWidget(this);
    auto* layout = new QVBoxLayout(central);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    m_groupBar = new GroupBar(m_groupModel, central);
    layout->addWidget(m_groupBar);

    // The editor and nothing else. The agent pane used to share a splitter
    // with it here and is a dock now, so what is left in the centre is the
    // group bar and the documents under it -- which is the part of the window
    // that cannot be closed, moved or floated away.
    m_editorArea = new EditorArea(central);
    layout->addWidget(m_editorArea, 1);

    setCentralWidget(central);

    QObject::connect(m_groupBar, &GroupBar::newAgentRequested, this, &MainWindow::onNewAgent);
    QObject::connect(m_groupBar, &GroupBar::destroyRequested, this, &MainWindow::onDestroyRequested);
    QObject::connect(m_groupBar, &GroupBar::closeGroupRequested, this, &MainWindow::onCloseGroup);

    if (menuTest().contains(QLatin1String("widgets"))) {
        // Not `singleShot(0)`: one of the four values is a tab's text colour,
        // and at the first turn of the event loop the daemon has not answered
        // `workspace.list` yet, so there are no tabs. Waits instead for the
        // model to report a tab in error, which is the state the fixture puts
        // one workspace in, and reports once. `GroupBar` connected to `changed`
        // before this did, and Qt delivers in connection order, so the tab bar
        // has already been rebuilt when this runs.
        QObject::connect(m_groupModel, &GroupModel::changed, this, [this] {
            if (m_seamWidgetsReported) {
                return;
            }
            const int group = m_groupModel->activeGroupIndex();
            for (int i = 0; i < m_groupModel->tabCount(group); ++i) {
                if (m_groupModel->tabStatus(group, i) == GroupBar::kStatusError) {
                    m_seamWidgetsReported = true;
                    reportSeamWidgets();
                    return;
                }
            }
        });
    }
    if (menuTest().contains(QLatin1String("signal-counts"))) {
        // One line per firing rather than a total read at a chosen moment:
        // counting lines on stdout after the run needs nothing to be timed. The
        // other half of the count, `layout-recorded`, is printed by the lambda
        // that writes the layout, wherever that ends up connected.
        QObject::connect(m_groupModel, &GroupModel::changed, this,
                         [this] { announceMenuTest("model-changed", QString(), QString()); });
    }
}

bool MainWindow::announceMenuTest(const char* what, const QString& target,
                                  const QString& question) const {
    if (menuTest().isEmpty()) {
        return false;
    }
    const QByteArray line = QStringLiteral("BS_MENU_TEST %1 target=%2 question=%3\n")
                                .arg(QString::fromUtf8(what), target, question)
                                .toUtf8();
    std::fwrite(line.constData(), 1, static_cast<size_t>(line.size()), stdout);
    std::fflush(stdout);
    // Not counted towards the script's wait. Most of what reaches here is not a
    // menu step at all -- `model-changed` fires on every workspace the daemon
    // reports -- and the one step whose whole point is to print nothing would
    // never be counted at all. `GroupBar::runMenuTests` reports the steps.
    return true;
}

void MainWindow::reportSeamWidgets() {
    // Built and thrown away without being shown: what is being read is how the
    // widgets were configured, and showing it would need somebody to dismiss it.
    NewAgentDialog dialog(m_controller, m_groupModel, QString(), this);
    const QLabel* status = dialog.findChild<QLabel*>(QStringLiteral("NewAgentStatus"));
    announceMenuTest("new-agent-status",
                     status == nullptr ? QStringLiteral("missing")
                                       : QString::fromUtf8(textFormatWord(status->textFormat())),
                     QString());
    const QLabel* hint = dialog.findChild<QLabel*>(QStringLiteral("NewAgentNameHint"));
    announceMenuTest("name-hint-style",
                     hint == nullptr ? QStringLiteral("missing") : hint->styleSheet(), QString());

    // Which palette the run is on, so the expected colour is not guessed at:
    // `theme::ink` lifts an accent for a dark palette and leaves it alone on a
    // light one, and the two hexes differ.
    announceMenuTest("palette",
                     theme::isDark(palette()) ? QStringLiteral("dark") : QStringLiteral("light"),
                     QString());

    // The text colour of the first tab the model reports in error. Read off the
    // tab bar itself rather than recomputed, so it is the colour a user sees.
    const QTabBar* tabs = m_groupBar->findChild<QTabBar*>(QStringLiteral("GroupBarAgentTabs"));
    const int group = m_groupModel->activeGroupIndex();
    QString colour = QStringLiteral("missing");
    if (tabs != nullptr) {
        for (int i = 0; i < tabs->count(); ++i) {
            if (m_groupModel->tabStatus(group, i) == GroupBar::kStatusError) {
                colour = tabs->tabTextColor(i).name();
                break;
            }
        }
    }
    announceMenuTest("error-tab-colour", colour, QString());
}

void MainWindow::showSetupPage() { openSettings(true); }

void MainWindow::onSettings() { openSettings(false); }

void MainWindow::openSettings(bool onSetup) {
    if (!m_settingsDialog.isNull()) {
        // Already up. A prerequisite re-check arrives every time a setup
        // terminal exits, so without this the dialog the user is logging in
        // through would collect a second one on top of it.
        m_settingsDialog->raise();
        m_settingsDialog->activateWindow();
        if (onSetup) {
            m_settingsDialog->revealSetup();
        }
        return;
    }
    SettingsDialog dialog(m_controller, this);
    m_settingsDialog = &dialog;
    // Set for every open, not only the automatic one. A user who reaches
    // Settings by hand while a blocking prerequisite is failing must not have
    // it thrown back at them the moment they close it either.
    //
    // Recorded on the controller rather than here: the decision it feeds is
    // the controller's, taken from the prerequisite answer it already holds,
    // and half of it living in this file is what made the rule untestable.
    m_controller->noteSetupShown();
    if (onSetup) {
        // Deferred by one turn of the event loop: the scroll area only knows
        // where its sections are once the dialog has been laid out, and it is
        // laid out by `exec`.
        QTimer::singleShot(0, &dialog, [&dialog] { dialog.revealSetup(); });
    }
    dialog.exec();
    m_settingsDialog = nullptr;
    // Whether the turn cost and the agent's own system lines are shown is a
    // setting, and the panes holding them are already on screen. Applied on the
    // way out rather than watched for: a checkbox the user can still cancel out
    // of has not changed anything yet, and `accept` is what writes it.
    m_agentArea->setShowMeta(m_controller->showAgentMeta());
    // Closing the dialog destroys the setup page, and with it the login
    // terminal's session, which closes the PTY. That is correct cleanup, but it
    // means `SetupPage::onTerminalExited` never runs -- so a user who pastes
    // the code, sees "Login successful" and closes Settings before the CLI
    // process exits would otherwise keep a stale `claude_auth: false` and go on
    // being offered the login button. Asking again here is what makes the
    // composer come back without an IDE restart.
    //
    // Only when something was run that could have changed an answer. Every
    // other visit -- a permission mode, an API key, a look at the rows -- left
    // the prerequisites exactly as the last check found them, and asking anyway
    // put a request on the wire and re-ran the whole auto-open decision on a
    // machine that is still blocked. The page is what runs those things, so it
    // is what is asked.
    const SetupPage* setup = dialog.findChild<SetupPage*>();
    if (setup != nullptr && setup->ranAction()) {
        m_controller->recheckPrereqs();
    }
}

void MainWindow::onPrereqsChecked(const QString& json) {
    // The payload is carried for the setup page, which draws a row per entry;
    // this window reads none of it. Counting the failures here meant parsing
    // the list a second time, and classifying them meant handing it straight
    // back for a third. Both answers are decided where the daemon's reply is
    // decoded, and so is whether the dialog should open: the list of
    // prerequisites there is no working around is the controller's, and so is
    // the memory of having already shown Settings for this run of them.
    Q_UNUSED(json);
    const bool anyFailed = m_controller->prereqsAnyFailed();

    m_sandboxIdleText = anyFailed ? "sandbox: prerequisites missing" : "sandbox: -";
    if (!anyFailed) {
        m_sandboxLabel->setToolTip(QString());
    }
    // Offered whenever anything is failing, blocking or not: Settings is a
    // dialog the user closes, so after closing it there has to be a way back.
    m_setupLabel->setVisible(anyFailed);
    updateWorkspaceStatus();

    if (m_controller->shouldAutoOpenSetup()) {
        // Deferred, because this runs inside the controller's own signal and
        // `openSettings` spins a nested event loop for the length of the
        // dialog. Queuing it lets this handler finish first.
        QTimer::singleShot(0, this, [this] { showSetupPage(); });
    }
}

void MainWindow::buildDocks() {
    m_explorer = new ExplorerDock(m_fileTreeModel, m_changesModel, m_controller, this);
    addDockWidget(Qt::LeftDockWidgetArea, m_explorer);
    // The toolbar sends the requests and reads their answers; the window is
    // what has a status bar and a set of workspace panes to put them in.
    QObject::connect(m_explorer->changesToolbar(), &ChangesToolbar::statusMessage, this,
                     &MainWindow::showOperationMessage);
    QObject::connect(m_explorer->changesToolbar(), &ChangesToolbar::workspaceError, this,
                     &MainWindow::showWorkspaceError);
    QObject::connect(m_explorer->changesToolbar(), &ChangesToolbar::workspaceRecovered, this,
                     &MainWindow::clearWorkspaceError);
    QObject::connect(m_explorer, &ExplorerDock::fileActivated, this, [this](const QString& path) {
        m_editorArea->openFile(activeWorkspaceId(), path);
    });
    QObject::connect(m_explorer, &ExplorerDock::diffActivated, this, [this](const QString& path) {
        m_editorArea->openDiff(activeWorkspaceId(), path);
    });

    // The agent pane, on the right of the documents. A dock rather than the
    // other half of a splitter: it gives the boundary the splitter never had --
    // a title bar and a separator wide enough to grab -- and it can be dragged
    // to the other side, torn off onto a second monitor, or closed by a user
    // who wants the whole width for the code.
    m_agentDock = new QDockWidget("Agent", this);
    m_agentDock->setObjectName("AgentDock");
    m_agentArea = new AgentArea(m_agentDock);
    m_agentArea->setPlaceholderText("No agent selected");
    m_agentDock->setWidget(m_agentArea);
    addDockWidget(Qt::RightDockWidgetArea, m_agentDock);

    m_bottomDock = new QDockWidget("Output", this);
    m_bottomDock->setObjectName("BottomDock");
    m_bottomTabs = new QTabWidget(m_bottomDock);
    m_runPanel = new RunPanel(m_runModel, m_controller, m_bottomTabs);
    m_bottomTabs->addTab(m_runPanel, "Run");
    m_shellArea = new AgentArea(m_bottomTabs);
    m_shellArea->setPlaceholderText("No workspace selected");
    m_bottomTabs->addTab(m_shellArea, "Terminal");
    m_bottomDock->setWidget(m_bottomTabs);
    addDockWidget(Qt::BottomDockWidgetArea, m_bottomDock);

    // The agent pane's own size hint is the placeholder label's until a
    // terminal is created in it, which would leave it a couple of dozen
    // columns wide for the life of the window. The widths are seeded here
    // instead; a restored layout overwrites them, which is what it is for.
    resizeDocks({ static_cast<QDockWidget*>(m_explorer), m_agentDock }, { 280, 600 },
                Qt::Horizontal);
    applySeparatorBand();

    // A dock that was moved, floated or hidden is part of what `saveState`
    // records, and none of those raise a resize on the window itself.
    for (QDockWidget* dock : { static_cast<QDockWidget*>(m_explorer), m_agentDock, m_bottomDock }) {
        QObject::connect(dock, &QDockWidget::dockLocationChanged, this,
                         [this](Qt::DockWidgetArea) { noteWindowState(); });
        QObject::connect(dock, &QDockWidget::topLevelChanged, this,
                         [this](bool) { noteWindowState(); });
        QObject::connect(dock, &QDockWidget::visibilityChanged, this,
                         [this](bool) { noteWindowState(); });
    }
}

void MainWindow::rebuildRunConfigMenu() {
    for (QAction* action : m_runPanel->configurationActions()) {
        m_runMenu->insertAction(m_runConfigMenuAnchor, action);
    }
}

void MainWindow::applySeparatorBand() {
    // A stylesheet because there is no other way at the separator: it is drawn
    // by the window's layout rather than by a widget anything can reach. The
    // selector names only the separator, so nothing else in the window is
    // handed a stylesheet style it did not ask for.
    setStyleSheet(QStringLiteral("QMainWindow::separator { background: %1; width: 6px; "
                                 "height: 6px; }")
                      .arg(theme::band(palette()).name()));
}

void MainWindow::resetLayout() {
    for (QDockWidget* dock : { static_cast<QDockWidget*>(m_explorer), m_agentDock, m_bottomDock }) {
        // Unfloated before it is re-docked: `addDockWidget` on a floating dock
        // records the area without bringing the window back, so a user who
        // tore one off and lost it behind the IDE would get a menu entry that
        // appeared to do nothing.
        dock->setFloating(false);
        dock->show();
    }
    addDockWidget(Qt::LeftDockWidgetArea, m_explorer);
    addDockWidget(Qt::RightDockWidgetArea, m_agentDock);
    addDockWidget(Qt::BottomDockWidgetArea, m_bottomDock);
    resizeDocks({ static_cast<QDockWidget*>(m_explorer), m_agentDock }, { 280, 600 },
                Qt::Horizontal);
    noteWindowState();
}

void MainWindow::buildStatusBar() {
    m_sandboxIdleText = "sandbox: -";
    m_daemonLabel = new QLabel(this);
    m_sandboxLabel = new QLabel(m_sandboxIdleText, this);
    m_branchLabel = new QLabel("-", this);
    m_costLabel = new QLabel(this);
    // Rich text so the offer is a link rather than an instruction to go and
    // find a menu item. The href is never followed by Qt itself.
    m_setupLabel = new QLabel("<a href=\"#setup\">Set up…</a>", this);
    m_setupLabel->setToolTip("Open Settings on the Setup section");
    m_setupLabel->setTextFormat(Qt::RichText);
    m_setupLabel->setOpenExternalLinks(false);
    m_setupLabel->setVisible(false);
    QObject::connect(m_setupLabel, &QLabel::linkActivated, this,
                     [this](const QString&) { showSetupPage(); });
    // The last merge or pull request, at the right of the row. Rich text so a
    // pull request's URL is a link; the href is never followed by Qt itself,
    // `openUrl` is the one place the IDE hands a URL to the system browser.
    m_opLabel = new QLabel(this);
    m_opLabel->setObjectName("OperationLabel");
    m_opLabel->setTextFormat(Qt::RichText);
    m_opLabel->setOpenExternalLinks(false);
    m_opLabel->setTextInteractionFlags(Qt::TextBrowserInteraction);
    m_opLabel->setVisible(false);
    QObject::connect(m_opLabel, &QLabel::linkActivated, this, [this](const QString&) {
        if (!m_opUrl.isEmpty()) {
            QDesktopServices::openUrl(QUrl(m_opUrl));
        }
    });
    // Amber, and beside the branch rather than at the right: an agent blocked
    // on a question is waiting for this user, and the dot on its tab is the
    // only other place that is said.
    m_attentionLabel = new QLabel(this);
    m_attentionLabel->setObjectName("AttentionLabel");
    m_attentionLabel->setTextFormat(Qt::PlainText);
    m_attentionLabel->setVisible(false);
    {
        QPalette attentionPalette = m_attentionLabel->palette();
        attentionPalette.setColor(QPalette::WindowText,
                                  theme::ink(theme::renamed(), theme::isDark(palette())));
        m_attentionLabel->setPalette(attentionPalette);
    }
    statusBar()->addWidget(m_daemonLabel);
    statusBar()->addWidget(m_sandboxLabel);
    statusBar()->addWidget(m_setupLabel);
    statusBar()->addWidget(m_branchLabel);
    statusBar()->addWidget(m_attentionLabel);
    statusBar()->addPermanentWidget(m_opLabel);
    statusBar()->addPermanentWidget(m_costLabel);
    updateCostLabel();
}

void MainWindow::showOperationMessage(const QString& text, const QString& url) {
    m_opUrl = url;
    m_opLabel->setText(url.isEmpty()
                           ? text.toHtmlEscaped()
                           : QStringLiteral("<a href=\"%1\">%2</a>")
                                 .arg(url.toHtmlEscaped(), text.toHtmlEscaped()));
    m_opLabel->setToolTip(url.isEmpty() ? text : url);
    m_opLabel->setVisible(!text.isEmpty());
}

void MainWindow::showWorkspaceError(const QString& workspaceId, const QString& title,
                                    const QString& detail, const QString& stderrText) {
    if (workspaceId.isEmpty()) {
        return;
    }
    // The banner is on the workspace's own pane, and the tab's red glyph is
    // what says so from a group the user is not looking at. Both are needed:
    // one of them is only visible when that workspace is in front.
    // A new failure replaces whatever the banner said, an agent stop included.
    m_agentStopped.remove(workspaceId);
    m_agentArea->showBanner(workspaceId, title, detail, stderrText);
    m_groupModel->setWorkspaceError(workspaceId, detail.isEmpty() ? title : title + "\n" + detail);
}

void MainWindow::clearWorkspaceError(const QString& workspaceId) {
    if (workspaceId.isEmpty()) {
        return;
    }
    m_agentStopped.remove(workspaceId);
    m_agentArea->clearBanner(workspaceId);
    m_groupModel->clearWorkspaceError(workspaceId);
}

void MainWindow::clearAgentStopped(const QString& workspaceId) {
    if (m_agentStopped.contains(workspaceId)) {
        clearWorkspaceError(workspaceId);
    }
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

    // Before `workspacesListed` is wired up, because the controller emits the
    // restore first and the reconcile then finds every workspace already
    // placed. Only the first list is a restore; every later one reconciles.
    QObject::connect(m_controller, &AppController::workspacesRestored, this,
                     [this](const QString& json) { m_groupModel->loadWorkspaces(json); });
    QObject::connect(m_controller, &AppController::workspacesListed, this,
                     [this](const QString& json) {
                         m_groupModel->reconcile(json);
                         noteWorkspaceStates(json);
                         resumeUnansweredRestarts(true);
                         // After the reconcile, not the restore: the restore
                         // places the tabs the session file remembered, and it
                         // is this list -- the daemon's own answer -- that says
                         // which of them still has an agent behind it.
                         startAgentsThatHaveNone();
                     });
    // Every arrangement the user makes -- a rename, a drag between groups, a
    // group closed -- reaches `state.json` from here. The model is the
    // authority on the arrangement; the controller only records what it says.
    //
    // `arrangementChanged` and not `changed`: `changed` also fires for a status
    // glyph, an agent heartbeat and a running cost, none of which `state.json`
    // records, and recording on it re-serialised and re-scheduled a write of
    // the whole file dozens of times a minute with nothing to write.
    QObject::connect(m_groupModel, &GroupModel::arrangementChanged, this, [this] {
        // Reported from inside the lambda that records rather than from a
        // second connection to the same signal: a seam wired to its own copy of
        // `arrangementChanged` would keep saying the right thing after this
        // line had been moved back to `changed`. Inert unless armed.
        announceMenuTest("layout-recorded", QString(), QString());
        m_controller->noteGroups(m_groupModel->groupsJson());
    });
    // The re-sync has finished. The panes re-attached themselves in Rust; what
    // is left is the window's own furniture.
    QObject::connect(m_controller, &AppController::reconnected, this, [this](::std::int64_t) {
        onConnectionStateChanged();
        updateWorkspaceStatus();
        // A daemon that restarted has no agents, and the panes that re-attached
        // themselves found nothing to attach to. Nobody has to press anything
        // to get them back.
        startAgentsThatHaveNone();
        showOperationMessage(QStringLiteral("Reconnected to the daemon."), QString());
    });
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
                         // The repository the user actually created something
                         // in, so the New Agent dialog offers it next time.
                         const QString repoPath = QJsonDocument::fromJson(info.toUtf8())
                                                      .object()
                                                      .value("repo_path")
                                                      .toString();
                         if (!repoPath.isEmpty()) {
                             m_controller->noteRecentRepo(repoPath);
                         }
                     });
    QObject::connect(m_controller, &AppController::workspaceChanged, this,
                     [this](const QString& info) {
                         m_groupModel->applyWorkspaceInfo(info);
                         noteWorkspaceStates(info);
                         resumeUnansweredRestarts(false);
                         // A workspace whose sandbox has just come up is a
                         // workspace that can now take an agent. On a restore
                         // every one of them is still starting when the list
                         // arrives, so this -- not the list -- is where most
                         // agents actually start.
                         startAgentsThatHaveNone();
                     });
    // Before `onWorkspaceDestroyed`, which takes the tab out and so provokes
    // the active-tab change that points the Run panel at whatever survived: the
    // dead workspace's runs, logs and blocked hosts have to be gone by then.
    QObject::connect(m_controller, &AppController::workspaceDestroyed, m_runModel,
                     &RunPanelModel::forgetWorkspace);
    QObject::connect(m_controller, &AppController::workspaceDestroyed, this, &MainWindow::onWorkspaceDestroyed);
    QObject::connect(m_controller, &AppController::workspaceRestarted, this,
                     &MainWindow::onWorkspaceRestarted);
    QObject::connect(m_controller, &AppController::workspaceDestroyRefused, this,
                     &MainWindow::onDestroyRefused);
    QObject::connect(m_controller, &AppController::workspaceRestartFailed, this,
                     &MainWindow::onWorkspaceRestartFailed);
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
                         // The other half of the fact `onAgentExited` raises:
                         // this workspace has a live agent again, so there is
                         // nothing left to restart. Without this a workspace
                         // whose agent died and came back keeps a button that
                         // would kill the conversation it just resumed.
                         m_agentArea->setRestartOffered(workspaceId, false);
                         m_autoStarting.remove(workspaceId);
                         // The banner that said it had stopped is out of date
                         // the moment it is running again.
                         clearAgentStopped(workspaceId);
                         rebindCost();
                         reportAgentStartedForTest(workspaceId);
                     });
    // A transcript pane with no agent -- a restored session, or one whose agent
    // exited -- offers to start one. The options are the tab's own, so a
    // restarted agent gets the model and permission mode the user chose.
    QObject::connect(m_agentArea, &AgentArea::startAgentRequested, this,
                     &MainWindow::onStartAgentRequested);
    // A model or permission mode chosen in the composer. It is answered exactly
    // as a restart is, because it is one: `claude -p` reads both flags when the
    // process starts and there is no way to change either in flight. The
    // options already carry the session id, so the conversation continues.
    QObject::connect(m_agentArea, &AgentArea::agentOptionsChanged, this,
                     [this](const QString& workspaceId, const QString& optionsJson) {
                         if (workspaceId.isEmpty() || optionsJson.isEmpty()) {
                             return;
                         }
                         m_groupModel->setTabOptions(workspaceId, optionsJson);
                         m_agentArea->setStarting(workspaceId, true);
                         m_controller->startAgent(workspaceId, optionsJson);
                     });
    // The small grey lines are a setting, so a pane built at any point after
    // this has to be born with the answer rather than showing them until the
    // next time Settings is closed.
    m_agentArea->setShowMeta(m_controller->showAgentMeta());
    // The chat gate. `claudeLoggedIn` is derived from the daemon's own
    // `claude_auth` prerequisite, so the composer comes back on the re-check a
    // successful login triggers -- no restart, and no second source of truth
    // about whether Claude Code can answer.
    QObject::connect(m_controller, &AppController::claudeLoggedInChanged, this, [this] {
        m_agentArea->setClaudeLoggedIn(m_controller->getClaudeLoggedIn());
    });
    m_agentArea->setClaudeLoggedIn(m_controller->getClaudeLoggedIn());
    QObject::connect(m_agentArea, &AgentArea::loginRequested, this, &MainWindow::showSetupPage);
    // A workspace that cannot run. Retry is its own request; Remove is the
    // tab menu's destroy, confirmation and busy check included, because it is
    // the same act reached from somewhere else.
    QObject::connect(m_agentArea, &AgentArea::retryWorkspaceRequested, this,
                     &MainWindow::onRetryWorkspace);
    QObject::connect(m_agentArea, &AgentArea::removeWorkspaceRequested, this,
                     [this](const QString& workspaceId) {
                         onDestroyRequested(workspaceId, m_groupModel->workspaceName(workspaceId));
                     });
    // Dismissing the banner is the user saying they have read it, which is also
    // what takes the red glyph off the tab.
    QObject::connect(m_agentArea, &AgentArea::bannerDismissed, this,
                     [this](const QString& workspaceId) {
                         m_agentStopped.remove(workspaceId);
                         m_groupModel->clearWorkspaceError(workspaceId);
                     });
    QObject::connect(m_controller, &AppController::agentStateChanged, this,
                     [this](const QString& agentId, const QString& state, const QString& detail) {
                         m_groupModel->setAgentStatus(agentId, state, detail);
                         noteAgentAttention(agentId, state);
                         onAgentExited(agentId, state, detail);
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
    // The three failure families that are routed rather than shown, each on a
    // signal of its own: which one a failure belongs to is the controller's to
    // say, and a window that told them apart by comparing the daemon's method
    // name was one rename away from putting a modal box over a dialog that had
    // already reported the same failure in place.
    QObject::connect(m_controller, &AppController::prereqsCheckFailed, this,
                     &MainWindow::onPrereqsCheckFailed);
    QObject::connect(m_controller, &AppController::repoInspectFailed, this,
                     &MainWindow::onRepoInspectFailed);
    QObject::connect(m_controller, &AppController::workspaceOpFailed, this,
                     &MainWindow::onWorkspaceOpFailed);
    // Connected after them: the typed signal for a failure is emitted first,
    // and this catch-all is what the failures with no family of their own
    // arrive on.
    QObject::connect(m_controller, &AppController::operationFailed, this, &MainWindow::onOperationFailed);
    QObject::connect(m_groupModel, &GroupModel::changed, this, &MainWindow::updateWorkspaceStatus);
    QObject::connect(m_groupModel, &GroupModel::changed, this, &MainWindow::onActiveTabChanged);
    // After the active tab has been shown, so the pane in front already exists
    // when its problem is put on it.
    QObject::connect(m_groupModel, &GroupModel::changed, this,
                     &MainWindow::syncWorkspaceProblems);

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
                     [this](EditorWidget*) {
                         updateEditActions();
                         noteEditorState();
                     });
    // A tab opened, closed or moved: which files are open and which is in front
    // are both part of what comes back next time.
    QObject::connect(m_editorArea, &EditorArea::openEditorsChanged, this,
                     &MainWindow::noteEditorState);
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
    if (!m_newAgentDialog.isNull()) {
        m_newAgentDialog->raise();
        m_newAgentDialog->activateWindow();
        return;
    }
    // Opened straight away, with no inspection in front of it. The window used
    // to ask the daemon about the repository first and open the dialog on the
    // answer, which meant a wait cursor, a borrowed status line, a guard object
    // and a second copy of the pipeline the dialog already has -- and, on a
    // repository reached through `/mnt/c`, a menu item that appeared to do
    // nothing for several seconds. The dialog inspects its own path now and
    // says so in its own status line, with a Cancel button beside it.
    NewAgentDialog dialog(m_controller, m_groupModel,
                          NewAgentDialog::initialRepoPath(m_controller), this);
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
        m_controller->createWorkspaceWithAgentAndRun(
            dialog.repoPath(), dialog.baseBranch(), dialog.name(), dialog.group(),
            dialog.optionsJson(), dialog.initialPrompt(), dialog.runConfig(),
            dialog.initIfMissing(), false);
        return;
    }
    m_controller->createWorkspaceWithRun(dialog.repoPath(), dialog.baseBranch(), dialog.name(),
                                         dialog.group(), dialog.adapter(), dialog.command(),
                                         dialog.runConfig(), dialog.initIfMissing(), false);
}

void MainWindow::onDestroyRequested(const QString& workspaceId, const QString& workspaceName) {
    // Destroyed by something else while the menu was up -- another IDE, or the
    // agent's own workspace going away. Nothing is left to ask about, and the
    // same silence is what `onCloseGroup` answers a group that has gone with.
    if (workspaceId.isEmpty() || !workspaceIsOpen(workspaceId)) {
        return;
    }
    if (isWorkspaceBusy(workspaceId)) {
        sayWorkspaceIsBusy();
        return;
    }
    // Named, not "this workspace": the menu that led here has been closed for
    // as long as it takes to read the question, and the tab it was opened over
    // may no longer be the one in front.
    const QString question =
        workspaceName.isEmpty()
            ? QStringLiteral("Destroy this workspace? Its sandbox and worktree are removed.")
            : QStringLiteral("Destroy workspace \"%1\"? Its sandbox and worktree are removed.")
                  .arg(workspaceName);
    if (announceMenuTest("destroy", workspaceId, question)) {
        // `destroy-yes` answers Yes with Force unticked, which is what a user
        // who just presses Enter on the banner's Remove sends.
        if (menuTestTarget("destroy-yes") == workspaceId) {
            m_controller->destroyWorkspace(workspaceId, false);
        }
        return;
    }
    QMessageBox box(this);
    box.setIcon(QMessageBox::Question);
    box.setWindowTitle("Destroy workspace");
    box.setText(question);
    box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    auto* force = new QCheckBox("Force (discard changes)", &box);
    box.setCheckBox(force);
    if (box.exec() != QMessageBox::Yes) {
        return;
    }
    // Asked again, because the question was on screen for as long as it took to
    // read it and a merge can have started in that time. This is the same
    // defect one layer up that the Changes toolbar was fixed for: the check
    // before a modal says nothing about the moment after it.
    if (isWorkspaceBusy(workspaceId)) {
        sayWorkspaceIsBusy();
        return;
    }
    m_controller->destroyWorkspace(workspaceId, force->isChecked());
}

void MainWindow::onDestroyRefused(const QString& workspaceId, bool dirty, bool unmerged) {
    if (workspaceId.isEmpty() || !workspaceIsOpen(workspaceId)) {
        return;
    }
    const QString name = m_groupModel->workspaceName(workspaceId);
    const QString subject = name.isEmpty() ? QStringLiteral("This workspace")
                                           : QStringLiteral("Workspace \"%1\"").arg(name);
    // Said the way the daemon knows it. A worktree git has lost track of is
    // reported dirty because nothing can be read from it, so "may have" is the
    // honest wording for dirty work; unmerged commits are read off the branch
    // and are certain.
    QStringList what;
    if (dirty) {
        what.append(QStringLiteral("may have uncommitted changes"));
    }
    if (unmerged) {
        what.append(QStringLiteral("has commits that are not merged into its base branch"));
    }
    const QString question =
        QStringLiteral("%1 %2. Destroy it anyway? Its uncommitted changes are discarded and its "
                       "branch is deleted, with any commits only it has.")
            .arg(subject, what.join(QStringLiteral(" and ")));
    if (announceMenuTest("destroy-refused", workspaceId, question)) {
        if (menuTestTarget("destroy-yes") == workspaceId) {
            m_controller->destroyWorkspace(workspaceId, true);
        }
        return;
    }
    QMessageBox box(this);
    box.setIcon(QMessageBox::Warning);
    box.setWindowTitle(QStringLiteral("Destroy workspace"));
    box.setText(question);
    QPushButton* remove =
        box.addButton(QStringLiteral("Destroy anyway"), QMessageBox::DestructiveRole);
    box.addButton(QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    box.exec();
    if (box.clickedButton() != remove) {
        return;
    }
    // Asked again after the modal, for the reason `onDestroyRequested` asks.
    if (!workspaceIsOpen(workspaceId)) {
        return;
    }
    if (isWorkspaceBusy(workspaceId)) {
        sayWorkspaceIsBusy();
        return;
    }
    m_controller->destroyWorkspace(workspaceId, true);
}

bool MainWindow::workspaceIsOpen(const QString& workspaceId) const {
    // By id, never by name. `GroupModel::workspaceName` answers an empty string
    // both for an id it cannot find and for a tab whose name is empty, and this
    // handler's own question text has a fallback for an unnamed workspace --
    // so the codebase expects those to exist, and telling the two apart by the
    // name would make one of them undestroyable in silence.
    for (int group = 0; group < m_groupModel->groupCount(); ++group) {
        for (int tab = 0; tab < m_groupModel->tabCount(group); ++tab) {
            if (m_groupModel->tabWorkspaceId(group, tab) == workspaceId) {
                return true;
            }
        }
    }
    return false;
}

bool MainWindow::isWorkspaceBusy(const QString& workspaceId) const {
    // The Changes toolbar greys its own Discard out while an operation is
    // running; this menu is the other way to the same call, and a destroy that
    // lands while a merge is still absorbing objects out of the workspace is
    // what leaves the base branch pointing at commits that no longer exist.
    return m_controller->isWorkspaceBusy(workspaceId);
}

void MainWindow::sayWorkspaceIsBusy() {
    QMessageBox::information(
        this, QStringLiteral("Destroy workspace"),
        QStringLiteral("This workspace has a merge, pull request or discard running. "
                       "Wait for it to finish, then try again."));
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

void MainWindow::onAgentExited(const QString& agentId, const QString& state,
                               const QString& detail) {
    if (state != QLatin1String("exited")) {
        return;
    }
    const QString workspaceId = m_groupModel->agentWorkspaceId(agentId);
    if (workspaceId.isEmpty()) {
        return;
    }
    // The one banner the IDE raises about an agent rather than about a git
    // command, and the only one that offers to act. It exists because the
    // composer's Start button does not any more: agents start themselves now,
    // so an agent that stopped by itself is the single case left where the
    // user has to be told and given something to press.
    //
    // Not restarted automatically. A crash that repeats would become a loop
    // reporting itself as a working agent, and the difference between "it came
    // back" and "it has died eleven times" is the thing the user most needs to
    // see.
    // A stop the user's own Retry asked for. `onWorkspaceRestarted` starts the
    // agent again; a failed Retry leaves the workspace's banner, whose Retry is
    // the way back for the agent too.
    //
    // The same for a workspace whose sandbox is down or failed: the agent went
    // with the sandbox, the workspace's banner already says so, and its Retry
    // brings the agent back too. Either way the offer is recorded, so a
    // workspace that comes back without its agent still has a Restart.
    //
    // And for one whose agent is being started again: the start was sent
    // before this exit arrived -- events and answers travel apart -- so the
    // agent that exited is the one being replaced.
    if (m_restarting.contains(workspaceId) || m_workspaceProblems.contains(workspaceId) ||
        m_autoStarting.contains(workspaceId)) {
        m_agentArea->setRestartOffered(workspaceId, true);
        return;
    }
    if (menuTest().contains(QLatin1String("sandbox-banner"))) {
        announceMenuTest("agent-stopped", workspaceId, detail);
    }
    m_agentArea->showBanner(workspaceId, QStringLiteral("The agent stopped."), detail, QString());
    // Said separately from the banner, because they are different facts. The
    // banner reports the latest failure; this reports that the agent is down,
    // and only the second one decides whether there is anything to press. Tied
    // together, a merge that failed over a still-dead agent would take the
    // user's only way back away with it.
    m_agentArea->setRestartOffered(workspaceId, true);
    m_agentStopped.insert(workspaceId);
    m_groupModel->setWorkspaceError(workspaceId, detail.isEmpty()
                                                     ? QStringLiteral("The agent stopped.")
                                                     : QStringLiteral("The agent stopped.\n") + detail);
}

void MainWindow::startAgentsThatHaveNone() {
    const QJsonArray groups = QJsonDocument::fromJson(m_groupModel->getStateJson().toUtf8())
                                  .object()
                                  .value("groups")
                                  .toArray();
    for (const QJsonValue& groupValue : groups) {
        for (const QJsonValue& tabValue : groupValue.toObject().value("tabs").toArray()) {
            const QJsonObject tab = tabValue.toObject();
            if (tab.value("adapter").toString() != QLatin1String("claude")) {
                continue;
            }
            if (!tab.value("agent_id").toString().isEmpty()) {
                continue;
            }
            // An agent that exited is left alone. That is a failure with a
            // banner and a Restart button on it, and starting it again from
            // here would turn one crash into a loop that reports itself as a
            // working agent. `Done` is what an exit is called here:
            // `TabStatus::from_agent_state` maps `AgentState::Exited` onto it.
            if (tab.value("agent_status").toString() == QLatin1String("Done")) {
                continue;
            }
            // The sandbox has to be up first. `agent.start` against a workspace
            // whose sandbox is still coming up fails with `SandboxError`, and
            // on a restore every workspace is in that state for the first few
            // seconds — so starting on the workspace list alone produced one
            // failed start per workspace, every launch. The workspaces that are
            // not ready yet are picked up by `workspaceChanged` as each one
            // becomes so.
            const QString status = tab.value("status").toString();
            if (status == QLatin1String("Creating") || status == QLatin1String("SandboxDown") ||
                status == QLatin1String("Error")) {
                continue;
            }
            const QString workspaceId = tab.value("workspace_id").toString();
            if (workspaceId.isEmpty() || m_autoStarting.contains(workspaceId)) {
                continue;
            }
            // Remembered until the start answers either way. This runs on every
            // workspace change, and a second `agent.start` for a workspace whose
            // first one is still in flight would give it two agents.
            m_autoStarting.insert(workspaceId);
            m_agentArea->setStarting(workspaceId, true);
            m_controller->startAgent(workspaceId, tab.value("options_json").toString());
        }
    }
}

void MainWindow::syncWorkspaceProblems() {
    const QJsonArray groups = QJsonDocument::fromJson(m_groupModel->getStateJson().toUtf8())
                                  .object()
                                  .value("groups")
                                  .toArray();
    const bool report = menuTest().contains(QLatin1String("sandbox-banner"));
    QSet<QString> present;
    for (const QJsonValue& groupValue : groups) {
        for (const QJsonValue& tabValue : groupValue.toObject().value("tabs").toArray()) {
            const QJsonObject tab = tabValue.toObject();
            const QString workspaceId = tab.value("workspace_id").toString();
            if (workspaceId.isEmpty()) {
                continue;
            }
            present.insert(workspaceId);
            const QJsonObject problem = tab.value("workspace_problem").toObject();
            if (problem.isEmpty()) {
                if (m_workspaceProblems.remove(workspaceId) > 0) {
                    m_agentArea->clearWorkspaceProblem(workspaceId);
                    if (report) {
                        announceMenuTest("sandbox-banner", workspaceId, QStringLiteral("cleared"));
                    }
                }
                continue;
            }
            const QString title = problem.value("title").toString();
            const QString detail = problem.value("detail").toString();
            const QString shown = title + QLatin1Char('\n') + detail;
            const auto known = m_workspaceProblems.constFind(workspaceId);
            if (known != m_workspaceProblems.constEnd() && *known == shown) {
                continue;
            }
            m_workspaceProblems.insert(workspaceId, shown);
            m_agentArea->setWorkspaceProblem(workspaceId, title, detail);
            // An agent that stopped just before its sandbox was reported down
            // stopped because of it. The problem says so and its Retry brings
            // the agent back, so "The agent stopped." and the red tab it
            // brought are the same news told wrongly.
            clearAgentStopped(workspaceId);
            if (report) {
                announceMenuTest("sandbox-banner", workspaceId,
                                 title + QStringLiteral(" | ") + detail);
            }
            pressBannerForTest(workspaceId, "sandbox-retry", "WorkspaceBannerRetryButton");
            pressBannerForTest(workspaceId, "sandbox-remove", "WorkspaceBannerRemoveButton");
        }
    }
    // A workspace that went away took its pane, and its banner, with it.
    for (auto it = m_workspaceProblems.begin(); it != m_workspaceProblems.end();) {
        if (present.contains(it.key())) {
            ++it;
        } else {
            it = m_workspaceProblems.erase(it);
        }
    }
}

void MainWindow::pressBannerForTest(const QString& workspaceId, const char* step,
                                    const char* button) {
    const QString key = QString::fromLatin1(step) + QLatin1Char(' ') + workspaceId;
    // Only the workspace the step names: see `menuTestTarget`.
    if (workspaceId.isEmpty() || menuTestTarget(step) != workspaceId ||
        m_bannerTestPressed.contains(key)) {
        return;
    }
    // After this turn, so the pane the model change is about to build is there.
    // The banner is that workspace's own page's, and only its named button is
    // pressed: a click anywhere wider could land on something the run never
    // asked about.
    const QString objectName = QString::fromLatin1(button);
    QTimer::singleShot(0, this, [this, workspaceId, key, objectName] {
        WorkspaceBanner* banner = m_agentArea->banner(workspaceId);
        QPushButton* target =
            banner == nullptr ? nullptr : banner->findChild<QPushButton*>(objectName);
        if (target == nullptr || target->isHidden() || !target->isEnabled() ||
            m_bannerTestPressed.contains(key)) {
            return;
        }
        m_bannerTestPressed.insert(key);
        target->click();
    });
}

void MainWindow::onRetryWorkspace(const QString& workspaceId) {
    if (workspaceId.isEmpty()) {
        return;
    }
    if (menuTest().contains(QLatin1String("sandbox-retry"))) {
        announceMenuTest("sandbox-retry", workspaceId, QString());
    }
    m_restarting.insert(workspaceId);
    m_restartUnanswered.remove(workspaceId);
    m_agentArea->setRetrying(workspaceId, true);
    m_controller->restartWorkspace(workspaceId);
}

void MainWindow::onWorkspaceRestarted(const QString& workspaceId, const QString& infoJson) {
    // Applying the answer is what takes the banner down: the model change it
    // provokes finds the workspace without a problem.
    m_restarting.remove(workspaceId);
    m_restartUnanswered.remove(workspaceId);
    m_groupModel->applyWorkspaceInfo(infoJson);
    noteWorkspaceStates(infoJson);
    m_agentArea->setRetrying(workspaceId, false);
    resumeAgentAfterRestart(workspaceId);
    startAgentsThatHaveNone();
}

void MainWindow::onWorkspaceRestartFailed(const QString& workspaceId, const QString& message,
                                          int kind) {
    // The `operationFailed` behind this is answered here, in the banner.
    noteFailureRouted(message);
    m_restarting.remove(workspaceId);
    m_agentArea->setRetrying(workspaceId, false);
    qWarning("workspace restart failed for %s: %s", qUtf8Printable(workspaceId),
             qUtf8Printable(message));
    // `RestartFailure` in the controller: 0 is the daemon's own reason.
    if (kind == 0) {
        m_groupModel->noteRestartFailed(workspaceId, message);
        return;
    }
    // Anything else is about the request, not the workspace, whose banner
    // keeps what it said. A refusal changed nothing; a timeout or a lost
    // connection may have been a restart that worked, so the agent comes back
    // when the workspace is next seen running.
    const QString name = m_groupModel->workspaceName(workspaceId);
    if (kind == 1) {
        showOperationMessage(QStringLiteral("Could not retry \"%1\" yet: %2").arg(name, message),
                             QString());
        return;
    }
    const bool connectionLost = kind != 2;
    m_restartUnanswered.insert(workspaceId, connectionLost);
    showOperationMessage(
        QStringLiteral("The daemon did not answer the retry of \"%1\" (%2). If the workspace "
                       "stays down, press Retry again.")
            .arg(name, message),
        QString());
    // A timeout on a live connection: the workspace may already read as
    // running, and nothing else would notice.
    if (!connectionLost) {
        resumeUnansweredRestarts(false);
    }
}

void MainWindow::noteWorkspaceStates(const QString& json) {
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    const QJsonArray infos = doc.isArray() ? doc.array() : QJsonArray{ doc.object() };
    for (const QJsonValue& value : infos) {
        const QJsonObject info = value.toObject();
        const QString id = info.value("id").toString();
        if (!id.isEmpty()) {
            m_workspaceStates.insert(id, info.value("state").toString());
        }
    }
}

void MainWindow::resumeUnansweredRestarts(bool listed) {
    const QList<QString> pending = m_restartUnanswered.keys();
    for (const QString& workspaceId : pending) {
        if (m_restartUnanswered.value(workspaceId) && !listed) {
            continue;
        }
        const QString state = m_workspaceStates.value(workspaceId);
        // Gone, or on its way: there is no agent to bring back, now or later.
        if (tabFor(workspaceId).isEmpty() || state == QLatin1String("destroying")) {
            m_restartUnanswered.remove(workspaceId);
            continue;
        }
        // Running is the one state an agent can be started in. Anything else
        // waits for the next report; after a fresh list that report no longer
        // has to be another list, because the connection is back.
        if (state != QLatin1String("ready") || m_restarting.contains(workspaceId)) {
            if (listed) {
                m_restartUnanswered.insert(workspaceId, false);
            }
            continue;
        }
        m_restartUnanswered.remove(workspaceId);
        resumeAgentAfterRestart(workspaceId);
    }
}

void MainWindow::resumeAgentAfterRestart(const QString& workspaceId) {
    // Unlike the automatic start this includes an agent that had ended -- the
    // one a daemon restart leaves behind in every workspace, and the one the
    // restart itself stops -- because the user asked for this workspace back
    // and an agent is what it is for. Booked in the same set as the automatic
    // start, so neither can ask for a second one.
    if (m_groupModel->agentNeedsStart(workspaceId) && !m_autoStarting.contains(workspaceId)) {
        m_autoStarting.insert(workspaceId);
        m_agentArea->setStarting(workspaceId, true);
        // The transcript's options when it has them, for the same reason a
        // Restart uses them: they carry the session to resume.
        TranscriptModel* model = m_agentArea->transcriptModel(workspaceId);
        const QString options = model == nullptr
                                    ? tabFor(workspaceId).value("options_json").toString()
                                    : model->restartOptionsJson();
        m_controller->startAgent(workspaceId, options);
    }
}

void MainWindow::reportAgentStartedForTest(const QString& workspaceId) {
    if (!menuTest().contains(QLatin1String("sandbox-banner"))) {
        return;
    }
    int status = -1;
    for (int group = 0; group < m_groupModel->groupCount(); ++group) {
        for (int tab = 0; tab < m_groupModel->tabCount(group); ++tab) {
            if (m_groupModel->tabWorkspaceId(group, tab) == workspaceId) {
                status = m_groupModel->tabStatus(group, tab);
            }
        }
    }
    const WorkspaceBanner* banner = m_agentArea->banner(workspaceId);
    const bool shown = banner != nullptr && !banner->isHidden();
    announceMenuTest("agent-started", workspaceId,
                     QStringLiteral("status=%1 banner=%2")
                         .arg(status)
                         .arg(shown ? QStringLiteral("shown") : QStringLiteral("hidden")));
}

QJsonObject MainWindow::tabFor(const QString& workspaceId) const {
    const QJsonArray groups = QJsonDocument::fromJson(m_groupModel->getStateJson().toUtf8())
                                  .object()
                                  .value("groups")
                                  .toArray();
    for (const QJsonValue& groupValue : groups) {
        for (const QJsonValue& tabValue : groupValue.toObject().value("tabs").toArray()) {
            const QJsonObject tab = tabValue.toObject();
            if (tab.value("workspace_id").toString() == workspaceId) {
                return tab;
            }
        }
    }
    return QJsonObject();
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
    // The transcript's own options, which are the tab's plus the session id it
    // saw in the history: a restart after an agent exited -- or after a daemon
    // restart left the workspace with none -- resumes the conversation rather
    // than starting a fresh one. A pane with no transcript (a terminal tab) has
    // nothing to resume and falls back to what the tab was created with.
    TranscriptModel* model = m_agentArea->transcriptModel(workspaceId);
    const QString options = model == nullptr ? tab.value("options_json").toString()
                                             : model->restartOptionsJson();
    m_controller->startAgent(workspaceId, options);
}

void MainWindow::onCloseGroup(const QString& groupName) {
    if (groupName.isEmpty()) {
        return;
    }
    // The group's tabs in order, from the model's own state, so the dialog's
    // rows and the run that follows are in the order the user sees. Looked up
    // by name: the bar resolved the name at the click, and the group may have
    // changed places since.
    const QJsonArray groups = QJsonDocument::fromJson(m_groupModel->getStateJson().toUtf8())
                                  .object()
                                  .value("groups")
                                  .toArray();
    QJsonArray tabs;
    bool found = false;
    for (const QJsonValue& value : groups) {
        const QJsonObject group = value.toObject();
        if (group.value("name").toString() == groupName) {
            tabs = group.value("tabs").toArray();
            found = true;
            break;
        }
    }
    // Closed by something else while the menu was up. Nothing is left to ask
    // about, and asking about an empty group the user never opened would be
    // worse than saying nothing.
    if (!found) {
        return;
    }
    if (announceMenuTest("close-group", groupName, QString())) {
        return;
    }
    ChangesToolbar* toolbar = m_explorer->changesToolbar();
    QList<CloseGroupChoice> workspaces;
    for (const QJsonValue& value : tabs) {
        const QJsonObject tab = value.toObject();
        CloseGroupChoice choice;
        choice.workspaceId = tab.value("workspace_id").toString();
        choice.name = tab.value("name").toString();
        if (choice.workspaceId.isEmpty()) {
            continue;
        }
        // Two things the toolbar only learns from a workspace being the active
        // tab, and a group can hold workspaces that never have been: the
        // branches its status line names after a merge, and the changed-file
        // count the discard confirmation below has to state. The answers land
        // while the modal dialog's own event loop is running.
        toolbar->noteBranches(choice.workspaceId, tab.value("branch").toString(),
                              tab.value("base_branch").toString());
        toolbar->requestSummary(choice.workspaceId);
        choice.busy = m_controller->isWorkspaceBusy(choice.workspaceId);
        workspaces.append(choice);
    }

    CloseGroupDialog dialog(groupName, workspaces, this);
    if (dialog.exec() != QDialog::Accepted) {
        return;
    }
    const QList<CloseGroupChoice> choices = dialog.choices();
    if (!confirmDiscards(choices)) {
        return;
    }
    auto* runner = new CloseGroupRunner(m_controller, m_groupModel, groupName, choices, this);
    QObject::connect(runner, &CloseGroupRunner::finished, this,
                     [this, groupName](bool ok, const QString&, const QString& message) {
                         // A stop is already on the workspace's own banner and
                         // on its tab, put there by the toolbar's handler for
                         // the same signal. The status bar is where the *group*
                         // says what became of it, without a second modal over
                         // an explanation the user already has.
                         showOperationMessage(
                             ok ? QStringLiteral("Closed the group \"%1\".").arg(groupName)
                                : QStringLiteral("\"%1\" was left open: %2")
                                      .arg(groupName, message),
                             QString());
                     });
    runner->start();
}

bool MainWindow::confirmDiscards(const QList<CloseGroupChoice>& choices) {
    ChangesToolbar* toolbar = m_explorer->changesToolbar();
    QStringList doomed;
    for (const CloseGroupChoice& choice : choices) {
        if (choice.action != CloseGroupAction::Discard) {
            continue;
        }
        const int files = toolbar->changedFilesFor(choice.workspaceId);
        // A count the daemon could not give is left out rather than guessed
        // at, exactly as the toolbar's own Discard does: "0 changed files" is
        // the one wording that would talk a user into a discard.
        doomed.append(files < 0
                          ? QStringLiteral("%1 (its changed files)").arg(choice.name)
                          : QStringLiteral("%1 (%2 changed file%3)")
                                .arg(choice.name)
                                .arg(files)
                                .arg(files == 1 ? QString() : QStringLiteral("s")));
    }
    if (doomed.isEmpty()) {
        return true;
    }
    // The combo says which workspaces are to be destroyed; this says what that
    // costs. A discard is the one choice in the dialog that cannot be undone,
    // and the plan requires a confirmation naming the workspace and what goes
    // with it before any of them.
    QMessageBox box(this);
    box.setIcon(QMessageBox::Warning);
    box.setWindowTitle(QStringLiteral("Discard workspaces"));
    box.setText(doomed.size() == 1
                    ? QStringLiteral("Discard %1?").arg(doomed.first())
                    : QStringLiteral("Discard %1 workspaces?").arg(doomed.size()));
    box.setInformativeText(
        QStringLiteral("%1\n\nTheir changed files and any unmerged commits will be lost. This "
                       "cannot be undone.")
            .arg(doomed.join(QLatin1Char('\n'))));
    box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    // Cancel abandons the whole run, merges included: the user answered a
    // question about the group, and half-closing it behind a refused answer
    // would be worse than doing nothing.
    return box.exec() == QMessageBox::Yes;
}

// ---------------------------------------------------------------------------
// Persistence. Everything the window knows that `state.json` remembers is
// reported through these four; the debounce and the file are the controller's.
// ---------------------------------------------------------------------------

void MainWindow::onStateLoaded(const QString& json) {
    const QJsonObject state = QJsonDocument::fromJson(json.toUtf8()).object();
    m_restoring = true;
    const QByteArray geometry =
        QByteArray::fromBase64(state.value("geometry_b64").toString().toLatin1());
    if (!geometry.isEmpty()) {
        restoreGeometry(geometry);
    }
    const QByteArray windowState =
        QByteArray::fromBase64(state.value("window_state_b64").toString().toLatin1());
    if (!windowState.isEmpty()) {
        // Named docks only: `restoreState` matches by object name, and a dock
        // whose name it does not find is left where `buildDocks` put it.
        //
        // Version 2 is the layout with the agent pane as a dock. A state saved
        // by a version that knew nothing of `AgentDock` is refused outright by
        // the version number rather than half-applied, which leaves the
        // default arrangement -- exactly what a session from before the dock
        // existed should come back as.
        restoreState(windowState, kWindowStateVersion);
    }

    // Held rather than opened: a workspace's editors are reopened the first
    // time its tab is shown, because opening a file asks the daemon for it and
    // there is no point asking for every workspace at start-up.
    const QJsonObject open = state.value("open_editors").toObject();
    const QJsonObject active = state.value("active_editor").toObject();
    m_editorsToRestore.clear();
    m_activeEditorToRestore.clear();
    for (auto it = open.constBegin(); it != open.constEnd(); ++it) {
        QStringList paths;
        for (const QJsonValue& value : it.value().toArray()) {
            const QString path = value.toString();
            if (!path.isEmpty()) {
                paths.append(path);
            }
        }
        if (paths.isEmpty()) {
            continue;
        }
        m_editorsToRestore.insert(it.key(), paths);
        // Kept beside the list rather than reordered into it: the list is the
        // tab order, and moving the active path to the end of it would bring a
        // session's second tab back as its fourth. `restoreEditorsFor` opens in
        // this order and activates the front one afterwards.
        const QString front = active.value(it.key()).toString();
        if (!front.isEmpty() && paths.contains(front)) {
            m_activeEditorToRestore.insert(it.key(), front);
        }
    }
    m_restoring = false;
}

void MainWindow::noteWindowState() {
    if (m_restoring || m_controller == nullptr) {
        return;
    }
    m_controller->noteWindow(QString::fromLatin1(saveState(kWindowStateVersion).toBase64()),
                             QString::fromLatin1(saveGeometry().toBase64()));
}

void MainWindow::noteEditorState() {
    if (m_restoring || m_restoringEditors) {
        return;
    }
    const QJsonObject open = QJsonDocument::fromJson(m_editorArea->openEditorsJson().toUtf8())
                                 .object();
    QSet<QString> seen;
    for (auto it = open.constBegin(); it != open.constEnd(); ++it) {
        seen.insert(it.key());
        m_controller->noteEditors(
            it.key(),
            QString::fromUtf8(QJsonDocument(it.value().toObject()).toJson(QJsonDocument::Compact)));
    }
    // A workspace whose last editor has just closed is recorded as having none,
    // rather than left describing tabs that are gone.
    for (const QString& workspaceId : m_notedEditors) {
        if (!seen.contains(workspaceId)) {
            m_controller->noteEditors(workspaceId, QStringLiteral("[]"));
        }
    }
    m_notedEditors = seen;
}

void MainWindow::restoreEditorsFor(const QString& workspaceId) {
    const QStringList paths = m_editorsToRestore.take(workspaceId);
    const QString front = m_activeEditorToRestore.take(workspaceId);
    if (paths.isEmpty()) {
        return;
    }
    // The opens are asynchronous and any of them may fail -- a file the agent
    // deleted since -- which the editor area reports in the tab itself. None of
    // that is a reason to refuse to show the workspace.
    m_restoringEditors = true;
    for (const QString& path : paths) {
        m_editorArea->openFile(workspaceId, path);
    }
    // Last, and in place: `openFile` on a path that is already open activates
    // its tab rather than adding another, so the session's front tab comes back
    // in front without its position in the row changing.
    if (!front.isEmpty()) {
        m_editorArea->openFile(workspaceId, front);
    }
    m_restoringEditors = false;
    // Once, at the end: the list that is now open is the list that was
    // restored, and recording it per open would have written it out in pieces.
    noteEditorState();
}

void MainWindow::noteFailureRouted(const QString& message) {
    m_routedFailure = message;
    m_routedFailureSet = true;
    // Valid for the rest of this turn of the event loop and no longer. Each
    // emitter sends the typed signal and its `operationFailed` from inside one
    // queued closure, so the pair is delivered before control returns to the
    // event loop and this fires strictly after it.
    //
    // A record still standing when it does is one whose `operationFailed` was
    // never sent -- which is what dropping a pairing looks like -- and leaving
    // it would let it swallow the next unrelated failure carrying the same
    // text. Not far-fetched: every request refused for want of a connection
    // carries the same sentence, whatever it was asking for. This is what lets
    // either side drop a pairing without the other having to change in the
    // same commit.
    //
    // One window remains, and it is one iteration of the event loop wide: a
    // zero timer is drained after the events already posted, not in strict FIFO
    // with them, so a dropped pairing's record outlives every event queued
    // alongside it. A second, unrelated failure carrying identical text and
    // queued in that same iteration would still be swallowed. Bounded rather
    // than unbounded, which is what this replaced, and not reachable today
    // while every emitter queues the pair in one closure. Stamping the record
    // with a monotonic turn counter and comparing it in `takeRoutedFailure`
    // would close it exactly, and would close the mirror-image window on the
    // Rust side too; a timer cannot.
    QTimer::singleShot(0, this, [this] {
        m_routedFailureSet = false;
        m_routedFailure.clear();
    });
}

bool MainWindow::takeRoutedFailure(const QString& message) {
    // Correlating on the text alone is enough because of the shape of the
    // emitter, not because of anything recorded here: `report_prereqs_failure`,
    // `report_inspect_failure` and `report_workspace_op_failure` each emit the
    // typed signal and the `operationFailed` inside one `qt.queue` closure, so
    // the pair arrives as one step on this thread and nothing can be
    // interleaved between them. A second failure cannot reach the record
    // before the `operationFailed` that clears it, and a pairing that is
    // dropped leaves a record that expires on its own; see `noteFailureRouted`.
    const bool routed = m_routedFailureSet && m_routedFailure == message;
    m_routedFailureSet = false;
    m_routedFailure.clear();
    return routed;
}

void MainWindow::onPrereqsCheckFailed(const QString& message) {
    noteFailureRouted(message);
    // Never a box. The commonest way to see this is closing Settings during a
    // reconnect: the close re-checks and the daemon is not there to answer. The
    // status bar is already saying the connection is down, and the checks are
    // re-run on every reconnect, so there is nothing for the user to do with a
    // modal about it.
    qWarning("prerequisite check failed: %s", qUtf8Printable(message));
}

void MainWindow::onRepoInspectFailed(const QString& path, const QString& message) {
    noteFailureRouted(message);
    // The New Agent dialog asks for every inspection there is and reports the
    // answer in its own status line, where the path that failed is the one the
    // user can correct. A box here would say the same thing twice, the second
    // time over a modal dialog.
    qWarning("repository inspection failed for %s: %s", qUtf8Printable(path),
             qUtf8Printable(message));
}

void MainWindow::onWorkspaceOpFailed(const QString& workspaceId, const QString& op,
                                     const QString& message) {
    noteFailureRouted(message);
    if (op == QLatin1String("agent.start")) {
        // The pane must stop saying it is starting something. The signal names
        // the workspace, so only the pane that asked comes out of it.
        m_agentArea->setStarting(workspaceId, false);
        // And the automatic start is no longer in flight, so a later workspace
        // change may try again. A sandbox that was still coming up is exactly
        // the failure worth retrying, and it is the common one.
        m_autoStarting.remove(workspaceId);
    }
    reportFailure(op, message);
}

void MainWindow::onOperationFailed(const QString& op, const QString& message) {
    // Every typed failure signal is followed by an `operationFailed` for the
    // same failure, emitted in the same step for as long as both are sent. The
    // failure has already found its home by then, and a box here would be a
    // second report of it.
    if (takeRoutedFailure(message)) {
        return;
    }
    // Nothing is told apart by its method name here any more: a failure that
    // reaches this handler is one no family claimed, and a box over the window
    // is what the window has to say about it.
    reportFailure(op, message);
}

void MainWindow::reportFailure(const QString& op, const QString& message) {
    // The New Agent dialog reports its own lookups inline, and while it is up
    // it is modal, so a box parented to this window could not be closed.
    if (!m_newAgentDialog.isNull()) {
        if (op == QLatin1String("repo.detect_run_configs")) {
            // A repository with no detectable run configuration is a normal
            // answer; the dialog says so on the Run config row.
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
    // Asked off the model directly rather than pulled out of `activeTab`, which
    // serialises the whole tab in Rust and parses it again here. Seven call
    // sites ask this -- every file opened, every pane rebound, every run the
    // panel is pointed at -- and none of them wants anything but the id. The
    // two accessors read the same `active_group`/`active_tab` pair `activeTab`
    // does, so the answer cannot differ from it; an index the model does not
    // have answers empty, which is what no selection means.
    return m_groupModel->tabWorkspaceId(m_groupModel->activeGroupIndex(),
                                        m_groupModel->activeTabIndex());
}

QString MainWindow::activeGroupName() const {
    return m_groupModel->groupName(m_groupModel->activeGroupIndex());
}

void MainWindow::stepAgent(int delta) {
    const int group = m_groupModel->activeGroupIndex();
    const int count = m_groupModel->tabCount(group);
    if (count <= 0) {
        return;
    }
    // The second modulo is not redundant: C++ leaves a negative dividend's
    // remainder negative, and Previous on the first tab is exactly that.
    const int next = ((m_groupModel->activeTabIndex() + delta) % count + count) % count;
    m_groupModel->setActive(group, next);
}

void MainWindow::onActiveTabChanged() {
    const QJsonObject active = activeTab();
    updateWindowTitle(active);
    if (active.isEmpty()) {
        // No tab at all: there is nothing to switch away *to*, and the id held
        // here may name a workspace that has just been destroyed.
        m_previousWorkspaceId.clear();
        m_agentArea->showPlaceholder();
        m_shellArea->showPlaceholder();
        m_explorer->setWorkspace(QString());
        // The editor row belongs to a workspace too: with none selected there
        // is nothing of anybody's to show.
        m_editorArea->setWorkspace(QString());
        m_pendingRunConfig.clear();
        m_runModel->setWorkspace(QString(), QString());
        rebindCost();
        return;
    }
    const QString workspaceId = active.value("workspace_id").toString();
    // Both halves of the tab change at once, and the model decides both: the
    // tab now in front stops asking, because the user is looking at it, and the
    // tab just left starts if its agent is still blocked on a question. The
    // second is not reachable from `agentStateChanged` -- switching away
    // changes the selection, not the agent -- and it is the case the whole
    // feature exists for. Answers `false` when neither moved, so an ordinary
    // tab change does not republish the model.
    //
    // The member is moved on *before* the call, not after: a `refreshAttention`
    // that changes something republishes the model, which re-enters this
    // function synchronously. Re-entering with the id already updated makes
    // that second pass a no-op on both halves, so the recursion is one level
    // deep by construction rather than by the model happening to answer
    // `false` twice.
    const QString previousWorkspaceId = m_previousWorkspaceId;
    m_previousWorkspaceId = workspaceId;
    m_groupModel->refreshAttention(previousWorkspaceId);
    m_explorer->setWorkspace(workspaceId);
    // The open files move with the agent: each has a worktree of its own, so a
    // row of tabs mixing them is a row where most of them belong to something
    // the user is not looking at. Before `restoreEditorsFor` below, which opens
    // this workspace's saved tabs into the row this just put in front.
    m_editorArea->setWorkspace(workspaceId);
    // After `setWorkspace`, which clears the header when it is handed an empty
    // id, and on every model change rather than only on a switch, so a branch
    // the daemon renamed reaches the strip.
    m_explorer->setWorkspaceHeader(active.value("name").toString(),
                                   active.value("branch").toString(),
                                   active.value("repo_path").toString(),
                                   active.value("base_branch").toString(),
                                   active.value("worktree_path").toString());
    // The tab's choice first, because `setWorkspace` publishes the list it
    // already has synchronously and the `configsChanged` slot above applies
    // this to it; the worktree path is the daemon's, from `WorkspaceInfo`.
    m_pendingRunConfig = active.value("run_config").toString();
    m_runModel->setWorkspace(workspaceId, active.value("worktree_path").toString());
    // Before `showWorkspace`, which is what builds the pane: the welcome is the
    // first thing an empty transcript has to say, and a pane built without it
    // would come up blank for as long as it took the next model change to
    // arrive.
    m_agentArea->setWelcome(workspaceId, QString::fromUtf8(QJsonDocument(active).toJson()));
    // Both areas create their terminal on the workspace's first activation and
    // keep it afterwards, so this runs on every model change and is a no-op
    // once the pane exists.
    // The agent id comes from the tab, so a workspace listed at start-up with
    // an agent already running attaches without waiting for `agentStarted`.
    m_agentArea->showWorkspace(workspaceId, active.value("adapter").toString(),
                               active.value("command").toString(),
                               active.value("agent_id").toString());
    // After the pane exists: the options are what a Restart resumes with, and
    // the transcript that holds them is created by `showWorkspace`.
    m_agentArea->setOptionsJson(workspaceId, active.value("options_json").toString());
    // The shell tab is a plain login shell in the same sandbox, whatever the
    // tab's adapter is.
    m_shellArea->showWorkspace(workspaceId, "terminal", QString());
    // The first time this workspace is shown in this run, its editors from the
    // last one come back. `restoreEditorsFor` takes the entry out of the map,
    // so this is a no-op on every later activation.
    restoreEditorsFor(workspaceId);
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
    setWindowTitle(name + dash + workspacelabel::origin(active) + dash +
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
    m_restarting.remove(workspaceId);
    m_restartUnanswered.remove(workspaceId);
    m_agentStopped.remove(workspaceId);
    m_workspaceStates.remove(workspaceId);
    // Panes first: the model change that follows re-selects a surviving tab,
    // and the areas must no longer hold the dead one when it does. The editor
    // tabs go with them: there is nothing left to save the file to.
    m_agentArea->removeWorkspace(workspaceId);
    m_shellArea->removeWorkspace(workspaceId);
    m_editorArea->closeWorkspace(workspaceId);
    m_groupModel->removeWorkspace(workspaceId);
    // The window's own bookkeeping for it. Harmless to leave -- the Rust side
    // has already forgotten the workspace, and a note for a dead one is a
    // no-op -- but a long session should not accumulate an entry per workspace
    // it has destroyed.
    m_editorsToRestore.remove(workspaceId);
    m_activeEditorToRestore.remove(workspaceId);
    m_notedEditors.remove(workspaceId);
    // After the model change, so the tab this rebinds to is the one that
    // survived rather than the one that has just gone.
    rebindCost();
}

void MainWindow::updateWorkspaceStatus() {
    const int group = m_groupModel->activeGroupIndex();
    const int tab = m_groupModel->activeTabIndex();
    const QJsonObject active = activeTab();
    // Whichever tab is in front, and whether there is one at all: the point of
    // the line is a tab the user is *not* on.
    const QString attention = m_groupModel->attentionText();
    m_attentionLabel->setText(attention);
    m_attentionLabel->setVisible(!attention.isEmpty());
    // Before the early return below, because the Workspace menu has to be
    // greyed out precisely in the case that returns early. Everything here
    // acts on the active workspace, and an entry that can only fail is worse
    // than one that is visibly unavailable; the five git actions grey
    // themselves, in the toolbar these are shared with. Next and Previous need
    // somewhere to go rather than merely something to leave.
    const bool live = !active.isEmpty();
    m_restartAgentAction->setEnabled(live);
    m_destroyAction->setEnabled(live);
    m_closeGroupAction->setEnabled(live);
    const bool several = m_groupModel->tabCount(group) > 1;
    m_nextAgentAction->setEnabled(several);
    m_prevAgentAction->setEnabled(several);
    if (active.isEmpty()) {
        m_branchLabel->setText("-");
        m_branchLabel->setToolTip(QString());
        m_sandboxLabel->setText(m_sandboxIdleText);
        return;
    }
    // The status bar is the widest of the four places a workspace is named, so
    // it is the one that spells the repository path out; the generated branch
    // and the worktree are a hover away.
    m_branchLabel->setText(workspacelabel::originFull(active));
    m_branchLabel->setToolTip(workspacelabel::detail(active));
    // The word itself comes from the model; this only frames it.
    m_sandboxLabel->setText(QString("sandbox: %1").arg(m_groupModel->statusWord(group, tab)));
}

void MainWindow::noteAgentAttention(const QString& agentId, const QString& state) {
    const QString workspaceId = m_groupModel->agentWorkspaceId(agentId);
    if (workspaceId.isEmpty()) {
        return;
    }
    // Only a tab the user is not looking at: the permission bar is already on
    // the pane in front of them, and a dot on the tab they are reading would be
    // pointing at itself.
    const bool background = workspaceId != activeWorkspaceId();
    if (background && state == QLatin1String("waiting_permission")) {
        // The sentence is the model's, so the tab's tooltip and this cannot
        // word it differently.
        m_groupModel->setWorkspaceAttention(
            workspaceId,
            m_groupModel->permissionAttention(m_groupModel->workspaceName(workspaceId)));
    } else {
        m_groupModel->clearWorkspaceAttention(workspaceId);
    }
}


// --- offscreen test entries --------------------------------------------------
//
// Compiled only into a development build: this is test code -- it builds
// widgets, leaks a QApplication and asserts -- and a shipped IDE has no caller
// for any of it. `build.rs` defines `BS_WIDGET_TESTS` for every profile but
// `release`, which is the one the packaged executable is built with.
#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`: the checks live beside the widget and
// answer a code. `bs_widget_test_begin` must have run first.
#include <QDir>
#include <QFile>
#include <cstdint>

namespace {

/// Points the IDE's `state.json` at a throwaway file, before any window exists.
///
/// A `MainWindow` reads that file in its constructor and writes it back
/// whenever a dock moves, and the path is settled once, at first use, for the
/// life of the process. Without this the checks below would restore the
/// developer's own session and then record this offscreen one over it.
void useThrowawayState() {
    const QString path = QDir::tempPath() + QStringLiteral("/bs-widget-tests-state.json");
    qputenv("BS_STATE_PATH", QFile::encodeName(path));
}

/// A window and the five models it is built from, all on the stack and in the
/// order that has the window destroyed first.
///
/// The models are real ones that were never connected to a daemon, which is
/// what the rest of this suite uses too: a window asks them for the tabs, the
/// file tree and the runs, and an unconnected model answers "none of those" to
/// every one of them without a socket anywhere.
struct WindowFixture {
    WindowFixture() = default;

    AppController controller;
    GroupModel groupModel;
    FileTreeModel fileTreeModel;
    ChangesModel changesModel;
    RunPanelModel runModel;
    MainWindow window{ &controller, &groupModel, &fileTreeModel, &changesModel, &runModel };
};

/// The menu bar's menu with this exact title, or null.
///
/// By title rather than by position: the order of the menus is a decision the
/// window is free to change, and a check that broke when Window moved past Run
/// would be checking the wrong thing.
QMenu* menuTitled(const MainWindow& window, const QString& title) {
    for (QMenu* candidate : window.menuBar()->findChildren<QMenu*>()) {
        if (candidate->title() == title) {
            return candidate;
        }
    }
    return nullptr;
}

/// The menu's first entry whose text holds `text`, or null.
///
/// A fragment rather than the whole text, so a check need not spell the
/// ellipsis that marks an entry which asks something first. The accelerator's
/// ampersand is taken out before the comparison, because it sits in the middle
/// of a word as often as in front of one -- `Ne&xt agent` -- and a check that
/// had to know where would be pinning the accelerator rather than the entry.
QAction* menuAction(const QMenu* menu, const QString& text) {
    if (menu == nullptr) {
        return nullptr;
    }
    for (QAction* action : menu->actions()) {
        if (QString(action->text()).remove(QLatin1Char('&')).contains(text)) {
            return action;
        }
    }
    return nullptr;
}

bool menuHasAction(const QMenu* menu, const QString& text) {
    return menuAction(menu, text) != nullptr;
}

bool menuActionEnabled(const QMenu* menu, const QString& text) {
    const QAction* action = menuAction(menu, text);
    return action != nullptr && action->isEnabled();
}

} // namespace

/// The agent pane is a dock: on the right, closable, floatable, movable, and
/// reachable again through its own toggle action once it has been closed.
///
/// The editor is the central widget and stays one, because a window whose
/// documents can be closed out from under it has nothing left to be.
extern "C" std::int32_t bs_widget_test_main_window_docks_the_agent_pane() {
    useThrowawayState();
    WindowFixture fixture;
    MainWindow& window = fixture.window;

    auto* agent = window.findChild<QDockWidget*>(QStringLiteral("AgentDock"));
    if (agent == nullptr) {
        return 1;
    }
    if (!agent->features().testFlag(QDockWidget::DockWidgetClosable) ||
        !agent->features().testFlag(QDockWidget::DockWidgetFloatable) ||
        !agent->features().testFlag(QDockWidget::DockWidgetMovable)) {
        return 2;
    }
    if (window.centralWidget() == nullptr) {
        return 3;
    }

    // Put back to the default first. The constructor restored whatever this
    // process last recorded, so what is checked below is the arrangement
    // `buildDocks` and `resetLayout` agree on rather than the one an earlier
    // check happened to leave behind.
    window.resetLayout();
    if (window.dockWidgetArea(agent) != Qt::RightDockWidgetArea) {
        return 4;
    }
    auto* explorer = window.findChild<QDockWidget*>(QStringLiteral("ExplorerDock"));
    if (explorer == nullptr || window.dockWidgetArea(explorer) != Qt::LeftDockWidgetArea) {
        return 5;
    }
    auto* bottom = window.findChild<QDockWidget*>(QStringLiteral("BottomDock"));
    if (bottom == nullptr || window.dockWidgetArea(bottom) != Qt::BottomDockWidgetArea) {
        return 6;
    }

    // `isHidden` rather than `isVisible`: nothing in this suite shows a window,
    // and every widget inside one that was never shown reports itself
    // invisible whatever its own state is.
    agent->close();
    if (!agent->isHidden()) {
        return 7;
    }
    agent->toggleViewAction()->trigger();
    if (agent->isHidden()) {
        return 8;
    }

    // And a dock dragged somewhere silly comes back from one call, which is
    // the whole of what Window > Reset layout offers.
    window.addDockWidget(Qt::LeftDockWidgetArea, agent);
    agent->setFloating(true);
    window.resetLayout();
    if (agent->isFloating() || window.dockWidgetArea(agent) != Qt::RightDockWidgetArea) {
        return 9;
    }
    return 0;
}

/// A dock closed by its own X has a way back, and the entry that is it says so
/// by being unticked.
///
/// The Output dock is the one this is really about: it is the one a user closes
/// by accident, and Qt's only route back to it is a right-click on the menu bar
/// that nobody finds.
extern "C" std::int32_t bs_widget_test_window_menu_brings_a_closed_dock_back() {
    useThrowawayState();
    WindowFixture fixture;
    MainWindow& window = fixture.window;

    QMenu* menu = menuTitled(window, QStringLiteral("Wi&ndow"));
    if (menu == nullptr || menu->actions().isEmpty()) {
        return 1;
    }
    for (const QString& text :
         QStringList{ QStringLiteral("Explorer"), QStringLiteral("Agent"),
                      QStringLiteral("Output"), QStringLiteral("Reset layout") }) {
        if (!menuHasAction(menu, text)) {
            return 2;
        }
    }
    auto* dock = window.findChild<QDockWidget*>(QStringLiteral("BottomDock"));
    QAction* entry = menuAction(menu, QStringLiteral("Output"));
    if (dock == nullptr || entry == nullptr || !entry->isCheckable()) {
        return 3;
    }
    // The entry *is* the dock's own toggle action, which is how the tick and
    // the dock are kept from disagreeing. Asserted by identity rather than by
    // reading the tick: Qt sets it from the show and hide events the dock
    // receives, and a window that is never shown -- which is every window in
    // this offscreen suite -- receives neither, so reading it here would be a
    // fact about the harness and not about the menu.
    if (entry != dock->toggleViewAction()) {
        return 4;
    }
    auto* agent = window.findChild<QDockWidget*>(QStringLiteral("AgentDock"));
    if (agent == nullptr || menuAction(menu, QStringLiteral("Agent")) != agent->toggleViewAction()) {
        return 5;
    }

    window.resetLayout();
    dock->close();
    if (!dock->isHidden()) {
        return 6;
    }
    entry->trigger();
    if (dock->isHidden()) {
        return 7;
    }
    return 0;
}

/// The Workspace menu holds every action that used to be somewhere less
/// findable, and the five git ones are the Changes toolbar's own objects
/// rather than copies.
///
/// The identity is the point. Enablement for those five is written once, in
/// the toolbar, and a menu carrying a second set would offer a merge on a
/// workspace that already has one out.
extern "C" std::int32_t bs_widget_test_workspace_menu_gathers_the_git_actions() {
    useThrowawayState();
    WindowFixture fixture;
    MainWindow& window = fixture.window;

    QMenu* menu = menuTitled(window, QStringLiteral("&Workspace"));
    if (menu == nullptr) {
        return 1;
    }
    for (const QString& text :
         QStringList{ QStringLiteral("New Agent"), QStringLiteral("Restart agent"),
                      QStringLiteral("Next agent"), QStringLiteral("Previous agent"),
                      QStringLiteral("Merge"), QStringLiteral("Rebase"), QStringLiteral("Squash"),
                      QStringLiteral("Create PR"), QStringLiteral("Discard"),
                      QStringLiteral("Destroy workspace"), QStringLiteral("Close group") }) {
        if (!menuHasAction(menu, text)) {
            return 2;
        }
    }
    auto* explorer = window.findChild<ExplorerDock*>(QStringLiteral("ExplorerDock"));
    if (explorer == nullptr ||
        menuAction(menu, QStringLiteral("Merge")) != explorer->changesToolbar()->mergeAction() ||
        menuAction(menu, QStringLiteral("Discard")) !=
            explorer->changesToolbar()->discardAction()) {
        return 3;
    }
    // With no workspace, everything that acts on one is dead rather than
    // offering an action that can only fail.
    for (const QString& text :
         QStringList{ QStringLiteral("Merge"), QStringLiteral("Discard"),
                      QStringLiteral("Restart agent"), QStringLiteral("Destroy workspace"),
                      QStringLiteral("Close group"), QStringLiteral("Next agent") }) {
        if (menuActionEnabled(menu, text)) {
            return 4;
        }
    }
    // New Agent needs nothing to act on: it is how a window with no workspaces
    // stops being one.
    if (!menuActionEnabled(menu, QStringLiteral("New Agent"))) {
        return 5;
    }
    if (menuAction(menu, QStringLiteral("Restart agent"))->shortcut() !=
        QKeySequence(QStringLiteral("Ctrl+Shift+R"))) {
        return 6;
    }
    return 0;
}

/// The Run menu drives the panel rather than copying it: the same actions, F5
/// where a hand expects it, and the detected configurations as one checkable
/// group so exactly one of them is what F5 will start.
extern "C" std::int32_t bs_widget_test_run_menu_drives_the_run_panel() {
    useThrowawayState();
    WindowFixture fixture;
    MainWindow& window = fixture.window;

    QMenu* menu = menuTitled(window, QStringLiteral("&Run"));
    auto* panel = window.findChild<RunPanel*>();
    if (menu == nullptr || panel == nullptr) {
        return 1;
    }
    for (const QString& text :
         QStringList{ QStringLiteral("Run"), QStringLiteral("Stop"), QStringLiteral("Restart"),
                      QStringLiteral("Open in browser"), QStringLiteral("Clear output"),
                      QStringLiteral("Copy output"), QStringLiteral("Allow blocked host") }) {
        if (!menuHasAction(menu, text)) {
            return 2;
        }
    }
    if (menuAction(menu, QStringLiteral("Run")) != panel->runAction() ||
        menuAction(menu, QStringLiteral("Stop")) != panel->stopAction()) {
        return 3;
    }
    if (panel->runAction()->shortcut() != QKeySequence(Qt::Key_F5) ||
        panel->stopAction()->shortcut() != QKeySequence(Qt::ShiftModifier | Qt::Key_F5)) {
        return 4;
    }
    // Nothing is running and no host has been refused, so the three that need
    // one or the other are dead.
    if (menuActionEnabled(menu, QStringLiteral("Stop")) ||
        menuActionEnabled(menu, QStringLiteral("Open in browser")) ||
        menuActionEnabled(menu, QStringLiteral("Allow blocked host"))) {
        return 5;
    }

    panel->noteConfigsForTest(
        QStringLiteral(R"([{"name":"web","port":3000},{"name":"api","port":8080}])"));
    const QList<QAction*> configs = panel->configurationActions();
    if (configs.size() != 2) {
        return 6;
    }
    int checkable = 0;
    for (QAction* action : menu->actions()) {
        if (action->isCheckable()) {
            ++checkable;
        }
    }
    if (checkable != 2) {
        // In the menu, not merely in the group: the entries are inserted in
        // front of an anchor, and an insertion that missed would leave the
        // list somewhere only the panel can see.
        return 7;
    }
    // One exclusive group, so picking the second unpicks the first and exactly
    // one configuration is what a Run will start. Asserted on the group rather
    // than by triggering one: which is ticked follows the model's own
    // selection, and the model here has no daemon to have detected anything,
    // so it would answer that none of these exists.
    QActionGroup* group = configs.at(0)->actionGroup();
    if (group == nullptr || !group->isExclusive() || configs.at(1)->actionGroup() != group) {
        return 8;
    }

    // A second answer from the daemon replaces the list rather than appending
    // to it, in the menu as well as in the panel.
    panel->noteConfigsForTest(QStringLiteral(R"([{"name":"web","port":3000}])"));
    checkable = 0;
    for (QAction* action : menu->actions()) {
        if (action->isCheckable()) {
            ++checkable;
        }
    }
    if (checkable != 1) {
        return 9;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
