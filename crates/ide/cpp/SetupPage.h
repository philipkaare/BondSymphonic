#pragma once
#include <QList>
#include <QString>
#include <QWidget>

class AppController;
class TerminalSession;
class TerminalWidget;
class QLabel;
class QPushButton;
class QVBoxLayout;

/// The first-run page: one row per prerequisite, a button on the ones the IDE
/// can fix by opening a terminal, and that terminal underneath.
///
/// The page owns no state of its own. Every row is rebuilt from the
/// `prereqsChecked` payload, so what it shows is always the daemon's last
/// answer rather than a copy made when a button was clicked. A fix ends the
/// same way whatever it was: the terminal exits, the controller re-checks, and
/// the rows redraw.
class SetupPage : public QWidget {
    Q_OBJECT
public:
    explicit SetupPage(AppController* controller, QWidget* parent = nullptr);

signals:
    /// Nothing is left to fix, or the user chose to carry on regardless. The
    /// window takes this as "show the workbench".
    void completed();

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
    /// Opens a login URL the terminal printed in the desktop browser.
    void onLinkDetected(const QString& url);
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
    QPushButton* m_continueButton = nullptr;
    /// The action whose terminal is being waited for, so a `setupPtyOpened`
    /// for something else -- there is nothing else today, but the signal is
    /// the controller's, not this page's -- is left alone. Non-empty means a
    /// request is in flight and every fix button is disabled.
    QString m_pendingAction;
    /// The fix buttons of the current rows, so they can be disabled together.
    /// Cleared with the rows they belong to.
    QList<QPushButton*> m_actionButtons;
};
