#include "McpPage.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QCheckBox>
#include <QComboBox>
#include <QDesktopServices>
#include <QDialog>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QGroupBox>
#include <QGridLayout>
#include <QHeaderView>
#include <QHBoxLayout>
#include <QHostAddress>
#include <QJsonDocument>
#include <QRegularExpression>
#include <QSet>
#include <QLabel>
#include <QLineEdit>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QStackedWidget>
#include <QTableWidget>
#include <QTcpServer>
#include <QTcpSocket>
#include <QTimer>
#include <QUrl>
#include <QUrlQuery>
#include <QUuid>
#include <QVBoxLayout>

namespace {
QTableWidget *secretTable(QWidget *parent, const QString &name) {
    auto *table = new QTableWidget(0, 4, parent);
    table->setObjectName(name);
    table->setHorizontalHeaderLabels({"Name", "Secret reference", "New value", "Action"});
    table->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    table->setMinimumHeight(100);
    return table;
}
void addSecretRow(QTableWidget *table, const QString &name, const QString &reference, bool existing) {
    const int row = table->rowCount(); table->insertRow(row);
    table->setItem(row, 0, new QTableWidgetItem(name));
    table->setItem(row, 1, new QTableWidgetItem(reference));
    auto *value = new QLineEdit(table); value->setEchoMode(QLineEdit::Password);
    value->setPlaceholderText(existing ? "Empty keeps stored value" : "Enter value");
    table->setCellWidget(row, 2, value);
    auto *action = new QComboBox(table);
    action->addItem("Keep", "keep"); action->addItem("Replace", "set"); action->addItem("Remove", "remove");
    if (!existing) action->setCurrentIndex(1);
    table->setCellWidget(row, 3, action);
}
QString cell(QTableWidget *table, int row, int col) {
    auto *item = table->item(row, col); return item ? item->text().trimmed() : QString();
}
bool prepareSecretRows(QTableWidget *table, const QString &serverId, QString &error) {
    for (int row = 0; row < table->rowCount(); ++row) {
        const QString name = cell(table, row, 0);
        if (name.isEmpty()) { error = "Every secret row needs a name."; return false; }
        QString ref = cell(table, row, 1);
        if (ref.isEmpty()) {
            ref = serverId + "_" + name;
            for (int i = 0; i < ref.size(); ++i)
                if (!ref.at(i).isLetterOrNumber() && ref.at(i) != '-' && ref.at(i) != '_') ref[i] = '_';
            table->setItem(row, 1, new QTableWidgetItem(ref));
        }
        auto *action = qobject_cast<QComboBox *>(table->cellWidget(row, 3));
        auto *value = qobject_cast<QLineEdit *>(table->cellWidget(row, 2));
        if (action && action->currentData().toString() == "set" && (!value || value->text().isEmpty())) {
            error = "Enter a new value or choose Keep or Remove for each secret."; return false;
        }
    }
    return true;
}
QJsonArray collectSecrets(QTableWidget *table, QJsonObject &refs) {
    QJsonArray writes;
    for (int row = 0; row < table->rowCount(); ++row) {
        const QString name = cell(table, row, 0), ref = cell(table, row, 1);
        if (name.isEmpty() || ref.isEmpty()) continue;
        auto *action = qobject_cast<QComboBox *>(table->cellWidget(row, 3));
        auto *value = qobject_cast<QLineEdit *>(table->cellWidget(row, 2));
        const QString choice = action ? action->currentData().toString() : "keep";
        if (choice == "remove") { writes.append(QJsonObject{{"action", "remove"}, {"reference", ref}}); continue; }
        refs.insert(name, ref);
        if (value && !value->text().isEmpty()) writes.append(QJsonObject{{"action", "set"}, {"reference", ref}, {"value", value->text()}});
        else if (choice == "keep") writes.append(QJsonObject{{"action", "keep"}, {"reference", ref}});
    }
    return writes;
}
void fillSecrets(QTableWidget *table, const QJsonObject &refs) {
    table->setRowCount(0);
    for (auto it = refs.begin(); it != refs.end(); ++it) addSecretRow(table, it.key(), it.value().toString(), true);
}
void removeSecretRow(QTableWidget *table) {
    const int row = table->currentRow();
    if (row < 0) return;
    if (cell(table, row, 1).isEmpty()) { table->removeRow(row); return; }
    auto *action = qobject_cast<QComboBox *>(table->cellWidget(row, 3));
    if (action) action->setCurrentIndex(action->findData("remove"));
}
QStringList lines(QPlainTextEdit *edit) {
    QStringList values;
    for (const auto &line : edit->toPlainText().split('\n')) if (!line.trimmed().isEmpty()) values.append(line);
    return values;
}
QSet<QString> referencesFor(const QJsonObject &transport) {
    QSet<QString> refs;
    if (transport.value("type").toString() == "stdio") {
        for (const auto &value : transport.value("env_secrets").toObject()) refs.insert(value.toString());
    } else {
        const auto auth = transport.value("auth").toObject();
        if (auth.value("type").toString() == "bearer") refs.insert(auth.value("token").toString());
        if (auth.value("type").toString() == "oauth") refs.insert(auth.value("client_secret").toString());
        for (const auto &value : auth.value("headers").toObject()) refs.insert(value.toString());
    }
    refs.remove(QString());
    return refs;
}
}

