#include "SettingsDialog.h"
#include "AgentChoices.h"
#include "SetupPage.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QApplication>
#include <QCheckBox>
#include <QComboBox>
#include <QDialogButtonBox>
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
        auto *actions = new QHBoxLayout();
        for (const auto &actionValue : descriptor.value("setup_actions").toArray()) {
            const QString action = actionValue.toString();
            const QString label = SetupPage::buttonTextFor(action);
            auto *button = new QPushButton(label, page);
            actions->addWidget(button);
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
                          : "Used when Claude has no login. Leave empty to keep the stored key.",
            page);
        keyNote->setWordWrap(true);
        backendForm->addRow(keyNote);
        const QString label = descriptor.value("label").toString(id);
        m_backends.insert(id, {label, enabled, key, remove, model, mode});
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
        entry.insert("default_permission_mode", it->mode->currentData().toString());
        entries.insert(it.key(), entry);
        const QString key = it->key->text().trimmed();
        if (!key.isEmpty() && !m_controller->setBackendApiKey(it.key(), key)) {
            QMessageBox::warning(this, "Settings", "The API key could not be stored.");
            return;
        }
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
    return 0;
}

#endif // BS_WIDGET_TESTS
