#pragma once

#include <QDialog>
#include <QPointer>
#include <QString>

class AppController;
class QLabel;
class QTableWidget;

/// Which MCP servers one workspace's agents get: each server's override,
/// Inherit (the server's default), Enabled or Disabled. Opened from Workspace ›
/// "MCP servers…" and bound to the workspace it was opened for, even if the
/// tab changes. A choice is saved as it is made.
class WorkspaceMcpDialog : public QDialog {
public:
    WorkspaceMcpDialog(AppController* controller, const QString& workspaceId,
                       const QString& workspaceName, QWidget* parent = nullptr);

private:
    void showListing(const QString& json);

    QPointer<AppController> m_controller;
    QString m_workspaceId;
    QTableWidget* m_table;
    QLabel* m_message;
};
