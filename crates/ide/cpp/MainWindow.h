#pragma once
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
class QJsonObject;
class QLabel;
class QMenu;
class QMoveEvent;
class QPlainTextEdit;
class QResizeEvent;
class QSplitter;
class QTabWidget;
class RunPanel;
class RunPanelModel;
class SettingsDialog;
class TranscriptModel;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
               ChangesModel* changesModel, RunPanelModel* runModel, QWidget* parent = nullptr);

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
    void buildDocks();
    void buildStatusBar();
    void connectController();
    void onConnectionStateChanged();
    /// File > New Agent. Asks the daemon about the repository the dialog will
    /// open on *before* opening it, so the base-branch combo is filled the
    /// moment the dialog appears.
    ///
    /// The dialog used to open first and fill itself in later, which on a large
    /// repository reached through `/mnt/c` meant a branch list that was still
    /// empty when the user pressed Create.
    void onNewAgent();
    /// Builds and runs the New Agent dialog, seeded with an inspection the
    /// window already has. `distroPath` empty means there was nothing to
    /// inspect; otherwise exactly one of `infoJson` and `error` is set.
    void openNewAgentDialog(const QString& initialPath, const QString& distroPath,
                            const QString& infoJson, const QString& error);
    void onAbout();
    /// "Destroy workspace..." from the agent tab's context menu. Both strings
    /// are the bar's reading of the tab that was clicked, taken before its menu
    /// ran: the id is what is destroyed and the name is what the confirmation
    /// asks about, so the question and the act cannot name different
    /// workspaces however much the model has moved since.
    void onDestroyRequested(const QString& workspaceId, const QString& workspaceName);
    void onOperationFailed(const QString& op, const QString& message);
    void onActiveTabChanged();
    /// Names the active workspace in the title bar: `<name> -- <branch> --
    /// BondSymphonic`, with em dashes, or the plain application name when no
    /// tab is selected. `active` is the tab JSON `onActiveTabChanged` already
    /// read, so the title and the Explorer's header cannot disagree.
    void updateWindowTitle(const QJsonObject& active);
    void onWorkspaceDestroyed(const QString& workspaceId);

    // --- persistence -------------------------------------------------------

    /// Applies `state.json` to the window: geometry, dock layout, the centre
    /// splitter and its swap, and the editor tabs to reopen per workspace.
    /// Called once, before the daemon is connected.
    void onStateLoaded(const QString& json);
    /// Records `saveState()` and `saveGeometry()`. A no-op while the restore is
    /// still running, so the layout being installed is not written back over
    /// itself.
    void noteWindowState();
    /// Records the centre splitter's sizes and whether its halves are swapped.
    void noteSplitterState();
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
    /// The transcript model of the visible pane when it is attached to
    /// `agentId`, or null after warning that the request was not routed.
    /// `what` names the request, for that warning.
    TranscriptModel* activeAgentModel(const QString& agentId, const char* what);
    /// Starts (or restarts) an agent for `workspaceId` with the options its tab
    /// was created with.
    void onStartAgentRequested(const QString& workspaceId);
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
    /// Whether the `repo.inspect` that precedes the dialog is still out. A
    /// second New Agent while one is being prepared would start a second
    /// inspection and open two dialogs when both answered.
    bool m_newAgentPending = false;
    /// The Settings dialog while it is up, so a second request raises it
    /// instead of opening another one over it.
    QPointer<SettingsDialog> m_settingsDialog;
    /// Whether Settings has already been shown during the current run of
    /// blocking prerequisite failures. See `AppController::shouldAutoOpenSetup`:
    /// closing the dialog re-checks, so without this a machine that is still
    /// blocked reopens it every time it is closed. Cleared the moment nothing
    /// blocks any more, so a later failure opens it again.
    bool m_setupShownForBlock = false;
    QSplitter* m_centerSplitter = nullptr;
    /// The centre pane: one tab per open file.
    EditorArea* m_editorArea = nullptr;
    /// Undo, Redo, Cut, Copy, Paste and Select All, enabled together.
    QList<QAction*> m_editActions;
    /// The per-workspace agent pane beside the editor.
    AgentArea* m_agentArea = nullptr;
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
    /// What that line said before "Reading repository…" took it over: the
    /// rendered text, its tooltip and the URL behind it. A pull request's
    /// answer is a link the user may still want, and opening New Agent is no
    /// reason to lose it.
    QString m_opTextBeforeInspect;
    QString m_opTipBeforeInspect;
    QString m_opUrlBeforeInspect;
    /// What the sandbox label shows when no tab is selected: normally a dash,
    /// or the prerequisite warning once the controller has reported one.
    QString m_sandboxIdleText;
    /// Whether the swap in the View menu has been applied, so the arrangement
    /// can be persisted and restored. The splitter itself only knows the order
    /// of its children.
    bool m_swapped = false;
    /// Set while `onStateLoaded` installs a layout, so the geometry and
    /// splitter changes it provokes are not written straight back.
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
