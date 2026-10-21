#include "SettingsDialog.h"
#include "AgentChoices.h"
#include "SetupPage.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QApplication>
#include <QCheckBox>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QDesktopServices>
#include <QDir>
#include <QFormLayout>
#include <QFrame>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPushButton>
#include <QScrollArea>
#include <QVBoxLayout>
#include <QJsonDocument>
#include <QJsonArray>
#include <QTabWidget>
#include <QSignalBlocker>
#include <QSpinBox>
#include <QTemporaryDir>
#include <QUrl>

namespace {

/// The size the dialog opens at. It hosts a terminal now, so the old
/// three-field height would start every login in a pane a few lines tall.
constexpr int kDialogWidth = 900;
constexpr int kDialogHeight = 700;

} // namespace

SettingsDialog::SettingsDialog(AppController *controller, QWidget *parent,
                               const QJsonArray &suppliedDescriptors)
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
    // The terminal opens in the Setup section, far above a backend tab's
    // buttons, so whichever button started it brings it into view.
    QObject::connect(m_setup, &SetupPage::actionStarted, this, [this] { revealSetup(); });

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
        auto *enabled = new QCheckBox("Enabled", page);
        enabled->setObjectName("BackendEnabled_" + id);
        enabled->setChecked(settings.value("enabled").toBool());
        backendForm->addRow(enabled);
        // One button per pair -- Install or Uninstall, Sign in or Sign out --
        // saying what it will do now. The Setup page owns their state, so they
        // follow its answer and go dead with its own buttons.
        backendForm->addRow("Sign-in and installation:", m_setup->backendActions(descriptor, page));
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
    auto* keyRow = new QHBoxLayout();
        auto *key = new QLineEdit(page);
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
                          : id == "opencode" ? "Used for the OpenAI provider. Other providers use OpenCode sign-in or the Windows user configuration below, which is also what decides which model servers agents can reach. Leave empty to keep the stored key."
                                             : "Used when Claude has no login. Leave empty to keep the stored key.",
            page);
        keyNote->setWordWrap(true);
        backendForm->addRow(keyNote);
        if (id == "opencode") {
            const QString configDirectory = m_controller->opencodeConfigDirectory();
            auto *configPath = new QLabel(configDirectory, page);
            configPath->setObjectName(QStringLiteral("OpenCodeConfigDirectory"));
            configPath->setTextFormat(Qt::PlainText);
            configPath->setTextInteractionFlags(Qt::TextSelectableByMouse);
            configPath->setWordWrap(true);
            backendForm->addRow("Windows user configuration:", configPath);
            auto *configHelp = new QLabel(
                "Edit opencode.json or opencode.jsonc in this folder (legacy config.json is also supported). Changes apply to "
                "newly started agents; restart an existing agent to use the updated configuration.",
                page);
            configHelp->setObjectName(QStringLiteral("OpenCodeConfigHelp"));
            configHelp->setWordWrap(true);
            backendForm->addRow(configHelp);
            auto *openConfigFolder = new QPushButton("Open config folder", page);
            openConfigFolder->setObjectName(QStringLiteral("OpenCodeOpenConfigFolder"));
            backendForm->addRow(openConfigFolder);
            QObject::connect(openConfigFolder, &QPushButton::clicked, this,
                             [this, configDirectory] {
                if (configDirectory.isEmpty() || !QDir().mkpath(configDirectory)) {
                    QMessageBox::warning(this, "Settings",
                                         "The OpenCode config folder could not be created.");
                    return;
                }
                if (!QDesktopServices::openUrl(QUrl::fromLocalFile(configDirectory))) {
                    QMessageBox::warning(this, "Settings",
                                         "The OpenCode config folder could not be opened.");
                }
            });
        }
        const QString label = descriptor.value("label").toString(id);
        m_backends.insert(id, {label, enabled, key, remove, model, mode});
        tabs->addTab(page, label);
        QObject::connect(enabled, &QCheckBox::toggled, this, [this] { refreshDefaults(); });
        QObject::connect(m_controller, &AppController::backendSettingsChanged, this,
                         [this, id, enabled, refreshKey,
                          persistedEnabled = settings.value("enabled").toBool()]() mutable {
                             const auto saved = QJsonDocument::fromJson(
                                 m_controller->backendSettingsJson().toUtf8()).object()
                                                    .value("backends").toObject()
                                                    .value(id).toObject();
                             const bool nowEnabled = saved.value("enabled").toBool();
                             // Authentication can finish while Settings is open. Follow
                             // the saved transition without replacing unsaved model edits
                             // or resetting a checkbox on every model-list notification.
                             if (nowEnabled && !persistedEnabled) enabled->setChecked(true);
                             persistedEnabled = nowEnabled;
                             refreshKey();
                         });
        QObject::connect(m_controller, &AppController::backendModelsChecked, this,
                         [id, model](const QString &backend, const QString &, qint64) {
                             if (backend == id) {
                                 const QSignalBlocker blocker(model);
                                 agentchoices::fillModelCombo(
                                     model, agentchoices::modelComboSelection(model), id);
                             }
                         });
        m_controller->refreshBackendModels(id);
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
    // The text sizes preview the same way, for the same reason: a size is
    // chosen by looking at it.
    const auto makeSize = [lookBox](const char* name, int value, const QString& tip) {
        auto* box = new QSpinBox(lookBox);
        box->setObjectName(QLatin1String(name));
        box->setRange(theme::kMinFontPointSize, theme::kMaxFontPointSize);
        box->setSuffix(QStringLiteral(" pt"));
        box->setValue(value);
        box->setToolTip(tip);
        return box;
    };
    m_uiFontSize = makeSize("SettingsUiFontSize", m_controller->uiFontSize(),
                            QStringLiteral("Menus, labels, buttons and the conversation."));
    m_codeFontSize = makeSize(
        "SettingsCodeFontSize", m_controller->codeFontSize(),
        QStringLiteral("The editor, diffs, terminals, Run output and a tool call's input and "
                       "result."));
    lookForm->addRow("Interface text:", m_uiFontSize);
    lookForm->addRow("Code text:", m_codeFontSize);
    const auto previewFonts = [this](int) {
        theme::applyFonts(*qApp, m_uiFontSize->value(), m_codeFontSize->value());
    };
    QObject::connect(m_uiFontSize, &QSpinBox::valueChanged, this, previewFonts);
    QObject::connect(m_codeFontSize, &QSpinBox::valueChanged, this, previewFonts);

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
        entry.insert("default_model", agentchoices::modelComboSelection(it->model));
        entry.insert("default_permission_mode", it->mode->currentData().toString());
        const QString key = it->key->text().trimmed();
        if (!key.isEmpty() && !m_controller->setBackendApiKey(it.key(), key)) {
            QMessageBox::warning(this, "Settings", "The API key could not be stored.");
            return;
        }
        // Read after the key is stored: the controller ticks the box when the key
        // enables a backend the user has not switched off.
        entry.insert("enabled", it->enabled->isChecked());
        entries.insert(it.key(), entry);
        it->key->clear();
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
    m_controller->setFontSizes(m_uiFontSize->value(), m_codeFontSize->value());
    QDialog::accept();
}

