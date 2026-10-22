#include "ProviderPage.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QCheckBox>
#include <QComboBox>
#include <QFormLayout>
#include <QHeaderView>
#include <QHBoxLayout>
#include <QJsonDocument>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QRegularExpression>
#include <QTableWidget>
#include <QVBoxLayout>
#include <QUuid>

ProviderPage::ProviderPage(AppController *controller, QWidget *parent)
    : QWidget(parent), m_controller(controller) {
    auto *layout = new QVBoxLayout(this);
    auto *top = new QHBoxLayout();
    m_choice = new QComboBox(this);
    m_choice->setObjectName("OpenCodeProviderList");
    auto *add = new QPushButton("Add provider", this);
    auto *removeButton = new QPushButton("Remove", this);
    top->addWidget(m_choice, 1);
    top->addWidget(add);
    top->addWidget(removeButton);
    layout->addLayout(top);
    auto *presets = new QHBoxLayout();
    for (const auto &name : {"Ollama", "vLLM", "SGLang", "Custom"}) {
        auto *button = new QPushButton(name, this);
        button->setObjectName(QString("ProviderPreset_%1").arg(name));
        presets->addWidget(button);
        connect(button, &QPushButton::clicked, this, [this, name] { preset(name); });
    }
    layout->addLayout(presets);
    auto *form = new QFormLayout();
    form->setRowWrapPolicy(QFormLayout::WrapLongRows);
    m_id = new QLineEdit(this); m_id->setObjectName("ProviderId");
    m_name = new QLineEdit(this); m_name->setObjectName("ProviderName");
    m_url = new QLineEdit(this); m_url->setObjectName("ProviderUrl");
    m_url->setPlaceholderText("http://localhost:11434/v1");
    m_target = new QComboBox(this); m_target->setObjectName("ProviderTarget");
    m_target->addItem("WSL host", "wsl");
    m_target->addItem("Windows host", "windows");
    m_target->addItem("Remote", "remote");
    form->addRow("Stable ID:", m_id);
    form->addRow("Name:", m_name);
    form->addRow("OpenAI-compatible base URL:", m_url);
    form->addRow("Endpoint is on:", m_target);
    m_models = new QTableWidget(0, 4, this);
    m_models->setObjectName("ProviderModels");
    m_models->setHorizontalHeaderLabels({"Model ID", "Display name", "Context limit", "Output limit"});
    m_models->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
    m_models->setMinimumHeight(120);
    auto *modelRow = new QVBoxLayout();
    modelRow->addWidget(m_models);
    auto *modelButtons = new QHBoxLayout();
    auto *addModel = new QPushButton("Add model", this);
    auto *removeModel = new QPushButton("Remove model", this);
    modelButtons->addWidget(addModel); modelButtons->addWidget(removeModel);
    modelRow->addLayout(modelButtons);
    form->addRow("Models and optional limits:", modelRow);
    m_key = new QLineEdit(this); m_key->setObjectName("ProviderKey");
    m_key->setEchoMode(QLineEdit::Password);
    m_key->setPlaceholderText("Empty keeps the stored key; local servers need no key");
    auto *keyRow = new QHBoxLayout();
    keyRow->addWidget(m_key);
    auto *removeKey = new QPushButton("Remove key", this);
    keyRow->addWidget(removeKey);
    form->addRow("API key:", keyRow);
    layout->addLayout(form);
    auto *saveButton = new QPushButton("Save provider", this);
    saveButton->setObjectName("SaveProvider");
    layout->addWidget(saveButton);
    m_status = new QLabel(this); m_status->setWordWrap(true); m_status->setTextFormat(Qt::PlainText);
    layout->addWidget(m_status);
    connect(add, &QPushButton::clicked, this, [this] { preset("Custom"); });
    connect(removeButton, &QPushButton::clicked, this, &ProviderPage::remove);
    connect(saveButton, &QPushButton::clicked, this, &ProviderPage::save);
    connect(addModel, &QPushButton::clicked, this, [this] { m_models->insertRow(m_models->rowCount()); });
    connect(removeModel, &QPushButton::clicked, this, [this] {
        if (m_models->currentRow() >= 0) m_models->removeRow(m_models->currentRow());
    });
    connect(m_choice, &QComboBox::currentIndexChanged, this, [this] { showProvider(m_choice->currentData().toString()); });
    connect(removeKey, &QPushButton::clicked, this, [this] {
        if (!m_controller->clearProviderApiKey(m_id->text())) { m_status->setText("The key could not be removed."); return; }
        m_key->clear(); m_key->setPlaceholderText("No key stored");
    });
    connect(controller, &AppController::settingsRpcFinished, this,
            [this](const QString &method, qint64 serial, const QString &json, const QString &error) {
                finished(method, serial, json, error);
            });
    load();
}

QJsonArray ProviderPage::providers() const { return m_providers; }

qint64 ProviderPage::call(const QString &method, const QJsonObject &params) {
    m_pending = m_controller->nextSettingsRequestId(); m_serial = m_pending; m_pendingMethod = method;
    m_controller->settingsRpc(method, QString::fromUtf8(QJsonDocument(params).toJson(QJsonDocument::Compact)), m_pending);
    return m_pending;
}

void ProviderPage::load() { call("opencode.providers.list", {}); }

void ProviderPage::preset(const QString &name) {
    m_choice->setCurrentIndex(-1);
    m_id->setText(QUuid::createUuid().toString(QUuid::WithoutBraces));
    m_id->setReadOnly(false);
    m_name->setText(name == "Custom" ? QString() : name);
    m_url->setText(name == "Ollama" ? "http://localhost:11434/v1" :
                   name == "vLLM" ? "http://localhost:8000/v1" :
                   name == "SGLang" ? "http://localhost:30000/v1" : QString());
    m_target->setCurrentIndex(0);
    m_models->setRowCount(0); m_key->clear();
    m_status->setText("Add at least one model ID. Discovery may add more after saving.");
}

