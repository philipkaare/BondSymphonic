#include "NetworkRulesDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QDialogButtonBox>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QStringList>
#include <QVBoxLayout>

namespace {
QString lines(const QJsonArray& entries) {
    QStringList result;
    for (const auto& entry : entries) { result.append(entry.toString()); }
    return result.join('\n');
}

QJsonArray entries(const QString& text) {
    QJsonArray result;
    for (const QString& line : text.split('\n')) {
        const QString host = line.trimmed();
        if (!host.isEmpty() && !result.contains(host)) { result.append(host); }
    }
    return result;
}
}

NetworkRulesDialog::NetworkRulesDialog(AppController* controller, const QString& workspaceId,
                                       const QString& workspaceName, QWidget* parent)
    : QDialog(parent), m_controller(controller), m_workspaceId(workspaceId) {
    setObjectName(QStringLiteral("NetworkRulesDialog"));
    setProperty("workspaceId", m_workspaceId);
    setWindowTitle(QStringLiteral("Network rules — %1").arg(workspaceName));
    setAttribute(Qt::WA_DeleteOnClose);
    resize(580, 520);
    auto* layout = new QVBoxLayout(this);
    auto* description = new QLabel(
        QStringLiteral("Rules apply to this workspace and are saved between sessions. "
                       "Enter one host or wildcard per line, such as api.example.com or *.example.com. "
                       "Block rules take precedence over allow rules. Changes apply to new connections."), this);
    description->setTextFormat(Qt::PlainText);
    description->setWordWrap(true);
    layout->addWidget(description);
    layout->addWidget(new QLabel(QStringLiteral("Allowed hosts"), this));
    m_allow = new QPlainTextEdit(this);
    m_allow->setObjectName(QStringLiteral("NetworkAllowedHosts"));
    m_allow->setPlaceholderText(QStringLiteral("api.example.com\n*.example.com"));
    layout->addWidget(m_allow);
    layout->addWidget(new QLabel(QStringLiteral("Always blocked hosts"), this));
    m_block = new QPlainTextEdit(this);
    m_block->setObjectName(QStringLiteral("NetworkBlockedHosts"));
    m_block->setPlaceholderText(QStringLiteral("blocked.example.com"));
    layout->addWidget(m_block);
    m_error = new QLabel(QStringLiteral("Loading network rules…"), this);
    m_error->setObjectName(QStringLiteral("NetworkRulesError"));
    m_error->setTextFormat(Qt::PlainText);
    m_error->setWordWrap(true);
    layout->addWidget(m_error);
    m_buttons = new QDialogButtonBox(QDialogButtonBox::Save | QDialogButtonBox::Cancel, this);
    layout->addWidget(m_buttons);
    connect(m_buttons, &QDialogButtonBox::accepted, this, &NetworkRulesDialog::save);
    connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    connect(controller, &AppController::networkRulesLoaded, this,
            [this](const QString& workspaceId, const QString& json) {
        if (workspaceId != m_workspaceId || m_loaded) { return; }
        const QJsonObject rules = QJsonDocument::fromJson(json.toUtf8()).object();
        m_originalRules = rules;
        m_allow->setPlainText(lines(rules.value("allow").toArray()));
        m_block->setPlainText(lines(rules.value("block").toArray()));
        m_loaded = true;
        m_error->clear();
        setBusy(false);
    });
    connect(controller, &AppController::networkRulesSaved, this, [this](const QString& workspaceId) {
        if (workspaceId == m_workspaceId && m_saving) { accept(); }
    });
    connect(controller, &AppController::workspaceOpFailed, this,
            [this](const QString& workspaceId, const QString& op, const QString& message) {
        if (workspaceId != m_workspaceId ||
            (op != QLatin1String("workspace.set_network_rules") &&
             (op != QLatin1String("workspace.get") || m_loaded))) { return; }
        m_saving = false;
        m_error->setText(message);
        setBusy(false);
    });
    setBusy(true);
    controller->loadNetworkRules(m_workspaceId);
}

void NetworkRulesDialog::setBusy(bool busy) {
    m_allow->setEnabled(!busy && m_loaded);
    m_block->setEnabled(!busy && m_loaded);
    m_buttons->button(QDialogButtonBox::Save)->setEnabled(!busy && m_loaded);
    m_buttons->button(QDialogButtonBox::Cancel)->setEnabled(!m_saving);
}

void NetworkRulesDialog::save() {
    if (!m_loaded || m_saving || m_controller.isNull()) { return; }
    const QJsonObject rules{{"allow", entries(m_allow->toPlainText())},
                            {"block", entries(m_block->toPlainText())},
                            {"expected_allow", m_originalRules.value("allow")},
                            {"expected_block", m_originalRules.value("block")}};
    m_saving = true;
    m_error->setText(QStringLiteral("Saving network rules…"));
    setBusy(true);
    m_controller->setNetworkRules(m_workspaceId,
        QString::fromUtf8(QJsonDocument(rules).toJson(QJsonDocument::Compact)));
}

#ifdef BS_WIDGET_TESTS
#include <cstdint>

extern "C" std::int32_t bs_widget_test_network_rules_dialog() {
    AppController controller;
    NetworkRulesDialog dialog(&controller, QStringLiteral("ws_rules"), QStringLiteral("Rules"));
    dialog.setAttribute(Qt::WA_DeleteOnClose, false);
    auto* allow = dialog.findChild<QPlainTextEdit*>(QStringLiteral("NetworkAllowedHosts"));
    auto* block = dialog.findChild<QPlainTextEdit*>(QStringLiteral("NetworkBlockedHosts"));
    auto* buttons = dialog.findChild<QDialogButtonBox*>();
    auto* error = dialog.findChild<QLabel*>(QStringLiteral("NetworkRulesError"));
    if (!allow || !block || !buttons || !error || buttons->button(QDialogButtonBox::Save)->isEnabled()) {
        return 1;
    }
    const QString rules = QStringLiteral("{\"allow\":[\"*.example.com\"],\"block\":[\"api.example.com\"]}");
    emit controller.networkRulesLoaded(QStringLiteral("ws_other"), rules);
    if (!allow->toPlainText().isEmpty() || allow->isEnabled()) { return 2; }
    emit controller.networkRulesLoaded(QStringLiteral("ws_rules"), rules);
    if (allow->toPlainText() != QLatin1String("*.example.com") ||
        block->toPlainText() != QLatin1String("api.example.com") ||
        !buttons->button(QDialogButtonBox::Save)->isEnabled()) { return 3; }
    block->setPlainText(QStringLiteral("https://bad.example"));
    emit controller.workspaceOpFailed(QStringLiteral("ws_other"),
        QStringLiteral("workspace.set_network_rules"), QStringLiteral("wrong workspace"));
    if (!error->text().isEmpty()) { return 4; }
    emit controller.workspaceOpFailed(QStringLiteral("ws_rules"),
        QStringLiteral("workspace.set_network_rules"), QStringLiteral("invalid host"));
    if (error->text() != QLatin1String("invalid host") ||
        block->toPlainText() != QLatin1String("https://bad.example") ||
        !buttons->button(QDialogButtonBox::Save)->isEnabled()) { return 5; }
    return 0;
}
#endif
