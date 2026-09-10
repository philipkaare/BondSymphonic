#pragma once
#include <QFont>
#include <QPointer>
#include <QSize>
#include <QString>
#include <QWidget>

class TerminalSession;
class QJsonArray;
class QPainter;
class QPushButton;
class QTimer;

/// Paints one `TerminalSession` as a terminal and feeds it keyboard input.
///
/// The widget holds no terminal state of its own: every repaint reads the
/// session's `rowsJson`, cursor and exit properties back out, and every key,
/// wheel notch and resize goes straight to the session. It owns the session
/// pointer only, never the session; the pointer is guarded so a session
/// destroyed before the widget cannot be dereferenced.
class TerminalWidget : public QWidget {
    Q_OBJECT
public:
    explicit TerminalWidget(TerminalSession* session, QWidget* parent = nullptr);

    TerminalSession* session() const;

    /// Opens the session's PTY in `workspaceId` running `command` (empty for
    /// the daemon's default shell).
    ///
    /// The PTY is not created until the widget is on screen and laid out: a
    /// terminal opened at the size an unshown widget happens to have starts the
    /// shell at that size, and the reflow when the real size arrives destroys
    /// everything the shell printed first.
    void openSession(const QString& workspaceId, const QString& command);

    QSize sizeHint() const override;
    QSize minimumSizeHint() const override;

protected:
    void paintEvent(QPaintEvent* event) override;
    void keyPressEvent(QKeyEvent* event) override;
    void wheelEvent(QWheelEvent* event) override;
    void resizeEvent(QResizeEvent* event) override;
    void focusInEvent(QFocusEvent* event) override;
    void focusOutEvent(QFocusEvent* event) override;
    void showEvent(QShowEvent* event) override;
    /// Keeps Tab out of the focus chain so it reaches the shell.
    bool focusNextPrevChild(bool next) override;

private:
    void paintRows(QPainter& painter, const QJsonArray& rows);
    void paintCursor(QPainter& painter, const QJsonArray& rows);
    void paintExitLine(QPainter& painter);
    void paintError(QPainter& painter);
    /// Recomputes the cell grid from the widget size and tells the session when
    /// it changed.
    void applySize();
    /// Opens the pending session once the widget is visible; a no-op otherwise.
    void maybeOpen();
    /// Shows the Reopen button when the shell is gone because the daemon
    /// restarted, and hides it otherwise -- including for an ordinary `exit`,
    /// where there is nothing to recover from and a button offering to start
    /// another shell would only be in the way.
    void updateReopenButton();
    /// Puts it in the top right corner, clear of the cursor's home position.
    void placeReopenButton();

    QPointer<TerminalSession> m_session;
    QFont m_font;
    /// Cell metrics, cached from `QFontMetrics` once in the constructor.
    int m_charWidth = 8;
    int m_lineHeight = 16;
    int m_ascent = 12;
    /// The grid size last handed to the session, so a resize that does not
    /// cross a cell boundary costs nothing.
    int m_cols = 80;
    int m_rows = 24;
    QTimer* m_blinkTimer = nullptr;
    /// Offered only over a shell the daemon's restart took away. A real child
    /// widget rather than a painted hotspot, so it is reachable from the
    /// keyboard and looks like the button it is.
    QPushButton* m_reopen = nullptr;
    bool m_blinkOn = true;
    /// The session's `error` at the moment the process exited, so a later one
    /// (a reply that lost its race with the exit) can be told apart from it.
    QString m_errorAtExit;
    /// An `openSession` that has not run yet, with its arguments.
    bool m_pendingOpen = false;
    QString m_pendingWorkspace;
    QString m_pendingCommand;
};
