#include "McpPage.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QApplication>
#include <QCheckBox>
#include <QClipboard>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPainter>
#include <QPixmap>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QRegularExpression>
#include <QSet>
#include <QTableWidget>
#include <QTreeWidget>
#include <QVBoxLayout>
#include <functional>

namespace {

/// The preset's ID, which "Restore Jira preset" brings back.
const QString kJiraId = QStringLiteral("jira");

/// When an edit reaches an agent. The daemon resolves the server set when an
/// agent starts, and stops a server's sockets the moment it is turned off.
const char* const kApplyNote =
    "New or enabled servers apply when an agent next starts. Disabling, removing or "
    "disconnecting a server takes effect immediately, in every running agent.";

QLabel* plainLabel(const QString& text, QWidget* parent) {
    auto* label = new QLabel(text, parent);
    label->setTextFormat(Qt::PlainText);
    label->setWordWrap(true);
    return label;
}

QJsonObject parse(const QString& json) {
    return QJsonDocument::fromJson(json.toUtf8()).object();
}

QString compact(const QJsonObject& object) {
    return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
}

QString authType(const QJsonObject& server) {
    const QJsonObject transport = server.value("transport").toObject();
    if (transport.value("type").toString() != QLatin1String("http")) { return QString(); }
    return transport.value("auth").toObject().value("type").toString(QStringLiteral("none"));
}

bool isRemote(const QJsonObject& server) {
    return server.value("transport").toObject().value("type").toString() == QLatin1String("http");
}

/// A filled circle in `colour`, the status dot in front of a server's name.
QIcon dot(const QColor& colour) {
    QPixmap pixmap(12, 12);
    pixmap.fill(Qt::transparent);
    QPainter painter(&pixmap);
    painter.setRenderHint(QPainter::Antialiasing);
    painter.setPen(Qt::NoPen);
    painter.setBrush(colour);
    painter.drawEllipse(1, 1, 10, 10);
    return QIcon(pixmap);
}

/// The words and the dot colour for one server's row.
struct StatusLook {
    QString text;
    QColor colour;
    QString state;
};

StatusLook statusLook(const QJsonObject& server, const QJsonObject& status, const QPalette& palette) {
    const bool dark = theme::isDark(palette);
    const QColor grey = theme::muted(palette);
    if (!server.value("enabled").toBool(true)) {
        return {QStringLiteral("Off"), grey, QStringLiteral("off")};
    }
    if (!isRemote(server)) {
        return {QStringLiteral("Starts with each agent"), grey, QStringLiteral("local")};
    }
    const QString state = status.value("state").toString(QStringLiteral("disconnected"));
    const QString reason = status.value("reason").toString();
    const QString withReason = reason.isEmpty() ? QString() : QStringLiteral(": ") + reason;
    if (state == QLatin1String("connected")) {
        return {QStringLiteral("Connected"), theme::ink(theme::added(), dark), state};
    }
    if (state == QLatin1String("connecting")) {
        return {QStringLiteral("Signing in…"), theme::ink(theme::changed(), dark), state};
    }
    if (state == QLatin1String("error")) {
        return {QStringLiteral("Error") + withReason, theme::ink(theme::removed(), dark), state};
    }
    return {authType(server) == QLatin1String("oauth") ? QStringLiteral("Not signed in")
                                                       : QStringLiteral("Not tested"),
            grey, state};
}

/// The ID the daemon gives an imported entry with no resolution: its name,
/// lower-cased, with anything but letters, digits, - and _ made _, at most 32.
QString generatedId(const QString& name) {
    QString id;
    for (const char32_t c : name.toUcs4()) {
        if (id.size() == 32) { break; }
        if (c < 0x80 && (QChar(c).isLetterOrNumber() || c == '-' || c == '_')) {
            id.append(QChar(c).toLower());
        } else {
            id.append(QLatin1Char('_'));
        }
    }
    return id;
}

bool validServerId(const QString& id) {
    static const QRegularExpression pattern(QStringLiteral("^[a-z0-9][a-z0-9_-]{0,31}$"));
    return pattern.match(id).hasMatch() && id != QLatin1String("bondsymphonic");
}

// --- the write-only secret table ---------------------------------------------
//
// Column 0 is the name (an env variable or a header), column 1 a password field
// that starts blank. A row the server already had keeps its secret reference in
// the name item's data and says so in the placeholder; deleting the row drops
// the reference, and the daemon drops the stored value with it.

QTableWidget* secretTable(const QString& name, const QString& column, QWidget* parent) {
    auto* table = new QTableWidget(0, 2, parent);
    table->setObjectName(name);
    table->setHorizontalHeaderLabels({column, QStringLiteral("Value")});
    table->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    table->verticalHeader()->hide();
    table->setMinimumHeight(90);
    return table;
}

void addSecretRow(QTableWidget* table, const QString& name, const QString& reference) {
    const int row = table->rowCount();
    table->insertRow(row);
    auto* item = new QTableWidgetItem(name);
    item->setData(Qt::UserRole, reference);
    // A stored secret is renamed by removing its row and adding another, so the
    // reference and the value it holds never drift apart.
    if (!reference.isEmpty()) { item->setFlags(item->flags() & ~Qt::ItemIsEditable); }
    table->setItem(row, 0, item);
    auto* value = new QLineEdit(table);
    value->setEchoMode(QLineEdit::Password);
    value->setPlaceholderText(reference.isEmpty() ? QStringLiteral("Value")
                                                  : QStringLiteral("Stored; leave blank to keep"));
    table->setCellWidget(row, 1, value);
}

QWidget* secretButtons(QTableWidget* table, const QString& addText, QWidget* parent) {
    auto* box = new QWidget(parent);
    auto* layout = new QHBoxLayout(box);
    layout->setContentsMargins(0, 0, 0, 0);
    auto* add = new QPushButton(addText, box);
    auto* remove = new QPushButton(QStringLiteral("Remove"), box);
    remove->setToolTip(QStringLiteral("Removes the row, and the stored value with it when saved."));
    layout->addWidget(add);
    layout->addWidget(remove);
    layout->addStretch(1);
    QObject::connect(add, &QPushButton::clicked, table, [table] {
        addSecretRow(table, QString(), QString());
        table->editItem(table->item(table->rowCount() - 1, 0));
    });
    QObject::connect(remove, &QPushButton::clicked, table, [table] {
        if (table->currentRow() >= 0) { table->removeRow(table->currentRow()); }
    });
    return box;
}

/// One row's name, its reference (`fallback(name)` for a new row) and the
/// secret write for its value. False with `error` set for a nameless row.
bool readSecretRows(QTableWidget* table, const std::function<QString(const QString&)>& reference,
                    QJsonObject& names, QJsonArray& secrets, QString& error) {
    for (int row = 0; row < table->rowCount(); ++row) {
        const QString name = table->item(row, 0) ? table->item(row, 0)->text().trimmed() : QString();
        auto* value = qobject_cast<QLineEdit*>(table->cellWidget(row, 1));
        if (name.isEmpty()) {
            error = QStringLiteral("Every secret row needs a name.");
            return false;
        }
        if (names.contains(name)) {
            error = QStringLiteral("%1 is listed twice.").arg(name);
            return false;
        }
        const QString stored = table->item(row, 0)->data(Qt::UserRole).toString();
        const QString ref = stored.isEmpty() ? reference(name) : stored;
        names.insert(name, ref);
        secrets.append(QJsonObject{{"reference", ref}, {"value", value ? value->text() : QString()}});
    }
    return true;
}

} // namespace

// --- McpPage -------------------------------------------------------------------

