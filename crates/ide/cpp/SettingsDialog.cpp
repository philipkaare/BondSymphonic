#include "SettingsDialog.h"
#include "AgentChoices.h"
#include "SetupPage.h"
#include "ProviderPage.h"
#include "McpPage.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QApplication>
#include <QCheckBox>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QFrame>
#include <QGroupBox>
#include <QGridLayout>
#include <QHBoxLayout>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPushButton>
#include <QTableWidget>
#include <QPlainTextEdit>
#include <QScrollArea>
#include <QVBoxLayout>
#include <QJsonDocument>
#include <QJsonArray>
#include <QTabWidget>
#include <QSignalBlocker>

namespace {

/// The size the dialog opens at. It hosts a terminal now, so the old
/// three-field height would start every login in a pane a few lines tall.
constexpr int kDialogWidth = 900;
constexpr int kDialogHeight = 700;

} // namespace

SettingsDialog::SettingsDialog(AppController *controller, QWidget *parent,
                               const QJsonArray &suppliedDescriptors,
                               const QString &workspaceId)
    : QDialog(parent), m_controller(controller) {
    setWindowTitle("Settings");
    // Modal, even though a login terminal runs inside it. Modality stops input
    // reaching *other* windows; it does not stop the event loop, so the PTY's
    // output still arrives and the terminal is as usable as it was on the
    // full-window page. Everything the login needs is in this dialog, and a
    // modeless one would let a second Settings open over the first.
    setModal(true);
    resize(kDialogWidth, kDialogHeight);

    auto* outer = new QVBoxLayout(this);

    // Scrolled, because the setup section brings a terminal and eight
    // prerequisite rows with it and the two fields below must stay reachable
    // on a short screen.
    m_scroll = new QScrollArea(this);
    m_scroll->setWidgetResizable(true);
    m_scroll->setFrameShape(QFrame::NoFrame);
    auto* body = new QWidget(m_scroll);
    auto* bodyLayout = new QVBoxLayout(body);
    bodyLayout->setContentsMargins(0, 0, 0, 0);
    m_scroll->setWidget(body);
    outer->addWidget(m_scroll, 1);

    // First, because it is the section a first run needs and the one the
    // status bar's "Set up…" link points at.
    auto* setupBox = new QGroupBox("Setup", body);
    auto* setupLayout = new QVBoxLayout(setupBox);
    m_setup = new SetupPage(m_controller, setupBox);
    setupLayout->addWidget(m_setup);
    bodyLayout->addWidget(setupBox, 1);

    auto* agentBox = new QGroupBox("Agents", body);
    auto* form = new QFormLayout(agentBox);
    bodyLayout->addWidget(agentBox);

    m_settings = QJsonDocument::fromJson(m_controller->backendSettingsJson().toUtf8()).object();
    const auto descriptors =
        suppliedDescriptors.isEmpty()
            ? QJsonDocument::fromJson(m_controller->backendDescriptorsJson().toUtf8()).array()
            : suppliedDescriptors;
    agentchoices::setDescriptors(descriptors);
    m_defaultBackend = new QComboBox(agentBox);
    m_defaultBackend->setObjectName(QStringLiteral("DefaultBackend"));
    form->addRow("New agents use:", m_defaultBackend);
    auto *tabs = new QTabWidget(agentBox);
    tabs->setObjectName(QStringLiteral("BackendTabs"));
    form->addRow(tabs);
    for (const auto &value : descriptors) {
        const auto descriptor = value.toObject();
        const QString id = descriptor.value("id").toString();
        const auto settings = m_settings.value("backends").toObject().value(id).toObject();
        auto *page = new QWidget(tabs);
        auto *backendForm = new QFormLayout(page);
        backendForm->setRowWrapPolicy(QFormLayout::WrapLongRows);
        auto *enabled = new QCheckBox("Enabled", page);
        enabled->setObjectName("BackendEnabled_" + id);
        enabled->setChecked(settings.value("enabled").toBool());
        backendForm->addRow(enabled);
        auto *actions = new QGridLayout();
        int actionIndex = 0;
        for (const auto &actionValue : descriptor.value("setup_actions").toArray()) {
            const QString action = actionValue.toString();
            const QString label = SetupPage::buttonTextFor(action);
            auto *button = new QPushButton(label, page);
            actions->addWidget(button, actionIndex / 2, actionIndex % 2);
            ++actionIndex;
            QObject::connect(button, &QPushButton::clicked, this, [this, action] {
                m_setup->runAction(action);
                revealSetup();
            });
        }
        backendForm->addRow("Sign-in and installation:", actions);
        auto *model = new QComboBox(page);
        model->setObjectName("BackendModel_" + id);
        agentchoices::fillModelCombo(model, settings.value("default_model").toString(), id);
        model->setInsertPolicy(QComboBox::NoInsert);
        backendForm->addRow("Default model:", model);
        auto *mode = new QComboBox(page);
        mode->setObjectName(id == "claude" ? QStringLiteral("SettingsPermissionMode")
                                           : "BackendMode_" + id);
        agentchoices::fillPermissionCombo(
            mode,
            settings.value("default_permission_mode")
                .toString(descriptor.value("default_permission_mode").toString()),
            id);
        backendForm->addRow("Default permissions:", mode);
        auto *note = new QLabel(descriptor.value("permission_note").toString(), page);
        note->setWordWrap(true);
        backendForm->addRow(note);
        QComboBox *provider = nullptr;
        if (id == "opencode") {
            provider = new QComboBox(page);
            provider->setObjectName("OpenCodeDefaultProvider");
            backendForm->addRow("Default provider:", provider);
            auto *providerPage = new ProviderPage(m_controller, page);
            backendForm->addRow(providerPage);
            const QString savedProvider = settings.value("default_provider_id").toString();
            QObject::connect(providerPage, &ProviderPage::providersChanged, this,
                [this, provider, model, savedProvider](const QJsonArray &providers) {
                    const QString previous = provider->currentData().toString();
                    const QSignalBlocker blocker(provider);
                    provider->clear();
                    for (const auto &value : providers) {
                        const auto entry = value.toObject();
                        provider->addItem(entry.value("name").toString(), entry.value("id").toString());
                    }
                    const QString wanted = previous.isEmpty() ? savedProvider : previous;
                    const int index = provider->findData(wanted);
                    provider->setCurrentIndex(index >= 0 ? index : (wanted.isEmpty() ? 0 : -1));
                    if (provider->currentIndex() >= 0) {
                        QList<agentchoices::Choice> choices;
                        for (const auto &value : providers) {
                            const auto entry = value.toObject();
                            if (entry.value("id").toString() != provider->currentData().toString()) continue;
                            for (const auto &modelValue : entry.value("models").toArray()) {
                                const auto choice = modelValue.toObject();
                                choices.append({choice.value("name").toString(), choice.value("id").toString()});
                            }
                        }
                        agentchoices::setModels("opencode", choices);
                        agentchoices::fillModelCombo(model, agentchoices::modelComboSelection(model), "opencode");
                        m_controller->refreshProviderModels(provider->currentData().toString());
                    }
                });
            QObject::connect(provider, &QComboBox::currentIndexChanged, this, [this, provider, model, providerPage] {
                const QString id = provider->currentData().toString();
                if (id.isEmpty()) return;
                QList<agentchoices::Choice> choices;
                for (const auto &value : providerPage->providers()) {
                    const auto entry = value.toObject(); if (entry.value("id").toString() != id) continue;
                    for (const auto &value : entry.value("models").toArray()) {
                        const auto choice = value.toObject(); choices.append({choice.value("name").toString(), choice.value("id").toString()});
                    }
                }
                agentchoices::setModels("opencode", choices);
                agentchoices::fillModelCombo(model, QString(), "opencode");
                m_controller->refreshProviderModels(id);
            });
            QObject::connect(m_controller, &AppController::providerModelsChecked, this,
                [provider, model](const QString &providerId, const QString &json, qint64) {
                    if (providerId != provider->currentData().toString()) return;
                    QList<agentchoices::Choice> choices;
                    for (const auto &value : QJsonDocument::fromJson(json.toUtf8()).array()) {
                        const auto choice = value.toObject();
                        choices.append({choice.value("label").toString(), choice.value("id").toString()});
                    }
                    agentchoices::setModels("opencode", choices);
                    agentchoices::fillModelCombo(model, agentchoices::modelComboSelection(model), "opencode");
                });
        }
        QLineEdit *key = nullptr;
        QPushButton *remove = nullptr;
        if (id != "opencode") {
        auto* keyRow = new QHBoxLayout();
        key = new QLineEdit(page);
        key->setObjectName("BackendKey_" + id);
        key->setEchoMode(QLineEdit::Password);
        auto *remove = new QPushButton("Remove key", page);
        const auto refreshKey = [this, id, key, remove] {
            const bool stored = m_controller->backendApiKeySet(id);
            key->setPlaceholderText(stored ? "Stored in Windows Credential Manager"
                                           : "Leave empty to use sign-in");
            remove->setEnabled(stored);
        };
        refreshKey();
        QObject::connect(remove, &QPushButton::clicked, this, [this, id, key, refreshKey] {
            if (!m_controller->clearBackendApiKey(id))
                QMessageBox::warning(this, "Settings", "The credential could not be removed.");
            key->clear();
            refreshKey();
        });
        keyRow->addWidget(key, 1);
        keyRow->addWidget(remove);
        backendForm->addRow(descriptor.value("credential_label").toString() + ":", keyRow);
        auto *keyNote = new QLabel(
            id == "codex" ? "A stored API key takes precedence over ChatGPT sign-in. Leave empty "
                            "to keep the stored key."
                          : "Used when Claude has no login. Leave empty to keep the stored key.",
            page);
        keyNote->setWordWrap(true);
        backendForm->addRow(keyNote);
        }
        const QString label = descriptor.value("label").toString(id);
        m_backends.insert(id, {label, enabled, key, remove, model, mode, provider});
        tabs->addTab(page, label);
        QObject::connect(enabled, &QCheckBox::toggled, this, [this] { refreshDefaults(); });
        QObject::connect(m_controller, &AppController::backendModelsChecked, this,
                         [id, model](const QString &backend, const QString &, qint64) {
                             if (backend == id) {
                                 const QSignalBlocker blocker(model);
                                 agentchoices::fillModelCombo(
                                     model, agentchoices::modelComboSelection(model), id);
                             }
                         });
        if (id != "opencode") m_controller->refreshBackendModels(id);
    }
    refreshDefaults();
    const int defaultIndex =
        m_defaultBackend->findData(m_settings.value("default_backend").toString());
    if (defaultIndex >= 0)
        m_defaultBackend->setCurrentIndex(defaultIndex);

    m_showMeta = new QCheckBox("Show turn cost and system lines", agentBox);
    m_showMeta->setObjectName(QStringLiteral("SettingsShowAgentMeta"));
    m_showMeta->setChecked(m_controller->showAgentMeta());
    m_showMeta->setToolTip(QStringLiteral(
        "The small italic lines under an answer: what the turn cost, how long it took, and what "
        "the agent's own startup reported."));
    form->addRow(QString(), m_showMeta);

    const auto capability = QJsonDocument::fromJson(m_controller->capabilitiesJson().toUtf8()).object()
                                .value("mcp").toObject();
    if (capability.value("registry").toBool()) {
        auto *mcpBox = new QGroupBox("MCP servers", body);
        auto *mcpLayout = new QVBoxLayout(mcpBox);
        mcpLayout->addWidget(new McpPage(m_controller, workspaceId, mcpBox));
        bodyLayout->addWidget(mcpBox);
    }

    auto* lookBox = new QGroupBox("Appearance", body);
    auto* lookForm = new QFormLayout(lookBox);
    bodyLayout->addWidget(lookBox);
    m_theme = new QComboBox(lookBox);
    m_theme->addItem("Follow system", theme::nameOfChoice(theme::Choice::System));
    m_theme->addItem("Light", theme::nameOfChoice(theme::Choice::Light));
    m_theme->addItem("Dark", theme::nameOfChoice(theme::Choice::Dark));
    const int themeIndex = m_theme->findData(m_controller->theme());
    if (themeIndex >= 0) {
        m_theme->setCurrentIndex(themeIndex);
    }
    lookForm->addRow("Theme:", m_theme);
    // Applied as it is picked rather than on OK: a palette is the one setting
    // whose effect is the whole point of choosing it, and a preview that waits
    // for a dialog to close is not a preview. `reject` is what puts it back.
    QObject::connect(m_theme, &QComboBox::currentIndexChanged, this, [this](int) {
        theme::apply(*qApp, theme::choiceFromName(m_theme->currentData().toString()));
    });

    auto* buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    outer->addWidget(buttons);

    QObject::connect(buttons, &QDialogButtonBox::accepted, this, &SettingsDialog::accept);
    QObject::connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
}

