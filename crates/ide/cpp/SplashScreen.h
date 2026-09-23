#pragma once
#include <QWidget>

class AppController;
class QLabel;
class QTimer;

/// What the IDE shows from process start until it is ready to be used: the
/// logo, the name, the version, one line naming the phase the launch is in,
/// and an indeterminate bar. A top-level of its own, centred on the primary
/// screen and kept above the window, which is shown behind it right away so
/// layout and dock restore are unchanged.
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
    /// broken, the cap reached, or a click. Once true it stays true, and
    /// `finished` has been emitted exactly once.
    bool isDone() const;

signals:
    /// The splash has hidden itself and is not coming back. Whoever built it
    /// raises the window and deletes the splash.
    void finished();

protected:
    /// A click anywhere is a way through: a splash that cannot be dismissed
    /// is a splash the user has to wait out.
    void mousePressEvent(QMouseEvent* event) override;
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

    AppController* m_controller;
    QLabel* m_version = nullptr;
    QLabel* m_status = nullptr;
    /// See [`kSplashCapMs`] in the source.
    QTimer* m_cap = nullptr;
    /// The first `workspacesListed` has arrived. Latched: the signal fires
    /// once per connect, and the connection is what a reconnect resets.
    bool m_workspacesListed = false;
    bool m_done = false;
};