McpPage::McpPage(AppController* controller, QWidget* parent)
    : QWidget(parent), m_controller(controller) {
    setObjectName(QStringLiteral("McpPage"));
    auto* layout = new QVBoxLayout(this);
    auto* intro = plainLabel(
        QStringLiteral("MCP servers give agents more tools. Local servers run inside each agent's "
                       "sandbox; remote servers are reached over HTTPS through the daemon, which "
                       "keeps their sign-ins and secrets out of the sandbox."),
        this);
    layout->addWidget(intro);
    auto* note = plainLabel(QString::fromUtf8(kApplyNote), this);
    note->setObjectName(QStringLiteral("McpApplyNote"));
    layout->addWidget(note);

    m_registryError = plainLabel(QStringLiteral("Loading MCP servers…"), this);
    m_registryError->setObjectName(QStringLiteral("McpRegistryError"));
    layout->addWidget(m_registryError);

    m_list = new QTreeWidget(this);
    m_list->setObjectName(QStringLiteral("McpServerList"));
    m_list->setColumnCount(4);
    m_list->setHeaderLabels({QStringLiteral("Server"), QStringLiteral("ID"),
                             QStringLiteral("Transport"), QStringLiteral("Status")});
    m_list->setRootIsDecorated(false);
    m_list->setSelectionMode(QAbstractItemView::SingleSelection);
    m_list->header()->setSectionResizeMode(QHeaderView::ResizeToContents);
    m_list->header()->setStretchLastSection(true);
    m_list->hide();
    layout->addWidget(m_list, 1);

    auto* buttons = new QHBoxLayout();
    const auto button = [this, buttons](const QString& text, const char* name) {
        auto* b = new QPushButton(text, this);
        b->setObjectName(QLatin1String(name));
        buttons->addWidget(b);
        return b;
    };
    m_add = button(QStringLiteral("Add…"), "McpAdd");
    m_edit = button(QStringLiteral("Edit…"), "McpEdit");
    m_remove = button(QStringLiteral("Remove"), "McpRemove");
    m_connect = button(QStringLiteral("Connect"), "McpConnect");
    m_connect->setToolTip(QStringLiteral("Sign in to this server in your browser."));
    m_disconnect = button(QStringLiteral("Disconnect"), "McpDisconnect");
    m_disconnect->setToolTip(QStringLiteral("Forget this server's sign-in. Its tools stop working at once."));
    m_test = button(QStringLiteral("Test"), "McpTest");
    m_test->setToolTip(QStringLiteral("Connect from the daemon and list the server's tools."));
    buttons->addStretch(1);
    m_import = button(QStringLiteral("Import…"), "McpImport");
    m_export = button(QStringLiteral("Export…"), "McpExport");
    layout->addLayout(buttons);

    m_restoreJira = new QPushButton(QStringLiteral("Restore Jira preset"), this);
    m_restoreJira->setObjectName(QStringLiteral("McpRestoreJira"));
    m_restoreJira->setToolTip(QStringLiteral("Add the Jira (Atlassian) server back, signed out."));
    m_restoreJira->hide();
    auto* restoreRow = new QHBoxLayout();
    restoreRow->addWidget(m_restoreJira);
    restoreRow->addStretch(1);
    layout->addLayout(restoreRow);

    m_status = plainLabel(QString(), this);
    m_status->setObjectName(QStringLiteral("McpStatus"));
    layout->addWidget(m_status);

    connect(m_list, &QTreeWidget::itemSelectionChanged, this, &McpPage::updateButtons);
    connect(m_list, &QTreeWidget::itemDoubleClicked, this, [this] { m_edit->click(); });
    connect(m_add, &QPushButton::clicked, this, [this] { editServer(QJsonObject()); });
    connect(m_edit, &QPushButton::clicked, this, [this] {
        const QJsonObject s = server(selectedId());
        if (!s.isEmpty()) { editServer(s); }
    });
    connect(m_remove, &QPushButton::clicked, this, [this] {
        const QString id = selectedId();
        if (id.isEmpty() || m_controller.isNull()) { return; }
        const QString name = server(id).value("name").toString(id);
        QString question = QStringLiteral("Remove \"%1\"? Running agents lose its tools at once, "
                                          "and its stored secrets and sign-in are deleted.").arg(name);
        if (id == kJiraId) { question += QStringLiteral(" \"Restore Jira preset\" brings the preset back."); }
        if (QMessageBox::question(this, QStringLiteral("Remove MCP server"), question) !=
            QMessageBox::Yes) {
            return;
        }
        m_status->setText(QStringLiteral("Removing %1…").arg(name));
        m_controller->mcpRemove(id);
    });
    connect(m_connect, &QPushButton::clicked, this, [this] {
        const QString id = selectedId();
        if (id.isEmpty() || m_controller.isNull()) { return; }
        m_status->setText(QStringLiteral("Preparing the sign-in…"));
        m_controller->mcpConnect(id);
    });
    connect(m_disconnect, &QPushButton::clicked, this, [this] {
        const QString id = selectedId();
        if (id.isEmpty() || m_controller.isNull()) { return; }
        m_status->setText(QStringLiteral("Disconnecting…"));
        m_controller->mcpDisconnect(id);
    });
    connect(m_test, &QPushButton::clicked, this, [this] {
        const QString id = selectedId();
        if (id.isEmpty() || m_controller.isNull()) { return; }
        m_status->setText(QStringLiteral("Testing %1…").arg(server(id).value("name").toString(id)));
        m_controller->mcpTest(id);
    });
    connect(m_import, &QPushButton::clicked, this, [this] {
        if (m_controller.isNull()) { return; }
        (new McpImportDialog(m_controller, this))->open();
    });
    connect(m_export, &QPushButton::clicked, this, [this] {
        if (m_controller.isNull()) { return; }
        m_status->setText(QStringLiteral("Exporting…"));
        m_controller->mcpExport();
    });
    connect(m_restoreJira, &QPushButton::clicked, this, [this] {
        if (m_controller.isNull()) { return; }
        m_status->setText(QStringLiteral("Restoring the Jira preset…"));
        m_controller->mcpRestorePreset();
    });
    connect(controller, &AppController::mcpChanged, this, &McpPage::showListing);
    connect(controller, &AppController::mcpOperationFinished, this, &McpPage::operationFinished);

    showListing(controller->mcpViewJson());
    controller->mcpList();
}

QString McpPage::selectedId() const {
    const auto items = m_list->selectedItems();
    return items.isEmpty() ? QString() : items.first()->data(0, Qt::UserRole).toString();
}

QJsonObject McpPage::server(const QString& id) const {
    for (const auto& value : m_servers) {
        if (value.toObject().value("id").toString() == id) { return value.toObject(); }
    }
    return QJsonObject();
}

