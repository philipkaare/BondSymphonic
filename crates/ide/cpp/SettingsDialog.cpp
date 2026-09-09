#include "SettingsDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPushButton>
#include <QVBoxLayout>

namespace {

/// The words the daemon validates `--permission-mode` against; it rejects
/// anything else, so they are spelled exactly as the CLI spells them.
const char* kPermissionModes[] = { "default", "acceptEdits", "plan", "dontAsk" };

} // namespace

SettingsDialog::SettingsDialog(AppController* controller, QWidget* parent)
    : QDialog(parent), m_controller(controller) {
    setWindowTitle("Settings");
    setModal(true);

    auto* outer = new QVBoxLayout(this);
    auto* form = new QFormLayout();
    outer->addLayout(form);

    auto* keyRow = new QHBoxLayout();
    m_apiKey = new QLineEdit(this);
    // Password echo, and never populated from the store: the field is a way in
    // for a new key, not a way to read the one that is already there.
    m_apiKey->setEchoMode(QLineEdit::Password);
    m_removeKey = new QPushButton("Remove key", this);
    keyRow->addWidget(m_apiKey, 1);
    keyRow->addWidget(m_removeKey, 0);
    form->addRow("Anthropic API key:", keyRow);

    auto* note = new QLabel(
        "Used only when Claude Code has no login of its own. Leave empty to keep the stored key.",
        this);
    note->setWordWrap(true);
    note->setEnabled(false);
    form->addRow(QString(), note);

    m_permissionMode = new QComboBox(this);
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
