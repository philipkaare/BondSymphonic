#pragma once
#include <QElapsedTimer>
#include <QHash>
#include <QKeySequence>
#include <QList>
#include <QMainWindow>
#include <QMetaObject>
#include <QPointer>
#include <QSet>
#include <QString>
#include <QStringList>

#include "CloseGroupDialog.h"

class AgentArea;
class AppController;
class ChangesModel;
class ChangesToolbar;
class EditorArea;
class ExplorerDock;
class FileTreeModel;
class GroupBar;
class GroupModel;
class NewAgentDialog;
class QAction;
class QCloseEvent;
class QDockWidget;
class QEvent;
class QJsonObject;
class QLabel;
class QMenu;
class QMoveEvent;
class QPlainTextEdit;
class QResizeEvent;
class QTabWidget;
class QTimer;
class RunPanel;
class RunPanelModel;
class SettingsDialog;
class TranscriptModel;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
               ChangesModel* changesModel, RunPanelModel* runModel, QWidget* parent = nullptr);

    /// Puts the three docks back where `buildDocks` puts them: Explorer left,
    /// Agent right, Output along the bottom, none of them floating or closed.
    ///
    /// Not a `restoreState` of a remembered default. A window the user has
    /// dragged into a corner needs the one arrangement that is guaranteed to
    /// exist rather than the one that happened to be saved, and the code that
    /// builds the window is where that arrangement is written down.
    void resetLayout();

protected:
    /// Refuses to close over unsaved editors without asking. Both ways out of
    /// the application arrive here -- the title bar's close button and File >
    /// Exit, which calls `close()` -- so this is the one place the question has
    /// to be put.
    ///
    /// Only the branches that really close announce the quit, through
    /// [`commitClose`]. A close the user cancels must leave a fully working
    /// IDE, and there is no way back from `AppController::prepareQuit()`.
    void closeEvent(QCloseEvent* event) override;

    /// Both record the window's geometry, debounced in Rust, so a window the
    /// user moved or resized comes back where they left it.
    void moveEvent(QMoveEvent* event) override;
    void resizeEvent(QResizeEvent* event) override;

    /// A palette change is the theme moving under the window, and the dock
    /// separators are painted in a colour mixed from it, so they have to be
    /// mixed again rather than kept. Deliberately deaf to `StyleChange`, which
    /// is what [`applySeparatorBand`]'s own `setStyleSheet` raises.
    ///
    /// Coming back to the window is the other thing that arrives here, and the
    /// moment a prerequisite is most likely to have expired while nobody was
    /// looking: see [`requestPrereqRecheck`].
    void changeEvent(QEvent* event) override;