void McpPage::showListing(const QString& json) {
    const QJsonObject listing = parse(json);
    const QString selected = selectedId();
    if (!listing.contains(QStringLiteral("registry"))) {
        // `{}` before the first answer, `{"error"}` from a degraded registry:
        // the message replaces the list rather than sitting under a stale one.
        m_servers = QJsonArray();
        m_statuses = QJsonArray();
        m_revision = -1;
        m_list->clear();
        m_list->hide();
        m_registryError->setText(listing.value("error").toString(QStringLiteral("Loading MCP servers…")));
        m_registryError->show();
        m_restoreJira->hide();
        updateButtons();
        return;
    }
    const QJsonObject registry = listing.value("registry").toObject();
    m_revision = registry.value("revision").toInteger();
    m_servers = registry.value("servers").toArray();
    m_statuses = listing.value("statuses").toArray();
    m_registryError->hide();
    m_list->show();
    m_list->clear();
    bool haveJira = false;
    for (const auto& value : m_servers) {
        const QJsonObject s = value.toObject();
        const QString id = s.value("id").toString();
        haveJira |= id == kJiraId;
        QJsonObject status;
        for (const auto& st : m_statuses) {
            if (st.toObject().value("server_id").toString() == id) { status = st.toObject(); }
        }
        const StatusLook look = statusLook(s, status, palette());
        auto* item = new QTreeWidgetItem(m_list);
        item->setData(0, Qt::UserRole, id);
        item->setData(3, Qt::UserRole, look.state);
        item->setIcon(0, dot(look.colour));
        item->setText(0, s.value("name").toString(id));
        item->setText(1, id);
        const QJsonObject transport = s.value("transport").toObject();
        item->setText(2, isRemote(s) ? transport.value("url").toString()
                                     : QStringLiteral("Local: ") + transport.value("command").toString());
        item->setText(3, look.text);
        item->setToolTip(3, look.text);
        if (id == selected) { item->setSelected(true); }
    }
    m_restoreJira->setVisible(!haveJira);
    updateButtons();
}

void McpPage::updateButtons() {
    const bool loaded = m_revision >= 0;
    const QJsonObject s = server(selectedId());
    const bool chosen = loaded && !s.isEmpty();
    const bool oauth = authType(s) == QLatin1String("oauth");
    m_add->setEnabled(loaded);
    m_edit->setEnabled(chosen);
    m_remove->setEnabled(chosen);
    // A browser sign-in only means something for OAuth; a server with no
    // authentication or a stored token is checked with Test instead.
    m_connect->setEnabled(chosen && oauth);
    m_disconnect->setEnabled(chosen && oauth);
    m_test->setEnabled(chosen && isRemote(s));
    m_import->setEnabled(loaded);
    m_export->setEnabled(loaded);
}

void McpPage::editServer(const QJsonObject& s) {
    if (m_controller.isNull()) { return; }
    auto* editor = new McpServerEditor(m_controller, s, m_revision, this);
    editor->open();
}

void McpPage::operationFinished(const QString& op, const QString& serverId, const QString& json,
                                const QString& error) {
    const QString name = server(serverId).value("name").toString(serverId);
    if (!error.isEmpty()) {
        // The editor, the import dialog and the workspace dialog say their own
        // failures beside their fields.
        if (op != QLatin1String("mcp.upsert") && op != QLatin1String("mcp.import_preview") &&
            op != QLatin1String("mcp.import_apply") && op != QLatin1String("mcp.override")) {
            m_status->setText(error);
        }
        return;
    }
    if (op == QLatin1String("mcp.upsert")) {
        m_status->setText(QStringLiteral("Saved %1. New or enabled servers apply when an agent next starts.").arg(name));
    } else if (op == QLatin1String("mcp.remove")) {
        m_status->setText(QStringLiteral("Removed %1.").arg(serverId));
    } else if (op == QLatin1String("mcp.restore_preset")) {
        m_status->setText(QStringLiteral("Restored the Jira preset. Select it and choose Connect to sign in."));
    } else if (op == QLatin1String("mcp.import_apply")) {
        m_status->setText(QStringLiteral("Imported. New servers apply when an agent next starts."));
    } else if (op == QLatin1String("mcp.connect")) {
        m_status->setText(parse(json).value("waiting").toBool()
                              ? QStringLiteral("Finish signing in to %1 in your browser. The sign-in "
                                               "stays open for 10 minutes.").arg(name)
                              : QStringLiteral("%1 is connected.").arg(name));
    } else if (op == QLatin1String("mcp.callback")) {
        m_status->setText(QStringLiteral("Signed in to %1.").arg(name));
    } else if (op == QLatin1String("mcp.disconnect")) {
        m_status->setText(QStringLiteral("Disconnected %1.").arg(name));
    } else if (op == QLatin1String("mcp.test")) {
        const QJsonObject result = parse(json);
        const QJsonObject status = result.value("status").toObject();
        QStringList tools;
        for (const auto& tool : result.value("tool_names").toArray()) { tools.append(tool.toString()); }
        if (status.value("state").toString() == QLatin1String("connected")) {
            m_status->setText(QStringLiteral("%1 answered with %2 tool%3").arg(name).arg(tools.size()).arg(QString(tools.size() == 1 ? "" : "s")) +
                              (tools.isEmpty() ? QStringLiteral(".") : QStringLiteral(": ") + tools.join(", ")));
        } else {
            const QString reason = status.value("reason").toString();
            m_status->setText(QStringLiteral("%1 did not answer%2").arg(name,
                reason.isEmpty() ? QStringLiteral(".") : QStringLiteral(": ") + reason));
        }
    } else if (op == QLatin1String("mcp.export")) {
        m_status->clear();
        showExport(json);
    }
}

void McpPage::showExport(const QString& document) {
    auto* dialog = new QDialog(this);
    dialog->setObjectName(QStringLiteral("McpExportDialog"));
    dialog->setAttribute(Qt::WA_DeleteOnClose);
    dialog->setWindowTitle(QStringLiteral("Export MCP servers"));
    dialog->resize(640, 480);
    auto* layout = new QVBoxLayout(dialog);
    layout->addWidget(plainLabel(
        QStringLiteral("The servers' configuration and secret references. Secret values and "
                       "sign-ins are not included."), dialog));
    auto* text = new QPlainTextEdit(dialog);
    text->setObjectName(QStringLiteral("McpExportText"));
    text->setReadOnly(true);
    text->setPlainText(document);
    layout->addWidget(text, 1);
    auto* buttons = new QDialogButtonBox(QDialogButtonBox::Close, dialog);
    auto* copy = buttons->addButton(QStringLiteral("Copy"), QDialogButtonBox::ActionRole);
    connect(copy, &QPushButton::clicked, dialog, [document] {
        QGuiApplication::clipboard()->setText(document);
    });
    connect(buttons, &QDialogButtonBox::rejected, dialog, &QDialog::close);
    layout->addWidget(buttons);
    dialog->open();
}

// --- McpServerEditor -------------------------------------------------------------

