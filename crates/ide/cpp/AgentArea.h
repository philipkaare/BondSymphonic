#pragma once
#include <QHash>
#include <QList>
#include <QStackedWidget>
#include <QString>

class TerminalSession;
class TerminalWidget;
class TranscriptModel;
class TranscriptView;
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

    /// Closes the workspace's PTY or transcript and drops its pane.
    void removeWorkspace(const QString& workspaceId);

    /// The sessions this area currently holds, for whole-area operations such
    /// as the daemon's "output dropped" notice.
    QList<TerminalSession*> sessions() const;

private:
    /// Creates the transcript pane for `workspaceId` if it has none, and
    /// attaches it to `agentId` when that is new. Returns the pane.
    TranscriptView* ensureTranscript(const QString& workspaceId, const QString& agentId);

    QLabel* m_placeholder = nullptr;
    /// What the placeholder says when nothing is showing, kept so the message
    /// for an adapter without a pane can replace it and be restored.
    QString m_placeholderText;
    QHash<QString, TerminalWidget*> m_terminals;
    QHash<QString, TranscriptView*> m_transcripts;
    /// The agent id each transcript is attached to, so re-showing a tab does
    /// not replay its history again.
    QHash<QString, QString> m_attached;
};
