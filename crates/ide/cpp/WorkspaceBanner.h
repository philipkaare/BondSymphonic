#pragma once
#include <QFrame>
#include <QString>

class QEvent;
class QLabel;
class QPlainTextEdit;
class QPushButton;
class QToolButton;

/// The strip at the top of a workspace's agent pane that reports a merge, pull
/// request or discard that did not work.
///
/// It holds a sentence, an optional second line of detail, and -- when the
/// daemon sent a `GitError` -- the command's stderr behind a disclosure button,
/// because that text is a wall and is only wanted by someone who has decided to
/// read it. Dismissing is the user saying they have read it, which is also what
/// takes the red glyph off the tab; the banner never takes itself down.
///
/// It decides nothing and asks the daemon for nothing. Every string is handed
/// in by whoever raised it.
class WorkspaceBanner : public QFrame {
    Q_OBJECT
public:
    explicit WorkspaceBanner(QWidget* parent = nullptr);

    /// Fills the banner in and shows it. `detail` may be empty, and so may
    /// `stderrText`, which hides the disclosure button rather than offering an
    /// empty box.
    void showError(const QString& title, const QString& detail, const QString& stderrText);

    /// Empties the banner and hides it, without emitting `dismissed`.
    void reset();

    /// Whether this banner offers to restart the agent.
    ///
    /// Only an agent that exited gets the button. Every other failure a banner
    /// reports -- a merge that conflicted, a pull request that was refused --
    /// has a live agent behind it that a restart would interrupt rather than
    /// repair. It is the one Start-shaped control left in the pane now that
    /// agents start themselves, and it is here because a failure is the one
    /// state where an agent being down is news rather than a transient.
    ///
    /// Sticky across `showError` and cleared by `reset`, so a second failure on
    /// the same dead agent still offers it.
    void setRestartOffered(bool offered);

    /// Puts the banner into the state that outranks every failure: the
    /// workspace itself cannot run. `title` and `detail` say why, and the
    /// buttons are Retry and Remove workspace instead of Dismiss and Restart --
    /// dismissing would leave a pane that can do nothing, and restarting an
    /// agent needs a sandbox to restart it in.
    ///
    /// A failure raised while this is up is kept and shown again once
    /// [`clearWorkspaceProblem`] runs, so a merge that failed just before the
    /// sandbox died is not lost to it.
    void showWorkspaceProblem(const QString& title, const QString& detail);

    /// Leaves that state: the banner goes back to the failure it was showing
    /// before, or hides when there was none.
    void clearWorkspaceProblem();

    /// Whether a Retry is in flight. The button is disabled and says so, so a
    /// sandbox start that takes a while is visibly being waited on.
    void setRetrying(bool retrying);

    /// Whether the banner is showing a workspace problem.
    bool hasWorkspaceProblem() const { return m_problem; }

signals:
    /// The user pressed Dismiss. The window answers by clearing the tab's error
    /// mark; this widget does not touch the model.
    void dismissed();

    /// The user pressed Restart. The area turns this into the workspace id the
    /// window needs; this widget knows nothing about agents.
    void restartRequested();

    /// The user pressed Retry on a workspace problem.
    void retryRequested();

    /// The user pressed Remove workspace on a workspace problem. The window
    /// asks for confirmation; this widget removes nothing.
    void removeRequested();

protected:
    /// A palette change is the theme moving under the banner. Its red is mixed
    /// into the pane's own background, so it has to be mixed again rather than
    /// kept.
    void changeEvent(QEvent* event) override;

private:
    /// Shows or hides the stderr box and puts the right arrow on the button.
    void setExpanded(bool expanded);

    /// Fills the labels and buttons in from whichever of the two layers is on
    /// top -- the workspace problem, or the latest failure -- and shows or
    /// hides the banner to match.
    void render();

    /// Mixes the red into whatever the pane is now and installs it. Called from
    /// the constructor and from every palette change.
    void applyWash();

    QLabel* m_title = nullptr;
    QLabel* m_detail = nullptr;
    /// Reveals `m_stderr`. Hidden when the failure carried no stderr.
    QToolButton* m_disclose = nullptr;
    QPlainTextEdit* m_stderr = nullptr;
    QPushButton* m_dismiss = nullptr;
    /// See [`setRestartOffered`]. Hidden unless the failure is an agent that
    /// exited.
    QPushButton* m_restart = nullptr;
    /// See [`showWorkspaceProblem`]. Hidden unless the workspace cannot run.
    QPushButton* m_retry = nullptr;
    QPushButton* m_remove = nullptr;
    /// The latest failure, kept whole so a workspace problem can sit over it
    /// and hand the banner back.
    QString m_errorTitle;
    QString m_errorDetail;
    QString m_errorStderr;
    bool m_error = false;
    bool m_restartOffered = false;
    /// The workspace problem, when there is one.
    QString m_problemTitle;
    QString m_problemDetail;
    bool m_problem = false;
    bool m_retrying = false;
    /// Whether [`applyWash`] is already running. Installing a palette raises
    /// the change event that calls it, so without this it would call itself.
    bool m_mixing = false;
};