McpServerEditor::McpServerEditor(AppController* controller, const QJsonObject& server,
                                 qint64 revision, QWidget* parent)
    : QDialog(parent), m_controller(controller), m_original(server), m_revision(revision) {
    setObjectName(QStringLiteral("McpServerEditor"));
    setAttribute(Qt::WA_DeleteOnClose);
    const bool existing = !server.isEmpty();
    setWindowTitle(existing ? QStringLiteral("Edit MCP server — %1").arg(server.value("name").toString())
                            : QStringLiteral("Add MCP server"));
    resize(620, 640);
    auto* outer = new QVBoxLayout(this);
    m_layout = new QFormLayout();
    m_layout->setRowWrapPolicy(QFormLayout::WrapLongRows);
    outer->addLayout(m_layout, 1);

    m_id = new QLineEdit(this);
    m_id->setObjectName(QStringLiteral("McpServerId"));
    m_id->setPlaceholderText(QStringLiteral("e.g. github"));
    m_id->setToolTip(QStringLiteral("Lowercase letters, digits, - and _; at most 32. Cannot be changed later."));
    m_name = new QLineEdit(this);
    m_name->setObjectName(QStringLiteral("McpServerName"));
    m_transport = new QComboBox(this);
    m_transport->setObjectName(QStringLiteral("McpTransport"));
    m_transport->addItem(QStringLiteral("Remote (Streamable HTTP)"), QStringLiteral("http"));
    m_transport->addItem(QStringLiteral("Local command in the sandbox (stdio)"), QStringLiteral("stdio"));
    m_layout->addRow(QStringLiteral("ID:"), m_id);
    m_layout->addRow(QStringLiteral("Name:"), m_name);
    m_layout->addRow(QStringLiteral("Transport:"), m_transport);

    // Local.
    m_command = new QLineEdit(this);
    m_command->setObjectName(QStringLiteral("McpCommand"));
    m_command->setPlaceholderText(QStringLiteral("A program installed in the sandbox, e.g. npx"));
    m_args = new QPlainTextEdit(this);
    m_args->setObjectName(QStringLiteral("McpArguments"));
    m_args->setPlaceholderText(QStringLiteral("One argument per line"));
    m_args->setMaximumHeight(80);
    m_env = new QPlainTextEdit(this);
    m_env->setObjectName(QStringLiteral("McpEnvironment"));
    m_env->setPlaceholderText(QStringLiteral("NAME=value, one per line. Not for secrets."));
    m_env->setMaximumHeight(80);
    m_secretEnv = secretTable(QStringLiteral("McpSecretEnv"), QStringLiteral("Variable"), this);
    m_secretEnvButtons = secretButtons(m_secretEnv, QStringLiteral("Add secret variable"), this);
    m_layout->addRow(QStringLiteral("Command:"), m_command);
    m_layout->addRow(QStringLiteral("Arguments:"), m_args);
    m_layout->addRow(QStringLiteral("Environment:"), m_env);
    m_layout->addRow(QStringLiteral("Secret variables:"), m_secretEnv);
    m_layout->addRow(QString(), m_secretEnvButtons);

    // Remote.
    m_url = new QLineEdit(this);
    m_url->setObjectName(QStringLiteral("McpUrl"));
    m_url->setPlaceholderText(QStringLiteral("https://example.com/mcp"));
    m_auth = new QComboBox(this);
    m_auth->setObjectName(QStringLiteral("McpAuth"));
    m_auth->addItem(QStringLiteral("None"), QStringLiteral("none"));
    m_auth->addItem(QStringLiteral("Sign in with OAuth"), QStringLiteral("oauth"));
    m_auth->addItem(QStringLiteral("Bearer token"), QStringLiteral("bearer"));
    m_auth->addItem(QStringLiteral("Secret headers"), QStringLiteral("headers"));
    m_bearer = new QLineEdit(this);
    m_bearer->setObjectName(QStringLiteral("McpBearerValue"));
    m_bearer->setEchoMode(QLineEdit::Password);
    m_removeBearer = new QCheckBox(QStringLiteral("Remove the stored token"), this);
    m_removeBearer->setObjectName(QStringLiteral("McpRemoveBearer"));
    m_headers = secretTable(QStringLiteral("McpHeaders"), QStringLiteral("Header"), this);
    m_headerButtons = secretButtons(m_headers, QStringLiteral("Add header"), this);
    m_clientId = new QLineEdit(this);
    m_clientId->setObjectName(QStringLiteral("McpOAuthClientId"));
    m_clientId->setPlaceholderText(QStringLiteral("Optional; registered automatically when blank"));
    m_metadataUrl = new QLineEdit(this);
    m_metadataUrl->setObjectName(QStringLiteral("McpOAuthMetadataUrl"));
    m_metadataUrl->setPlaceholderText(QStringLiteral("Optional client metadata document URL"));
    m_scopes = new QLineEdit(this);
    m_scopes->setObjectName(QStringLiteral("McpOAuthScopes"));
    m_scopes->setPlaceholderText(QStringLiteral("Optional, space-separated"));
    m_clientSecret = new QLineEdit(this);
    m_clientSecret->setObjectName(QStringLiteral("McpOAuthSecretValue"));
    m_clientSecret->setEchoMode(QLineEdit::Password);
    m_removeClientSecret = new QCheckBox(QStringLiteral("Remove the stored client secret"), this);
    m_removeClientSecret->setObjectName(QStringLiteral("McpRemoveClientSecret"));
    m_layout->addRow(QStringLiteral("URL:"), m_url);
    m_layout->addRow(QStringLiteral("Authentication:"), m_auth);
    m_layout->addRow(QStringLiteral("Token:"), m_bearer);
    m_layout->addRow(QString(), m_removeBearer);
    m_layout->addRow(QStringLiteral("Headers:"), m_headers);
    m_layout->addRow(QString(), m_headerButtons);
    m_layout->addRow(QStringLiteral("Client ID:"), m_clientId);
    m_layout->addRow(QStringLiteral("Client metadata URL:"), m_metadataUrl);
    m_layout->addRow(QStringLiteral("Scopes:"), m_scopes);
    m_layout->addRow(QStringLiteral("Client secret:"), m_clientSecret);
    m_layout->addRow(QString(), m_removeClientSecret);

    m_enabled = new QCheckBox(QStringLiteral("Enabled"), this);
    m_enabled->setObjectName(QStringLiteral("McpEnabled"));
    m_defaultWorkspace = new QCheckBox(QStringLiteral("On in every workspace unless the workspace says otherwise"), this);
    m_defaultWorkspace->setObjectName(QStringLiteral("McpDefaultWorkspace"));
    auto* backends = new QWidget(this);
    auto* backendLayout = new QHBoxLayout(backends);
    backendLayout->setContentsMargins(0, 0, 0, 0);
    m_claude = new QCheckBox(QStringLiteral("Claude"), backends);
    m_claude->setObjectName(QStringLiteral("McpBackend_claude"));
    m_codex = new QCheckBox(QStringLiteral("Codex"), backends);
    m_codex->setObjectName(QStringLiteral("McpBackend_codex"));
    m_opencode = new QCheckBox(QStringLiteral("OpenCode"), backends);
    m_opencode->setObjectName(QStringLiteral("McpBackend_opencode"));
    backendLayout->addWidget(m_claude);
    backendLayout->addWidget(m_codex);
    backendLayout->addWidget(m_opencode);
    backendLayout->addStretch(1);
    m_layout->addRow(QString(), m_enabled);
    m_layout->addRow(QString(), m_defaultWorkspace);
    m_layout->addRow(QStringLiteral("Agents:"), backends);

    m_error = plainLabel(QString(), this);
    m_error->setObjectName(QStringLiteral("McpEditorError"));
    outer->addWidget(m_error);
    auto* buttons = new QDialogButtonBox(QDialogButtonBox::Save | QDialogButtonBox::Cancel, this);
    m_save = buttons->button(QDialogButtonBox::Save);
    m_save->setObjectName(QStringLiteral("McpEditorSave"));
    outer->addWidget(buttons);
    connect(buttons, &QDialogButtonBox::accepted, this, &McpServerEditor::save);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    connect(m_transport, &QComboBox::currentIndexChanged, this, &McpServerEditor::updateVisibility);
    connect(m_auth, &QComboBox::currentIndexChanged, this, &McpServerEditor::updateVisibility);
    connect(controller, &AppController::mcpOperationFinished, this,
            [this](const QString& op, const QString& serverId, const QString&, const QString& error) {
        if (!m_saving || op != QLatin1String("mcp.upsert") ||
            (!serverId.isEmpty() && serverId != m_id->text().trimmed())) {
            return;
        }
        m_saving = false;
        m_save->setEnabled(true);
        if (error.isEmpty()) {
            accept();
        } else {
            m_error->setText(error);
        }
    });

    // Fill from the server. Secret fields stay blank: the listing carries the
    // references, never the values, and blank is what keeps them.
    m_enabled->setChecked(server.value("enabled").toBool(true));
    m_defaultWorkspace->setChecked(server.value("default_workspace_enabled").toBool(true));
    const QJsonArray backendList = existing ? server.value("enabled_backends").toArray()
                                            : QJsonArray{"claude", "codex", "opencode"};
    m_claude->setChecked(backendList.contains(QStringLiteral("claude")));
    m_codex->setChecked(backendList.contains(QStringLiteral("codex")));
    m_opencode->setChecked(backendList.contains(QStringLiteral("opencode")));
    if (existing) {
        m_id->setText(server.value("id").toString());
        m_id->setReadOnly(true);
        m_name->setText(server.value("name").toString());
    }
    const QJsonObject transport = server.value("transport").toObject();
    if (transport.value("type").toString() == QLatin1String("stdio")) {
        m_transport->setCurrentIndex(m_transport->findData(QStringLiteral("stdio")));
        m_command->setText(transport.value("command").toString());
        QStringList args;
        for (const auto& arg : transport.value("args").toArray()) { args.append(arg.toString()); }
        m_args->setPlainText(args.join('\n'));
        QStringList env;
        const QJsonObject vars = transport.value("env").toObject();
        for (auto it = vars.begin(); it != vars.end(); ++it) {
            env.append(it.key() + QLatin1Char('=') + it.value().toString());
        }
        m_env->setPlainText(env.join('\n'));
        // For a local server the variable's name is its secret reference.
        for (const auto& name : transport.value("secret_env").toArray()) {
            addSecretRow(m_secretEnv, name.toString(), name.toString());
        }
    } else if (existing) {
        m_url->setText(transport.value("url").toString());
        const QJsonObject auth = transport.value("auth").toObject();
        const QString type = auth.value("type").toString(QStringLiteral("none"));
        m_auth->setCurrentIndex(qMax(0, m_auth->findData(type)));
        const QJsonObject headers = auth.value("headers").toObject();
        for (auto it = headers.begin(); it != headers.end(); ++it) {
            addSecretRow(m_headers, it.key(), it.value().toString());
        }
        m_clientId->setText(auth.value("client_id").toString());
        m_metadataUrl->setText(auth.value("client_metadata_url").toString());
        QStringList scopes;
        for (const auto& scope : auth.value("scopes").toArray()) { scopes.append(scope.toString()); }
        m_scopes->setText(scopes.join(' '));
    }
    const QJsonObject auth = transport.value("auth").toObject();
    const bool storedToken = auth.value("type").toString() == QLatin1String("bearer");
    m_bearer->setPlaceholderText(storedToken ? QStringLiteral("Stored; leave blank to keep")
                                             : QStringLiteral("Token"));
    m_removeBearer->setEnabled(storedToken);
    const bool storedSecret = auth.contains(QStringLiteral("client_secret")) &&
                              !auth.value("client_secret").isNull();
    m_clientSecret->setPlaceholderText(storedSecret ? QStringLiteral("Stored; leave blank to keep")
                                                    : QStringLiteral("Optional"));
    m_removeClientSecret->setEnabled(storedSecret);
    updateVisibility();
}

