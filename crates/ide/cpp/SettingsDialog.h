#pragma once
#include <QDialog>

class AppController;
class SetupPage;
class QCheckBox;
class QComboBox;
class QLineEdit;
class QPushButton;
class QScrollArea;

/// The application settings: setup and logins, the Anthropic API key, what
/// permission mode a new agent starts on, and which palette the application
/// wears.
///
/// Setup is the first section, and it is the whole `SetupPage`: the
/// prerequisite rows, their fix buttons, the login terminal and the sign-in
/// link row. This is where logging in to Claude Code and to GitHub happens.
/// It is here rather than under Help because a login is a setting the user
/// comes back to -- a token expires, an account changes -- and Help is where
/// people look for documentation, not for a terminal.
///
/// The key is write-only here. The dialog can put one in the Windows
/// credential store and take one out again, but it never reads one back: the
/// field shows a placeholder saying a key is stored, not the key. Nothing in
/// this class keeps the text after `accept` has handed it to the controller.
class SettingsDialog : public QDialog {
    Q_OBJECT
public:
    explicit SettingsDialog(AppController* controller, QWidget* parent = nullptr);

    /// Scrolls the Setup section into view and gives it the focus. What
    /// `MainWindow::showSetupPage` calls, so the status-bar "Set up…" link and
    /// the first-run check land on the section they are about rather than at
    /// the top of a form.
    void revealSetup();

    void accept() override;

    /// Puts the palette back to the stored choice. The Theme combo applies as
    /// it is picked, so a dialog dismissed without this would leave the window
    /// wearing a palette the user just declined to keep.
    void reject() override;

private:
    /// Points the field's placeholder at whatever the credential store now
    /// says, and greys "Remove key" out when there is nothing to remove.
    void refreshKeyState();
    void removeKey();

    AppController* m_controller;
    /// The whole setup page, hosted as this dialog's first section.
    SetupPage* m_setup = nullptr;
    /// The scroller the sections live in, so `revealSetup` can bring the first
    /// one back into view in a dialog the user has scrolled.
    QScrollArea* m_scroll = nullptr;
    QLineEdit* m_apiKey = nullptr;
    QPushButton* m_removeKey = nullptr;
    QComboBox* m_permissionMode = nullptr;
    /// Whether the transcripts show the turn cost and the agent's own system
    /// lines. Applied to every open pane when this dialog closes, which is the
    /// window's job: this dialog knows the setting and no transcripts.
    QCheckBox* m_showMeta = nullptr;
    QComboBox* m_theme = nullptr;
};
