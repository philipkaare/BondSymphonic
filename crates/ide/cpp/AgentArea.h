#pragma once
#include <QHash>
#include <QList>
#include <QSet>
#include <QStackedWidget>
#include <QString>

class TerminalSession;
class TerminalWidget;
class TranscriptModel;
class TranscriptView;
class WorkspaceBanner;
class QLabel;

/// A stack of agent panes, one per workspace, with a placeholder page for when
/// no workspace is showing.
///
/// The area creates a workspace's pane the first time it is shown and keeps it
/// until the workspace is destroyed, so switching tabs preserves a terminal's
/// scrollback and a transcript's scroll position. Which pane a workspace gets
/// is the tab's adapter: `terminal` gets a `TerminalWidget` over a
/// `TerminalSession`, `claude` a `TranscriptView` over a `TranscriptModel`.
///
/// It is used twice: as the agent pane, where the tab's adapter decides, and as
/// the bottom dock's shell tab, where every workspace runs the daemon default.
class AgentArea : public QStackedWidget {
    Q_OBJECT
public:
    explicit AgentArea(QWidget* parent = nullptr);
    void setBackendAuth(const QString &backend, bool loggedIn, const QString &failure);

signals:
    /// A transcript pane asked for an agent to be started (or restarted) in
    /// `workspaceId`. The window is what knows the options the tab was created
    /// with and how to reach the controller.
    void startAgentRequested(const QString& workspaceId);

    /// `workspaceId`'s composer chose a different model or permission mode, and
    /// `optionsJson` is the whole `AgentStartOptions` that choice means. The
    /// window answers it the way it answers a restart, because that is what it
    /// is: `claude -p` reads both flags once, when the process starts.
    void agentOptionsChanged(const QString& workspaceId, const QString& optionsJson);

    /// The user dismissed `workspaceId`'s error banner. The window answers by
    /// clearing the tab's error mark; the area touches no model.
    void bannerDismissed(const QString& workspaceId);

    /// `workspaceId`'s banner asked for its sandbox to be started again.
    void retryWorkspaceRequested(const QString& workspaceId);

    /// `workspaceId`'s banner asked for the workspace to be removed. The window
    /// asks for confirmation first, exactly as the tab's own menu does.
    void removeWorkspaceRequested(const QString& workspaceId);

    /// A transcript pane's "Log in to Claude Code…" was pressed. The window
    /// answers by opening Settings on its Setup section.
    void loginRequested();

public:
    /// What the placeholder page says while nothing is showing.
    void setPlaceholderText(const QString& text);

    /// Shows `workspaceId`, creating its pane on first use. `adapter` picks
    /// which pane: `terminal`, `claude`, or the placeholder for anything else.
    /// An empty `command` runs the daemon's default shell; `agentId` attaches
    /// a transcript to an agent that is already running, and is empty until
    /// `agent.start` has answered.
    void showWorkspace(const QString& workspaceId, const QString& adapter, const QString& command,
                       const QString& agentId = QString());

    /// Points `workspaceId`'s transcript at `agentId`, creating nothing: a
    /// workspace whose pane has not been shown yet picks the id up from
    /// `showWorkspace` when it is.
    ///
    /// Attaching is not idempotent on the model side -- it clears the
    /// transcript and replays -- so an id that is already attached is ignored.
    void setAgent(const QString& workspaceId, const QString& agentId);

    /// The transcript model behind `workspaceId`'s pane, or null when the
    /// workspace has no transcript. The window reads the running cost off it.
    TranscriptModel* transcriptModel(const QString& workspaceId) const;

    /// Shows the placeholder without disturbing any existing pane.
    void showPlaceholder();

    /// Records whether an `agent.start` for `workspaceId` is in flight, so its
    /// transcript pane can say "starting" rather than offering a Start button.
    /// Remembered even for a workspace whose pane does not exist yet, because a
    /// Claude workspace is created before its tab is first shown.
    void setStarting(const QString& workspaceId, bool starting);

    /// Whether Claude Code is logged in, per the daemon's `claude_auth`
    /// prerequisite. Applied to every transcript pane, now and as each one is
    /// built: a workspace whose tab has not been opened yet has no pane to
    /// tell, and it must not be built with the composer open.
    ///
    /// Terminal panes are untouched. A shell is a shell whether or not Claude
    /// Code is logged in -- and the setup terminal is where the login happens.
    void setClaudeLoggedIn(bool loggedIn);