void McpServerEditor::updateVisibility() {
    const bool stdio = m_transport->currentData().toString() == QLatin1String("stdio");
    const QString auth = m_auth->currentData().toString();
    for (QWidget* w : {static_cast<QWidget*>(m_command), static_cast<QWidget*>(m_args),
                       static_cast<QWidget*>(m_env), static_cast<QWidget*>(m_secretEnv),
                       m_secretEnvButtons}) {
        m_layout->setRowVisible(w, stdio);
    }
    m_layout->setRowVisible(m_url, !stdio);
    m_layout->setRowVisible(m_auth, !stdio);
    m_layout->setRowVisible(m_bearer, !stdio && auth == QLatin1String("bearer"));
    m_layout->setRowVisible(m_removeBearer, !stdio && auth == QLatin1String("bearer"));
    m_layout->setRowVisible(m_headers, !stdio && auth == QLatin1String("headers"));
    m_layout->setRowVisible(m_headerButtons, !stdio && auth == QLatin1String("headers"));
    for (QWidget* w : {static_cast<QWidget*>(m_clientId), static_cast<QWidget*>(m_metadataUrl),
                       static_cast<QWidget*>(m_scopes), static_cast<QWidget*>(m_clientSecret),
                       static_cast<QWidget*>(m_removeClientSecret)}) {
        m_layout->setRowVisible(w, !stdio && auth == QLatin1String("oauth"));
    }
}

QJsonObject McpServerEditor::form(QString& error) const {
    const QString id = m_id->text().trimmed();
    const QString name = m_name->text().trimmed();
    if (id.isEmpty() || name.isEmpty()) {
        error = QStringLiteral("Give the server an ID and a name.");
        return QJsonObject();
    }
    QJsonArray backends;
    if (m_claude->isChecked()) { backends.append("claude"); }
    if (m_codex->isChecked()) { backends.append("codex"); }
    if (m_opencode->isChecked()) { backends.append("opencode"); }
    QJsonArray secrets;
    QJsonObject transport;
    const QJsonObject originalAuth =
        m_original.value("transport").toObject().value("auth").toObject();
    if (m_transport->currentData().toString() == QLatin1String("stdio")) {
        QJsonArray args;
        for (const QString& line : m_args->toPlainText().split('\n')) {
            if (!line.trimmed().isEmpty()) { args.append(line); }
        }
        QJsonObject env;
        for (const QString& line : m_env->toPlainText().split('\n')) {
            if (line.trimmed().isEmpty()) { continue; }
            const int equals = line.indexOf('=');
            if (equals <= 0) {
                error = QStringLiteral("Environment lines are NAME=value.");
                return QJsonObject();
            }
            env.insert(line.left(equals).trimmed(), line.mid(equals + 1));
        }
        QJsonObject names;
        if (!readSecretRows(m_secretEnv, [](const QString& n) { return n; }, names, secrets, error)) {
            return QJsonObject();
        }
        QJsonArray secretEnv;
        for (auto it = names.begin(); it != names.end(); ++it) { secretEnv.append(it.key()); }
        transport = {{"type", "stdio"}, {"command", m_command->text().trimmed()}, {"args", args},
                     {"env", env}, {"secret_env", secretEnv}};
    } else {
        const QString type = m_auth->currentData().toString();
        QJsonObject auth{{"type", type}};
        if (type == QLatin1String("bearer")) {
            const QString ref = originalAuth.value("token").toString(QStringLiteral("token"));
            auth.insert("token", ref);
            secrets.append(QJsonObject{{"reference", ref}, {"value", m_bearer->text()},
                                       {"remove", m_removeBearer->isChecked()}});
        } else if (type == QLatin1String("headers")) {
            QJsonObject headers;
            if (!readSecretRows(m_headers, [](const QString& n) { return n; }, headers, secrets, error)) {
                return QJsonObject();
            }
            auth.insert("headers", headers);
        } else if (type == QLatin1String("oauth")) {
            if (!m_clientId->text().trimmed().isEmpty()) { auth.insert("client_id", m_clientId->text().trimmed()); }
            if (!m_metadataUrl->text().trimmed().isEmpty()) {
                auth.insert("client_metadata_url", m_metadataUrl->text().trimmed());
            }
            QJsonArray scopes;
            for (const QString& scope : m_scopes->text().split(' ', Qt::SkipEmptyParts)) { scopes.append(scope); }
            auth.insert("scopes", scopes);
            const QString stored = originalAuth.value("client_secret").toString();
            const QString ref = stored.isEmpty() ? QStringLiteral("client_secret") : stored;
            if (m_removeClientSecret->isChecked()) {
                // Out of the configuration, and the value deleted with it.
                secrets.append(QJsonObject{{"reference", ref}, {"remove", true}});
            } else if (!stored.isEmpty() || !m_clientSecret->text().isEmpty()) {
                auth.insert("client_secret", ref);
                secrets.append(QJsonObject{{"reference", ref}, {"value", m_clientSecret->text()}});
            }
        }
        transport = {{"type", "http"}, {"url", m_url->text().trimmed()}, {"auth", auth}};
    }
    QJsonObject result{{"server", QJsonObject{{"id", id}, {"name", name}, {"transport", transport},
                                              {"enabled", m_enabled->isChecked()},
                                              {"default_workspace_enabled", m_defaultWorkspace->isChecked()},
                                              {"enabled_backends", backends}}},
                       {"secrets", secrets}};
    if (m_revision >= 0) { result.insert("expected_revision", m_revision); }
    return result;
}