McpPage::McpPage(AppController *controller, const QString &workspaceId, QWidget *parent)
    : QWidget(parent), m_controller(controller), m_workspaceId(workspaceId) {
    auto *layout = new QVBoxLayout(this);
    auto *top = new QGridLayout();
    m_choice = new QComboBox(this); m_choice->setObjectName("McpServerList");
    top->addWidget(m_choice, 0, 0, 1, 4);
    auto *jira = new QPushButton("Add Jira Cloud", this); jira->setObjectName("AddJiraMcp");
    auto *restoreJira = new QPushButton("Restore Jira preset", this); restoreJira->setObjectName("RestoreJiraMcp");
    auto *add = new QPushButton("Add server", this);
    auto *removeButton = new QPushButton("Remove", this);
    top->addWidget(jira, 1, 0); top->addWidget(restoreJira, 1, 1);
    top->addWidget(add, 1, 2); top->addWidget(removeButton, 1, 3);
    layout->addLayout(top);
    auto *form = new QFormLayout(); m_form = form; form->setRowWrapPolicy(QFormLayout::WrapLongRows);
    m_id = new QLineEdit(this); m_id->setObjectName("McpServerId");
    m_name = new QLineEdit(this); m_name->setObjectName("McpServerName");
    m_transport = new QComboBox(this); m_transport->setObjectName("McpTransport");
    m_transport->addItem("Sandbox command (stdio)", "stdio");
    m_transport->addItem("Streamable HTTP", "http");
    form->addRow("Stable ID:", m_id); form->addRow("Name:", m_name);
    form->addRow("Transport:", m_transport);
    m_command = new QLineEdit(this); m_command->setObjectName("McpCommand");
    m_args = new QPlainTextEdit(this); m_args->setObjectName("McpArguments");
    m_args->setPlaceholderText("One argument per line"); m_args->setMaximumHeight(75);
    m_env = new QPlainTextEdit(this); m_env->setObjectName("McpEnvironment");
    m_env->setPlaceholderText("Nonsecret NAME=value, one per line"); m_env->setMaximumHeight(75);
    m_envSecrets = secretTable(this, "McpEnvSecrets");
    auto *envButtons = new QHBoxLayout(); m_envButtons = envButtons;
    auto *addEnv = new QPushButton("Add secret", this), *removeEnv = new QPushButton("Remove row", this);
    envButtons->addWidget(addEnv); envButtons->addWidget(removeEnv);
    form->addRow("Command:", m_command); form->addRow("Arguments:", m_args);
    form->addRow("Environment:", m_env); form->addRow("Environment secrets:", m_envSecrets);
    form->addRow(QString(), envButtons);
    m_url = new QLineEdit(this); m_url->setObjectName("McpUrl");
    m_auth = new QComboBox(this); m_auth->setObjectName("McpAuth");
    for (auto pair : {qMakePair("None", "none"), qMakePair("Bearer", "bearer"),
                      qMakePair("Secret headers", "headers"), qMakePair("OAuth", "oauth")}) m_auth->addItem(pair.first, pair.second);
    m_bearerRef = new QLineEdit(this); m_bearerRef->setObjectName("McpBearerReference");
    m_bearer = new QLineEdit(this); m_bearer->setObjectName("McpBearerValue"); m_bearer->setEchoMode(QLineEdit::Password);
    m_bearer->setPlaceholderText("Empty keeps stored token");
    m_removeBearer = new QCheckBox("Remove stored bearer token", this);
    m_headers = secretTable(this, "McpHeaders");
    auto *headerButtons = new QHBoxLayout(); m_headerButtons = headerButtons;
    auto *addHeader = new QPushButton("Add header", this), *removeHeader = new QPushButton("Remove row", this);
    headerButtons->addWidget(addHeader); headerButtons->addWidget(removeHeader);
    m_clientId = new QLineEdit(this); m_clientId->setObjectName("McpOAuthClientId");
    m_metadataUrl = new QLineEdit(this); m_metadataUrl->setObjectName("McpOAuthMetadataUrl");
    m_scopes = new QLineEdit(this); m_scopes->setObjectName("McpOAuthScopes");
    m_scopes->setPlaceholderText("Space-separated scopes");
    m_clientSecretRef = new QLineEdit(this); m_clientSecretRef->setObjectName("McpOAuthSecretReference");
    m_clientSecret = new QLineEdit(this); m_clientSecret->setObjectName("McpOAuthSecretValue");
    m_clientSecret->setEchoMode(QLineEdit::Password); m_clientSecret->setPlaceholderText("Empty keeps stored value");
    m_removeClientSecret = new QCheckBox("Remove client secret", this);
    form->addRow("HTTPS URL:", m_url); form->addRow("Authentication:", m_auth);
    form->addRow("Bearer reference:", m_bearerRef); form->addRow("New bearer token:", m_bearer);
    form->addRow(QString(), m_removeBearer);
    form->addRow("Secret headers:", m_headers); form->addRow(QString(), headerButtons);
    form->addRow("OAuth client ID:", m_clientId); form->addRow("OAuth metadata URL:", m_metadataUrl);
    form->addRow("OAuth scopes:", m_scopes); form->addRow("Client secret reference:", m_clientSecretRef);
    form->addRow("New client secret:", m_clientSecret); form->addRow(QString(), m_removeClientSecret);
    m_enabled = new QCheckBox("Enabled globally", this); m_defaultWorkspace = new QCheckBox("Enabled in new workspaces", this);
    m_claude = new QCheckBox("Claude", this); m_codex = new QCheckBox("Codex", this);
    m_opencode = new QCheckBox("OpenCode", this);
    auto *backends = new QHBoxLayout(); backends->addWidget(m_claude); backends->addWidget(m_codex); backends->addWidget(m_opencode);
    form->addRow(m_enabled); form->addRow(m_defaultWorkspace); form->addRow("Available to:", backends);
    m_override = new QComboBox(this); m_override->setObjectName("McpWorkspaceOverride");
    m_override->addItem("Use default", "inherit"); m_override->addItem("Enabled", "enabled"); m_override->addItem("Disabled", "disabled");
    form->addRow(QString("This workspace (%1):").arg(workspaceId.isEmpty() ? "none selected" : workspaceId), m_override);
    m_override->setEnabled(!workspaceId.isEmpty());
    layout->addLayout(form);
    auto *buttons = new QGridLayout();
    auto *saveButton = new QPushButton("Save server", this); saveButton->setObjectName("SaveMcpServer");
    auto *test = new QPushButton("Test connection", this); m_testButton = test; test->setObjectName("TestMcpServer");
    auto *connectButton = new QPushButton("Connect account", this); m_connectButton = connectButton; connectButton->setObjectName("ConnectMcpServer");
    auto *disconnect = new QPushButton("Disconnect", this); m_disconnectButton = disconnect; disconnect->setObjectName("DisconnectMcpServer");
    m_testBackend = new QComboBox(this); for (const auto &id : {"claude", "codex", "opencode"}) m_testBackend->addItem(id, id);
    buttons->addWidget(saveButton, 0, 0); buttons->addWidget(m_testBackend, 0, 1); buttons->addWidget(test, 0, 2);
    buttons->addWidget(connectButton, 1, 0, 1, 2); buttons->addWidget(disconnect, 1, 2);
    layout->addLayout(buttons);
    auto *importBox = new QGroupBox("Import or export", this);
    auto *importLayout = new QVBoxLayout(importBox);
    m_import = new QPlainTextEdit(importBox); m_import->setObjectName("McpImportDocument");
    m_import->setPlaceholderText("Paste a common mcpServers JSON document"); m_import->setMaximumHeight(100);
    importLayout->addWidget(m_import);
    m_collisions = new QTableWidget(0, 2, importBox); m_collisions->setObjectName("McpImportCollisions");
    m_collisions->setHorizontalHeaderLabels({"Imported name", "Existing stable ID to replace"});
    m_collisions->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    m_collisions->setMaximumHeight(100); importLayout->addWidget(m_collisions);
    auto *importButtons = new QHBoxLayout();
    auto *preview = new QPushButton("Preview import", importBox); preview->setObjectName("PreviewMcpImport");
    m_applyImport = new QPushButton("Apply preview", importBox); m_applyImport->setObjectName("ApplyMcpImport");
    m_applyImport->setEnabled(false);
    auto *exportButton = new QPushButton("Export configuration", importBox);
    importButtons->addWidget(preview); importButtons->addWidget(m_applyImport); importButtons->addWidget(exportButton);
    importLayout->addLayout(importButtons); layout->addWidget(importBox);
    m_status = new QLabel(this); m_status->setObjectName("McpStatus");
    m_status->setWordWrap(true); m_status->setTextFormat(Qt::PlainText); layout->addWidget(m_status);
    connect(m_choice, &QComboBox::currentIndexChanged, this, [this] {
        m_pending = m_controller->nextSettingsRequestId(); m_serial = m_pending; m_pendingMethod.clear();
        closeCallback(); showServer(m_choice->currentData().toString());
    });
    connect(m_transport, &QComboBox::currentIndexChanged, this, &McpPage::updateTransport);
    connect(m_auth, &QComboBox::currentIndexChanged, this, &McpPage::updateAuth);
    connect(add, &QPushButton::clicked, this, [this] {
        m_choice->setCurrentIndex(-1); m_id->setText(QUuid::createUuid().toString(QUuid::WithoutBraces));
        m_id->setReadOnly(false);
        m_name->clear(); m_transport->setCurrentIndex(0); m_command->clear(); m_args->clear(); m_env->clear();
        m_originalArgs = {}; m_originalArgsText.clear();
        m_envSecrets->setRowCount(0); m_url->clear(); m_auth->setCurrentIndex(0); m_headers->setRowCount(0);
        m_bearerRef->clear(); m_bearer->clear(); m_removeBearer->setChecked(false);
        m_clientId->clear(); m_metadataUrl->clear(); m_scopes->clear();
        m_clientSecretRef->clear(); m_clientSecret->clear(); m_removeClientSecret->setChecked(false);
        m_override->blockSignals(true); m_override->setCurrentIndex(0); m_override->blockSignals(false);
        m_enabled->setChecked(true); m_defaultWorkspace->setChecked(true);
        m_claude->setChecked(true); m_codex->setChecked(true); m_opencode->setChecked(true);
        m_status->setText("Configure the server, then save it.");
    });
    connect(jira, &QPushButton::clicked, this, [this, add] {
        const int existing = m_choice->findData("jira");
        if (existing >= 0) { m_choice->setCurrentIndex(existing); return; }
        add->click(); m_name->setText("Jira Cloud"); m_transport->setCurrentIndex(1);
        m_id->setText("jira");
        m_url->setText("https://mcp.atlassian.com/v2/mcp"); m_auth->setCurrentIndex(m_auth->findData("oauth"));
    });
    connect(restoreJira, &QPushButton::clicked, this, [this] {
        closeCallback(); call("mcp.restore_preset", {}); m_status->setText("Restoring Jira preset…");
    });
    connect(removeButton, &QPushButton::clicked, this, &McpPage::remove);
    connect(saveButton, &QPushButton::clicked, this, &McpPage::save);
    connect(addEnv, &QPushButton::clicked, this, [this] { addSecretRow(m_envSecrets, {}, {}, false); });
    connect(removeEnv, &QPushButton::clicked, this, [this] { removeSecretRow(m_envSecrets); });
    connect(addHeader, &QPushButton::clicked, this, [this] { addSecretRow(m_headers, {}, {}, false); });
    connect(removeHeader, &QPushButton::clicked, this, [this] { removeSecretRow(m_headers); });
    connect(test, &QPushButton::clicked, this, [this] {
        if (m_workspaceId.isEmpty()) { m_status->setText("Select a workspace to test tools."); return; }
        if (!m_testButton->isEnabled()) { m_status->setText(m_testButton->toolTip()); return; }
        call("mcp.test", {{"server_id", m_choice->currentData().toString()}, {"workspace_id", m_workspaceId}, {"backend", m_testBackend->currentData().toString()}});
    });
    connect(connectButton, &QPushButton::clicked, this, &McpPage::connectServer);
    connect(disconnect, &QPushButton::clicked, this, [this] {
        closeCallback(); call("mcp.disconnect", {{"server_id", m_choice->currentData().toString()}});
    });
    connect(m_override, &QComboBox::currentIndexChanged, this, [this] {
        const QString id = m_choice->currentData().toString();
        if (id.isEmpty() || m_workspaceId.isEmpty()) return;
        call("mcp.override", {{"server_id", id}, {"workspace_id", m_workspaceId},
                              {"value", m_override->currentData().toString()}});
    });
    connect(m_import, &QPlainTextEdit::textChanged, this, [this] { m_applyImport->setEnabled(false); });
    connect(preview, &QPushButton::clicked, this, &McpPage::previewImport);
    connect(m_applyImport, &QPushButton::clicked, this, &McpPage::applyImport);
    connect(exportButton, &QPushButton::clicked, this, &McpPage::exportRegistry);
    connect(controller, &AppController::settingsRpcFinished, this,
        [this](const QString &method, qint64 serial, const QString &json, const QString &error) { finished(method, serial, json, error); });
    connect(controller, &AppController::connectionStateChanged, this, [this] {
        if (m_callback && static_cast<quint64>(m_controller->connectionGenerationNumber()) != m_callbackConnection)
            closeCallback();
    });
    const auto capabilities = QJsonDocument::fromJson(controller->capabilitiesJson().toUtf8()).object().value("mcp").toObject();
    const bool sandboxTest = capabilities.value("stdio").toBool() && capabilities.value("streamable_http").toBool();
    m_testButton->setEnabled(sandboxTest && !workspaceId.isEmpty());
    m_testBackend->setEnabled(m_testButton->isEnabled());
    if (!sandboxTest) m_testButton->setToolTip("MCP tests require a Linux sandbox bridge.");
    else if (workspaceId.isEmpty()) m_testButton->setToolTip("Select a workspace to test tools.");
    updateTransport(); updateAuth(); load();
}

