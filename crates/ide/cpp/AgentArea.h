#pragma once
#include <QHash>
#include <QList>
#include <QStackedWidget>
#include <QString>

class TerminalSession;
class TerminalWidget;
class QLabel;

/// A stack of terminals, one per workspace, with a placeholder page for when
/// no workspace is showing.
///
/// The area creates a `TerminalSession` and its `TerminalWidget` the first time
/// a workspace is shown and keeps them until the workspace is destroyed, so
/// switching tabs preserves each terminal's scrollback. It is used twice: as
/// the agent pane, where the tab's adapter and command decide what runs, and as
/// the bottom dock's shell tab, where every workspace runs the daemon default.
class AgentArea : public QStackedWidget {
    Q_OBJECT
public:
    explicit AgentArea(QWidget* parent = nullptr);

    /// What the placeholder page says while nothing is showing.
    void setPlaceholderText(const QString& text);

    /// Shows `workspaceId`, creating its terminal on first use. `adapter` picks
    /// the pane: only `terminal` has one, anything else shows the placeholder.
    /// An empty `command` runs the daemon's default shell.
    void showWorkspace(const QString& workspaceId, const QString& adapter, const QString& command);

    /// Shows the placeholder without disturbing any existing terminal.
    void showPlaceholder();

    /// Closes the workspace's PTY and drops its terminal.
    void removeWorkspace(const QString& workspaceId);

    /// The sessions this area currently holds, for whole-area operations such
    /// as the daemon's "output dropped" notice.
    QList<TerminalSession*> sessions() const;

private:
    QLabel* m_placeholder = nullptr;
    /// What the placeholder says when nothing is showing, kept so the message
    /// for an adapter without a pane can replace it and be restored.
    QString m_placeholderText;
    QHash<QString, TerminalWidget*> m_terminals;
};
