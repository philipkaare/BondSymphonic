#pragma once
#include <QList>
#include <QString>
#include <QWidget>
#include <functional>

class AppController;
class TerminalSession;
class TerminalWidget;
class QLabel;
class QPushButton;
class QResizeEvent;
class QTimer;
class QVBoxLayout;

/// The Setup section of the Settings dialog: one row per prerequisite, a button
/// on the ones the IDE can fix by opening a terminal, that terminal underneath,
/// and the sign-in link the terminal printed.
///
/// The page owns no state of its own. Every row is rebuilt from the
/// `prereqsChecked` payload, so what it shows is always the daemon's last
/// answer rather than a copy made when a button was clicked. A fix ends the
/// same way whatever it was: the terminal exits, the controller re-checks, and
/// the rows redraw.
///
/// It is a section rather than a page: `SettingsDialog` is the only thing that
/// builds one, and the dialog's own buttons are what close it. It carries no
/// heading and no "Continue anyway" of its own -- what a blocking prerequisite
/// gets instead is the dialog opening by itself, which the user closes when
/// they choose to carry on regardless.
class SetupPage : public QWidget {
    Q_OBJECT
public:
    explicit SetupPage(AppController* controller, QWidget* parent = nullptr);

    /// Closes the terminal the page opened, if it still has one.
    ///
    /// The PTY the page asked the daemon for is a host process outside any
    /// workspace. Nothing else knows about it: the page is the only thing that
    /// was ever told its id, so a page that goes away without closing it leaves
    /// a login prompt running in the distro with nobody attached.
    ~SetupPage() override;

    /// Replaces what the page does with that PTY when it is torn down. The
    /// default tells the terminal session to close it; this is the seam a test
    /// answers through.
    void setPtyCloser(std::function<void(const QString& ptyId)> close);

protected:
    /// Re-elides the sign-in URL: the label's width is only known once the
    /// layout has run, and it changes with the dialog.
    void resizeEvent(QResizeEvent* event) override;

private:
    /// Rebuilds the rows from a `prereqsChecked` payload.
    void applyPrereqs(const QString& json);
    /// Drops every row widget. The rows are rebuilt wholesale rather than
    /// patched: there are eight of them, and a partial update is how a page
    /// ends up showing a fix button for something that has since been fixed.
    void clearRows();
    /// Adds one row: glyph, name, detail, and either the button that fixes it
    /// or the command that would.
    void addRow(const QString& name, bool ok, const QString& detail, const QString& fixHint);
    /// The setup action that fixes `name`, or empty when only a person with a
    /// package manager can.
    static QString actionFor(const QString& name);
    /// What the button for `action` says.
    static QString buttonTextFor(const QString& action);

    /// Starts `action`'s terminal and reveals the pane it will appear in.
    /// Refused while another action's `system.setup_pty` is still in flight:
    /// the reply carries no pty id this page could close, so a second request
    /// would leave the first one's process running with nothing attached.
    void runAction(const QString& action);
    void onSetupPtyOpened(const QString& action, const QString& ptyId);
    /// A failed `system.setup_pty`. The page has to come out of its in-flight
    /// state itself: nothing else will, and the buttons stay dead until it does.
    void onOperationFailed(const QString& op, const QString& message);
    /// Enables or disables every fix button at once, so a click cannot start a
    /// second action while one is being opened.
    void setActionsEnabled(bool enabled);
    /// Opens a login URL the terminal printed in the desktop browser, and
    /// raises the row that offers it again by hand.
    ///
    /// The automatic open is kept, not replaced: it is right nearly every
    /// time. The row is for the times it is not -- no default browser, a
    /// browser that swallowed the URL, a login that has to happen on another
    /// machine -- when the alternative was reading a wrapped URL off a
    /// terminal and typing it out.
    void onLinkDetected(const QString& url);
    /// Puts the current URL on the clipboard and says so for a few seconds.
    void copyLink();
    /// Hands the current URL to the desktop browser.
    void openLink();
    /// Copy and open together: what clicking the URL text itself does.
    void useLink();
    /// Re-renders the URL label at the width it now has, elided in the middle
    /// so the host and the tail of the query both stay readable.
    void updateLinkElide();
    /// Takes the row down and forgets the URL.
    void clearLink();
    /// The terminal's process ended, so whatever it was fixing is either fixed
    /// or not: ask the daemon rather than guessing.
    void onTerminalExited();

    /// The terminal's current grid, which the pane keeps on the session.
    int terminalCols() const;
    int terminalRows() const;

    AppController* m_controller;
    QVBoxLayout* m_rowsLayout = nullptr;
    QWidget* m_rowsHost = nullptr;
    QWidget* m_terminalHost = nullptr;
    TerminalSession* m_session = nullptr;
    TerminalWidget* m_terminal = nullptr;
    QLabel* m_terminalLabel = nullptr;
    QPushButton* m_recheckButton = nullptr;
    /// The "Sign-in link" row under the terminal, hidden until a login URL is
    /// detected and taken down again when the terminal exits.
    QWidget* m_linkRow = nullptr;
    /// The URL itself, elided in the middle and clickable.
    QLabel* m_linkLabel = nullptr;
    /// Transient "Link copied" feedback. The page has no status bar of its
    /// own -- it lives in a dialog -- so the acknowledgement is on the row.
    QLabel* m_linkFeedback = nullptr;
    QPushButton* m_copyLink = nullptr;
    QPushButton* m_openLink = nullptr;
    /// The URL as printed, which is what is copied and opened. The label shows
    /// an elided rendering of it and is never the source.
    QString m_linkUrl;
    /// Takes the "Link copied" acknowledgement down again. One timer, restarted
    /// by a second copy, so two clicks do not leave two of them running.
    QTimer* m_linkFeedbackTimer = nullptr;
    /// The action whose terminal is being waited for, so a `setupPtyOpened`
    /// for something else -- there is nothing else today, but the signal is
    /// the controller's, not this page's -- is left alone. Non-empty means a
    /// request is in flight and every fix button is disabled.
    QString m_pendingAction;
    /// The fix buttons of the current rows, so they can be disabled together.
    /// Cleared with the rows they belong to.
    QList<QPushButton*> m_actionButtons;
    /// The PTY the last `setupPtyOpened` named, empty once its process has
    /// ended or it has been closed. Remembered here rather than read back off
    /// the session, which learns the id a turn of the event loop later and so
    /// cannot answer for a page that is being destroyed right now.
    QString m_ptyId;
    /// Never null: the constructor installs the session close.
    std::function<void(const QString& ptyId)> m_closePty;
};
