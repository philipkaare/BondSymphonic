#pragma once
#include <QJsonObject>
#include <QPointer>
#include <QString>
#include <QWidget>

class QComboBox;
class QFrame;
class QLabel;
class QPlainTextEdit;
class QPushButton;
class QVBoxLayout;
class RunPanelModel;

namespace runpanel {

/// Appends one item per run configuration in `configsJson` (the array
/// `RunPanelModel::configsJson` and `AppController::runConfigsDetected` both
/// carry) to `combo`, without clearing what is already there.
///
/// The item's text is the configuration's name, with the port spelled out when
/// the daemon only guessed it; its user data is the name, which is what
/// `selectConfig` and `createWorkspaceWithRun` want; and a configuration the
/// daemon marked with a `disabled_reason` is added greyed out with the reason
/// as its tooltip, so it is visible without being offerable.
///
/// The Run panel and the New Agent dialog both call this rather than each
/// spelling out the same rendering, so the list a user picks from before the
/// workspace exists cannot drift from the list they see afterwards.
void appendConfigItems(QComboBox* combo, const QString& configsJson);

} // namespace runpanel

/// The bottom dock's Run tab: which configuration to run, Start and Stop, the
/// bridged URL and a way into the browser, the run's output, and the toast that
/// offers to allow a host the sandbox's proxy blocked.
///
/// It decides nothing. Which configurations exist, which one is selected,
/// whether a run is alive and what its URL is, which lines belong to which run
/// and which blocked host is being offered are all `RunPanelModel`'s, read back
/// through its properties and JSON accessors whenever it says they changed.
/// The panel does not even hold the workspace: `MainWindow` points the model at
/// the active tab, and this repaints from what the model then publishes.
class RunPanel : public QWidget {
    Q_OBJECT
public:
    explicit RunPanel(RunPanelModel* model, QWidget* parent = nullptr);

private:
    void buildTopRow(QVBoxLayout* outer);
    void buildToast(QVBoxLayout* outer);
    void buildLog(QVBoxLayout* outer);
    void connectModel();

    /// Rebuilds the combo from `configsJson()` and puts it back on whatever the
    /// model says is selected.
    void rebuildConfigs();
    /// Moves the combo onto `selectedConfig` without telling the model about a
    /// selection it made itself.
    void syncSelection();
    /// The user picked an entry: offer it to the model and take back whatever
    /// it settled on, which for a refused name is the previous selection.
    void onConfigActivated(int index);
    /// Puts `runId`'s whole output in the log widget, unless it is already
    /// showing it. The empty id empties the widget.
    void showLogOf(const QString& runId);
    void onOutputAppended(const QString& runId, const QString& line);
    /// Start/Stop/Open enablement, the URL label, the state glyph and the
    /// status line under them, all from what the model publishes.
    void updateRow();
    /// The most recent run of the selected configuration, live or finished, or
    /// an empty object when it has never been run. What the glyph and the
    /// status line fall back to once `activeState` has gone empty again.
    QJsonObject lastRunOfSelected() const;
    /// Raises the toast for `host`, which is the head of the model's queue for
    /// the workspace on screen.
    void showToast(const QString& host);
    /// Takes it down. The model says when: the host was allowed or dismissed,
    /// or the panel has moved to a workspace with nothing waiting.
    void hideToast();
    /// Greys the toast's two buttons out while an `allowHost` is on its way, so
    /// the offer cannot be answered twice while it is out.
    void setToastBusy(bool busy);
    void onAllowClicked();
    void onDismissClicked();
    void openUrl();

    QPointer<RunPanelModel> m_model;
    QComboBox* m_configs = nullptr;
    QPushButton* m_start = nullptr;
    QPushButton* m_stop = nullptr;
    QLabel* m_glyph = nullptr;
    QLabel* m_url = nullptr;
    QPushButton* m_open = nullptr;
    QLabel* m_status = nullptr;
    QFrame* m_toast = nullptr;
    QLabel* m_toastText = nullptr;
    QPushButton* m_allow = nullptr;
    QPushButton* m_dismiss = nullptr;
    QPlainTextEdit* m_log = nullptr;

    /// The host the toast is offering, or empty when it is down. The queue is
    /// the model's, one host at a time; this is only the one on screen.
    QString m_deniedHost;
    /// Set when the log widget is showing a workspace that is no longer the
    /// one on screen, so the next repaint refills it even if the run id has
    /// not changed (both workspaces having no run).
    bool m_logStale = false;
    /// The run the log widget is showing, so a line for another run is left in
    /// the model's ring buffer instead of being appended to the wrong log.
    QString m_shownRunId;
    /// The last message from `errorOccurred`, shown until something succeeds.
    QString m_lastError;
};
