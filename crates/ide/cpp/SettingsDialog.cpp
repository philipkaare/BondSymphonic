#include "SettingsDialog.h"
#include "SetupPage.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
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

namespace {

/// The words the daemon validates `--permission-mode` against; it rejects
/// anything else, so they are spelled exactly as the CLI spells them.
const char* kPermissionModes[] = { "default", "acceptEdits", "plan", "dontAsk" };

/// The size the dialog opens at. It hosts a terminal now, so the old
/// three-field height would start every login in a pane a few lines tall.
constexpr int kDialogWidth = 900;
constexpr int kDialogHeight = 700;

} // namespace

SettingsDialog::SettingsDialog(AppController* controller, QWidget* parent)
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
    m_setup->setEmbedded(true);
    setupLayout->addWidget(m_setup);
    bodyLayout->addWidget(setupBox, 1);

    auto* agentBox = new QGroupBox("Agents", body);
    auto* form = new QFormLayout(agentBox);
    bodyLayout->addWidget(agentBox);

    auto* keyRow = new QHBoxLayout();
    m_apiKey = new QLineEdit(agentBox);
    // Password echo, and never populated from the store: the field is a way in
    // for a new key, not a way to read the one that is already there.
    m_apiKey->setEchoMode(QLineEdit::Password);
    m_removeKey = new QPushButton("Remove key", agentBox);
    keyRow->addWidget(m_apiKey, 1);
    keyRow->addWidget(m_removeKey, 0);
    form->addRow("Anthropic API key:", keyRow);

    auto* note = new QLabel(
        "Used only when Claude Code has no login of its own. Leave empty to keep the stored key.",
        agentBox);
    note->setWordWrap(true);
    note->setEnabled(false);
    form->addRow(QString(), note);

    m_permissionMode = new QComboBox(agentBox);
    for (const char* mode : kPermissionModes) {
        m_permissionMode->addItem(QString::fromUtf8(mode), QString::fromUtf8(mode));
    }
    const int index = m_permissionMode->findData(m_controller->defaultPermissionMode());
    if (index >= 0) {
        m_permissionMode->setCurrentIndex(index);
    }
    form->addRow("Default permission mode:", m_permissionMode);

    auto* buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    outer->addWidget(buttons);

    QObject::connect(buttons, &QDialogButtonBox::accepted, this, &SettingsDialog::accept);
    QObject::connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    QObject::connect(m_removeKey, &QPushButton::clicked, this, &SettingsDialog::removeKey);

    refreshKeyState();
}

void SettingsDialog::revealSetup() {
    if (m_setup == nullptr) {
        return;
    }
    m_scroll->ensureWidgetVisible(m_setup);
    m_setup->setFocus();
}

void SettingsDialog::refreshKeyState() {
    const bool stored = m_controller->apiKeySet();
    m_apiKey->setPlaceholderText(stored ? "stored in the Windows credential store"
                                        : "sk-ant-…");
    m_removeKey->setEnabled(stored);
}

void SettingsDialog::removeKey() {
    if (!m_controller->clearApiKey()) {
        QMessageBox::warning(this, "Settings", "The key could not be removed from the credential "
                                               "store. Windows Credential Manager has the detail.");
    }
    m_apiKey->clear();
    refreshKeyState();
}

void SettingsDialog::accept() {
    m_controller->setDefaultPermissionMode(m_permissionMode->currentData().toString());
    // An empty field means "leave the stored key alone", which is what makes
    // the placeholder honest: a dialog opened to change the permission mode
    // must not wipe the key on the way out.
    const QString key = m_apiKey->text();
    if (!key.isEmpty() && !m_controller->setApiKey(key)) {
        QMessageBox::warning(this, "Settings", "The key could not be stored in the Windows "
                                               "credential store, so it has not been saved.");
        return;
    }
    m_apiKey->clear();
    QDialog::accept();
}
