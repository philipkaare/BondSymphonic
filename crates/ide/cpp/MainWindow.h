#pragma once
#include <QMainWindow>
#include <QPointer>
#include <QString>

class AppController;
class FileTreeModel;
class GroupBar;
class GroupModel;
class NewAgentDialog;
class QLabel;
class QSplitter;
class QPlainTextEdit;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
               QWidget* parent = nullptr);

private:
    void buildMenus();
    void buildCentral();
    void buildDocks();
    void buildStatusBar();
    void connectController();
    void onConnectionStateChanged();
    void onNewAgent();
    void onDestroyRequested(const QString& workspaceId);
    void onOperationFailed(const QString& op, const QString& message);
    void updateWorkspaceStatus();

    AppController* m_controller;
    GroupModel* m_groupModel;
    FileTreeModel* m_fileTreeModel;
    GroupBar* m_groupBar = nullptr;
    /// The New Agent dialog while it is up, so daemon failures can be parented
    /// to it rather than to a window the modal dialog is blocking.
    QPointer<NewAgentDialog> m_newAgentDialog;
    QSplitter* m_centerSplitter = nullptr;
    QPlainTextEdit* m_editorPlaceholder = nullptr;
    QPlainTextEdit* m_agentPlaceholder = nullptr;
    QLabel* m_daemonLabel = nullptr;
    QLabel* m_sandboxLabel = nullptr;
    QLabel* m_branchLabel = nullptr;
    QLabel* m_costLabel = nullptr;
    /// What the sandbox label shows when no tab is selected: normally a dash,
    /// or the prerequisite warning once the controller has reported one.
    QString m_sandboxIdleText;
};