void McpServerEditor::save() {
    if (m_saving || m_controller.isNull()) { return; }
    QString error;
    const QJsonObject f = form(error);
    if (f.isEmpty()) {
        m_error->setText(error);
        return;
    }
    m_saving = true;
    m_save->setEnabled(false);
    m_error->setText(QStringLiteral("Saving…"));
    m_controller->mcpUpsert(compact(f));
}

// --- McpImportDialog -------------------------------------------------------------

McpImportDialog::McpImportDialog(AppController* controller, QWidget* parent)
    : QDialog(parent), m_controller(controller) {
    setObjectName(QStringLiteral("McpImportDialog"));
    setAttribute(Qt::WA_DeleteOnClose);
    setWindowTitle(QStringLiteral("Import MCP servers"));
    resize(640, 560);
    auto* layout = new QVBoxLayout(this);
    layout->addWidget(plainLabel(
        QStringLiteral("Paste a document with an \"mcpServers\" object, as Claude Code, Cursor and "
                       "others write it. Environment and header values are stored as secrets. "
                       "Preview shows what would be added before anything is saved."), this));
    m_document = new QPlainTextEdit(this);
    m_document->setObjectName(QStringLiteral("McpImportDocument"));
    m_document->setPlaceholderText(QStringLiteral("{\n  \"mcpServers\": {\n    \"github\": { \"url\": \"https://…\" }\n  }\n}"));
    layout->addWidget(m_document, 1);
    m_preview = new QPushButton(QStringLiteral("Preview"), this);
    m_preview->setObjectName(QStringLiteral("McpImportPreview"));
    auto* previewRow = new QHBoxLayout();
    previewRow->addWidget(m_preview);
    previewRow->addStretch(1);
    layout->addLayout(previewRow);
    m_summary = plainLabel(QString(), this);
    m_summary->setObjectName(QStringLiteral("McpImportSummary"));
    layout->addWidget(m_summary);
    m_collisions = new QTableWidget(0, 3, this);
    m_collisions->setObjectName(QStringLiteral("McpImportCollisions"));
    m_collisions->setHorizontalHeaderLabels(
        {QStringLiteral("Entry"), QStringLiteral("Resolution"), QStringLiteral("New ID")});
    m_collisions->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    m_collisions->verticalHeader()->hide();
    m_collisions->hide();
    layout->addWidget(m_collisions);
    m_error = plainLabel(QString(), this);
    m_error->setObjectName(QStringLiteral("McpImportError"));
    layout->addWidget(m_error);
    auto* buttons = new QDialogButtonBox(QDialogButtonBox::Cancel, this);
    m_apply = buttons->addButton(QStringLiteral("Import"), QDialogButtonBox::AcceptRole);
    m_apply->setObjectName(QStringLiteral("McpImportApply"));
    m_apply->setEnabled(false);
    layout->addWidget(buttons);

    for (const auto& value : parse(controller->mcpViewJson()).value("registry").toObject().value("servers").toArray()) {
        m_existingIds.append(value.toObject().value("id"));
    }
    connect(m_preview, &QPushButton::clicked, this, &McpImportDialog::preview);
    connect(m_document, &QPlainTextEdit::textChanged, this, &McpImportDialog::updateApply);
    connect(buttons, &QDialogButtonBox::accepted, this, &McpImportDialog::apply);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    connect(controller, &AppController::mcpOperationFinished, this,
            [this](const QString& op, const QString&, const QString& json, const QString& error) {
        if (op == QLatin1String("mcp.import_preview")) {
            if (!m_waiting) {
                return;
            }
            if (error.isEmpty()) {
                showPreview(json);
            } else {
                m_waiting = false;
                m_previewed.clear();
                m_error->setText(error);
                updateApply();
            }
        } else if (op == QLatin1String("mcp.import_apply")) {
            if (error.isEmpty()) {
                accept();
            } else {
                m_error->setText(error);
                updateApply();
            }
        }
    });
    connect(controller, &AppController::mcpChanged, this, [this](const QString& json) {
        const QJsonObject registry = parse(json).value("registry").toObject();
        if (registry.isEmpty()) { return; }
        m_existingIds = QJsonArray();
        for (const auto& value : registry.value("servers").toArray()) {
            m_existingIds.append(value.toObject().value("id"));
        }
        updateApply();
    });
}

void McpImportDialog::preview() {
    if (m_controller.isNull()) { return; }
    // Remembered before the answer: the text may change while it is checked.
    m_previewed = m_document->toPlainText();
    m_waiting = true;
    m_serverCount = 0;
    m_hasIssues = false;
    m_collisions->setRowCount(0);
    m_collisions->hide();
    m_error->setText(QStringLiteral("Checking…"));
    updateApply();
    m_controller->mcpImportPreview(m_previewed);
}

void McpImportDialog::showPreview(const QString& json) {
    if (!m_waiting) { return; }
    m_waiting = false;
    const QJsonObject result = parse(json);
    const QJsonArray servers = result.value("servers").toArray();
    const QJsonArray issues = result.value("issues").toArray();
    const QJsonArray collisions = result.value("collisions").toArray();
    m_serverCount = servers.size();
    m_hasIssues = !issues.isEmpty();
    QStringList lines;
    lines.append(QStringLiteral("%1 server%2 to import.").arg(servers.size()).arg(QString(servers.size() == 1 ? "" : "s")));
    for (const auto& value : issues) {
        const QJsonObject issue = value.toObject();
        lines.append(QStringLiteral("%1, %2: %3").arg(issue.value("entry").toString(),
                                                      issue.value("field").toString(),
                                                      issue.value("reason").toString()));
    }
    if (m_hasIssues) {
        lines.append(QStringLiteral("Fix or remove the entries above; nothing is imported while any is unsupported."));
    }
    m_summary->setText(lines.join('\n'));
    m_error->clear();

    // Which entries collide, by their names in the document: the daemon keys
    // resolutions by name, and its preview may have renamed an entry whose
    // name was not a plain token.
    QSet<QString> colliding;
    for (const auto& id : collisions) { colliding.insert(id.toString()); }
    const QJsonObject entries =
        QJsonDocument::fromJson(m_previewed.toUtf8()).object().value("mcpServers").toObject();
    m_collisions->setRowCount(0);
    for (auto it = entries.begin(); it != entries.end(); ++it) {
        const QString id = generatedId(it.key());
        if (!colliding.contains(id)) { continue; }
        const int row = m_collisions->rowCount();
        m_collisions->insertRow(row);
        auto* entry = new QTableWidgetItem(it.key());
        entry->setFlags(entry->flags() & ~Qt::ItemIsEditable);
        entry->setData(Qt::UserRole, id);
        m_collisions->setItem(row, 0, entry);
        auto* choice = new QComboBox(m_collisions);
        choice->addItem(QStringLiteral("Choose…"), QString());
        if (m_existingIds.contains(id)) {
            choice->addItem(QStringLiteral("Replace existing \"%1\"").arg(id), QStringLiteral("replace"));
        }
        choice->addItem(QStringLiteral("Import under a new ID"), QStringLiteral("rename"));
        m_collisions->setCellWidget(row, 1, choice);
        auto* newId = new QLineEdit(m_collisions);
        newId->setPlaceholderText(QStringLiteral("new-id"));
        newId->setEnabled(false);
        m_collisions->setCellWidget(row, 2, newId);
        connect(choice, &QComboBox::currentIndexChanged, this, [this, choice, newId] {
            newId->setEnabled(choice->currentData().toString() == QLatin1String("rename"));
            updateApply();
        });
        connect(newId, &QLineEdit::textChanged, this, &McpImportDialog::updateApply);
    }
    m_collisions->setVisible(m_collisions->rowCount() > 0);
    if (m_collisions->rowCount() > 0) {
        m_summary->setText(m_summary->text() + QLatin1Char('\n') +
                           QStringLiteral("Some entries have an ID already in use. Choose what to do with each."));
    }
    updateApply();
}