void SettingsDialog::revealSetup() {
    if (m_setup == nullptr) {
        return;
    }
    m_scroll->ensureWidgetVisible(m_setup);
    m_setup->setFocus();
}

void SettingsDialog::refreshDefaults() {
    const QString selected = m_defaultBackend->currentData().toString();
    m_defaultBackend->clear();
    auto entries = m_settings.value("backends").toObject();
    for (auto it = m_backends.cbegin(); it != m_backends.cend(); ++it) {
        if (it->enabled->isChecked())
            m_defaultBackend->addItem(it->label, it.key());
        auto entry = entries.value(it.key()).toObject();
        entry.insert("enabled", it->enabled->isChecked());
        entries.insert(it.key(), entry);
}
    const int index = m_defaultBackend->findData(selected);
    if (index >= 0)
        m_defaultBackend->setCurrentIndex(index);
    m_settings.insert("backends", entries);
    m_setup->setBackendSettings(entries);
}

void SettingsDialog::accept() {
    auto entries = m_settings.value("backends").toObject();
    for (auto it = m_backends.cbegin(); it != m_backends.cend(); ++it) {
        auto entry = entries.value(it.key()).toObject();
        entry.insert("enabled", it->enabled->isChecked());
        entry.insert("default_model", agentchoices::modelComboSelection(it->model));
        if (it->provider) {
            const QString providerId = it->provider->currentData().toString();
            if (it->enabled->isChecked() && providerId.isEmpty()) {
                QMessageBox::warning(this, "Settings", "Choose an OpenCode provider before enabling OpenCode.");
                return;
            }
            entry.insert("default_provider_id", providerId);
        }
        entry.insert("default_permission_mode", it->mode->currentData().toString());
        entries.insert(it.key(), entry);
        const QString key = it->key ? it->key->text().trimmed() : QString();
        if (!key.isEmpty() && !m_controller->setBackendApiKey(it.key(), key)) {
            QMessageBox::warning(this, "Settings", "The API key could not be stored.");
            return;
        }
        if (it->key) it->key->clear();
    }
    m_settings.insert("backends", entries);
    m_settings.insert("default_backend", m_defaultBackend->currentData().toString());
    if (!m_controller->saveBackendSettings(
            QString::fromUtf8(QJsonDocument(m_settings).toJson(QJsonDocument::Compact)))) {
        QMessageBox::warning(this, "Settings", "The backend settings could not be saved.");
        return;
    }
    m_controller->setShowAgentMeta(m_showMeta->isChecked());
    m_controller->setTheme(m_theme->currentData().toString());
    QDialog::accept();
}