qint64 McpPage::call(const QString &method, const QJsonObject &params) {
    m_pending = m_controller->nextSettingsRequestId(); m_serial = m_pending; m_pendingMethod = method;
    m_controller->settingsRpc(method, QString::fromUtf8(QJsonDocument(params).toJson(QJsonDocument::Compact)), m_pending);
    return m_pending;
}

void McpPage::load() { call("mcp.list", {}); }

void McpPage::updateTransport() {
    const bool stdio = m_transport->currentData().toString() == "stdio";
    for (QWidget *widget : QList<QWidget *>{m_command, m_args, m_env, m_envSecrets}) m_form->setRowVisible(widget, stdio);
    m_form->setRowVisible(m_envButtons, stdio);
    m_form->setRowVisible(m_url, !stdio);
    m_form->setRowVisible(m_auth, !stdio);
    updateAuth();
}

void McpPage::updateAuth() {
    const bool http = m_transport->currentData().toString() == "http";
    const QString auth = m_auth->currentData().toString();
    const bool oauth = http && auth == "oauth";
    m_connectButton->setVisible(oauth);
    m_disconnectButton->setVisible(oauth);
    for (QWidget *widget : QList<QWidget *>{m_bearerRef, m_bearer, m_removeBearer}) m_form->setRowVisible(widget, http && auth == "bearer");
    m_form->setRowVisible(m_headers, http && auth == "headers");
    m_form->setRowVisible(m_headerButtons, http && auth == "headers");
    for (QWidget *widget : QList<QWidget *>{m_clientId, m_metadataUrl, m_scopes,
                         m_clientSecretRef, m_clientSecret, m_removeClientSecret}) m_form->setRowVisible(widget, oauth);
}