    /// Whether the prerequisite check has answered at all, per the controller's
    /// `prereqsAnswered`. Applied to every transcript pane the same way, and
    /// for the same reason: a pane built before the answer must say it is
    /// checking, not that nobody is logged in.
    void setPrereqsAnswered(bool answered);

    /// What the Claude Code CLI itself said when an agent could not
    /// authenticate, per the controller's `claudeAuthFailure`, or empty.
    /// Applied to every transcript pane the same way, and remembered for the
    /// ones built afterwards: the gate the pane is born with must say what the
    /// CLI said, not the paraphrase the daemon's tick contradicts.
    void setClaudeAuthFailure(const QString& sentence);

    /// Whether every transcript shows the turn cost and the agent's own system
    /// lines. Applied to the panes that exist and remembered for the ones built
    /// afterwards, for the same reason the login gate is: a workspace whose tab
    /// has never been opened has no pane to tell, and it must not be built
    /// showing lines the user turned off.
    void setShowMeta(bool show);

    /// `system.list_models` answered: every transcript pane's model combo is
    /// re-filled from the new list, keeping its current choice. Nothing to
    /// remember for panes built afterwards -- a pane reads `agentchoices::models()`
    /// fresh when it is built, so it is already born with the new list.
    void refillModels();

    /// Closes the workspace's PTY or transcript and drops its pane.
    void removeWorkspace(const QString& workspaceId);

    /// The sessions this area currently holds, for whole-area operations such
    /// as the daemon's "output dropped" notice.
    QList<TerminalSession*> sessions() const;

    /// Raises the error banner over `workspaceId`'s pane.
    ///
    /// A workspace whose pane has not been created yet -- one that failed to
    /// merge from the close-group dialog, say, without the user ever having
    /// opened its tab -- has nowhere to put a banner, so the strings are held
    /// and raised when that pane is first built. Without that the tab's red
    /// glyph would lead to a pane that says nothing about why it is red.
    void showBanner(const QString& workspaceId, const QString& title, const QString& detail,
                    const QString& stderrText);

    /// Takes it down again, without emitting `bannerDismissed`. What a
    /// successful operation on that workspace does to the banner its last
    /// failure left.
    void clearBanner(const QString& workspaceId);

    /// Whether `workspaceId`'s pane offers to restart its agent.
    ///
    /// A fact about the agent -- is it down? -- and not about the latest
    /// failure, which is why it does not ride in on `showBanner`. Tying the two
    /// together is wrong in both directions: a merge that fails over a
    /// still-dead agent would take away the one way back, and an agent that
    /// came back would keep a button that kills a working conversation.
    ///
    /// Independent of whether a banner is up. Raising it on a workspace with
    /// nothing to report is remembered and applied to the next banner, and held
    /// for a workspace whose pane does not exist yet, because an agent that
    /// died before its tab was ever opened is exactly the one that needs it.
    void setRestartOffered(const QString& workspaceId, bool offered);

    /// Says that `workspaceId` cannot run, and why: its banner offers Retry
    /// and Remove over whatever it was showing, and its composer stops
    /// offering to talk to an agent that has no sandbox to run in.
    ///
    /// A fact about the workspace, held like the restart offer for a pane
    /// that has not been built yet. Re-stating the same problem changes
    /// nothing, so the window may call this on every model change.
    /// `inPlace` words the banner's Remove button as Close, and `whatChanged`
    /// is the longer explanation the daemon sent with the reason, which the
    /// banner puts behind its disclosure.
    void setWorkspaceProblem(const QString& workspaceId, const QString& title,
                             const QString& detail, bool inPlace = false,
                             const QString& whatChanged = QString());

    /// The workspace can run again: the banner goes back to what it was
    /// showing before, and any Retry in flight is over.
    void clearWorkspaceProblem(const QString& workspaceId);

    /// Whether a Retry for `workspaceId` is in flight.
    void setRetrying(const QString& workspaceId, bool retrying);

    /// The banner over `workspaceId`'s pane, or null before the pane exists.
    WorkspaceBanner* banner(const QString& workspaceId) const {
        return m_banners.value(workspaceId);
    }

    /// Records what `workspaceId`'s pane should say while its transcript is
    /// empty, from the tab JSON the window already holds: the agent's name, the
    /// repository and branch the work forked from, and the worktree it happens
    /// in.
    ///
    /// Held for a workspace whose pane has not been built yet, for the same
    /// reason a banner is: a Claude workspace is created before its tab is
    /// first shown, and the welcome is the one thing that pane has to say.
    void setWelcome(const QString& workspaceId, const QString& tabJson);

