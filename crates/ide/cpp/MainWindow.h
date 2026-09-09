#pragma once
#include <QMainWindow>
#include <QPointer>
#include <QString>

class AgentArea;
class AppController;
class ExplorerDock;
class FileTreeModel;
class GroupBar;
class GroupModel;
class NewAgentDialog;
class QLabel;
class QSplitter;
class QPlainTextEdit;
class QTabWidget;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
               QWidget* parent = nullptr);

private:
    void buildMenus();
    void buildCentral();
    void buildDocks();
    void buildToolBar();
    void buildStatusBar();
    void connectController();
    void onConnectionStateChanged();
    void onNewAgent();
    void onDestroyRequested(const QString& workspaceId);
    void onOperationFailed(const QString& op, const QString& message);
    void onActiveTabChanged();
    void onWorkspaceDestroyed(const QString& workspaceId);
    void updateWorkspaceStatus();

    AppController* m_controller;
    GroupModel* m_groupModel;
    FileTreeModel* m_fileTreeModel;
    GroupBar* m_groupBar = nullptr;
    /// The left dock: the active workspace's worktree.
    ExplorerDock* m_explorer = nullptr;
    /// The New Agent dialog while it is up, so daemon failures can be parented
    /// to it rather than to a window the modal dialog is blocking.
    QPointer<NewAgentDialog> m_newAgentDialog;
    QSplitter* m_centerSplitter = nullptr;
    QPlainTextEdit* m_editorPlaceholder = nullptr;
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