void McpImportDialog::updateApply() {
    bool ready = !m_waiting && !m_previewed.isEmpty() &&
                 m_previewed == m_document->toPlainText() && !m_hasIssues && m_serverCount > 0;
    QSet<QString> chosen;
    for (int row = 0; ready && row < m_collisions->rowCount(); ++row) {
        auto* choice = qobject_cast<QComboBox*>(m_collisions->cellWidget(row, 1));
        auto* newId = qobject_cast<QLineEdit*>(m_collisions->cellWidget(row, 2));
        const QString mode = choice ? choice->currentData().toString() : QString();
        if (mode == QLatin1String("replace")) {
            ready = !chosen.contains(m_collisions->item(row, 0)->data(Qt::UserRole).toString());
            chosen.insert(m_collisions->item(row, 0)->data(Qt::UserRole).toString());
        } else if (mode == QLatin1String("rename")) {
            const QString id = newId ? newId->text().trimmed() : QString();
            ready = validServerId(id) && !m_existingIds.contains(id) && !chosen.contains(id);
            chosen.insert(id);
        } else {
            ready = false;
        }
    }
    m_apply->setEnabled(ready);
}

void McpImportDialog::apply() {
    if (!m_apply->isEnabled() || m_controller.isNull()) { return; }
    QJsonObject resolutions;
    for (int row = 0; row < m_collisions->rowCount(); ++row) {
        auto* choice = qobject_cast<QComboBox*>(m_collisions->cellWidget(row, 1));
        auto* newId = qobject_cast<QLineEdit*>(m_collisions->cellWidget(row, 2));
        const QString name = m_collisions->item(row, 0)->text();
        resolutions.insert(name, choice->currentData().toString() == QLatin1String("replace")
                                     ? m_collisions->item(row, 0)->data(Qt::UserRole).toString()
                                     : newId->text().trimmed());
    }
    m_apply->setEnabled(false);
    m_error->setText(QStringLiteral("Importing…"));
    m_controller->mcpImportApply(m_previewed, compact(resolutions));
}

// --- offscreen test entries ------------------------------------------------------
//
// Compiled only into a development build; see the note in `EditorArea.cpp`.
// `bs_widget_test_begin` must have run first. Nothing here shows a top-level
// window: the dialogs are built and driven without being shown.
#if defined(BS_WIDGET_TESTS)
#include <cstdint>

namespace {
const char* const kListing = R"({"registry":{"revision":3,"servers":[
  {"id":"jira","name":"Jira Atlassian","transport":{"type":"http","url":"https://mcp.atlassian.com/v2/mcp",
   "auth":{"type":"oauth","scopes":[]}},"enabled":true,"default_workspace_enabled":true,
   "enabled_backends":["claude","codex","opencode"]},
  {"id":"tokened","name":"Tokened","transport":{"type":"http","url":"https://example.com/mcp",
   "auth":{"type":"bearer","token":"token"}},"enabled":true,"default_workspace_enabled":false,
   "enabled_backends":["claude"]},
  {"id":"local","name":"Local","transport":{"type":"stdio","command":"npx","args":["-y","server"],
   "env":{"MODE":"fast"},"secret_env":["API_KEY"]},"enabled":true,"default_workspace_enabled":true,
   "enabled_backends":["claude","codex"]}],
  "overrides":{"ws_mcp":{"local":"disabled"}}},
  "statuses":[{"server_id":"jira","state":"connected"},{"server_id":"tokened","state":"error","reason":"401"}]})";

QTreeWidgetItem* row(QTreeWidget* list, const QString& id) {
    for (int i = 0; i < list->topLevelItemCount(); ++i) {
        if (list->topLevelItem(i)->data(0, Qt::UserRole).toString() == id) { return list->topLevelItem(i); }
    }
    return nullptr;
}
} // namespace

/// The page lists the servers with their status, says when changes apply,
/// shows a degraded registry's error instead of the list, offers Connect only
/// for OAuth and Test for remote servers, and Restore Jira only without Jira.
extern "C" std::int32_t bs_widget_test_mcp_page_lists_servers() {
    AppController controller;
    McpPage page(&controller);
    auto* list = page.findChild<QTreeWidget*>(QStringLiteral("McpServerList"));
    auto* error = page.findChild<QLabel*>(QStringLiteral("McpRegistryError"));
    auto* note = page.findChild<QLabel*>(QStringLiteral("McpApplyNote"));
    auto* connectButton = page.findChild<QPushButton*>(QStringLiteral("McpConnect"));
    auto* test = page.findChild<QPushButton*>(QStringLiteral("McpTest"));
    auto* restore = page.findChild<QPushButton*>(QStringLiteral("McpRestoreJira"));
    if (!list || !error || !note || !connectButton || !test || !restore) return 1;
    if (!note->text().contains("next starts") || !note->text().contains("immediately")) return 2;
    if (!list->isHidden() || error->isHidden()) return 3;
    emit controller.mcpChanged(QString::fromUtf8(kListing));
    if (list->isHidden() || !error->isHidden() || list->topLevelItemCount() != 3) return 4;
    if (row(list, "jira")->text(3) != QLatin1String("Connected") ||
        !row(list, "tokened")->text(3).contains("401") ||
        row(list, "local")->text(3) != QLatin1String("Starts with each agent")) return 5;
    if (!restore->isHidden()) return 6;
    row(list, "jira")->setSelected(true);
    if (!connectButton->isEnabled() || !test->isEnabled()) return 7;
    list->clearSelection();
    row(list, "tokened")->setSelected(true);
    if (connectButton->isEnabled() || !test->isEnabled()) return 8;
    list->clearSelection();
    row(list, "local")->setSelected(true);
    if (connectButton->isEnabled() || test->isEnabled()) return 9;
    // Jira deleted: the preset can be restored.
    emit controller.mcpChanged(QStringLiteral(R"({"registry":{"revision":4,"servers":[]},"statuses":[]})"));
    if (restore->isHidden() || list->topLevelItemCount() != 0) return 10;
    // A degraded registry: its message replaces the list.
    emit controller.mcpChanged(QStringLiteral(R"({"error":"cannot read registry.json: corrupt"})"));
    if (!list->isHidden() || error->isHidden() || !error->text().contains("registry.json")) return 11;
    // Failures of the page's own actions are said under it.
    emit controller.mcpOperationFinished("mcp.test", "jira", "", "upstream refused");
    if (page.findChild<QLabel*>(QStringLiteral("McpStatus"))->text() != QLatin1String("upstream refused")) return 12;
    return 0;
}

