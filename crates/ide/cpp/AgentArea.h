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

signals:
    /// A transcript pane asked for an agent to be started (or restarted) in
    /// `workspaceId`. The window is what knows the options the tab was created
    /// with and how to reach the controller.
    void startAgentRequested(const QString& workspaceId);

    /// The user dismissed `workspaceId`'s error banner. The window answers by
    /// clearing the tab's error mark; the area touches no model.
    void bannerDismissed(const QString& workspaceId);

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

    /// Records the `AgentStartOptions` the workspace's tab was created with on
    /// its transcript model, so a Restart resumes the same conversation with
    /// the same model and permission mode. A workspace with no transcript is
    /// ignored.
    void setOptionsJson(const QString& workspaceId, const QString& optionsJson);

private:
    /// Creates the transcript pane for `workspaceId` if it has none, and
    /// attaches it to `agentId` when that is new. Returns the pane.
    TranscriptView* ensureTranscript(const QString& workspaceId, const QString& agentId);

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
    /// A banner raised for a workspace that had no pane yet: its title, detail
    /// and stderr, in that order, waiting for the pane to be built.
    QHash<QString, QStringList> m_pendingBanners;
    /// The agent id each transcript is attached to, so re-showing a tab does
    /// not replay its history again.
    QHash<QString, QString> m_attached;
    /// Workspaces with an `agent.start` in flight, kept here rather than only
    /// on the pane because the start begins before the pane exists.
    QSet<QString> m_starting;
    /// See [`setClaudeLoggedIn`]. False until the first prerequisite check
    /// answers, so a pane built in the seconds before it opens no composer.
    bool m_claudeLoggedIn = false;
};
