#pragma once
#include <QHash>
#include <QPointer>
#include <QSet>
#include <QStringList>
#include <QString>
#include <QToolBar>

class AppController;
class QAction;

namespace changestoolbar {

/// The repo-relative paths in a `mergeFinished` conflict list, as one phrase:
/// up to a handful named, then a count of the rest, so a merge that touched
/// forty files says so in a sentence rather than in a paragraph. Empty for an
/// empty or unparseable list.
///
/// Shared with the close-group runner, which reports the same conflicts from
/// the same signal and must not word them differently.
QString conflictList(const QString& conflictsJson);

} // namespace changestoolbar

/// The strip across the top of the Explorer's Changes tab: what to do with the
/// work the list underneath is showing.
///
/// Merge, Rebase and Squash land the workspace's branch on its base; Create PR
/// pushes it and opens a pull request; Discard destroys the workspace and
/// everything unmerged in it. Every one of them puts the workspace's name and
/// its base branch in front of the user before anything is sent, because all
/// five move or destroy work that an agent produced and the user has not
/// necessarily read.
///
/// The toolbar decides nothing about git. It sends one request, disables itself
/// until that request answers, and turns the answer into either a line for the
/// status bar or an error for the workspace's banner. Which workspace it acts
/// on is handed in by the dock, never looked up.
class ChangesToolbar : public QToolBar {
    Q_OBJECT
public:
    ChangesToolbar(AppController* controller, QWidget* parent = nullptr);

    /// Points the toolbar at a workspace. An empty `workspaceId` is the
    /// no-workspace state and greys everything out. `name`, `branch` and
    /// `baseBranch` are the active tab's, and are what the confirmations name.
    void setWorkspace(const QString& workspaceId, const QString& name, const QString& branch,
                      const QString& baseBranch);

    /// Asks the daemon again what would be lost with the current workspace, so
    /// the Discard confirmation counts the right number of files. Called by the
    /// dock whenever the changed-file list moves.
    void refreshSummary();

    /// The same, for a workspace that is named rather than current: a merge
    /// answers after the user may have switched tabs, and the count it
    /// invalidated is that workspace's.
    void requestSummary(const QString& workspaceId);

signals:
    /// Something to put in the status bar. `url` is empty for a plain message;
    /// when it is set the window shows the text as a link to it.
    void statusMessage(const QString& text, const QString& url);

    /// A merge, pull request or discard on `workspaceId` failed or stopped.
    /// The window raises the workspace's banner with these three strings and
    /// marks the tab; `stderrText` is empty unless the daemon sent a
    /// `GitError`.
    void workspaceError(const QString& workspaceId, const QString& title, const QString& detail,
                        const QString& stderrText);

    /// An operation on `workspaceId` succeeded, so any banner it is carrying is
    /// stale. The window takes it down.
    void workspaceRecovered(const QString& workspaceId);

private:
    /// Merge and Rebase: one confirmation, then straight out.
    void onMerge(const QString& mode);
    /// Squash: asks for the summary line first. An empty answer is legitimate
    /// and lets the daemon take the subject of the workspace's last commit.
    void onSquash();
    void onCreatePr();
    void onDiscard();

    void onMergeFinished(const QString& workspaceId, bool ok, const QString& conflictsJson,
                         const QString& reason);
    void onPrCreated(const QString& workspaceId, const QString& url);
    void onOperationFailed(const QString& workspaceId, const QString& op, const QString& message,
                           const QString& dataJson);
    void onSummarized(const QString& workspaceId, const QString& json);

    /// Greys the five actions out when there is no workspace, or when this
    /// workspace already has a request in flight.
    void updateActions();
    /// Books a request in for the current workspace and greys the toolbar out.
    /// Returns false when one is already out, which is what makes a second
    /// click on a slow merge do nothing.
    bool beginOperation();
    /// Books it out again. Called for every answer, success or failure.
    void endOperation(const QString& workspaceId);

    /// The number of changed files in the current workspace, or -1 when the
    /// daemon has not been asked or could not answer.
    int changedFiles() const;

    QPointer<AppController> m_controller;
    QString m_workspaceId;
    QString m_name;
    QString m_branch;
    QString m_baseBranch;
    QAction* m_merge = nullptr;
    QAction* m_rebase = nullptr;
    QAction* m_squash = nullptr;
    QAction* m_pr = nullptr;
    QAction* m_discard = nullptr;
    /// Workspaces with a request out. A set rather than one id because the user
    /// can switch tabs while a merge is running, and the toolbar must grey out
    /// again when they come back to it.
    QSet<QString> m_busy;
    /// The last `workspaceSummarized` per workspace, so the Discard
    /// confirmation can name a number without waiting for a round trip at the
    /// moment of the click.
    QHash<QString, int> m_changedFiles;
    /// Each workspace's branch and base branch, in that order, so the line a
    /// merge writes into the status bar names the branches that moved even when
    /// the answer arrives after the user has switched tabs.
    QHash<QString, QStringList> m_branchOf;
};
