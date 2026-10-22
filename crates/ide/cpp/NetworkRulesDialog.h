#pragma once

#include <QDialog>
#include <QPointer>
#include <QJsonObject>
#include <QString>

class AppController;
class QDialogButtonBox;
class QLabel;
class QPlainTextEdit;

/// Edits the effective rules of one captured workspace, even if the tab changes.
class NetworkRulesDialog : public QDialog {
public:
    NetworkRulesDialog(AppController* controller, const QString& workspaceId,
                       const QString& workspaceName, QWidget* parent = nullptr);

private:
    void setBusy(bool busy);
    void save();
    QPointer<AppController> m_controller;
    QString m_workspaceId;
    QPlainTextEdit* m_allow;
    QPlainTextEdit* m_block;
    QLabel* m_error;
    QDialogButtonBox* m_buttons;
    QJsonObject m_originalRules;
    bool m_loaded = false;
    bool m_saving = false;
};