void ProviderPage::showProvider(const QString &id) {
    for (const auto &value : m_providers) {
        const auto p = value.toObject();
        if (p.value("id").toString() != id) continue;
        m_id->setText(id); m_name->setText(p.value("name").toString());
        m_id->setReadOnly(true);
        m_url->setText(p.value("base_url").toString());
        m_target->setCurrentIndex(qMax(0, m_target->findData(p.value("endpoint_target").toString())));
        const auto models = p.value("models").toArray();
        m_models->setRowCount(models.size());
        for (int row = 0; row < models.size(); ++row) {
            const auto model = models[row].toObject();
            const QStringList fields{model.value("id").toString(), model.value("name").toString(),
                model.value("context_limit").isDouble() ? QString::number(model.value("context_limit").toInteger()) : QString(),
                model.value("output_limit").isDouble() ? QString::number(model.value("output_limit").toInteger()) : QString()};
            for (int col = 0; col < 4; ++col) m_models->setItem(row, col, new QTableWidgetItem(fields[col]));
        }
        m_key->clear();
        m_key->setPlaceholderText(m_controller->providerApiKeySet(id) ? "Stored in Windows Credential Manager" : "No key stored; local servers need no key");
        return;
    }
}

void ProviderPage::save() {
    const QString id = m_id->text().trimmed();
    if (id.isEmpty() || m_name->text().trimmed().isEmpty() || m_url->text().trimmed().isEmpty()) {
        m_status->setText("Stable ID, name, and base URL are required."); return;
    }
    if (!QRegularExpression("^[A-Za-z0-9_-]{1,128}$").match(id).hasMatch()) {
        m_status->setText("Stable ID may contain letters, digits, hyphens, and underscores."); return;
    }
    QJsonArray models;
    for (int row = 0; row < m_models->rowCount(); ++row) {
        auto text = [this, row](int col) { auto *item = m_models->item(row, col); return item ? item->text().trimmed() : QString(); };
        if (text(0).isEmpty()) continue;
        QJsonObject model{{"id", text(0)}, {"name", text(1).isEmpty() ? text(0) : text(1)}};
        for (const auto &limit : {qMakePair(2, "context_limit"), qMakePair(3, "output_limit")}) {
            if (text(limit.first).isEmpty()) continue;
            bool ok = false; const auto number = text(limit.first).toULongLong(&ok);
            if (!ok || number == 0) { m_status->setText("Model limits must be positive integers."); return; }
            model.insert(limit.second, static_cast<qint64>(number));
        }
        models.append(model);
    }
    const qint64 serial = call("opencode.providers.upsert", {{"expected_revision", m_revision},
         {"provider", QJsonObject{{"id", id}, {"name", m_name->text().trimmed()},
          {"base_url", m_url->text().trimmed()}, {"endpoint_target", m_target->currentData().toString()}, {"models", models}}}});
    if (!m_key->text().isEmpty()) m_pendingKeys.insert(serial, {id, m_key->text()});
    m_status->setText("Saving provider…");
}

void ProviderPage::remove() {
    const QString id = m_choice->currentData().toString();
    if (id.isEmpty()) return;
    const qint64 serial = call("opencode.providers.remove", {{"expected_revision", m_revision}, {"provider_id", id}});
    m_pendingRemovals.insert(serial, id);
    m_status->setText("Removing provider…");
}

void ProviderPage::finished(const QString &method, qint64 serial, const QString &json, const QString &error) {
    bool credentialSaveFailed = false, credentialRemovalFailed = false;
    if (method == "opencode.providers.upsert" && m_pendingKeys.contains(serial)) {
        const auto pending = m_pendingKeys.take(serial);
        if (error.isEmpty()) credentialSaveFailed = !m_controller->setProviderApiKey(pending.first, pending.second);
    }
    if (method == "opencode.providers.remove" && m_pendingRemovals.contains(serial)) {
        const QString removed = m_pendingRemovals.take(serial);
        if (error.isEmpty()) credentialRemovalFailed = !m_controller->clearProviderApiKey(removed);
    }
    if (serial != m_pending || method != m_pendingMethod) return;
    if (!error.isEmpty()) { m_status->setText(error); return; }
    const auto snapshot = QJsonDocument::fromJson(json.toUtf8()).object();
    m_revision = snapshot.value("revision").toInteger();
    m_providers = snapshot.value("providers").toArray();
    const QString selected = m_choice->currentData().toString().isEmpty()
        ? m_id->text() : m_choice->currentData().toString();
    m_choice->blockSignals(true); m_choice->clear();
    for (const auto &value : m_providers) {
        const auto p = value.toObject(); m_choice->addItem(p.value("name").toString(), p.value("id").toString());
    }
    m_choice->setCurrentIndex(qMax(0, m_choice->findData(selected)));
    m_choice->blockSignals(false);
    showProvider(m_choice->currentData().toString());
    if (credentialSaveFailed) m_status->setText("Provider saved, but its key could not be stored.");
    else m_status->setText(credentialRemovalFailed
        ? "Provider removed, but its Windows credential could not be removed."
        : method == "opencode.providers.list" ? QString() : "Provider settings saved.");
    if (method == "opencode.providers.upsert" && m_id->text() == selected) m_key->clear();
    emit providersChanged(m_providers);
}