private:
    /// Where a close request has got to. `Idle` is the ordinary state; the
    /// window is `WaitingForSaves` between a "Save All" answer and the last
    /// write landing, and `Confirmed` once it may close without asking again.
    enum class CloseState { Idle, WaitingForSaves, Confirmed };

    /// Announces the quit, writes any pending state and lets the close through.
    ///
    /// The one place the IDE says it is quitting. `prepareQuit()` cannot be
    /// undone, so it must not run on a path that can still be refused: an Exit
    /// the user cancels at the unsaved-editors prompt would otherwise leave a
    /// running IDE whose reconnect loop never relaunches the daemon again.
    void commitClose(QCloseEvent* event);

    /// Watches the editor area until nothing is dirty, then closes the window.
    /// A failed write cancels the wait instead: the edit is still only in the
    /// pane, which is what the prompt was protecting.
    void armCloseAfterSaves();
    /// Drops those watches and returns to `Idle`.
    void disarmCloseAfterSaves();

    void buildMenus();
    void buildCentral();
    /// Widens the separators between the docks and paints them in the band
    /// colour. Six pixels rather than Qt's three: the separator is the only
    /// thing saying where one pane stops and the next begins, and three pixels
    /// of window grey is neither grabbable nor visible.
    void applySeparatorBand();
    /// Opens Settings on its Setup section: the prerequisite rows, the fix
    /// buttons, the login terminal and the sign-in link.
    ///
    /// The one way in, so the status bar's "Set up..." link, the first run and
    /// anything else that wants a login all land in the same place. There is no
    /// separate setup page any more: a login is a setting, and File > Settings
    /// is where the user looks for one.
    void showSetupPage();
    void onSettings();
    /// Opens the Settings dialog, on the Setup section when asked. A second
    /// call while one is up raises the one that is up rather than stacking a
    /// second: a prerequisite re-check arrives every time a setup terminal
    /// exits, and each of those would otherwise open another dialog.
    void openSettings(bool onSetup);
    /// Decides, from the daemon's prerequisite list, between opening Settings
    /// on Setup, a status-bar warning with a way there, and neither.
    void onPrereqsChecked(const QString& json);
    /// Asks the daemon to check the prerequisites again, once, a beat from now.
    ///
    /// The prerequisites are checked at start-up and when Settings closes, and
    /// for `claude_auth` that is not enough: an OAuth session that times out
    /// mid-session leaves the IDE with a green tick, an open composer and a
    /// prompt that fails every time. Nothing polls -- a check runs the daemon's
    /// whole prerequisite suite, and one every few seconds for an answer that
    /// changes twice a month is a cost paid for nothing -- so the two things
    /// that *suggest* an answer has gone stale ask instead: an agent that
    /// exited, and the window being activated after a long absence.
    ///
    /// Debounced, because the first of those arrives in bursts: a daemon whose
    /// sandbox backend died reports every agent in the window exiting within a
    /// few milliseconds of each other, and one check answers for all of them.
    void requestPrereqRecheck();
    void buildDocks();
    /// Puts the Run panel's configuration actions back into the Run menu, in
    /// front of [`m_runConfigMenuAnchor`]. The old ones are gone by the time
    /// this runs: the panel deletes them, and a deleted `QAction` takes itself
    /// out of every menu showing it.
    void rebuildRunConfigMenu();
    void buildStatusBar();
    void connectController();
    void onConnectionStateChanged();
    /// File > New Agent. Opens the dialog; the dialog inspects the repository
    /// it opens on and reports that inspection in its own status line, so the
    /// window neither waits for the daemon nor duplicates the pipeline.
    void onNewAgent();
    void onAbout();
    /// "Destroy workspace..." from the agent tab's context menu. Both strings
    /// are the bar's reading of the tab that was clicked, taken before its menu
    /// ran: the id is what is destroyed and the name is what the confirmation
    /// asks about, so the question and the act cannot name different
    /// workspaces however much the model has moved since.
    void onDestroyRequested(const QString& workspaceId, const QString& workspaceName);
    /// An unforced destroy the daemon refused for what it would discard. Asks
    /// once more, saying what a yes costs, and a yes is the forced destroy --
    /// which is the only way to remove a workspace whose worktree git has lost
    /// track of, since the daemon cannot tell whether it is clean.
    void onDestroyRefused(const QString& workspaceId, bool dirty, bool unmerged);
    /// Whether the model still has a tab for `workspaceId`. By id, because a
    /// workspace may legitimately have no name.
    bool workspaceIsOpen(const QString& workspaceId) const;
    /// Whether `workspaceId` has a merge, pull request or discard running.
    /// Asked twice by a destroy -- before the confirmation and after it --
    /// because a merge can start while the question is on screen.
    bool isWorkspaceBusy(const QString& workspaceId) const;
    /// Says so, which is what a destroy does when it finds one.
    void sayWorkspaceIsBusy(const QString& workspaceId);
    /// A `system.check_prereqs` that did not answer. Logged, never shown.
    void onPrereqsCheckFailed(const QString& message);
    /// A `repo.inspect` that did not answer, naming the path. Logged: the New
    /// Agent dialog reports its own inspections in its own status line.
    void onRepoInspectFailed(const QString& path, const QString& message);
    /// A failure belonging to one workspace, naming it and the daemon method.
    void onWorkspaceOpFailed(const QString& workspaceId, const QString& op,
                             const QString& message);
    /// The catch-all: a failure with no family of its own.
    void onOperationFailed(const QString& op, const QString& message);
    /// Puts `message` in front of the user, parented so a modal New Agent
    /// dialog can still be closed. The one place a failure becomes a box.
    void reportFailure(const QString& op, const QString& message);
    /// Records that a typed signal has taken responsibility for `message`, so
    /// the `operationFailed` that follows it does not report it again.
    void noteFailureRouted(const QString& message);
    /// Whether `message` is the failure a typed signal just took. Consumes the
    /// record either way: it stands for exactly one `operationFailed`.
    bool takeRoutedFailure(const QString& message);
    void onActiveTabChanged();
    /// Names the active workspace in the title bar: `<name> -- <branch> --
    /// BondSymphonic`, with em dashes, or the plain application name when no
    /// tab is selected. `active` is the tab JSON `onActiveTabChanged` already
    /// read, so the title and the Explorer's header cannot disagree.
    void updateWindowTitle(const QJsonObject& active);
    void onWorkspaceDestroyed(const QString& workspaceId);

    // --- persistence -------------------------------------------------------

    /// Applies `state.json` to the window: geometry, the dock layout, and the
    /// editor tabs to reopen per workspace. Called once, before the daemon is
    /// connected.
    void onStateLoaded(const QString& json);
    /// Records `saveState()` and `saveGeometry()`. A no-op while the restore is
    /// still running, so the layout being installed is not written back over
    /// itself.
    void noteWindowState();
    /// Records every workspace's open editor tabs, and empties the record of a
    /// workspace whose last tab has just gone.
    void noteEditorState();
    /// Reopens the editors `state.json` remembered for `workspaceId`, once,
    /// the first time its tab is shown. Failures are ignored: a file the agent
    /// has since deleted is not a reason to refuse to show the workspace.
    void restoreEditorsFor(const QString& workspaceId);

    // --- merge, pull requests and closing a group ---------------------------

    /// Puts a line in the status bar. A non-empty `url` makes it a link that
    /// opens in the system browser.
    void showOperationMessage(const QString& text, const QString& url);
    /// Raises `workspaceId`'s banner and marks its tab, from a failed merge,
    /// pull request or discard.
    void showWorkspaceError(const QString& workspaceId, const QString& title,
                            const QString& detail, const QString& stderrText);
    /// Takes both down again, after an operation on that workspace worked.
    void clearWorkspaceError(const QString& workspaceId);
    /// Asks what to do with each workspace in the group called `groupName` and
    /// runs the answers. By name rather than by index for the same reason the
    /// bar sends a name: the group tabs may have moved between the click and
    /// the menu being answered.
    void onCloseGroup(const QString& groupName);
    /// Puts one confirmation in front of a close-group run that would destroy
    /// anything, naming each workspace and what goes with it. True when there
    /// is nothing to destroy, or the user said yes; false cancels the whole
    /// run, merges included.
    bool confirmDiscards(const QList<CloseGroupChoice>& choices);
    /// Test seam, armed only by `BS_MENU_TEST` and inert in every ordinary run.
    /// Prints what a context menu resolved -- the target, and the question the
    /// window would have put -- and answers true so the caller returns instead
    /// of raising a modal dialog an automated run has nobody to answer. See
    /// `GroupBar`'s own half of the seam.
    bool announceMenuTest(const char* what, const QString& target, const QString& question) const;
    /// Reports the widget settings that have no other observable effect: the
    /// New Agent dialog's status text format and name-hint colour, the palette
    /// the run is on, and the text colour of an agent tab in error. Each is a
    /// value a widget was configured with and nothing reads back, so the seam
    /// is the only way to see from outside that it is still set. Test seam; see
    /// [`announceMenuTest`].
    void reportSeamWidgets();
    /// Whether [`reportSeamWidgets`] has already run. The report waits for a
    /// tab in error and then happens once; `changed` goes on firing after it.
    bool m_seamWidgetsReported = false;

    /// Points the status bar's cost at the active tab's transcript, dropping
    /// the watch on the tab before it. A tab with no transcript costs nothing.
    void rebindCost();
    /// Writes the transcript's running cost into the label, in the four
    /// decimals a fraction of a cent needs. The number is the model's; this
    /// only frames it.
    void updateCostLabel();
    void updateWorkspaceStatus();
    /// Marks or unmarks the tab whose agent just changed state, so a permission
    /// request raised by an agent the user is *not* looking at is visible from
    /// wherever they are: a dot on that tab and a line in the status bar.
    ///
    /// Driven by `agent.state` rather than by the permission bar, because the
    /// bar lives on the workspace's own pane and a tab that has never been
    /// opened has no pane at all. The daemon moves the agent off
    /// `waiting_permission` as soon as a reply reaches it, which is what takes
    /// the mark down again.
    void noteAgentAttention(const QString& agentId, const QString& state);
    /// The tab the group model has selected, or an empty object when none is.
    QJsonObject activeTab() const;
    /// The workspace the Explorer, the panes and a newly opened file belong to.
    QString activeWorkspaceId() const;
    /// The group whose tabs are in front, or an empty string when the model has
    /// no group at that index. What Workspace > Close group acts on.
    QString activeGroupName() const;
    /// Selects the agent `delta` places along in the active group, wrapping at
    /// either end.
    ///
    /// Wrapping because the alternative is a menu entry that does nothing on
    /// the last tab, which a user cannot tell from one that is broken.
    void stepAgent(int delta);
    /// The transcript model of the visible pane when it is attached to
    /// `agentId`, or null after warning that the request was not routed.
    /// `what` names the request, for that warning.
    TranscriptModel* activeAgentModel(const QString& agentId, const char* what);
    /// Starts (or restarts) an agent for `workspaceId` with the options its tab
    /// was created with.
    void onStartAgentRequested(const QString& workspaceId);

    /// Raises the one banner that is about an agent rather than about a git
    /// command: `state` is `exited`, so this workspace's agent stopped by
    /// itself. Does nothing for every other state.
    ///
    /// It offers a Restart, and it is the only thing that does now that agents
    /// start themselves and the composer has no Start button. Deliberately not
    /// a restart of its own: see [`startAgentsThatHaveNone`].
    void onAgentExited(const QString& agentId, const QString& state, const QString& detail);

    /// Starts an agent for every Claude workspace that has none.
    ///
    /// Called after a session restore and after a reconnect, which are the two
    /// ways a Claude tab comes to exist without a start already on its way;
    /// creation is the third and starts one itself. Nobody presses anything:
    /// a Start button was a button for a thing with exactly one sensible
    /// answer.
    ///
    /// A workspace whose agent exited is skipped. That is a failure with a
    /// banner and a Restart button on it, and starting it again from here
    /// would turn one crash into a loop that looks like a working agent.
    /// Selecting such a tab is the one thing that does start it again: see
    /// [`armAutoRestart`].
    void startAgentsThatHaveNone();

    /// Starts `workspaceId`'s ended agent again, shortly, because the user has
    /// just selected its tab -- which is them asking for it.
    ///
    /// The banner's Restart is still there and still says what happened. What
    /// this adds is that looking at the tab counts as pressing it: the agent is
    /// what the workspace is for, and a user who has switched to a dead one has
    /// no other reason to be there. Delayed by [`kAutoRestartDelayMs`] and
    /// re-checked when it fires, so stepping through the tabs does not start an
    /// agent in each one on the way past.
    ///
    /// `previousWorkspaceId` is the tab this one replaced. A selection is the
    /// only trigger: `onActiveTabChanged` also runs on every republish of the
    /// model -- a status glyph, a heartbeat, a workspace the daemon renamed --
    /// and arming on those would restart an agent in a workspace the user is
    /// merely watching. A previous tab that is *gone* is not a selection
    /// either: the tab that comes forward when its neighbour is destroyed was
    /// chosen by nobody.
    void armAutoRestart(const QString& workspaceId, const QString& previousWorkspaceId);
    /// Whether an automatic start for `workspaceId` is allowed at all, asked
    /// once when the timer is armed and again when it fires. Everything that
    /// owns the failure itself says no here: a workspace with a problem (its
    /// banner has the Retry), one being restarted or destroyed, one whose
    /// sandbox is not up, one with a start already in flight, one that has
    /// already had its automatic start for this death, and one barred by
    /// [`m_autoRestartBarred`].
    bool mayAutoRestart(const QString& workspaceId) const;
    /// When each workspace's automatic start was sent, on [`m_clock`].
    ///
    /// Two jobs, both about not doing this twice. An entry means this
    /// workspace has had its one automatic start for the death it is in, so
    /// re-selecting the tab while that start is on its way -- or just after it
    /// answered, while the tab still reads `Done` because no `agent.state` has
    /// arrived yet -- cannot ask for a second agent. And the moment it was sent
    /// is what [`onAgentExited`] measures the next exit against.
    QHash<QString, qint64> m_autoRestartedAt;
    /// Workspaces whose agent died again straight after an automatic start, and
    /// so get no more of them.
    ///
    /// This is what keeps the property the banner was protecting: a crash that
    /// repeats must not become a loop that reports itself as a working agent.
    /// Lifted by the user asking for the agent themselves -- the banner's
    /// Restart -- or by a workspace restart, because both are someone deciding
    /// the thing is worth another try with the failure in front of them.
    QSet<QString> m_autoRestartBarred;
    /// The workspace an armed automatic start is for, so a burst of model
    /// changes arms one timer rather than one each.
    QString m_autoRestartArmed;
    /// The window's own monotonic clock, started in the constructor. Not the
    /// wall clock: both things measured against it are "how long ago", and a
    /// timezone change or an NTP step would make that negative.
    QElapsedTimer m_clock;
    /// The debounce behind [`requestPrereqRecheck`], made on first use.
    QTimer* m_prereqRecheck = nullptr;
    /// When the last re-check this window asked for was sent, on [`m_clock`].
    /// Zero is the launch, which is when the controller's own first check runs,
    /// so an activation in the first minute of the run asks for nothing.
    qint64 m_prereqRecheckedAt = 0;
    /// Test seam: drives the active tab to and from the workspace
    /// `select-tab=<id>` names, so the automatic start above and the guard on
    /// it can be seen from outside. See [`announceMenuTest`].
    void selectTabsForTest(const QString& workspaceId);
    /// Whether that chain of selections has been armed, and how many it has
    /// made. Test-only, and inert without `BS_SMOKE_SCRIPT`.
    bool m_seamSelecting = false;
    int m_seamSelects = 0;

    /// Workspaces whose automatic `agent.start` is still in flight.
    ///
    /// [`startAgentsThatHaveNone`] runs on every workspace change, because a
    /// sandbox that was not up on the first pass usually is on a later one. A
    /// workspace is in here from the moment its start is sent until it answers
    /// — `agentStarted` or an `agent.start` failure — so those repeated passes
    /// cannot ask for a second agent while the first request is still on the
    /// wire.
    QSet<QString> m_autoStarting;

    // --- a workspace that cannot run ----------------------------------------

    /// Puts every workspace's problem -- a sandbox that is down, a workspace
    /// that could not be started -- on its pane's banner, and takes it down
    /// from the ones that recovered. Runs on every model change; the area
    /// ignores a problem it is already showing.
    void syncWorkspaceProblems();
    /// The banner's Retry: `workspace.restart`, with the button busy until it
    /// answers.
    void onRetryWorkspace(const QString& workspaceId);
    /// The restart answered with the workspace running. The user asked for the
    /// workspace back, so its agent comes back too -- including one that had
    /// ended, which the automatic start leaves alone.
    void onWorkspaceRestarted(const QString& workspaceId, const QString& infoJson);
    /// Test seam: says what `workspaceId`'s tab and banner look like once its
    /// agent has started -- the status code the tab paints and whether the
    /// banner is up. See [`announceMenuTest`].
    void reportAgentStartedForTest(const QString& workspaceId);
    /// The tab JSON for `workspaceId`, or an empty object.
    QJsonObject tabFor(const QString& workspaceId) const;
    /// Test seam: presses the named button on `workspaceId`'s banner once, if
    /// `BS_MENU_TEST` asked for `step`. See [`announceMenuTest`].
    void pressBannerForTest(const QString& workspaceId, const char* step, const char* button);
    /// Test seam: opens `workspaceId`'s banner disclosure once, if
    /// `BS_MENU_TEST` asked for `sandbox-what-changed`, and says what it reveals.
    void revealBannerDetailForTest(const QString& workspaceId);
    /// What each workspace's banner is showing as its problem, title and
    /// detail joined, so a sync only touches the ones that changed.
    QHash<QString, QString> m_workspaceProblems;
    /// Workspaces with a `workspace.restart` in flight. The daemon stops every
    /// agent in the workspace before it restarts the sandbox, and the `exited`
    /// those stops report is the Retry at work rather than a crash: it must not
    /// raise "The agent stopped." over a workspace that is coming back, whose
    /// agent the answer starts again anyway.
    QSet<QString> m_restarting;
    /// A Retry that failed without the daemon saying why, for workspaces the
    /// daemon may well have restarted anyway, with whether the connection
    /// went with it. Resumed once the workspace is next seen running by
    /// [`resumeUnansweredRestarts`] -- after a lost connection, only once a
    /// new one has listed the workspaces, since a start sent before that
    /// fails for want of a connection.
    QHash<QString, bool> m_restartUnanswered;
    /// The daemon's own word for each workspace's state ("ready",
    /// "destroying", ...), from the last `WorkspaceInfo` the window was handed.
    /// The tab cannot say it: a workspace being destroyed and a running one
    /// whose agent ended both read as done there.
    QHash<QString, QString> m_workspaceStates;
    /// Records the state of every `WorkspaceInfo` in `json`, which is one info
    /// object or an array of them.
    void noteWorkspaceStates(const QString& json);
    /// Workspaces whose banner is showing "The agent stopped." from
    /// `onAgentExited`, so the agent coming back -- or its sandbox turning
    /// out to be what stopped it -- can take exactly that down.
    QSet<QString> m_agentStopped;
    /// A Retry that failed. See `AppController::workspaceRestartFailed`.
    void onWorkspaceRestartFailed(const QString& workspaceId, const QString& message, int kind);
    /// Starts `workspaceId`'s agent again after its sandbox came back, if it
    /// needs one. Booked in `m_autoStarting`.
    void resumeAgentAfterRestart(const QString& workspaceId);
    /// Resumes the agents of the workspaces in `m_restartUnanswered` that are
    /// running now. `listed` is true when a fresh `workspace.list` is what
    /// says so, which is the one proof a lost connection is back.
    void resumeUnansweredRestarts(bool listed);
    /// Takes down the "The agent stopped." layer on `workspaceId`, and only
    /// that one.
    void clearAgentStopped(const QString& workspaceId);
    /// The banner buttons the seam has already pressed, by step and workspace.
    QSet<QString> m_bannerTestPressed;
    /// Adds one Edit menu item forwarding to the current editor's view, and
    /// books it in for enabling and disabling together with its siblings.
    ///
    /// `shortcut` is printed beside the item and deliberately not registered:
    /// `QPlainTextEdit` already implements all six itself while it has the
    /// focus, and a window-wide copy of Ctrl+C would be taken out of the
    /// terminal pane's keyboard before it ever got there.
    void addEditAction(QMenu* menu, const QString& text, QKeySequence::StandardKey shortcut,
                       void (QPlainTextEdit::*slot)());
    void forwardToEditor(void (QPlainTextEdit::*slot)());
    /// Greys the Edit items out when the current tab is not an editor.
    void updateEditActions();

    AppController* m_controller;
    GroupModel* m_groupModel;
    FileTreeModel* m_fileTreeModel;
    ChangesModel* m_changesModel;
    RunPanelModel* m_runModel;
    GroupBar* m_groupBar = nullptr;
    /// The left dock: the active workspace's worktree.
    ExplorerDock* m_explorer = nullptr;
    /// The New Agent dialog while it is up, so daemon failures can be parented
    /// to it rather than to a window the modal dialog is blocking.
    QPointer<NewAgentDialog> m_newAgentDialog;
    /// The Settings dialog while it is up, so a second request raises it
    /// instead of opening another one over it.
    QPointer<SettingsDialog> m_settingsDialog;
    /// The failure a typed signal has just routed. `AppController` emits
    /// `operationFailed` alongside each typed failure signal, in the same step
    /// and straight after it, for as long as both are sent; without this the
    /// catch-all would put a box over a failure that has already been reported
    /// where it belongs.
    ///
    /// Consumed by the `operationFailed` that follows it, and dropped at the
    /// end of the turn either way, so a signal that stops being paired costs
    /// nothing here. See [`noteFailureRouted`] and [`takeRoutedFailure`].
    QString m_routedFailure;
    bool m_routedFailureSet = false;
    /// The centre pane: one tab per open file. Central and fixed, in the shape
    /// Visual Studio uses -- the documents are what the window is for, and the
    /// tool windows dock around them.
    EditorArea* m_editorArea = nullptr;
    /// Undo, Redo, Cut, Copy, Paste and Select All, enabled together.
    QList<QAction*> m_editActions;
    /// The Workspace menu's own entries -- the ones that are not the Changes
    /// toolbar's, which grey themselves. Each acts on the active workspace, so
    /// each is dead while there is not one; `updateWorkspaceStatus` is where
    /// that is decided, because it already runs on every tab change.
    QAction* m_restartAgentAction = nullptr;
    QAction* m_destroyAction = nullptr;
    QAction* m_closeGroupAction = nullptr;
    /// The two that need a *second* tab to go to rather than merely one tab.
    QAction* m_nextAgentAction = nullptr;
    QAction* m_prevAgentAction = nullptr;
    /// The Run menu, and the separator the detected configurations are
    /// inserted in front of. The list is the daemon's and is rebuilt whenever
    /// the panel rebuilds its combo, so the menu needs a fixed place to put it
    /// back into rather than an index that moves.
    QMenu* m_runMenu = nullptr;
    QAction* m_runConfigMenuAnchor = nullptr;
    /// The right-hand dock, holding the agent pane.
    QDockWidget* m_agentDock = nullptr;
    /// The per-workspace agent pane inside it.
    AgentArea* m_agentArea = nullptr;
    /// The bottom dock, holding the Run and Terminal tabs. A member rather than
    /// a local in `buildDocks` because the Window menu and `resetLayout` both
    /// have to reach it.
    QDockWidget* m_bottomDock = nullptr;
    QTabWidget* m_bottomTabs = nullptr;
    /// The bottom dock's Terminal tab: one shell per workspace.
    AgentArea* m_shellArea = nullptr;
    /// The bottom dock's Run tab.
    RunPanel* m_runPanel = nullptr;
    /// The run configuration the active tab was created with, still waiting for
    /// the model to have a list it appears in. `setWorkspace` publishes what it
    /// already knows before re-detecting, and `selectConfig` refuses a name that
    /// is not in the current list, so the request is offered again on every
    /// `configsChanged` until the model takes it.
    QString m_pendingRunConfig;
    QLabel* m_daemonLabel = nullptr;
    QLabel* m_sandboxLabel = nullptr;
    QLabel* m_branchLabel = nullptr;
    /// "<agent> is waiting for permission" for the first tab that is asking,
    /// or hidden. The sentence is the model's, so it and the tab's tooltip
    /// cannot word it differently.
    QLabel* m_attentionLabel = nullptr;
    /// The workspace that was in front before the current one, or empty. Kept
    /// because switching *away* from a tab whose agent is waiting produces no
    /// event of its own -- nothing about the agent changed, only the selection
    /// -- so the tab being left has to be named on the way past.
    QString m_previousWorkspaceId;
    QLabel* m_costLabel = nullptr;
    /// The status bar's way to Settings > Setup, shown while any prerequisite
    /// is failing.
    QLabel* m_setupLabel = nullptr;
    /// The last merge, pull request or close-group result. A permanent widget
    /// rather than `showMessage`, which would hide every other status widget
    /// while it was up; rich text, because a pull request's answer is a link.
    QLabel* m_opLabel = nullptr;
    /// The URL behind that line, so the click has something to open.
    QString m_opUrl;
    /// What the sandbox label shows when no tab is selected: normally a dash,
    /// or the prerequisite warning once the controller has reported one.
    QString m_sandboxIdleText;
    /// Set while `onStateLoaded` installs a layout, so the geometry and dock
    /// changes it provokes are not written straight back.
    ///
    /// Starts true and is cleared at the end of that call, which is the last
    /// thing the constructor does: the resizes the window issues while it is
    /// being built describe nothing the user chose, and recording them would
    /// overwrite the layout that is about to be restored.
    bool m_restoring = true;
    /// Editors to reopen per workspace, from `state.json`, taken out of the map
    /// the first time that workspace's tab is shown. The list is the saved tab
    /// order and is restored in it.
    QHash<QString, QStringList> m_editorsToRestore;
    /// Which of those was in front, kept apart from the order so restoring it
    /// does not move its tab.
    QHash<QString, QString> m_activeEditorToRestore;
    /// The workspaces whose editor lists were last recorded, so a workspace
    /// that has just lost its last tab has its entry emptied rather than left
    /// describing tabs that are gone.
    QSet<QString> m_notedEditors;
    /// Set while `restoreEditorsFor` is opening tabs, so the opens it makes are
    /// not recorded one at a time over the list being restored.
    bool m_restoringEditors = false;
    /// The watch on the active transcript's cost, dropped and remade whenever
    /// the active tab changes.
    QMetaObject::Connection m_costWatch;
    CloseState m_closeState = CloseState::Idle;
    /// The two connections `armCloseAfterSaves` makes, so they can be dropped
    /// again whichever way the wait ends.
    QList<QMetaObject::Connection> m_closeWatch;
};
