#include "WorkspaceMcpDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QComboBox>
#include <QDialogButtonBox>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPushButton>
#include <QTableWidget>
#include <QVBoxLayout>

namespace {

/// Column of the override picker, and of the words saying what it adds up to.
constexpr int kServerColumn = 0;
constexpr int kOverrideColumn = 1;
constexpr int kEffectColumn = 2;

/// The daemon's rule for whether an agent started here gets the server; see
/// `model::mcp::effective_in_workspace`.
bool effective(const QJsonObject& server, const QString& value) {
    if (!server.value("enabled").toBool(true)) { return false; }
    if (value == QLatin1String("enabled")) { return true; }
    if (value == QLatin1String("disabled")) { return false; }
    return server.value("default_workspace_enabled").toBool(true);
}

QString effectText(const QJsonObject& server, const QString& value) {
    if (!server.value("enabled").toBool(true)) { return QStringLiteral("Off (disabled in Settings)"); }
    return effective(server, value) ? QStringLiteral("On") : QStringLiteral("Off");
}

} // namespace

WorkspaceMcpDialog::WorkspaceMcpDialog(AppController* controller, const QString& workspaceId,
                                       const QString& workspaceName, QWidget* parent)
    : QDialog(parent), m_controller(controller), m_workspaceId(workspaceId) {
    setObjectName(QStringLiteral("WorkspaceMcpDialog"));
    setProperty("workspaceId", m_workspaceId);
    setWindowTitle(QStringLiteral("MCP servers — %1").arg(workspaceName));
    setAttribute(Qt::WA_DeleteOnClose);
    resize(620, 420);
    auto* layout = new QVBoxLayout(this);
    auto* description = new QLabel(
        QStringLiteral("Choose which MCP servers agents in this workspace get. Inherit follows the "
                       "server's own default from Settings. A choice is saved as you make it: "
                       "turning a server on applies when an agent next starts, turning it off "
                       "stops its tools at once."), this);
    description->setTextFormat(Qt::PlainText);
    description->setWordWrap(true);
    layout->addWidget(description);
    m_table = new QTableWidget(0, 3, this);
    m_table->setObjectName(QStringLiteral("WorkspaceMcpServers"));
    m_table->setHorizontalHeaderLabels(
        {QStringLiteral("Server"), QStringLiteral("In this workspace"), QStringLiteral("Agents get it")});
    m_table->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    m_table->verticalHeader()->hide();
    m_table->setSelectionMode(QAbstractItemView::NoSelection);
    m_table->setEditTriggers(QAbstractItemView::NoEditTriggers);
    layout->addWidget(m_table, 1);
    m_message = new QLabel(this);
    m_message->setObjectName(QStringLiteral("WorkspaceMcpMessage"));
    m_message->setTextFormat(Qt::PlainText);
    m_message->setWordWrap(true);
    layout->addWidget(m_message);
    auto* buttons = new QDialogButtonBox(QDialogButtonBox::Close, this);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    layout->addWidget(buttons);

    connect(controller, &AppController::mcpChanged, this, &WorkspaceMcpDialog::showListing);
    connect(controller, &AppController::mcpOperationFinished, this,
            [this](const QString& op, const QString&, const QString&, const QString& error) {
        if (op == QLatin1String("mcp.override")) {
            m_message->setText(error.isEmpty() ? QStringLiteral("Saved.") : error);
        }
    });
    showListing(controller->mcpViewJson());
    controller->mcpList();
}