void SettingsDialog::reject() {
    // The stored word, not the combo's: the combo is what the user was trying
    // out, and Cancel is them saying they would rather not keep it.
    theme::apply(*qApp, theme::choiceFromName(m_controller->theme()));
    QDialog::reject();
}

// --- offscreen test entries --------------------------------------------------
//
// Compiled only into a development build: this is test code -- it builds
// widgets, leaks a QApplication and asserts -- and a shipped IDE has no caller
// for any of it. `build.rs` defines `BS_WIDGET_TESTS` for every profile but
// `release`, which is the one the packaged executable is built with.
#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.
#include <cstdint>
#include <QDir>
#include <QPixmap>
#include <QScrollBar>
#include <QFontDatabase>
#include <QTcpServer>
#include <QPointer>

static bool settingsFitNarrowWindow(SettingsDialog &dialog) {
    dialog.resize(640, 600);
    dialog.show();
    QCoreApplication::processEvents();
    auto *scroll = dialog.findChild<QScrollArea *>();
    const bool fits = scroll && scroll->horizontalScrollBar()->maximum() == 0;
    dialog.hide();
    return fits;
}

// Optional rendered artifacts for visual inspection of the real widgets.
// Uses synthetic prerequisite data and never starts a setup command.
static bool captureSettings(SettingsDialog &dialog, AppController &controller) {
    const QString directory = qEnvironmentVariable("BS_SETTINGS_SCREENSHOTS");
    if (directory.isEmpty())
        return true;
    if (!QDir().mkpath(directory))
        return false;
    controller.prereqsChecked(QStringLiteral(R"([
        {"name":"git","ok":true,"detail":"git version 2.43.0"},
        {"name":"bwrap","ok":true,"detail":"bubblewrap 0.9.0"},
        {"name":"userns","ok":true,"detail":"available"},
        {"name":"sandbox","ok":true,"detail":"available"},
        {"name":"gh","ok":true,"detail":"gh version 2.86.0"},
        {"name":"gh_auth","ok":false,"detail":"not logged in"},
        {"name":"claude","ok":true,"detail":"installed"},
        {"name":"claude_auth","ok":false,"detail":"not logged in"}
    ])"));
    for (const auto &choice : {theme::Choice::Light, theme::Choice::Dark}) {
        theme::apply(*qApp, choice);
        const QString palette = choice == theme::Choice::Light ? "light" : "dark";
        for (const QSize size : {QSize(900, 700), QSize(640, 600)}) {
            dialog.resize(size);
            dialog.show();
            QCoreApplication::sendPostedEvents(nullptr, QEvent::DeferredDelete);
            QCoreApplication::processEvents();
            auto *scroll = dialog.findChild<QScrollArea *>();
            for (const bool bottom : {false, true}) {
                scroll->verticalScrollBar()->setValue(bottom ? scroll->verticalScrollBar()->maximum() : 0);
                QCoreApplication::processEvents();
                const QString file = QString("/settings-%1-%2-%3.png")
                    .arg(palette).arg(size.width()).arg(bottom ? "bottom" : "top");
                if (!dialog.grab().save(directory + file))
                    return false;
            }
        }
    }
    dialog.hide();
    theme::apply(*qApp, theme::choiceFromName(controller.theme()));
    return true;
}