void McpPage::showServer(const QString &id) {
    for (const auto &value : m_servers) {
        const auto server = value.toObject();
        if (server.value("id").toString() != id) continue;
        m_id->setText(id); m_name->setText(server.value("name").toString());
        m_id->setReadOnly(true);
        m_enabled->setChecked(server.value("enabled").toBool(true));
        m_defaultWorkspace->setChecked(server.value("default_workspace_enabled").toBool(true));
        const auto backends = server.value("enabled_backends").toArray();
        m_claude->setChecked(backends.contains("claude")); m_codex->setChecked(backends.contains("codex"));
        m_opencode->setChecked(backends.contains("opencode"));
        const auto transport = server.value("transport").toObject();
        const QString type = transport.value("type").toString();
        m_transport->setCurrentIndex(qMax(0, m_transport->findData(type)));
        if (type == "stdio") {
            m_command->setText(transport.value("command").toString());
            QStringList args; for (const auto &v : transport.value("args").toArray()) args.append(v.toString());
            m_args->setPlainText(args.join('\n'));
            m_originalArgs = transport.value("args").toArray(); m_originalArgsText = m_args->toPlainText();
            QStringList env; const auto vars = transport.value("env").toObject();
            for (auto it = vars.begin(); it != vars.end(); ++it) env.append(it.key() + "=" + it.value().toString());
            m_env->setPlainText(env.join('\n'));
            fillSecrets(m_envSecrets, transport.value("env_secrets").toObject());
        } else {
            m_originalArgs = {}; m_originalArgsText.clear();
            m_url->setText(transport.value("url").toString());
            const auto auth = transport.value("auth").toObject();
            const QString authType = auth.value("type").toString("none");
            m_auth->setCurrentIndex(qMax(0, m_auth->findData(authType)));
            m_bearerRef->setText(auth.value("token").toString()); m_bearer->clear(); m_removeBearer->setChecked(false);
            fillSecrets(m_headers, auth.value("headers").toObject());
            m_clientId->setText(auth.value("client_id").toString());
            m_metadataUrl->setText(auth.value("client_metadata_url").toString());
            m_clientSecretRef->setText(auth.value("client_secret").toString());
            m_clientSecret->clear(); m_removeClientSecret->setChecked(false);
            QStringList scopes; for (const auto &v : auth.value("scopes").toArray()) scopes.append(v.toString());
            m_scopes->setText(scopes.join(' '));
        }
        // The registry snapshot owns overrides and is refreshed after edits.
        const auto workspace = m_overrides.value(m_workspaceId).toObject();
        const int overrideIndex = m_override->findData(workspace.value(id).toString("inherit"));
        m_override->blockSignals(true); m_override->setCurrentIndex(qMax(0, overrideIndex)); m_override->blockSignals(false);
        m_status->setText("Connection status unavailable.");
        for (const auto &status : m_statuses) {
            const auto s = status.toObject();
            if (s.value("server_id").toString() == id)
                m_status->setText(QString("%1%2").arg(s.value("state").toString(),
                    s.value("reason").toString().isEmpty() ? QString() : ": " + s.value("reason").toString()));
        }
        return;
    }
}

