#pragma once
#include <QDialog>

class AppController;
class QComboBox;
class QLineEdit;
class QPushButton;

/// The application settings: the Anthropic API key, and what permission mode a
/// new agent starts on.
///
/// The key is write-only here. The dialog can put one in the Windows
/// credential store and take one out again, but it never reads one back: the
/// field shows a placeholder saying a key is stored, not the key. Nothing in
/// this class keeps the text after `accept` has handed it to the controller.
class SettingsDialog : public QDialog {
    Q_OBJECT
public:
    explicit SettingsDialog(AppController* controller, QWidget* parent = nullptr);

    void accept() override;

private:
    /// Points the field's placeholder at whatever the credential store now
    /// says, and greys "Remove key" out when there is nothing to remove.
    void refreshKeyState();
    void removeKey();

    AppController* m_controller;
    QLineEdit* m_apiKey = nullptr;
    QPushButton* m_removeKey = nullptr;
    QComboBox* m_permissionMode = nullptr;
};