    /// Records the `AgentStartOptions` the workspace's tab was created with on
    /// its transcript model, so a Restart resumes the same conversation with
    /// the same model and permission mode. A workspace with no transcript is
    /// ignored.
    void setOptionsJson(const QString& workspaceId, const QString& optionsJson);

private:
  QHash<QString, bool> m_backendLoggedIn;
  QHash<QString, QString> m_backendAuthFailure;
    /// Creates the transcript pane for `workspaceId` if it has none, and
    /// attaches it to `agentId` when that is new. Returns the pane.
    TranscriptView* ensureTranscript(const QString& workspaceId, const QString& agentId);

    /// Puts the workspace's held tab JSON on its pane as the welcome's three
    /// lines. Does nothing for a workspace the window has not described.
    void applyWelcome(const QString& workspaceId, TranscriptView* view);

    /// Puts the remembered restart offer on the workspace's banner. Called
    /// wherever the banner could have lost it -- when it is built, and after a
    /// failure is raised on it -- so the widget always agrees with the fact
    /// rather than with whatever the last `showError` left behind.
    void applyRestartOffer(const QString& workspaceId);

    /// A failure raised for a workspace whose pane did not exist yet, held
    /// until it does. Three strings and nothing about the agent: whether a
    /// restart is offered is [`setRestartOffered`]'s business and outlives any
    /// one failure.
    struct PendingBanner {
        QString title;
        QString detail;
        QString stderrText;
    };

    /// Puts the remembered workspace problem, and whether a Retry is in
    /// flight, on the workspace's banner and pane.
    void applyWorkspaceProblem(const QString& workspaceId);

    /// A workspace that cannot run: see [`setWorkspaceProblem`].
    struct WorkspaceProblem {
        QString title;
        QString detail;
        bool retrying = false;
        bool inPlace = false;
        /// What the daemon sent to explain `detail`, or empty. See
        /// [`WorkspaceBanner::showWorkspaceProblem`].
        QString whatChanged;
    };

    /// Wraps `body` in the page this area actually stacks: a banner above the
    /// pane, hidden until something fails. Records both and returns the page.
    QWidget* makePage(const QString& workspaceId, QWidget* body);

    QLabel* m_placeholder = nullptr;
    /// What the placeholder says when nothing is showing, kept so the message
    /// for an adapter without a pane can replace it and be restored.
    QString m_placeholderText;
    QHash<QString, TerminalWidget*> m_terminals;
    QHash<QString, TranscriptView*> m_transcripts;
    /// The stacked page for each workspace: the banner and the pane together.
    /// The stack holds these, not the panes, so a banner can appear above a
    /// terminal or a transcript without either widget knowing about it.
    QHash<QString, QWidget*> m_pages;
    QHash<QString, WorkspaceBanner*> m_banners;
    /// The tab JSON each workspace's welcome is written from, kept because the
    /// window says it once and the pane may be built long afterwards.
    QHash<QString, QString> m_welcomes;
    /// The failures waiting for a pane to be built. See [`PendingBanner`].
    QHash<QString, PendingBanner> m_pendingBanners;
    /// Which workspaces offer a restart. Kept apart from the banners on
    /// purpose: see [`setRestartOffered`]. A workspace absent from this offers
    /// none, which is what a workspace nobody has said anything about should
    /// do.
    QHash<QString, bool> m_restartOffered;
    /// The workspaces that cannot run. Absent means the workspace can.
    QHash<QString, WorkspaceProblem> m_problems;
    /// The agent id each transcript is attached to, so re-showing a tab does
    /// not replay its history again.
    QHash<QString, QString> m_attached;
    /// Workspaces with an `agent.start` in flight, kept here rather than only
    /// on the pane because the start begins before the pane exists.
    QSet<QString> m_starting;
    /// See [`setShowMeta`]. True until the window says otherwise, which is the
    /// setting's own default.
    bool m_showMeta = true;
    /// See [`setClaudeLoggedIn`]. False until the first prerequisite check
    /// answers, so a pane built in the seconds before it opens no composer.
    bool m_claudeLoggedIn = false;
    /// See [`setPrereqsAnswered`]. False until the first check answers, which
    /// is what makes a pane built before then say "checking" rather than
    /// offer a login to a user who already has one.
    bool m_prereqsAnswered = false;
    /// See [`setClaudeAuthFailure`]. Empty until the CLI has said otherwise.
    QString m_claudeAuthFailure;
};