/// Settings offers the same four modes the rest of the IDE does.
///
/// This dropdown had its own array, and that array had gone stale: it still
/// held `default`, the spelling the IDE retired, and `dontAsk`, which denies in
/// silence. A user who picked either from here was choosing a behaviour for
/// every agent they would create afterwards out of a list nothing else in the
/// application agreed with.
extern "C" std::int32_t bs_widget_test_settings_offers_the_one_permission_list() {
#if defined(Q_OS_WIN)
    // The offscreen platform does not discover Windows fonts automatically.
    // Use the application's normal desktop font for meaningful layout checks.
    const QString fonts = qEnvironmentVariable("WINDIR") + "/Fonts/";
    QFontDatabase::addApplicationFont(fonts + "segoeui.ttf");
    QFontDatabase::addApplicationFont(fonts + "segoeuib.ttf");
    QFontDatabase::addApplicationFont(fonts + "seguisym.ttf");
    qApp->setFont(QFont(QStringLiteral("Segoe UI"), 9));
#endif
    AppController controller;
    SettingsDialog dialog(&controller);
    if (dialog.findChild<QCheckBox *>(QStringLiteral("BackendEnabled_claude")) == nullptr ||
        dialog.findChild<QComboBox *>(QStringLiteral("DefaultBackend")) == nullptr ||
        dialog.findChild<QComboBox *>(QStringLiteral("BackendModel_claude")) == nullptr) {
        return 20;
    }
    auto* modes = dialog.findChild<QComboBox*>(QStringLiteral("SettingsPermissionMode"));
    if (modes == nullptr) {
        return 1;
    }
    if (modes->count() != agentchoices::permissionModes().size()) {
        return 2;
    }
    for (int i = 0; i < modes->count(); ++i) {
        if (modes->itemData(i).toString() != agentchoices::permissionModes().at(i).id) {
            return 3;
        }
    }
    if (modes->findData(QStringLiteral("default")) >= 0 ||
        modes->findData(QStringLiteral("dontAsk")) >= 0) {
        return 4;
    }
    if (!captureSettings(dialog, controller))
        return 26;
    if (!settingsFitNarrowWindow(dialog))
        return 27;
    const auto dark = theme::explicitPalette(true);
    if (dark.color(QPalette::PlaceholderText).lightness() <= dark.base().color().lightness())
        return 28;
    auto descriptors =
        QJsonDocument::fromJson(controller.backendDescriptorsJson().toUtf8()).array();
    descriptors.append(
        QJsonObject{{"id", "codex"},
                    {"label", "Codex"},
                    {"default_permission_mode", "never"},
                    {"permission_modes",
                     QJsonArray{QJsonObject{{"id", "never"}, {"label", "YOLO (sandboxed)"}},
                                QJsonObject{{"id", "on-request"}, {"label", "Ask when needed"}}}},
                    {"setup_actions", QJsonArray{"install_codex", "codex_login", "codex_logout"}},
                    {"credential_label", "OpenAI API key"}});
    SettingsDialog both(&controller, nullptr, descriptors);
    auto *enabled = both.findChild<QCheckBox *>("BackendEnabled_codex");
    auto *model = both.findChild<QComboBox *>("BackendModel_codex");
    auto *mode = both.findChild<QComboBox *>("BackendMode_codex");
    auto *key = both.findChild<QLineEdit *>("BackendKey_codex");
    auto *defaults = both.findChild<QComboBox *>("DefaultBackend");
    if (!enabled || !model || !mode || !key || !defaults || mode->count() != 2 ||
        !model->isEditable() || key->echoMode() != QLineEdit::Password)
        return 21;
    if (both.findChild<QCheckBox *>("BackendEnabled_opencode"))
        return 22;
    enabled->setChecked(true);
    if (defaults->findData("codex") < 0)
        return 23;
    both.findChild<QCheckBox *>("BackendEnabled_claude")->setChecked(false);
    enabled->setChecked(false);
    if (defaults->count() != 0)
        return 24;
    model->setEditText("typed-codex-model");
    agentchoices::setModels("codex", {{"New model", "new-codex"}});
    controller.backendModelsChecked("codex", "[]", 99);
    if (agentchoices::modelComboSelection(model) != "typed-codex-model")
        return 25;
    agentchoices::resetModelsForTest();
    return 0;
}