void McpPage::save() {
    const QString id = m_id->text().trimmed(), name = m_name->text().trimmed();
    if (id.isEmpty() || name.isEmpty()) { m_status->setText("Stable ID and name are required."); return; }
    if (!QRegularExpression("^[A-Za-z0-9_-]{1,128}$").match(id).hasMatch()) {
        m_status->setText("Stable ID may contain letters, digits, hyphens, and underscores."); return;
    }
    QJsonArray writes, backends;
    QString secretError;
    const bool stdio = m_transport->currentData().toString() == "stdio";
    const bool needsHeaders = !stdio && m_auth->currentData().toString() == "headers";
    if ((stdio && !prepareSecretRows(m_envSecrets, id, secretError)) ||
        (needsHeaders && !prepareSecretRows(m_headers, id, secretError))) {
        m_status->setText(secretError); return;
    }
    if (m_claude->isChecked()) backends.append("claude");
    if (m_codex->isChecked()) backends.append("codex");
    if (m_opencode->isChecked()) backends.append("opencode");
    QJsonObject transport;
    if (m_transport->currentData().toString() == "stdio") {
        if (m_command->text().trimmed().isEmpty()) { m_status->setText("Enter the sandbox command."); return; }
        QJsonArray args;
        if (m_args->toPlainText() == m_originalArgsText) args = m_originalArgs;
        else if (!m_args->toPlainText().isEmpty())
            for (const auto &line : m_args->toPlainText().split('\n', Qt::KeepEmptyParts)) args.append(line);
        QJsonObject env;
        for (const auto &line : lines(m_env)) {
            const int equals = line.indexOf('=');
            if (equals <= 0) { m_status->setText("Environment lines must be NAME=value."); return; }
            env.insert(line.left(equals).trimmed(), line.mid(equals + 1));
        }
        QJsonObject refs; writes = collectSecrets(m_envSecrets, refs);
        transport = {{"type", "stdio"}, {"command", m_command->text().trimmed()}, {"args", args},
                     {"env", env}, {"env_secrets", refs}};
    } else {
        const QUrl url(m_url->text().trimmed());
        if (!url.isValid() || url.scheme() != "https") { m_status->setText("Enter an HTTPS MCP URL."); return; }
        QJsonObject auth{{"type", m_auth->currentData().toString()}};
        const QString type = m_auth->currentData().toString();
        if (type == "bearer") {
            const QString ref = m_bearerRef->text().trimmed().isEmpty() ? id + "_bearer" : m_bearerRef->text().trimmed();
            if (m_bearerRef->text().trimmed().isEmpty() && m_bearer->text().isEmpty() && !m_removeBearer->isChecked()) {
                m_status->setText("Enter a bearer token for this new reference."); return;
            }
            auth.insert("token", ref);
            if (m_removeBearer->isChecked()) writes.append(QJsonObject{{"action", "remove"}, {"reference", ref}});
            else if (!m_bearer->text().isEmpty()) writes.append(QJsonObject{{"action", "set"}, {"reference", ref}, {"value", m_bearer->text()}});
            else writes.append(QJsonObject{{"action", "keep"}, {"reference", ref}});
        } else if (type == "headers") {
            QJsonObject refs; writes = collectSecrets(m_headers, refs); auth.insert("headers", refs);
        } else if (type == "oauth") {
            if (!m_clientId->text().trimmed().isEmpty()) auth.insert("client_id", m_clientId->text().trimmed());
            if (!m_metadataUrl->text().trimmed().isEmpty()) auth.insert("client_metadata_url", m_metadataUrl->text().trimmed());
            QJsonArray scopes; for (const auto &scope : m_scopes->text().split(' ', Qt::SkipEmptyParts)) scopes.append(scope);
            auth.insert("scopes", scopes);
            if (!m_clientSecretRef->text().trimmed().isEmpty()) {
                const QString ref = m_clientSecretRef->text().trimmed(); auth.insert("client_secret", ref);
                if (m_removeClientSecret->isChecked()) writes.append(QJsonObject{{"action", "remove"}, {"reference", ref}});
                else if (!m_clientSecret->text().isEmpty()) writes.append(QJsonObject{{"action", "set"}, {"reference", ref}, {"value", m_clientSecret->text()}});
                else writes.append(QJsonObject{{"action", "keep"}, {"reference", ref}});
            }
        }
        transport = {{"type", "http"}, {"url", url.toString()}, {"auth", auth}};
    }
    for (const auto &existingValue : m_servers) {
        const auto existing = existingValue.toObject();
        if (existing.value("id").toString() != id) continue;
        const auto dropped = referencesFor(existing.value("transport").toObject()) - referencesFor(transport);
        for (const auto &reference : dropped)
            writes.append(QJsonObject{{"action", "remove"}, {"reference", reference}});
    }
    closeCallback();
    call("mcp.upsert", {{"server", QJsonObject{{"id", id}, {"name", name}, {"transport", transport},
        {"enabled", m_enabled->isChecked()}, {"default_workspace_enabled", m_defaultWorkspace->isChecked()},
        {"enabled_backends", backends}}}, {"secrets", writes}, {"expected_revision", m_revision}});
    m_status->setText("Saving server…");
}

