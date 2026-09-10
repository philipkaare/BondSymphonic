#pragma once
#include <QFrame>
#include <QString>

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

signals:
    /// The user pressed Dismiss. The window answers by clearing the tab's error
    /// mark; this widget does not touch the model.
    void dismissed();

private:
    /// Shows or hides the stderr box and puts the right arrow on the button.
    void setExpanded(bool expanded);

    QLabel* m_title = nullptr;
    QLabel* m_detail = nullptr;
    /// Reveals `m_stderr`. Hidden when the failure carried no stderr.
    QToolButton* m_disclose = nullptr;
    QPlainTextEdit* m_stderr = nullptr;
    QPushButton* m_dismiss = nullptr;
};