void SettingsDialog::reject() {
    // The stored word, not the combo's: the combo is what the user was trying
    // out, and Cancel is them saying they would rather not keep it.
    theme::apply(*qApp, theme::choiceFromName(m_controller->theme()));
    // The stored sizes likewise -- and only when a preview moved them, since
    // putting them back re-sizes every widget in the application.
    const int ui = m_controller->uiFontSize();
    const int code = m_controller->codeFontSize();
    if (m_uiFontSize->value() != ui || m_codeFontSize->value() != code) {
        theme::applyFonts(*qApp, ui, code);
    }
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

/// Settings offers the same four modes the rest of the IDE does.
///
/// This dropdown had its own array, and that array had gone stale: it still
/// held `default`, the spelling the IDE retired, and `dontAsk`, which denies in
/// silence. A user who picked either from here was choosing a behaviour for
/// every agent they would create afterwards out of a list nothing else in the
/// application agreed with.
extern "C" std::int32_t bs_widget_test_settings_offers_the_one_permission_list() {
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
    descriptors.append(QJsonObject{{"id", "opencode"}, {"label", "OpenCode"},
        {"default_permission_mode", "ask"},
        {"permission_modes", QJsonArray{QJsonObject{{"id", "ask"}, {"label", "Ask"}},
                                         QJsonObject{{"id", "yolo"}, {"label", "YOLO"}}}},
        {"setup_actions", QJsonArray{"install_opencode", "opencode_login", "opencode_logout"}},
        {"credential_label", "OpenAI API key"}});
    QTemporaryDir settingsDir;
    if (!settingsDir.isValid()) return 26;
    const auto priorPath = qgetenv("BS_SETTINGS_PATH");
    struct RestorePath {
        QByteArray value;
        ~RestorePath() { if (value.isNull()) qunsetenv("BS_SETTINGS_PATH"); else qputenv("BS_SETTINGS_PATH", value); }
    } restorePath{priorPath};
    qputenv("BS_SETTINGS_PATH", (settingsDir.path() + "/settings.json").toUtf8());
    if (!controller.saveBackendSettings(QStringLiteral(R"({"backends":{"opencode":{"enabled":false,"default_permission_mode":"ask"}},"default_backend":"claude"})")))
        return 27;
    SettingsDialog three(&controller, nullptr, descriptors);
    auto *openCodeEnabled = three.findChild<QCheckBox *>("BackendEnabled_opencode");
    auto *openCodeModel = three.findChild<QComboBox *>("BackendModel_opencode");
    auto *openCodeKey = three.findChild<QLineEdit *>("BackendKey_opencode");
    if (!openCodeEnabled || openCodeEnabled->isChecked() || !openCodeModel ||
        !openCodeKey || openCodeKey->echoMode() != QLineEdit::Password) return 28;
    // Users need the config source for this Windows account, with a folder
    // action available only on the OpenCode tab. Do not launch Explorer here.
    auto *configDirectory = three.findChild<QLabel *>("OpenCodeConfigDirectory");
    auto *openConfigFolder = three.findChild<QPushButton *>("OpenCodeOpenConfigFolder");
    auto *configHelp = three.findChild<QLabel *>("OpenCodeConfigHelp");
    if (!configDirectory || configDirectory->text().isEmpty() ||
        configDirectory->text() != controller.opencodeConfigDirectory() ||
        !openConfigFolder || !configHelp ||
        !configHelp->text().contains("restart", Qt::CaseInsensitive)) return 32;
    if (both.findChild<QPushButton *>("OpenCodeOpenConfigFolder")) return 33;
    openCodeModel->setEditText("local/my-model");
    if (!controller.saveBackendSettings(QStringLiteral(R"({"backends":{"opencode":{"enabled":true,"default_permission_mode":"ask"}},"default_backend":"claude"})")))
        return 29;
    if (!openCodeEnabled->isChecked() || agentchoices::modelComboSelection(openCodeModel) != "local/my-model")
        return 30;
    if (three.findChild<QComboBox *>("DefaultBackend")->findData("opencode") < 0) return 31;
    return 0;
}

/// A backend tab offers one button per pair, saying what it will do now.
///
/// The tab used to list every action its descriptor named -- Install, Sign in
/// and Sign out side by side, all live whatever was installed or signed in.
/// Now the install button is Install or Uninstall and the sign-in button Sign
/// in or Sign out, from the Setup page's last answer; an uninstall the daemon
/// did not list is never offered; and nothing is live before there is an
/// answer or while an action's terminal is in hand.
extern "C" std::int32_t bs_widget_test_settings_backend_tab_offers_one_button_per_pair() {
    AppController controller;
    const QJsonArray descriptors{
        QJsonObject{{"id", "codex"},
                    {"label", "Codex"},
                    {"default_permission_mode", "never"},
                    {"prerequisite_names", QJsonArray{"codex", "codex_auth"}},
                    {"setup_actions",
                     QJsonArray{"install_codex", "uninstall_codex", "codex_login", "codex_logout"}},
                    {"credential_label", "OpenAI API key"}},
        // A daemon that does not list an uninstall for OpenCode.
        QJsonObject{{"id", "opencode"},
                    {"label", "OpenCode"},
                    {"default_permission_mode", "ask"},
                    {"prerequisite_names", QJsonArray{"opencode", "opencode_auth"}},
                    {"setup_actions",
                     QJsonArray{"install_opencode", "opencode_login", "opencode_logout"}},
                    {"credential_label", "OpenAI API key"}}};
    SettingsDialog dialog(&controller, nullptr, descriptors);
    auto* codexInstall = dialog.findChild<QPushButton*>("BackendInstall_codex");
    auto* codexAuth = dialog.findChild<QPushButton*>("BackendAuth_codex");
    auto* openInstall = dialog.findChild<QPushButton*>("BackendInstall_opencode");
    auto* openAuth = dialog.findChild<QPushButton*>("BackendAuth_opencode");
    if (!codexInstall || !codexAuth || !openInstall || !openAuth) {
        return 1;
    }
    // No answer yet: nothing to act on.
    if (codexInstall->isEnabled() || codexAuth->isEnabled()) {
        return 2;
    }
    // Every other button on the tabs is the dialog's own, not a setup action.
    for (QPushButton* button : dialog.findChildren<QPushButton*>()) {
        if (button->text() == QLatin1String("Sign out of Codex") && button != codexAuth) {
            return 3;
        }
    }

    controller.prereqsChecked(QStringLiteral(
        R"([{"name":"codex","ok":false,"detail":"missing"},{"name":"codex_auth","ok":false,"detail":"none"},)"
        R"({"name":"opencode","ok":true,"detail":"installed"},{"name":"opencode_auth","ok":true,"detail":"signed in"}])"));
    if (codexInstall->text() != QLatin1String("Install Codex") || !codexInstall->isEnabled()) {
        return 4;
    }
    // Signing in to a CLI that is not there is offered, dead, saying why.
    if (codexAuth->text() != QLatin1String("Sign in to ChatGPT") || codexAuth->isEnabled() ||
        !codexAuth->toolTip().contains("Install")) {
        return 5;
    }
    // Installed, with no uninstall listed: no install button at all.
    if (!openInstall->isHidden()) {
        return 6;
    }
    if (openAuth->text() != QLatin1String("Sign out of OpenCode") || !openAuth->isEnabled()) {
        return 7;
    }

    controller.prereqsChecked(QStringLiteral(
        R"([{"name":"codex","ok":true,"detail":"installed"},{"name":"codex_auth","ok":true,"detail":"signed in"},)"
        R"({"name":"opencode","ok":false,"detail":"missing"},{"name":"opencode_auth","ok":false,"detail":"none"}])"));
    if (codexInstall->text() != QLatin1String("Uninstall Codex") || !codexInstall->isEnabled() ||
        codexInstall->isHidden()) {
        return 8;
    }
    if (codexAuth->text() != QLatin1String("Sign out of Codex") || !codexAuth->isEnabled()) {
        return 9;
    }
    if (openInstall->isHidden() || openInstall->text() != QLatin1String("Install OpenCode")) {
        return 10;
    }

    // A click runs the action the button says, and the tab goes dead with
    // the rows while it is in hand.
    auto* setup = dialog.findChild<SetupPage*>();
    if (setup == nullptr) {
        return 11;
    }
    setup->setPtyCloser([](const QString&) {});
    QString started;
    QObject::connect(setup, &SetupPage::actionStarted,
                     [&started](const QString& action) { started = action; });
    codexAuth->click();
    if (started != QLatin1String("codex_logout") || codexInstall->isEnabled() ||
        codexAuth->isEnabled() || openInstall->isEnabled()) {
        return 12;
    }
    return 0;
}

/// Settings offers both text sizes, in the bounds the IDE lays out, and they
/// apply as they are changed and go back on Cancel -- the same as the theme.
extern "C" std::int32_t bs_widget_test_settings_offers_the_text_sizes() {
    const int uiBefore = QApplication::font().pointSize();
    const int codeBefore = theme::codePointSizeSlot();
    std::int32_t failure = 0;
    {
        AppController controller;
        SettingsDialog dialog(&controller);
        auto* ui = dialog.findChild<QSpinBox*>(QStringLiteral("SettingsUiFontSize"));
        auto* code = dialog.findChild<QSpinBox*>(QStringLiteral("SettingsCodeFontSize"));
        if (ui == nullptr || code == nullptr) {
            failure = 1;
        } else if (ui->minimum() != theme::kMinFontPointSize ||
                   ui->maximum() != theme::kMaxFontPointSize ||
                   code->minimum() != theme::kMinFontPointSize ||
                   code->maximum() != theme::kMaxFontPointSize) {
            failure = 2;
        } else if (ui->value() != controller.uiFontSize() ||
                   code->value() != controller.codeFontSize()) {
            failure = 3;
        } else {
            // A preview: picked sizes are worn at once, by the dialog too.
            const int pickedUi = controller.uiFontSize() == 20 ? 21 : 20;
            const int pickedCode = controller.codeFontSize() == 22 ? 23 : 22;
            ui->setValue(pickedUi);
            code->setValue(pickedCode);
            if (QApplication::font().pointSize() != pickedUi ||
                theme::codePointSizeSlot() != pickedCode || dialog.font().pointSize() != pickedUi) {
                failure = 4;
            } else {
                // Cancel puts back what is stored, and stores nothing.
                dialog.reject();
                if (QApplication::font().pointSize() != controller.uiFontSize() ||
                    theme::codePointSizeSlot() != controller.codeFontSize()) {
                    failure = 5;
                }
            }
        }
    }
    theme::applyFonts(*qApp, uiBefore, codeBefore);
    return failure;
}

#endif // BS_WIDGET_TESTS