void McpPage::remove() {
    const QString id = m_choice->currentData().toString(); if (id.isEmpty()) return;
    closeCallback(); call("mcp.remove", {{"server_id", id}}); m_status->setText("Removing server…");
}

void McpPage::previewImport() {
    m_pendingDocument = m_import->toPlainText(); m_applyImport->setEnabled(false);
    call("mcp.import_preview", {{"document", m_pendingDocument}, {"resolutions", QJsonObject{}}, {"expected_revision", m_revision}});
    m_status->setText("Checking imported servers…");
}

void McpPage::applyImport() {
    if (!m_applyImport->isEnabled() || m_pendingDocument != m_import->toPlainText()) return;
    QJsonObject resolutions;
    for (int row = 0; row < m_collisions->rowCount(); ++row) {
        const QString name = cell(m_collisions, row, 0), id = cell(m_collisions, row, 1);
        if (name.isEmpty() || id.isEmpty()) { m_status->setText("Resolve every collision before applying."); return; }
        resolutions.insert(name, id);
    }
    call("mcp.import_apply", {{"document", m_pendingDocument}, {"resolutions", resolutions}, {"expected_revision", m_revision}});
    m_applyImport->setEnabled(false); m_status->setText("Applying import…");
}

void McpPage::exportRegistry() { call("mcp.export", {}); }