extern "C" std::int32_t bs_widget_test_mcp_and_opencode_settings_forms() {
    AppController controller;
    const QJsonArray descriptors{
        QJsonObject{{"id", "opencode"}, {"label", "OpenCode"},
                    {"default_permission_mode", "allow"},
                    {"permission_modes", QJsonArray{QJsonObject{{"id", "allow"}, {"label", "YOLO"}},
                                                       QJsonObject{{"id", "ask"}, {"label", "Ask"}}}},
                    {"setup_actions", QJsonArray{"install_opencode"}},
                    {"credential_label", "Provider key (optional)"}}};
    SettingsDialog dialog(&controller, nullptr, descriptors, "workspace-1");
    auto *provider = dialog.findChild<ProviderPage *>();
    if (!provider || !dialog.findChild<QComboBox *>("OpenCodeDefaultProvider")) return 1;
    auto *body = dialog.findChild<QScrollArea *>()->widget();
    auto *mcp = new McpPage(&controller, "workspace-1", body);
    body->layout()->addWidget(mcp);
    const QString providerSnapshot = R"({"revision":1,"providers":[{"id":"local-a","name":"Local A","base_url":"http://localhost:11434/v1","endpoint_target":"wsl","models":[{"id":"qwen","name":"Qwen","context_limit":8192,"output_limit":1024}]},{"id":"local-b","name":"Local B","base_url":"http://localhost:8000/v1","endpoint_target":"windows","models":[{"id":"other","name":"Other"}]}]})";
    controller.settingsRpcFinished("opencode.providers.list", provider->requestSerialForTest(), providerSnapshot, "");
    auto *providerList = provider->findChild<QComboBox *>("OpenCodeProviderList");
    auto *providerKey = provider->findChild<QLineEdit *>("ProviderKey");
    if (!providerList || providerList->currentData().toString() != "local-a" ||
        !providerKey || providerKey->echoMode() != QLineEdit::Password || !providerKey->text().isEmpty()) return 2;
    providerKey->setText("fixture-secret");
    provider->findChild<QPushButton *>("SaveProvider")->click();
    const qint64 pendingKey = provider->requestSerialForTest();
    providerList->setCurrentIndex(1);
    if (provider->pendingCredentialProviderForTest(pendingKey) != "local-a") return 12;
    auto *providerModels = provider->findChild<QTableWidget *>("ProviderModels");
    if (!providerModels || providerModels->item(0, 2)->text() != "" || providerModels->item(0, 3)->text() != "") return 14;
    const QString mcpSnapshot = R"({"registry":{"revision":3,"servers":[{"id":"jira","name":"Jira Cloud","transport":{"type":"http","url":"https://mcp.atlassian.com/v2/mcp","auth":{"type":"bearer","token":"secret-reference"}},"enabled":true,"default_workspace_enabled":true,"enabled_backends":["claude","codex","opencode"]},{"id":"local","name":"Local MCP","transport":{"type":"stdio","command":"mcp-server","args":[],"env":{},"env_secrets":{}},"enabled":true,"default_workspace_enabled":true,"enabled_backends":["claude"]}],"overrides":{}},"statuses":[{"server_id":"jira","state":"disconnected"},{"server_id":"local","state":"connected"}]})";
    const qint64 oldListSerial = mcp->requestSerialForTest();
    controller.settingsRpcFinished("mcp.list", oldListSerial, mcpSnapshot, "");
    auto *bearer = mcp->findChild<QLineEdit *>("McpBearerValue");
    auto *reference = mcp->findChild<QLineEdit *>("McpBearerReference");
    auto *apply = mcp->findChild<QPushButton *>("ApplyMcpImport");
    if (!bearer || !reference || !apply || bearer->echoMode() != QLineEdit::Password ||
        !bearer->text().isEmpty() || reference->text() != "secret-reference" || apply->isEnabled()) return 3;
    const auto labelHidden = [mcp](const QString &caption) {
        for (auto *label : mcp->findChildren<QLabel *>()) if (label->text() == caption) return label->isHidden();
        return false;
    };
    if (!labelHidden("Command:") || labelHidden("Bearer reference:")) return 15;
    if (mcp->findChild<QPushButton *>("TestMcpServer")->isEnabled()) return 16;
    auto *import = mcp->findChild<QPlainTextEdit *>("McpImportDocument");
    import->setPlainText(R"({"mcpServers":{"local":{"command":"server"}}})");
    mcp->findChild<QPushButton *>("PreviewMcpImport")->click();
    controller.settingsRpcFinished("mcp.list", oldListSerial, "{}", ""); // superseded list cannot erase this form
    if (reference->text() != "secret-reference" || apply->isEnabled()) return 4;
    controller.settingsRpcFinished("mcp.import_preview", mcp->requestSerialForTest(),
        R"({"servers":[{"id":"local","name":"local","transport":{"type":"stdio","command":"server","args":[],"env":{},"env_secrets":{}},"enabled":true,"default_workspace_enabled":true,"enabled_backends":["claude"]}],"issues":[],"collisions":[]})", "");
    if (!apply->isEnabled()) return 5;
    import->insertPlainText(" "); // changing the document invalidates the preview
    if (apply->isEnabled()) return 6;
    mcp->findChild<QPushButton *>("TestMcpServer")->click();
    const qint64 staleTestSerial = mcp->requestSerialForTest();
    auto *mcpList = mcp->findChild<QComboBox *>("McpServerList");
    mcpList->setCurrentIndex(1);
    if (labelHidden("Command:") || !labelHidden("Bearer reference:")) return 17;
    const QString selectedStatus = mcp->findChild<QLabel *>("McpStatus")->text();
    controller.settingsRpcFinished("mcp.test", staleTestSerial,
        R"({"status":{"server_id":"jira","state":"connected"},"tool_names":["stale_tool"]})", "");
    if (mcp->findChild<QLabel *>("McpStatus")->text() != selectedStatus) return 13;
    mcpList->setCurrentIndex(0);
    if (!mcp->findChild<QPushButton *>("ConnectMcpServer")->isHidden()) return 18;
    auto *authMode = mcp->findChild<QComboBox *>("McpAuth");
    authMode->setCurrentIndex(authMode->findData("oauth"));
    if (mcp->findChild<QPushButton *>("ConnectMcpServer")->isHidden()) return 19;
    mcp->findChild<QPushButton *>("ConnectMcpServer")->click();
    QPointer<QTcpServer> listener = mcp->findChild<QTcpServer *>();
    if (!listener || !listener->isListening()) return 7;
    mcp->findChild<QPushButton *>("DisconnectMcpServer")->click();
    if (listener->isListening()) return 8;
    const QString directory = qEnvironmentVariable("BS_SETTINGS_SCREENSHOTS");
    if (!directory.isEmpty()) {
        if (!QDir().mkpath(directory)) return 9;
        for (const auto choice : {theme::Choice::Light, theme::Choice::Dark}) {
            theme::apply(*qApp, choice);
            for (const QSize size : {QSize(900, 700), QSize(640, 600)}) {
                dialog.resize(size); dialog.show(); QCoreApplication::processEvents();
                auto *scroll = dialog.findChild<QScrollArea *>();
                for (auto page : {static_cast<QWidget *>(provider), static_cast<QWidget *>(mcp)}) {
                    scroll->ensureWidgetVisible(page); QCoreApplication::processEvents();
                    const QString file = QString("/settings-%1-%2-%3.png")
                        .arg(choice == theme::Choice::Light ? "light" : "dark")
                        .arg(size.width()).arg(page == provider ? "providers" : "mcp");
                    if (!dialog.grab().save(directory + file)) return 10;
                }
                if (scroll->horizontalScrollBar()->maximum() != 0) return 11;
            }
        }
        dialog.hide();
        theme::apply(*qApp, theme::choiceFromName(controller.theme()));
    }
    agentchoices::setDescriptors(QJsonDocument::fromJson(controller.backendDescriptorsJson().toUtf8()).array());
    agentchoices::resetModelsForTest();
    return 0;
}

#endif // BS_WIDGET_TESTS
