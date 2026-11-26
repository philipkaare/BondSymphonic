#pragma once
#include <QDialog>

class HostSetup;
class QLabel;
class QPlainTextEdit;
class QProgressBar;
class QPushButton;

/// The first-run setup of the Linux the daemon runs in -- the WSL distro on
/// Windows, the Lima VM on macOS -- made visible.
///
/// Modeless and hidden while the quick check runs: a machine that is already
/// set up never sees it. It appears when there is work to watch (creating,
/// starting or provisioning), a decision to make (installing WSL needs
/// administrator permission), or a failure to read, and goes away by itself
/// once the setup is done.
class HostSetupDialog : public QDialog {
    Q_OBJECT
public:
    explicit HostSetupDialog(HostSetup* setup, QWidget* parent = nullptr);

private:
    void refresh();

    HostSetup* m_setup;
    QLabel* m_message = nullptr;
    QProgressBar* m_busy = nullptr;
    QPlainTextEdit* m_log = nullptr;
    QPushButton* m_installWsl = nullptr;
    QPushButton* m_retry = nullptr;
    QPushButton* m_skip = nullptr;
    QPushButton* m_close = nullptr;
};