void McpPage::closeCallback() {
    if (m_callback) { m_callback->close(); m_callback->deleteLater(); m_callback = nullptr; }
    m_callbackServer.clear(); m_callbackState.clear(); m_callbackConnection = 0;
}

void McpPage::connectServer() {
    const QString id = m_choice->currentData().toString(); if (id.isEmpty()) return;
    closeCallback();
    m_callback = new QTcpServer(this);
    if (!m_callback->listen(QHostAddress::LocalHost, 0)) {
        m_status->setText("Could not open a temporary browser callback on localhost."); closeCallback(); return;
    }
    m_callbackServer = id;
    const quint64 attempt = ++m_callbackAttempt;
    m_callbackConnection = static_cast<quint64>(m_controller->connectionGenerationNumber());
    connect(m_callback, &QTcpServer::newConnection, this, &McpPage::receiveCallback);
    QTimer::singleShot(300000, this, [this, id, generation = m_callbackConnection, attempt] {
        if (m_callback && m_callbackServer == id && m_callbackConnection == generation && m_callbackAttempt == attempt) {
            closeCallback(); m_status->setText("Sign-in timed out. Choose Connect account to retry.");
        }
    });
    const QString uri = QString("http://127.0.0.1:%1/callback").arg(m_callback->serverPort());
    call("mcp.connect", {{"server_id", id}, {"redirect_uri", uri}});
    m_status->setText("Preparing browser sign-in…");
}

void McpPage::receiveCallback() {
    if (!m_callback || !m_callback->hasPendingConnections()) return;
    auto *socket = m_callback->nextPendingConnection();
    QTimer::singleShot(30000, socket, [socket] { socket->disconnectFromHost(); });
    connect(socket, &QTcpSocket::readyRead, this, [this, socket] {
        if (!m_callback || socket->bytesAvailable() > 8192) { socket->disconnectFromHost(); return; }
        QByteArray request = socket->property("mcpRequest").toByteArray();
        request += socket->readAll(); socket->setProperty("mcpRequest", request);
        if (request.size() > 8192) { socket->disconnectFromHost(); return; }
        const int end = request.indexOf("\r\n"); if (end < 0) return;
        const auto first = request.left(end).split(' ');
        const QUrl url = first.size() >= 2 ? QUrl(QString::fromUtf8(first[1])) : QUrl();
        const QUrlQuery query(url);
        const QString state = query.queryItemValue("state", QUrl::FullyDecoded);
        const QString code = query.queryItemValue("code", QUrl::FullyDecoded);
        const QString issuer = query.queryItemValue("iss", QUrl::FullyDecoded);
        const bool valid = first.size() >= 2 && first[0] == "GET" && url.path() == "/callback" &&
            !state.isEmpty() && !code.isEmpty() && state == m_callbackState &&
            static_cast<quint64>(m_controller->connectionGenerationNumber()) == m_callbackConnection;
        const QByteArray body = valid ? "Sign-in received. Return to BondSymphonic." : "This sign-in attempt is no longer active.";
        socket->write("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: " + QByteArray::number(body.size()) + "\r\n\r\n" + body);
        socket->disconnectFromHost();
        if (!valid) return;
        const QString server = m_callbackServer;
        closeCallback();
        QJsonObject params{{"server_id", server}, {"state", state}, {"code", code}};
        if (query.hasQueryItem("iss")) params.insert("issuer", issuer);
        call("mcp.callback", params); m_status->setText("Completing sign-in…");
    });
    connect(socket, &QTcpSocket::disconnected, socket, &QObject::deleteLater);
}

