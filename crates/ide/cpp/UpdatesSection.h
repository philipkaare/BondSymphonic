#pragma once
#include <QGroupBox>

class QCheckBox;
class QLabel;
class QProgressBar;
class QPushButton;
class UpdateController;

/// Settings > General > Updates: what version this is, what the updater is
/// doing, the automatic/manual switch, Check now, and Restart to update once
/// a download is ready.
///
/// `updates` may be null (a widget test builds the dialog with no
/// controller); the section then says updates are unavailable and offers
/// nothing.
class UpdatesSection : public QGroupBox {
    Q_OBJECT
public:
    explicit UpdatesSection(UpdateController* updates, QWidget* parent = nullptr);

    /// Stores the automatic/manual choice. `SettingsDialog::accept` calls it,
    /// so Cancel leaves the setting as it was, like every other field.
    void apply();

signals:
    /// Restart to update was clicked. The dialog closes itself first and
    /// then asks the controller, so the window is not closed under it.
    void restartClicked();

private:
    void refresh();

    UpdateController* m_updates;
    QLabel* m_status = nullptr;
    QProgressBar* m_progress = nullptr;
    QCheckBox* m_automatic = nullptr;
    QPushButton* m_check = nullptr;
    QPushButton* m_restart = nullptr;
};
