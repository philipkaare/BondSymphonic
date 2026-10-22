#pragma once
#include <QJsonArray>
#include <QJsonObject>
#include <QWidget>

class AppController;
class QComboBox;
class QLineEdit;
class QPlainTextEdit;
class QCheckBox;
class QLabel;
class QPushButton;
class QTableWidget;
class QTcpServer;
class QFormLayout;
class QHBoxLayout;

/// Registry editor shared by Claude, Codex and OpenCode. The visible fields
/// construct typed MCP config; secret values only appear in write requests.
class McpPage : public QWidget {
    Q_OBJECT
public:
    explicit McpPage(AppController *controller, const QString &workspaceId, QWidget *parent = nullptr);
#if defined(BS_WIDGET_TESTS)
    qint64 requestSerialForTest() const { return m_pending; }
#endif
private:
    qint64 call(const QString &method, const QJsonObject &params);
    void finished(const QString &method, qint64 serial, const QString &json, const QString &error);
    void load();
    void showServer(const QString &id);
    void updateTransport();
    void updateAuth();
    void save();
    void remove();
    void previewImport();
    void applyImport();
    void exportRegistry();
    void connectServer();
    void receiveCallback();
    void closeCallback();
    AppController *m_controller;
    QString m_workspaceId;
    QComboBox *m_choice;
    QFormLayout *m_form;
    QHBoxLayout *m_envButtons;
    QHBoxLayout *m_headerButtons;
    QLineEdit *m_id;
    QLineEdit *m_name;
    QComboBox *m_transport;
    QLineEdit *m_command;
    QPlainTextEdit *m_args;
    QPlainTextEdit *m_env;
    QTableWidget *m_envSecrets;
    QLineEdit *m_url;
    QComboBox *m_auth;
    QLineEdit *m_bearer;
    QLineEdit *m_bearerRef;
    QCheckBox *m_removeBearer;
    QTableWidget *m_headers;
    QLineEdit *m_clientId;
    QLineEdit *m_metadataUrl;
    QLineEdit *m_clientSecret;
    QLineEdit *m_clientSecretRef;
    QCheckBox *m_removeClientSecret;
    QLineEdit *m_scopes;
    QCheckBox *m_enabled;
    QCheckBox *m_defaultWorkspace;
    QCheckBox *m_claude;
    QCheckBox *m_codex;
    QCheckBox *m_opencode;
    QComboBox *m_override;
    QComboBox *m_testBackend;
    QPushButton *m_testButton;
    QPushButton *m_connectButton;
    QPushButton *m_disconnectButton;
    QPlainTextEdit *m_import;
    QTableWidget *m_collisions;
    QPushButton *m_applyImport;
    QLabel *m_status;
    QJsonArray m_servers;
    QJsonArray m_statuses;
    QJsonObject m_overrides;
    qint64 m_revision = 0;
    qint64 m_serial = 0;
    qint64 m_pending = 0;
    QString m_pendingMethod;
    QString m_pendingDocument;
    QTcpServer *m_callback = nullptr;
    QString m_callbackServer;
    QString m_callbackState;
    quint64 m_callbackConnection = 0;
    quint64 m_callbackAttempt = 0;
    QJsonArray m_originalArgs;
    QString m_originalArgsText;
};
