#pragma once
#include <QKeySequence>
#include <QList>
#include <QMainWindow>
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
class QJsonObject;
class QLabel;
class QMenu;
class QPlainTextEdit;
class QSplitter;
class QTabWidget;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
               ChangesModel* changesModel, QWidget* parent = nullptr);

private:
    void buildMenus();
    void buildCentral();
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
    void updateWorkspaceStatus();
    /// The tab the group model has selected, or an empty object when none is.
    QJsonObject activeTab() const;
    /// The workspace the Explorer, the panes and a newly opened file belong to.
    QString activeWorkspaceId() const;
    /// Adds one Edit menu item forwarding to the focused editor's view, and
    /// books it in for enabling and disabling together with its siblings.
    void addEditAction(QMenu* menu, const QString& text, QKeySequence::StandardKey shortcut,
                       void (QPlainTextEdit::*slot)());
    void forwardToEditor(void (QPlainTextEdit::*slot)());
    /// The Edit items act on the editor, so they are live only while the editor
    /// has the focus. That is not tidiness: their shortcuts are window-wide, and
    /// an enabled Ctrl+C here would be taken from the terminal pane.
    void updateEditActions();

    AppController* m_controller;
    GroupModel* m_groupModel;
    FileTreeModel* m_fileTreeModel;
    ChangesModel* m_changesModel;
    GroupBar* m_groupBar = nullptr;
    /// The left dock: the active workspace's worktree.
    ExplorerDock* m_explorer = nullptr;
    /// The New Agent dialog while it is up, so daemon failures can be parented
    /// to it rather than to a window the modal dialog is blocking.
    QPointer<NewAgentDialog> m_newAgentDialog;
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
    QLabel* m_daemonLabel = nullptr;
    QLabel* m_sandboxLabel = nullptr;
    QLabel* m_branchLabel = nullptr;
    QLabel* m_costLabel = nullptr;
    /// What the sandbox label shows when no tab is selected: normally a dash,
    /// or the prerequisite warning once the controller has reported one.
    QString m_sandboxIdleText;
};
