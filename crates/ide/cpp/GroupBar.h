#pragma once
#include <QString>
#include <QStringList>
#include <QWidget>

class GroupModel;
class QAction;
class QEvent;
class QJsonArray;
class QLabel;
class QMenu;
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
    /// `GroupModel::tabStatus`'s code for an agent or workspace in error: the
    /// one status this bar paints in the theme's red. Named here because the
    /// window's seam has to find a tab in error too, and the two files spelling
    /// the number separately is how they come to disagree.
    static constexpr int kStatusError = 3;

    explicit GroupBar(GroupModel* model, QWidget* parent = nullptr);

    /// Name of the group whose tabs are currently shown, or empty when there is
    /// no group. The New Agent dialog defaults to it.
    QString currentGroupName() const;

signals:
    /// The New agent button, or "New agent…" from the agent tab context menu.
    void newAgentRequested();
    /// "Destroy workspace…" on the agent tab for this workspace. Both the id
    /// and the name are read off the tab that was clicked, *before* the menu is
    /// run, so a tab added or removed while the menu is up cannot move the
    /// request onto a different workspace and the window's confirmation can
    /// name the one the user pointed at.
    void destroyRequested(const QString& workspaceId, const QString& workspaceName);
    /// "Close group…" on the group tab with this name, read before the menu is
    /// run for the same reason. The window asks what to do with each of its
    /// workspaces; the bar only says which group.
    void closeGroupRequested(const QString& groupName);

protected:
    /// A palette change is the theme moving under the bar. Its band fill and
    /// the grey of the two captions are derived from the window's own colours,
    /// so they have to be derived again rather than kept.
    void changeEvent(QEvent* event) override;

private:
    /// Re-derives the band fill and the muted captions from the palette the
    /// bar would otherwise wear. Called from the constructor and from every
    /// palette change.
    void applyBand();
    void rebuild();
    /// The displayed group's tabs, as the model's `stateJson` describes them.
    /// Neither a tab's name nor its branch is an invokable of its own, and the
    /// label is assembled here rather than in Rust, so the one JSON the model
    /// already publishes is where both come from. Empty when the state does not
    /// describe that group.
    QJsonArray displayedTabsJson() const;
    void onGroupCurrentChanged(int index);
    void onAgentCurrentChanged(int index);
    /// The two context-menu slots. All they do is turn the click into a tab or
    /// group index and a screen position and hand both on, so that the two
    /// below can be opened over a named index by something that is not a mouse
    /// -- which is what the `BS_MENU_TEST` seam needs.
    void showGroupMenu(const QPoint& pos);
    void showAgentMenu(const QPoint& pos);
    /// Builds the menu for the group or tab at `index`, runs it, and acts on
    /// what was chosen. Everything acted on -- the group's name, the
    /// workspace's id and name -- is read off the model *before* the menu runs,
    /// because the menu is a nested event loop and a daemon event arriving
    /// during it can move what `index` names.
    void openGroupMenu(int index, const QPoint& globalPos);
    void openAgentMenu(int index, const QPoint& globalPos);
    /// Where the group called `name` is now, or -1 for a name no group has.
    /// What a menu item resolved before a nested event loop is turned back into
    /// an index with, once that loop has returned.
    int groupIndexOf(const QString& name) const;
    /// Runs `menu` and answers with the item chosen, or null for a menu that
    /// was dismissed. The one place the widget gives up control to a nested
    /// event loop, and therefore the one place the model can change underneath
    /// it: everything a menu acts on is resolved before this is called.
    ///
    /// Under the `BS_MENU_TEST` seam it answers without a user, having first
    /// made exactly that change. Unset in every ordinary run.
    QAction* execMenu(QMenu& menu, const QPoint& globalPos);
    /// Test seam, inert unless `BS_MENU_TEST` names a step this bar runs.
    /// Schedules [`runMenuTests`] once the model is large enough for the menus
    /// to have something to act on, and only ever once.
    void armMenuTest();
    /// Opens each menu the seam asked for over a named index, having asked the
    /// smoke script for the model state to install while it is up. The states
    /// themselves are the script's: this widget holds no application state, and
    /// a fixture is application state.
    void runMenuTests();

    GroupModel* m_model;
    QTabBar* m_groupTabs = nullptr;
    QTabBar* m_agentTabs = nullptr;
    QToolButton* m_addButton = nullptr;
    /// Says so, in the agent row, while the displayed group has no tabs. An
    /// empty group is otherwise a bare strip with a button on it.
    QLabel* m_emptyLabel = nullptr;
    /// The "Agents" caption, kept because its grey is re-derived whenever the
    /// palette changes.
    QLabel* m_caption = nullptr;
    /// Set while [`applyBand`] is installing a palette. Installing one is
    /// itself a palette change, which arrives back here as `changeEvent`, and
    /// without this the bar would band its own band until the stack ran out.
    bool m_banding = false;
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
    /// The steps `BS_MENU_TEST` asks for, empty in every ordinary run.
    QStringList m_menuTestSteps;
    /// Whether [`runMenuTests`] has already been scheduled.
    bool m_menuTestArmed = false;
    /// The item [`execMenu`] answers with, and the state it installs first.
    /// Both empty except while a test step is running a menu.
    QString m_menuTestChoice;
    QString m_menuTestState;
};