void McpPage::finished(const QString &method, qint64 serial, const QString &json, const QString &error) {
    if (serial != m_pending || method != m_pendingMethod) return;
    if (!error.isEmpty()) {
        if (method == "mcp.connect") closeCallback();
        m_status->setText(error); return;
    }
    const auto document = QJsonDocument::fromJson(json.toUtf8());
    if (method == "mcp.connect") {
        const QUrl authorization(document.object().value("authorization_url").toString());
        m_callbackState = QUrlQuery(authorization).queryItemValue("state", QUrl::FullyDecoded);
        if (m_callbackState.isEmpty() || !authorization.isValid() ||
            static_cast<quint64>(m_controller->connectionGenerationNumber()) != m_callbackConnection) {
            closeCallback(); m_status->setText("The sign-in link was invalid or the connection changed. Try again."); return;
        }
        if (!QDesktopServices::openUrl(authorization)) {
            closeCallback(); m_status->setText("Could not open the browser. Try Connect account again."); return;
        }
        m_status->setText("Finish sign-in in your browser. This window is listening only for this attempt.");
        return;
    }
    if (method == "mcp.callback" || method == "mcp.disconnect") {
        m_status->setText(method == "mcp.callback" ? "Account connected." : "Account disconnected.");
        load(); return;
    }
    if (method == "mcp.test") {
        const auto result = document.object();
        QStringList tools; for (const auto &tool : result.value("tool_names").toArray()) tools.append(tool.toString());
        m_status->setText(QString("%1%2").arg(result.value("status").toObject().value("state").toString(),
            tools.isEmpty() ? QString() : ": " + tools.join(", ")));
        return;
    }
    if (method == "mcp.export") {
        auto *dialog = new QDialog(this); dialog->setAttribute(Qt::WA_DeleteOnClose);
        dialog->setWindowTitle("MCP configuration export"); dialog->resize(650, 450);
        auto *layout = new QVBoxLayout(dialog);
        auto *edit = new QPlainTextEdit(dialog); edit->setReadOnly(true);
        const auto wrapped = QJsonDocument::fromJson(("[" + json + "]").toUtf8());
        edit->setPlainText(wrapped.isArray() ? wrapped.array().at(0).toString(json) : json);
        layout->addWidget(edit);
        auto *buttons = new QDialogButtonBox(QDialogButtonBox::Close, dialog);
        connect(buttons, &QDialogButtonBox::rejected, dialog, &QDialog::close);
        layout->addWidget(buttons); dialog->open(); return;
    }
    if (method == "mcp.import_preview") {
        const auto preview = document.object();
        const auto issues = preview.value("issues").toArray();
        const auto collisions = preview.value("collisions").toArray();
        const auto servers = preview.value("servers").toArray();
        m_collisions->setRowCount(0);
        for (const auto &idValue : collisions) {
            const QString id = idValue.toString();
            QString source = id;
            for (const auto &serverValue : servers) {
                const auto server = serverValue.toObject();
                if (server.value("id").toString() == id) source = server.value("name").toString(id);
            }
            const int row = m_collisions->rowCount(); m_collisions->insertRow(row);
            m_collisions->setItem(row, 0, new QTableWidgetItem(source));
            m_collisions->setItem(row, 1, new QTableWidgetItem(id));
        }
        QStringList details;
        for (const auto &issueValue : issues) {
            const auto issue = issueValue.toObject();
            details.append(QString("%1 — %2: %3").arg(issue.value("entry").toString(),
                issue.value("field").toString(), issue.value("reason").toString()));
        }
        m_status->setText(details.isEmpty() ? QString("%1 server(s) ready. Check collisions before applying.").arg(servers.size())
                                            : details.join('\n'));
        m_applyImport->setEnabled(!servers.isEmpty() && issues.isEmpty()); return;
    }
    if (method == "mcp.import_apply" || method == "mcp.upsert" || method == "mcp.remove" ||
        method == "mcp.override" || method == "mcp.restore_preset") {
        m_status->setText(method == "mcp.import_apply" ? "Import applied." : "Server settings saved.");
        load(); return;
    }
    if (method == "mcp.list") {
        const auto listing = document.object();
        const auto registry = listing.value("registry").toObject();
        m_revision = registry.value("revision").toInteger();
        m_servers = registry.value("servers").toArray();
        m_overrides = registry.value("overrides").toObject();
        m_statuses = listing.value("statuses").toArray();
        const QString selected = m_choice->currentData().toString();
        m_choice->blockSignals(true); m_choice->clear();
        for (const auto &value : m_servers) {
            const auto server = value.toObject();
            m_choice->addItem(server.value("name").toString(), server.value("id").toString());
        }
        m_choice->setCurrentIndex(qMax(0, m_choice->findData(selected)));
        m_choice->blockSignals(false);
        showServer(m_choice->currentData().toString());
    }
}