void WorkspaceMcpDialog::showListing(const QString& json) {
    const QJsonObject listing = QJsonDocument::fromJson(json.toUtf8()).object();
    m_table->setRowCount(0);
    if (!listing.contains(QStringLiteral("registry"))) {
        m_table->hide();
        m_message->setText(listing.value("error").toString(QStringLiteral("Loading MCP servers…")));
        return;
    }
    m_table->show();
    const QJsonObject registry = listing.value("registry").toObject();
    const QJsonObject overrides =
        registry.value("overrides").toObject().value(m_workspaceId).toObject();
    const QJsonArray servers = registry.value("servers").toArray();
    if (servers.isEmpty()) {
        m_message->setText(QStringLiteral("No MCP servers yet. Add them in Settings › MCP servers."));
    } else if (m_message->text().startsWith(QStringLiteral("Loading")) || m_message->text().startsWith(QStringLiteral("No MCP"))) {
        m_message->clear();
    }
    for (const auto& value : servers) {
        const QJsonObject server = value.toObject();
        const QString id = server.value("id").toString();
        const QString current = overrides.value(id).toString(QStringLiteral("inherit"));
        const int row = m_table->rowCount();
        m_table->insertRow(row);
        m_table->setItem(row, kServerColumn, new QTableWidgetItem(server.value("name").toString(id)));
        auto* choice = new QComboBox(m_table);
        choice->setObjectName(QStringLiteral("WorkspaceMcpOverride_") + id);
        const bool byDefault = server.value("default_workspace_enabled").toBool(true);
        choice->addItem(byDefault ? QStringLiteral("Inherit (on)") : QStringLiteral("Inherit (off)"),
                        QStringLiteral("inherit"));
        choice->addItem(QStringLiteral("Enabled"), QStringLiteral("enabled"));
        choice->addItem(QStringLiteral("Disabled"), QStringLiteral("disabled"));
        choice->setCurrentIndex(qMax(0, choice->findData(current)));
        m_table->setCellWidget(row, kOverrideColumn, choice);
        auto* effect = new QTableWidgetItem(effectText(server, current));
        m_table->setItem(row, kEffectColumn, effect);
        connect(choice, &QComboBox::currentIndexChanged, this, [this, choice, id, server, effect] {
            if (m_controller.isNull()) { return; }
            const QString value = choice->currentData().toString();
            effect->setText(effectText(server, value));
            m_message->setText(QStringLiteral("Saving…"));
            m_controller->mcpSetOverride(m_workspaceId, id, value);
        });
    }
}

#ifdef BS_WIDGET_TESTS
#include <cstdint>

/// A server with no override for this workspace shows Inherit, worded with
/// the server's default; another workspace's override is not this one's.
extern "C" std::int32_t bs_widget_test_workspace_dialog_shows_inherit() {
    AppController controller;
    WorkspaceMcpDialog dialog(&controller, QStringLiteral("ws_mcp"), QStringLiteral("Mcp"));
    dialog.setAttribute(Qt::WA_DeleteOnClose, false);
    auto* message = dialog.findChild<QLabel*>(QStringLiteral("WorkspaceMcpMessage"));
    if (!message || !message->text().startsWith("Loading")) return 1;
    emit controller.mcpChanged(QStringLiteral(R"({"registry":{"revision":2,"servers":[)"
        R"({"id":"jira","name":"Jira Atlassian","transport":{"type":"http","url":"https://mcp.atlassian.com/v2/mcp","auth":{"type":"oauth"}},"enabled":true,"default_workspace_enabled":true,"enabled_backends":["claude"]},)"
        R"({"id":"quiet","name":"Quiet","transport":{"type":"http","url":"https://q.example/mcp","auth":{"type":"none"}},"enabled":true,"default_workspace_enabled":false,"enabled_backends":["claude"]},)"
        R"({"id":"local","name":"Local","transport":{"type":"stdio","command":"npx"},"enabled":true,"default_workspace_enabled":true,"enabled_backends":["claude"]}],)"
        R"("overrides":{"ws_mcp":{"local":"disabled"},"ws_other":{"jira":"disabled"}}},"statuses":[]})"));
    auto* jira = dialog.findChild<QComboBox*>(QStringLiteral("WorkspaceMcpOverride_jira"));
    auto* quiet = dialog.findChild<QComboBox*>(QStringLiteral("WorkspaceMcpOverride_quiet"));
    auto* local = dialog.findChild<QComboBox*>(QStringLiteral("WorkspaceMcpOverride_local"));
    if (!jira || !quiet || !local) return 2;
    if (jira->currentData().toString() != "inherit" || jira->currentText() != "Inherit (on)") return 3;
    if (quiet->currentData().toString() != "inherit" || quiet->currentText() != "Inherit (off)") return 4;
    if (local->currentData().toString() != "disabled") return 5;
    auto* table = dialog.findChild<QTableWidget*>(QStringLiteral("WorkspaceMcpServers"));
    if (table->item(0, 2)->text() != "On" || table->item(1, 2)->text() != "Off" ||
        table->item(2, 2)->text() != "Off") return 6;
    // A choice is sent as it is made; the controller has no daemon here, so
    // what shows is the row's new effect.
    quiet->setCurrentIndex(quiet->findData("enabled"));
    if (table->item(1, 2)->text() != "On") return 7;
    emit controller.mcpOperationFinished("mcp.override", "quiet", "", "MCP server not found");
    if (message->text() != "MCP server not found") return 8;
    // A degraded registry is said instead of the table.
    emit controller.mcpChanged(QStringLiteral(R"({"error":"cannot read registry.json"})"));
    if (!table->isHidden() || !message->text().contains("registry.json")) return 9;
    return 0;
}
#endif
