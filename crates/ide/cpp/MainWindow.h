#pragma once
#include <QKeySequence>
#include <QList>
#include <QMainWindow>
#include <QMetaObject>
#include <QPointer>
#include <QString>

class AgentArea;
class AppController;
class ChangesModel;
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
class QPlainTextEdit;
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
    void closeEvent(QCloseEvent* event) override;

private:
    /// Where a close request has got to. `Idle` is the ordinary state; the
    /// window is `WaitingForSaves` between a "Save All" answer and the last
    /// write landing, and `Confirmed` once it may close without asking again.
    enum class CloseState { Idle, WaitingForSaves, Confirmed };

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
    void buildToolBar();
    void buildStatusBar();
    void connectController();
    void onConnectionStateChanged();
    void onNewAgent();
    void onAbout();
    void onDestroyRequested(const QString& workspaceId);
    void onOperationFailed(const QString& op, const QString& message);
    void onActiveTabChanged();
    void onWorkspaceDestroyed(const QString& workspaceId);
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
    /// What the sandbox label shows when no tab is selected: normally a dash,
    /// or the prerequisite warning once the controller has reported one.
    QString m_sandboxIdleText;
    /// The watch on the active transcript's cost, dropped and remade whenever
    /// the active tab changes.
    QMetaObject::Connection m_costWatch;
    CloseState m_closeState = CloseState::Idle;
    /// The two connections `armCloseAfterSaves` makes, so they can be dropped
    /// again whichever way the wait ends.
    QList<QMetaObject::Connection> m_closeWatch;
};
