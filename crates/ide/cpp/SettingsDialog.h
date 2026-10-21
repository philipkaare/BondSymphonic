#pragma once
#include <QDialog>
#include <QMap>
#include <QJsonObject>
#include <QJsonArray>

class AppController;
class SetupPage;
class QCheckBox;
class QComboBox;
class QLabel;
class QLineEdit;
class QPushButton;
class QScrollArea;
class QSpinBox;
class QWidget;

/// Creates and opens `directory`, OpenCode's Windows user configuration
/// folder, warning over `parent` when either fails. The one action behind
/// Settings' "Open config folder" and an OpenCode notice's button.
void openOpenCodeConfigFolder(QWidget* parent, const QString& directory);

/// The application settings: setup and logins, the Anthropic API key, what
/// permission mode a new agent starts on, and the palette and text sizes the
/// application wears.
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
  explicit SettingsDialog(AppController *controller, QWidget *parent = nullptr,
                          const QJsonArray &descriptors = QJsonArray());

    /// Scrolls the Setup section into view and gives it the focus. What
    /// `MainWindow::showSetupPage` calls, so the status-bar "Set up…" link and
    /// the first-run check land on the section they are about rather than at
    /// the top of a form.
    void revealSetup();

    void accept() override;

    /// Puts the palette and the text sizes back to the stored choice. Both
    /// apply as they are picked, so a dialog dismissed without this would leave
    /// the window wearing a look the user just declined to keep.
    void reject() override;

private:
    /// Points the field's placeholder at whatever the credential store now
    /// says, and greys "Remove key" out when there is nothing to remove.
  void refreshDefaults();

    AppController* m_controller;
    /// The whole setup page, hosted as this dialog's first section.
    SetupPage* m_setup = nullptr;
    /// The scroller the sections live in, so `revealSetup` can bring the first
    /// one back into view in a dialog the user has scrolled.
    QScrollArea* m_scroll = nullptr;
  struct BackendControls {
      QString label;
      QCheckBox *enabled;
      QLineEdit *key;
      QPushButton *removeKey;
      QComboBox *model;
      QComboBox *mode;
  };
  QMap<QString, BackendControls> m_backends;
  QJsonObject m_settings;
  QComboBox *m_defaultBackend = nullptr;
    /// Whether the transcripts show the turn cost and the agent's own system
    /// lines. Applied to every open pane when this dialog closes, which is the
    /// window's job: this dialog knows the setting and no transcripts.
    QCheckBox* m_showMeta = nullptr;
    QComboBox* m_theme = nullptr;
    /// The interface and code text sizes, in points.
    QSpinBox* m_uiFontSize = nullptr;
    QSpinBox* m_codeFontSize = nullptr;

    /// The OpenCode tab's list of model servers: built empty and hidden, and
    /// shown when the daemon answers `opencode.list_providers` -- which a
    /// daemon that does not offer it is never asked, so there it stays hidden.
    QWidget* buildOpenCodeProviders(QWidget* parent);
    /// Rebuilds the rows from an `OpenCodeProvidersResult` (or `{"error"}`).
    void showOpenCodeProviders(const QString& json);
    /// Puts one connection test's answer under its provider's row.
    void showOpenCodeProviderTest(const QString& provider, const QString& json);
    struct ProviderRow {
        QPushButton* test;
        QLabel* result;
    };
    QWidget* m_openCodeProviders = nullptr;
    QWidget* m_openCodeProviderRows = nullptr;
    QLabel* m_openCodeProvidersNote = nullptr;
    QMap<QString, ProviderRow> m_providerRows;
};
