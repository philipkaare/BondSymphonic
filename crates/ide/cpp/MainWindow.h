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
class QStackedWidget;
class QTabWidget;
class RunPanel;
class RunPanelModel;
class SetupPage;
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
    /// Puts the setup page in front of the workbench, or takes it away again.
    /// Both directions go through here so the two are never both current.
    void showSetupPage();
    void showWorkbench();
    void onSettings();
    /// Decides, from the daemon's prerequisite list, between the setup page,
    /// a status-bar warning with a way back to it, and neither.
    void onPrereqsChecked(const QString& json);
    void buildDocks();
    void buildStatusBar();
    void connectController();
    void onConnectionStateChanged();
    void onNewAgent();
    void onAbout();
    void onDestroyRequested(const QString& workspaceId);
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
    /// Asks what to do with each workspace in the group at `groupIndex` and
    /// runs the answers.
    void onCloseGroup(int groupIndex);
    /// Puts one confirmation in front of a close-group run that would destroy
    /// anything, naming each workspace and what goes with it. True when there
    /// is nothing to destroy, or the user said yes; false cancels the whole
    /// run, merges included.
    bool confirmDiscards(const QList<CloseGroupChoice>& choices);
    /// Points the status bar's cost at the active tab's transcript, dropping
    /// the watch on the tab before it. A tab with no transcript costs nothing.
    void rebindCost();
    /// Writes the transcript's running cost into the label, in the four
    /// decimals a fraction of a cent needs. The number is the model's; this
    /// only frames it.
    void updateCostLabel();
    void updateWorkspaceStatus();
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
    /// The central widget: the workbench, and the setup page in front of it.
    QStackedWidget* m_stack = nullptr;
    QWidget* m_workbench = nullptr;
    SetupPage* m_setupPage = nullptr;
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
    QLabel* m_costLabel = nullptr;
    /// The status bar's way back to the setup page, shown only while a
    /// non-blocking prerequisite is failing.
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
