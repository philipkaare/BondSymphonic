#pragma once
#include <QHash>
#include <QPointer>
#include <QStringList>
#include <QString>
#include <QToolBar>
#include <functional>

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

    /// The changed-file count last reported for `workspaceId`, or -1 when the
    /// daemon has not been asked or could not answer.
    ///
    /// The close-group dialog names the same number in its own confirmation,
    /// and both must come from the same place: two ways of counting what a
    /// discard costs is one way too many.
    int changedFilesFor(const QString& workspaceId) const;

    /// Records a workspace's branch and base branch without making it current.
    ///
    /// The toolbar learns these from `setWorkspace`, i.e. only for workspaces
    /// that have been the active tab. A close-group run merges workspaces that
    /// may never have been, and the line it writes into the status bar has to
    /// name the branches that moved.
    void noteBranches(const QString& workspaceId, const QString& branch,
                      const QString& baseBranch);

    /// Which confirmation an action puts in front of the user.
    enum class Ask { Merge, Rebase, Squash, Pr, Discard };

    /// What one came back with.
    ///
    /// `accepted` false is a cancelled prompt and the only field the caller
    /// always reads; each of the others belongs to the one action that asked
    /// for it.
    struct Answer {
        bool accepted = false;
        /// Squash: the subject line for the squashed commit. Empty is a real
        /// answer and lets the daemon take the workspace's last commit subject.
        QString summary;
        /// Create PR.
        QString title;
        QString body;
        bool draft = false;
    };

    /// Replaces the confirmation every action puts up. The default is the
    /// modal box, input dialog or `PrDialog` the action would show; this is the
    /// seam that lets the answer -- and the pause while it is being given --
    /// come from somewhere else.
    /// `workspaceId` is the workspace the action was started on, which is what
    /// the question is about.
    void setConfirmPrompt(std::function<Answer(Ask, const QString& workspaceId)> ask);

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

    /// The prompt each action shows when nothing has replaced it: the one
    /// place the five modals live. `workspaceId` is the one the caller read
    /// before it asked, so the question and the request cannot be about
    /// different workspaces even in the box that names a changed-file count.
    Answer askModal(Ask ask, const QString& workspaceId);

    /// Greys the five actions out when there is no workspace, or when this
    /// workspace already has a request in flight.
    void updateActions();
    /// Whether the controller has a merge, pull request, discard or destroy out
    /// for `workspaceId`. The controller owns that set, so a merge started from
    /// the close-group runner or a destroy from the tab context menu greys this
    /// toolbar out too.
    bool busy(const QString& workspaceId) const;
    /// Whether a request may be started for `workspaceId`. Only a pre-check:
    /// the controller books the workspace in and refuses a second one itself,
    /// and answers `workspaceBusyChanged` either way.
    ///
    /// Takes the workspace rather than reading `m_workspaceId`, so it checks
    /// the one the confirmation named and not whatever the toolbar happens to
    /// be pointed at by the time the confirmation is answered.
    bool beginOperation(const QString& workspaceId);

    /// Whether the toolbar is still pointed at `workspaceId`.
    ///
    /// Each of the five confirmations runs an event loop of its own, and the
    /// Explorer moves this toolbar to whatever tab becomes active while one is
    /// up. Every action asks this between its confirmation and its request, so
    /// a yes given about one workspace can never be spent on another.
    bool stillOn(const QString& workspaceId) const;
    /// Says on the status bar that `what` was called off because the workspace
    /// moved. What every action does when [`stillOn`] answers false.
    void reportCancelled(const QString& what);

    QPointer<AppController> m_controller;
    /// Never null: the constructor installs the modals.
    std::function<Answer(Ask, const QString& workspaceId)> m_ask;
    QString m_workspaceId;
    QString m_name;
    QString m_branch;
    QString m_baseBranch;
    QAction* m_merge = nullptr;
    QAction* m_rebase = nullptr;
    QAction* m_squash = nullptr;
    QAction* m_pr = nullptr;
    QAction* m_discard = nullptr;
    /// The last `workspaceSummarized` per workspace, so the Discard
    /// confirmation can name a number without waiting for a round trip at the
    /// moment of the click.
    QHash<QString, int> m_changedFiles;
    /// Each workspace's branch and base branch, in that order, so the line a
    /// merge writes into the status bar names the branches that moved even when
    /// the answer arrives after the user has switched tabs.
    QHash<QString, QStringList> m_branchOf;
};
