#pragma once
#include <QWidget>

class AppController;
class QCloseEvent;
class QLabel;
class QPushButton;
class QTimer;

/// What the IDE shows from process start until it is ready to be used: the
/// logo, the name, the version, one line naming the phase the launch is in,
/// and an indeterminate bar. A top-level of its own, centred on the primary
/// screen and kept above the window. The window is constructed behind it for
/// state restore, then shown when the splash finishes.
///
/// Ready means the controller's connection is `Connected`, the first
/// `system.check_prereqs` has answered and the first `workspace.list` has
/// arrived. It does not wait for each workspace's restore: a cold restore on a
/// Windows drive can take minutes, and the window already lists those as
/// `Creating` and brings the front tab up itself when its restore finishes.
/// The user this is for watched an empty Setup page and a "not logged in"
/// composer gate for ten seconds at launch, took both for the IDE's verdict,
/// and quit; during those seconds the IDE was checking, and now says so.
///
/// The readiness logic is driven entirely by the controller's own signals, so
/// a check can build one over an unstarted `AppController`, set the state and
/// read `isDone` without ever showing it. Nothing here is modal and no nested
/// event loop runs.
class SplashScreen : public QWidget {
    Q_OBJECT
public:
    explicit SplashScreen(AppController* controller, QWidget* parent = nullptr);

    /// Whether the splash has decided it is over: ready, the connection
    /// broken, the cap reached, or a click after preparation. Once true it
    /// stays true, and `finished` has been emitted exactly once.
    bool isDone() const;

signals:
    /// The splash has hidden itself and is not coming back. Whoever built it
    /// raises the window and deletes the splash.
    void finished();
    /// Quit was explicitly requested while preparation may still be active.
    /// The application owns shutdown; this must never reveal the main window.
    void quitRequested();

protected:
    /// A click dismisses only after daemon installation and harness updates;
    /// the Quit button and the window close action work during preparation.
    void mousePressEvent(QMouseEvent* event) override;
    void closeEvent(QCloseEvent* event) override;
    void changeEvent(QEvent* event) override;
    void paintEvent(QPaintEvent* event) override;

private:
    /// Re-reads the controller, updates the status line and finishes if the
    /// three conditions all hold, or the connection is beyond recovering.
    void refresh();
    /// Hides, stops the cap and emits `finished`, once.
    void finish();
    /// The palette-derived colours, re-derived on a theme change.
    void restyle();
    /// Fits progress text and the temporary Quit button, then recentres.
    void fitAndCenter();

    AppController* m_controller;
    QLabel* m_version = nullptr;
    QLabel* m_status = nullptr;
    QPushButton* m_quit = nullptr;
    /// See [`kSplashCapMs`] in the source.
    QTimer* m_cap = nullptr;
    /// The first `workspacesListed` has arrived. Latched: the signal fires
    /// once per connect, and the connection is what a reconnect resets.
    bool m_workspacesListed = false;
    bool m_done = false;
};
