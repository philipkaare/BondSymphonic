#pragma once

#include <QDialog>
#include <QJsonArray>
#include <QJsonObject>
#include <QPointer>
#include <QString>
#include <QWidget>

class AppController;
class QCheckBox;
class QComboBox;
class QFormLayout;
class QLabel;
class QLineEdit;
class QPlainTextEdit;
class QPushButton;
class QTableWidget;
class QTreeWidget;
class QWidget;

/// Settings' "MCP servers" tab: the user's MCP servers with a status dot each,
/// and Add / Edit / Remove / Connect / Disconnect / Test / Import / Export.
///
/// The daemon owns the registry. This page draws the controller's listing
/// (`mcpChanged`), which already dropped replies older than the one on
/// screen, and sends each action as one controller call. Secret values never
/// come back from the daemon, so nothing here can show one.
class McpPage : public QWidget {
public:
    explicit McpPage(AppController* controller, QWidget* parent = nullptr);

private:
    void showListing(const QString& json);
    void operationFinished(const QString& op, const QString& serverId, const QString& json,
                           const QString& error);
    void updateButtons();
    QString selectedId() const;
    QJsonObject server(const QString& id) const;
    void editServer(const QJsonObject& server);
    void showExport(const QString& document);

    QPointer<AppController> m_controller;
    QTreeWidget* m_list;
    QLabel* m_registryError;
    QLabel* m_status;
    QPushButton* m_add;
    QPushButton* m_edit;
    QPushButton* m_remove;
    QPushButton* m_connect;
    QPushButton* m_disconnect;
    QPushButton* m_test;
    QPushButton* m_import;
    QPushButton* m_export;
    QPushButton* m_restoreJira;
    QJsonArray m_servers;
    QJsonArray m_statuses;
    qint64 m_revision = -1;
};

/// Adds or edits one server. Secret fields are write-only: they start blank
/// whatever is stored, blank keeps the stored value, and a Remove box deletes
/// it. Closes itself once the daemon has saved the server.
class McpServerEditor : public QDialog {
public:
    /// `server` is empty for a new one. `revision` is the registry revision
    /// the page showed, so a save over a newer registry is refused.
    McpServerEditor(AppController* controller, const QJsonObject& server, qint64 revision,
                    QWidget* parent = nullptr);

    /// The `mcpUpsert` form the fields describe, or an empty object with
    /// `error` set to what to fix.
    QJsonObject form(QString& error) const;

private:
    void updateVisibility();
    void save();

    QPointer<AppController> m_controller;
    QJsonObject m_original;
    qint64 m_revision;
    QFormLayout* m_layout;
    QLineEdit* m_id;
    QLineEdit* m_name;
    QComboBox* m_transport;
    QLineEdit* m_command;
    QPlainTextEdit* m_args;
    QPlainTextEdit* m_env;
    QTableWidget* m_secretEnv;
    QWidget* m_secretEnvButtons;
    QLineEdit* m_url;
    QComboBox* m_auth;
    QLineEdit* m_bearer;
    QCheckBox* m_removeBearer;
    QTableWidget* m_headers;
    QWidget* m_headerButtons;
    QLineEdit* m_clientId;
    QLineEdit* m_metadataUrl;
    QLineEdit* m_scopes;
    QLineEdit* m_clientSecret;
    QCheckBox* m_removeClientSecret;
    QCheckBox* m_enabled;
    QCheckBox* m_defaultWorkspace;
    QCheckBox* m_claude;
    QCheckBox* m_codex;
    QCheckBox* m_opencode;
    QLabel* m_error;
    QPushButton* m_save;
    bool m_saving = false;
};

/// Pastes an `mcpServers` document, previews what it would add, and applies
/// it once every ID collision has a resolution: replace the existing server,
/// or import under another ID.
class McpImportDialog : public QDialog {
public:
    McpImportDialog(AppController* controller, QWidget* parent = nullptr);

private:
    void preview();
    void showPreview(const QString& json);
    void updateApply();
    void apply();

    QPointer<AppController> m_controller;
    QPlainTextEdit* m_document;
    QLabel* m_summary;
    QTableWidget* m_collisions;
    QLabel* m_error;
    QPushButton* m_preview;
    QPushButton* m_apply;
    /// The document the preview on screen was made from. Editing the text
    /// invalidates it, and Apply sends exactly this.
    QString m_previewed;
    /// A preview has been asked for and not yet answered.
    bool m_waiting = false;
    bool m_hasIssues = false;
    int m_serverCount = 0;
    QJsonArray m_existingIds;
};
