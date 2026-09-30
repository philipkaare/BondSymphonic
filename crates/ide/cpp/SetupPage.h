#pragma once
#include <QList>
#include <QString>
#include <QWidget>
#include <QJsonObject>
#include <functional>

class AppController;
class TerminalSession;
class TerminalWidget;
class QColor;
class QHBoxLayout;
class QLabel;
class QPushButton;
class QResizeEvent;
class QTimer;
class QVBoxLayout;

/// The Setup section of the Settings dialog: one row per prerequisite, a button
/// on the ones the IDE can fix by opening a terminal and on the two sign-ins it
/// can undo again, that terminal underneath, and the sign-in link the terminal
/// printed.
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

    /// Whether the user started a setup action while this page was open.
    ///
    /// What closing Settings asks, before deciding whether to re-check the
    /// prerequisites. An unconditional re-check meant every visit to Settings
    /// -- to change the permission mode, to paste an API key -- put a
    /// `system.check_prereqs` on the wire and, on a machine that is blocked,
    /// re-ran the whole decision about opening this page. A prerequisite can
    /// only have changed if something was run to change it, and this is the
    /// page that runs those things.
    bool ranAction() const;
    static QString buttonTextFor(const QString &action);
    void runAction(const QString &action);
    void setBackendSettings(const QJsonObject &settings);

protected:
    /// Re-elides the sign-in URL: the label's width is only known once the
    /// layout has run, and it changes with the dialog.
    void resizeEvent(QResizeEvent* event) override;

private:
    /// Rebuilds the rows from a `prereqsChecked` payload.
    void applyPrereqs(const QString& json);
    /// One row saying the check has not answered yet, in place of the rows.
    ///
    /// Drawn while the controller has no payload at all, which on a freshly
    /// booted distro is the first ten seconds or so. The section used to be
    /// blank for those seconds, and a user sent here by the composer gate to
    /// log in found nothing to log in with and quit -- logged in all along.
    void showCheckingRow();
    /// The check could not be answered before any answer was drawn: one row
    /// saying so, with the message, and "Re-check" beneath it as the way on.
    /// After an answer the rows stay as they are -- they are still the last
    /// thing the daemon said -- and the status bar reports the failure.
    void onPrereqsCheckFailed(const QString& message);
    /// Drops every row widget. The rows are rebuilt wholesale rather than
    /// patched: there are eight of them, and a partial update is how a page
    /// ends up showing a fix button for something that has since been fixed.
    void clearRows();
    /// Adds one row: glyph, name, detail, and then -- for a failing one -- the
    /// button that fixes it or the command that would, and for a passing
    /// sign-in the button that undoes it.
    void addRow(const QString& name, bool ok, const QString& detail, const QString& fixHint);
    /// The front of every row -- glyph, name, detail -- laid out the same way
    /// whichever row it is, and the layout to put the rest on. An invalid
    /// `glyphColour` leaves the glyph in the palette's own ink.
    QWidget* makeRow(const QString& glyph, const QColor& glyphColour, const QString& name,
                     const QString& detail, QHBoxLayout** layout);
    /// The setup action that fixes `name`, or empty when only a person with a
    /// package manager can.
    static QString actionFor(const QString& name);
    /// The setup action that undoes `name`, or empty for a prerequisite that
    /// is nothing to do with a sign-in.
    ///
    /// Only the two OAuth sessions have one. A stale `claude` or `gh` login
    /// that the matching `auth login` will not overwrite -- a switched
    /// account, a token the provider has since revoked -- leaves the row
    /// ticked and the agent failing, and the only way out of that from inside
    /// the IDE is to throw the session away first.
    static QString logoutActionFor(const QString& name);
    /// The setup action that offers a long-lived token in place of `name`'s
    /// sign-in, or empty everywhere but `claude_auth`.
    ///
    /// Offered whether the row passes or fails, and kept apart from
    /// `actionFor`/`logoutActionFor` because it does not replace either: a
    /// failing row still wants its login button beside it, and a passing one
    /// still wants its logout, since `claude setup-token` neither needs nor
    /// disturbs an existing session. Keyed on the CLI's own detail string --
    /// `"long-lived token"` -- rather than a flag the daemon would have to
    /// invent, because that string already carries the answer.
    static QString tokenActionFor(const QString& name, bool ok, const QString& detail);
    /// What the button for `action` says.

    /// Starts `action`'s terminal and reveals the pane it will appear in.
    /// Refused while another action's `system.setup_pty` is still in flight:
    /// the reply carries no pty id this page could close, so a second request
    /// would leave the first one's process running with nothing attached.
    QJsonObject m_backendSettings;
    void onSetupPtyOpened(const QString& action, const QString& ptyId);
    /// A failed `system.setup_pty`. The page has to come out of its in-flight
    /// state itself: nothing else will, and the buttons stay dead until it does.
    void onOperationFailed(const QString& op, const QString& message);
    /// Enables or disables every action button at once, so a click cannot
    /// start a second action while one is being opened.
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
    /// Puts the focus back on the terminal, which is where a sign-in carries
    /// on once the browser has been dealt with.
    void focusTerminal();
    /// Says over the terminal that a paste of `characters` went in, because the
    /// terminal itself cannot.
    ///
    /// `claude auth login` reads its sign-in code with the echo turned off, the
    /// way a password prompt does, so a paste that worked perfectly leaves the
    /// screen exactly as it was. That is the step this page exists for, and a
    /// user with no sign that the code landed has no reason to press Enter.
    void onPasted(int characters);
    /// Sets the line above the terminal and remembers it, so a transient note
    /// -- a paste, say -- has something to go back to.
    void setTerminalNote(const QString& text);
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
    /// What the label says when nothing transient is being said: the running
    /// action, or why it could not start.
    QString m_terminalNote;
    /// Takes a paste's acknowledgement back off the label. One timer, restarted
    /// by a second paste.
    QTimer* m_pasteNoteTimer = nullptr;
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
    /// The action buttons of the current rows -- the fixes and the two
    /// logouts alike -- so they can be disabled together. Cleared with the
    /// rows they belong to.
    QList<QPushButton*> m_actionButtons;
    /// The PTY the last `setupPtyOpened` named, empty once its process has
    /// ended or it has been closed. Remembered here rather than read back off
    /// the session, which learns the id a turn of the event loop later and so
    /// cannot answer for a page that is being destroyed right now.
    QString m_ptyId;
    /// Never null: the constructor installs the session close.
    std::function<void(const QString& ptyId)> m_closePty;
    /// See [`ranAction`]. Set when an action's terminal is opened and never
    /// cleared: the page is destroyed with the dialog that asks.
    bool m_ranAction = false;
    /// Whether the rows on the page are a real answer. What a failed check
    /// asks before replacing them: the page's own record, rather than the
    /// controller's copy of the payload, because it is the rows that are
    /// being kept or not.
    bool m_answered = false;
};
