#pragma once
#include <QString>
#include <QWidget>

class GroupModel;
class QJsonArray;
class QLabel;
class QTabBar;
class QToolButton;

/// The two stacked rows above the editor: the "Agents" caption and the group
/// tabs on top, the displayed group's agent tabs below, with a "New agent"
/// button beside them.
///
/// The widget holds no application state. Every rebuild reads labels, tooltips
/// and status codes back out of `GroupModel`; the only thing decided here is
/// which colour a status code paints in and which group's tabs are on show.
class GroupBar : public QWidget {
    Q_OBJECT
public:
    explicit GroupBar(GroupModel* model, QWidget* parent = nullptr);

    /// Name of the group whose tabs are currently shown, or empty when there is
    /// no group. The New Agent dialog defaults to it.
    QString currentGroupName() const;

signals:
    /// The New agent button, or "New agent…" from the agent tab context menu.
    void newAgentRequested();
    /// "Destroy workspace…" on the agent tab for this workspace id.
    void destroyRequested(const QString& workspaceId);
    /// "Close group…" on the group tab at this index. The window asks what to
    /// do with each of its workspaces; the bar only says which group.
    void closeGroupRequested(int groupIndex);

private:
    void rebuild();
    /// The displayed group's tabs, as the model's `stateJson` describes them.
    /// Neither a tab's name nor its branch is an invokable of its own, and the
    /// label is assembled here rather than in Rust, so the one JSON the model
    /// already publishes is where both come from. Empty when the state does not
    /// describe that group.
    QJsonArray displayedTabsJson() const;
    void onGroupCurrentChanged(int index);
    void onAgentCurrentChanged(int index);
    void showGroupMenu(const QPoint& pos);
    void showAgentMenu(const QPoint& pos);

    GroupModel* m_model;
    QTabBar* m_groupTabs = nullptr;
    QTabBar* m_agentTabs = nullptr;
    QToolButton* m_addButton = nullptr;
    /// Says so, in the agent row, while the displayed group has no tabs. An
    /// empty group is otherwise a bare strip with a button on it.
    QLabel* m_emptyLabel = nullptr;
    /// Which group's tabs the agent row shows. Usually the model's active
    /// group; it differs only while an empty group is selected, which the model
    /// cannot represent because it has no tab to make active.
    int m_displayGroup = 0;
    /// The model's active group as of the last rebuild, so a change made by the
    /// model (a new tab, a session restore) can be told from one made here.
    int m_modelGroup = 0;
    /// Set while `rebuild()` writes into the tab bars, so the `currentChanged`
    /// signals it provokes are not read back as user clicks.
    bool m_rebuilding = false;
};
