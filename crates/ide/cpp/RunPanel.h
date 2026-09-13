#pragma once
#include <QJsonObject>
#include <QPointer>
#include <QString>
#include <QWidget>

class AppController;
class QAction;
class QActionGroup;
class QComboBox;
class QFrame;
class QLabel;
class QPlainTextEdit;
class QPushButton;
class QSpinBox;
class QVBoxLayout;
class RunPanelModel;

namespace runpanel {

/// Appends one item per run configuration in `configsJson` to `combo`, without
/// clearing what is already there.
///
/// Takes either shape the two sources use: the bare array
/// `RunPanelModel::configsJson` answers, or the `DetectRunConfigsResult` object
/// `AppController::runConfigsDetected` carries, whose `configs` member is that
/// same array.
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
    /// `controller` is only ever asked for the port override this workspace and
    /// configuration were last given, and told when the user changes it. It may
    /// be null in a test that builds the panel on its own.
    RunPanel(RunPanelModel* model, AppController* controller, QWidget* parent = nullptr);

    /// What the panel can do, as actions it owns.
    ///
    /// Every button in the panel is the face of one of these rather than the
    /// other way round, so the Run menu offers the same objects: a Stop that is
    /// dead because nothing is running is dead in both places for one reason,
    /// and a panel nobody has opened is not a reason the menu cannot drive a
    /// run. Three of them -- restart, and the two that act on the output -- have
    /// no button at all and exist only here.
    QAction* runAction() const;
    QAction* stopAction() const;
    QAction* restartAction() const;
    QAction* openAction() const;
    QAction* clearOutputAction() const;
    QAction* copyOutputAction() const;
    QAction* allowHostAction() const;

    /// One checkable action per detected configuration, in the combo's order
    /// and in one exclusive group, so exactly one of them is the one a Run will
    /// start. Rebuilt whenever the combo is, which is what
    /// [`configurationsChanged`] announces.
    QList<QAction*> configurationActions() const;

#if defined(BS_WIDGET_TESTS)
    /// Fills the configuration list from `configsJson` as a `configsChanged`
    /// from the model would.
    ///
    /// The one thing a check cannot reach any other way: the list is the
    /// daemon's answer to a detection, and an offscreen panel has no daemon to
    /// answer it. Everything downstream of the list -- the combo, the checkable
    /// actions, the menu they are inserted into -- is the real path.
    void noteConfigsForTest(const QString& configsJson);
#endif

signals:
    /// The configuration list has been rebuilt, so anything mirroring it --
    /// the Run menu -- must ask for [`configurationActions`] again. The
    /// actions from before the signal have been deleted.
    void configurationsChanged();

private:
    void buildTopRow(QVBoxLayout* outer);
    /// Builds the actions above. First, before the buttons: each of them binds
    /// to the action it is the face of.
    void buildActions();
    /// Rebuilds the checkable configuration actions from the combo and emits
    /// [`configurationsChanged`].
    void rebuildConfigActions();
    /// Ticks the configuration action for what the model says is selected, and
    /// unticks the rest.
    void syncConfigActionChecks();
    /// Offers `name` to the model and takes back whatever it settled on. What
    /// both the combo and the menu's configuration entries go through.
    void selectConfig(const QString& name);
    /// Starts the selected configuration. The Run action's body.
    void startRun();
    /// Stops what is running and starts it again once the daemon says it has
    /// stopped. A run cannot be replaced in one call: the model refuses a
    /// second run of a configuration that is still alive, so the start has to
    /// wait for the stop to land.
    void onRestart();
    void buildToast(QVBoxLayout* outer);
    void buildLog(QVBoxLayout* outer);
    void connectModel();

    /// Rebuilds the combo from `configsJson()` and puts it back on whatever the
    /// model says is selected.
    void rebuildConfigs();
    /// Shows what the daemon had to complain about in `bondsymphonic.toml`:
    /// every complaint as the combo's tooltip, and one line under the row --
    /// the complaint itself when there is one, "N problems with
    /// bondsymphonic.toml" when there are more. Both go away when the file is
    /// clean.
    void updateWarnings();
    /// Moves the combo onto `selectedConfig` without telling the model about a
    /// selection it made itself.
    void syncSelection();
    /// The user picked an entry: offer it to the model and take back whatever
    /// it settled on, which for a refused name is the previous selection.
    void onConfigActivated(int index);
    /// Points the port box at the selected configuration: the override the
    /// workspace already has, or the configuration's own port as a hint, and
    /// editable only when the daemon guessed that port.
    ///
    /// A configured port is the repository's decision and is shown greyed
    /// rather than hidden, so a user who wonders why they cannot change it can
    /// see the number they would be arguing with.
    void syncPort();
    /// The user moved the box: record the override, or clear it at 0.
    void onPortChanged(int port);
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
    /// The `allowHost` for `host` failed: re-arm its buttons and say why, in
    /// the toast itself.
    ///
    /// Bound to the model's `denialFailed` rather than to the model-wide
    /// `busy`, so an unrelated `run.start` finishing mid-allow cannot re-arm an
    /// offer whose answer is still out.
    void onDenialFailed(const QString& host, const QString& message);
    void onAllowClicked();
    void onDismissClicked();
    void openUrl();

    QPointer<RunPanelModel> m_model;
    QPointer<AppController> m_controller;
    QComboBox* m_configs = nullptr;
    /// The port the next run of the selected configuration uses. 0 is "the
    /// configuration's own", shown as `auto`.
    QSpinBox* m_port = nullptr;
    /// Set while `syncPort` writes into the box, so the `valueChanged` it
    /// provokes is not recorded as a change the user made.
    bool m_syncingPort = false;
    QAction* m_runAction = nullptr;
    QAction* m_stopAction = nullptr;
    QAction* m_restartAction = nullptr;
    QAction* m_openAction = nullptr;
    QAction* m_clearOutputAction = nullptr;
    QAction* m_copyOutputAction = nullptr;
    QAction* m_allowHostAction = nullptr;
    /// The configuration actions, exclusive: picking one unpicks the last.
    QActionGroup* m_configActions = nullptr;
    /// Set between a restart's stop and the start that follows it. Cleared by
    /// the start, and by a failure, so a stop that never lands cannot make an
    /// unrelated idle moment start a run nobody asked for.
    bool m_restartPending = false;
    QPushButton* m_start = nullptr;
    QPushButton* m_stop = nullptr;
    QLabel* m_glyph = nullptr;
    QLabel* m_url = nullptr;
    QPushButton* m_open = nullptr;
    QLabel* m_status = nullptr;
    /// What the daemon could not load out of `bondsymphonic.toml`, or hidden
    /// when it had nothing to say. Kept apart from `m_status`, which is the
    /// run's own news.
    QLabel* m_warnings = nullptr;
    QFrame* m_toast = nullptr;
    QLabel* m_toastText = nullptr;
    QPushButton* m_allow = nullptr;
    QPushButton* m_dismiss = nullptr;
    QPlainTextEdit* m_log = nullptr;

    /// The host the toast is offering, or empty when it is down. The queue is
    /// the model's, one host at a time; this is only the one on screen.
    QString m_deniedHost;
    /// Why the last `allowHost` for the host on screen failed, or empty. Shown
    /// in the toast, so the reason sits with the offer it belongs to.
    QString m_denialError;
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
