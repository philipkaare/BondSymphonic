#pragma once
#include <QColor>
#include <QFont>
#include <QJsonArray>
#include <QPointF>
#include <QPointer>
#include <QSize>
#include <QString>
#include <QWidget>
#include <functional>

class TerminalSession;
class QContextMenuEvent;
class QEvent;
class QMouseEvent;
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

    /// Pastes `text` into the session as one block, rather than as the keys
    /// that would have typed it.
    void paste(const QString& text);

    /// Pastes what the desktop clipboard holds. What Ctrl+V, Shift+Insert and
    /// the context menu all do.
    ///
    /// A terminal with no paste is a terminal that cannot answer `claude auth
    /// login`: the browser hands back a sign-in code far too long to read off
    /// a screen and type in again.
    void pasteFromClipboard();

    /// Replaces what a paste does with its text. The default hands it to the
    /// session; this is the seam a test answers through.
    void setPasteSink(std::function<void(const QString& text)> sink);

    /// Puts the selected text on the clipboard. Nothing selected, nothing
    /// copied -- and in particular the clipboard is left as it was, so a Copy
    /// that finds no selection cannot lose what was on it.
    void copySelection();

    /// Whether anything is selected in this pane.
    bool hasSelection() const;

signals:
    /// A paste went to the program, carrying how many characters.
    ///
    /// Whether any of them appear on screen is the program's decision, and one
    /// of the programs this IDE opens a terminal for decides not to:
    /// `claude auth login` reads its sign-in code with the terminal's echo
    /// turned off, exactly as a password prompt does, so a correct paste looks
    /// from the outside like a paste that never happened. Anything hosting a
    /// terminal where that matters can say so itself; see `SetupPage`.
    void pasted(int characters);

public:
    /// The colour the error banner's text is drawn in.
    ///
    /// Its own function so the painter and the check that the palette is
    /// honoured read the same expression rather than two copies of it.
    QColor errorInk() const;

    /// How many times the widget has parsed the session's grid.
    ///
    /// A test seam. The grid changes once per frame and is repainted far more
    /// often than that -- the cursor blinks twice a second over a shell that
    /// has printed nothing -- and nothing else about the widget says whether a
    /// repaint re-read it.
    int rowsParseCount() const;

protected:
    void paintEvent(QPaintEvent* event) override;
    void keyPressEvent(QKeyEvent* event) override;
    /// Left button: starts a selection, and takes the focus. Anything else is
    /// left to Qt, which is what raises the context menu.
    void mousePressEvent(QMouseEvent* event) override;
    /// Drags the loose end of the selection while the button is held.
    void mouseMoveEvent(QMouseEvent* event) override;
    void mouseReleaseEvent(QMouseEvent* event) override;
    /// Selects the word under the pointer, and keeps selecting by words if the
    /// press turns into a drag.
    void mouseDoubleClickEvent(QMouseEvent* event) override;
    void wheelEvent(QWheelEvent* event) override;
    void resizeEvent(QResizeEvent* event) override;
    void focusInEvent(QFocusEvent* event) override;
    void focusOutEvent(QFocusEvent* event) override;
    /// The right-click menu, which is the paste a keyboard shortcut does not
    /// announce.
    void contextMenuEvent(QContextMenuEvent* event) override;
    void showEvent(QShowEvent* event) override;
    /// Keeps Tab out of the focus chain so it reaches the shell.
    bool focusNextPrevChild(bool next) override;
    /// Re-tells the session what the pane paints with when the desktop theme
    /// or the widget's style changes under it.
    void changeEvent(QEvent* event) override;

private:
    /// The session's grid, parsed once per frame and handed out as often as
    /// the widget is painted.
    const QJsonArray& rows();
    /// The next reader parses again. Connected to the session's `rowsJson`,
    /// which is the only thing that can move the grid.
    void invalidateRows();

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
    /// The cell under a point in the widget, and which half of it, as the
    /// session's selection calls take them. Clamped to the grid: a drag runs
    /// off the edge of the pane all the time.
    void cellAt(const QPointF& position, int& col, int& row, bool& rightHalf) const;
    /// Tells the session the pane's default colours and cell size, which is
    /// what the terminal answers a program that asks about either. The palette
    /// follows the desktop, so this is not a constant and cannot be one.
    void applyAppearance();

    QPointer<TerminalSession> m_session;
    /// Never null: the constructor installs the session paste.
    std::function<void(const QString& text)> m_paste;
    /// The last parse of the session's `rowsJson`, and whether it still
    /// describes the session.
    QJsonArray m_parsedRows;
    bool m_parsedRowsValid = false;
    /// How many parses there have been. Test seam; see `rowsParseCount`.
    int m_parsedRowsCount = 0;
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
    /// Whether the left button is down and dragging out a selection.
    bool m_selecting = false;
    /// Whether that drag selects by word rather than by cell, which is what a
    /// double-click and drag does.
    bool m_selectingWords = false;
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