/// Editing a server with stored secrets shows none of them, and the form it
/// would save asks to keep each one with a blank value.
extern "C" std::int32_t bs_widget_test_mcp_secret_editor_is_write_only() {
    AppController controller;
    const QJsonObject listing = QJsonDocument::fromJson(QByteArray(kListing)).object();
    const QJsonArray servers = listing.value("registry").toObject().value("servers").toArray();
    const QJsonObject tokened = servers.at(1).toObject();
    const QJsonObject local = servers.at(2).toObject();

    McpServerEditor bearer(&controller, tokened, 3);
    bearer.setAttribute(Qt::WA_DeleteOnClose, false);
    auto* value = bearer.findChild<QLineEdit*>(QStringLiteral("McpBearerValue"));
    auto* id = bearer.findChild<QLineEdit*>(QStringLiteral("McpServerId"));
    if (!value || !id) return 1;
    if (!value->text().isEmpty() || value->echoMode() != QLineEdit::Password ||
        !value->placeholderText().contains("blank")) return 2;
    if (!id->isReadOnly()) return 3;
    QString error;
    QJsonObject form = bearer.form(error);
    QJsonArray secrets = form.value("secrets").toArray();
    if (secrets.size() != 1 || secrets.at(0).toObject().value("reference").toString() != "token" ||
        !secrets.at(0).toObject().value("value").toString().isEmpty() ||
        secrets.at(0).toObject().value("remove").toBool()) return 4;
    if (form.value("expected_revision").toInteger() != 3) return 5;
    if (form.value("server").toObject().value("transport").toObject().value("auth").toObject()
            .value("token").toString() != "token") return 6;
    value->setText("s3cr3t-sentinel");
    secrets = bearer.form(error).value("secrets").toArray();
    if (secrets.at(0).toObject().value("value").toString() != "s3cr3t-sentinel") return 7;
    bearer.findChild<QCheckBox*>(QStringLiteral("McpRemoveBearer"))->setChecked(true);
    if (!bearer.form(error).value("secrets").toArray().at(0).toObject().value("remove").toBool()) return 8;

    McpServerEditor stdio(&controller, local, 3);
    stdio.setAttribute(Qt::WA_DeleteOnClose, false);
    auto* table = stdio.findChild<QTableWidget*>(QStringLiteral("McpSecretEnv"));
    if (!table || table->rowCount() != 1 || table->item(0, 0)->text() != "API_KEY") return 9;
    auto* cell = qobject_cast<QLineEdit*>(table->cellWidget(0, 1));
    if (!cell || !cell->text().isEmpty() || cell->echoMode() != QLineEdit::Password) return 10;
    form = stdio.form(error);
    const QJsonObject transport = form.value("server").toObject().value("transport").toObject();
    if (transport.value("secret_env").toArray() != QJsonArray{"API_KEY"} ||
        transport.value("env").toObject().value("MODE").toString() != "fast" ||
        transport.value("args").toArray() != QJsonArray{"-y", "server"}) return 11;
    secrets = form.value("secrets").toArray();
    if (secrets.size() != 1 || secrets.at(0).toObject().value("reference").toString() != "API_KEY" ||
        !secrets.at(0).toObject().value("value").toString().isEmpty()) return 12;
    // A failed save is said in the editor, which stays open.
    stdio.findChild<QPushButton*>(QStringLiteral("McpEditorSave"))->click();
    emit controller.mcpOperationFinished("mcp.upsert", "local", "", "MCP registry revision conflict");
    if (stdio.findChild<QLabel*>(QStringLiteral("McpEditorError"))->text() != "MCP registry revision conflict") return 13;

    // A new server: every agent ticked, ID editable.
    McpServerEditor fresh(&controller, QJsonObject(), 3);
    fresh.setAttribute(Qt::WA_DeleteOnClose, false);
    if (fresh.findChild<QLineEdit*>(QStringLiteral("McpServerId"))->isReadOnly() ||
        !fresh.findChild<QCheckBox*>(QStringLiteral("McpBackend_opencode"))->isChecked()) return 14;
    if (!fresh.form(error).isEmpty() || error.isEmpty()) return 15;
    return 0;
}

/// Apply stays off until a preview has answered for exactly the text on screen,
/// with no unsupported entries, and every collision has a resolution.
extern "C" std::int32_t bs_widget_test_mcp_import_preview_before_apply() {
    AppController controller;
    // The dialog reads the existing IDs from the controller's listing, which
    // is empty here; it follows `mcpChanged` too.
    McpImportDialog dialog(&controller);
    dialog.setAttribute(Qt::WA_DeleteOnClose, false);
    emit controller.mcpChanged(QString::fromUtf8(kListing));
    auto* document = dialog.findChild<QPlainTextEdit*>(QStringLiteral("McpImportDocument"));
    auto* previewButton = dialog.findChild<QPushButton*>(QStringLiteral("McpImportPreview"));
    auto* apply = dialog.findChild<QPushButton*>(QStringLiteral("McpImportApply"));
    auto* collisions = dialog.findChild<QTableWidget*>(QStringLiteral("McpImportCollisions"));
    if (!document || !previewButton || !apply || !collisions) return 1;
    const QString text = QStringLiteral(
        R"({"mcpServers":{"Jira":{"url":"https://mcp.atlassian.com/v2/mcp"},"other":{"url":"https://o.example/mcp"}}})");
    document->setPlainText(text);
    if (apply->isEnabled()) return 2;
    previewButton->click(); // not connected: the answer below stands in for the daemon's
    if (apply->isEnabled()) return 3;
    const QString preview = QStringLiteral(
        R"({"servers":[{"id":"jira","name":"Jira","transport":{"type":"http","url":"https://mcp.atlassian.com/v2/mcp"}},)"
        R"({"id":"other","name":"other","transport":{"type":"http","url":"https://o.example/mcp"}}],)"
        R"("issues":[],"collisions":["jira"]})");
    emit controller.mcpOperationFinished("mcp.import_preview", "", preview, "");
    if (collisions->isHidden() || collisions->rowCount() != 1 || collisions->item(0, 0)->text() != "Jira") return 4;
    if (apply->isEnabled()) return 5; // the collision is unresolved
    auto* choice = qobject_cast<QComboBox*>(collisions->cellWidget(0, 1));
    auto* newId = qobject_cast<QLineEdit*>(collisions->cellWidget(0, 2));
    if (!choice || !newId) return 6;
    choice->setCurrentIndex(choice->findData("rename"));
    if (apply->isEnabled()) return 7; // no new ID yet
    newId->setText("tokened"); // taken
    if (apply->isEnabled()) return 8;
    newId->setText("jira-two");
    if (!apply->isEnabled()) return 9;
    choice->setCurrentIndex(choice->findData("replace"));
    if (!apply->isEnabled()) return 10;
    // Editing the text invalidates the preview.
    document->setPlainText(text + " ");
    if (apply->isEnabled()) return 11;
    document->setPlainText(text);
    if (!apply->isEnabled()) return 12;
    // An unsupported entry blocks the whole import.
    previewButton->click();
    emit controller.mcpOperationFinished("mcp.import_preview", "",
        QStringLiteral(R"({"servers":[],"issues":[{"entry":"entry 1","field":"type","reason":"SSE servers are not supported"}],"collisions":[]})"), "");
    if (apply->isEnabled()) return 13;
    if (!dialog.findChild<QLabel*>(QStringLiteral("McpImportSummary"))->text().contains("SSE")) return 14;
    return 0;
}

#endif // BS_WIDGET_TESTS
