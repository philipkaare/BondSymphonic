#include "GroupBar.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/smoke.cxx.h"
#include <QAction>
#include <QByteArray>
#include <QChar>
#include <QColor>
#include <QFont>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QMenu>
#include <QPalette>
#include <QStyle>
#include <QStringList>
#include <QTabBar>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>

namespace {

/// The one mapping table C++ is allowed to hold: `GroupModel::tabStatus` codes
/// onto tab text colours. The status words come from `statusWord` and the tab
/// glyph from `tabLabel`, so nothing else here switches on the code.
///
/// `dark` is whether the bar sits on a dark palette. It matters only to the
/// error red, which is the one entry read out of `theme` rather than picked
/// here: the theme's accents are chosen as fills for a light background and
/// have to go through `theme::ink` before they are drawn as text.
QColor statusColour(int status, bool dark) {
    switch (status) {
    case 0:
        return QColor(0x88, 0x88, 0x88); // idle
    case 1:
        return QColor(0x2f, 0x80, 0xed); // working
    case 2:
        return QColor(0xf2, 0xa9, 0x00); // waiting for permission
    case GroupBar::kStatusError:
        // The IDE's one red. `theme` has no "error" of its own; a line that is
        // gone and an agent that failed are the same judgement, so the accent
        // is read from there rather than written out again here.
        return theme::ink(theme::removed(), dark);
    case 4:
        return QColor(0x27, 0xae, 0x60); // done
    case 5:
        return QColor(0x88, 0x88, 0x88); // creating
    case 6:
        return QColor(0xf2, 0x99, 0x4a); // sandbox down
    default:
        return QColor();
    }
}

/// The two spaces, middle dot and two spaces between a name and its branch.
/// U+00B7 as a code point rather than as a character in a literal, so no
/// compiler's idea of this file's source encoding can change what it means.
QString separator() {
    return QStringLiteral("  ") + QString(QChar(0x00B7)) + QStringLiteral("  ");
}

/// U+25CF, the black circle, at the head of an agent tab. It is painted in the
/// tab's text colour, which is the one `statusColour` gives the status.
QString statusDot() {
    return QString(QChar(0x25CF));
}

/// U+2022, the bullet, at the *tail* of a tab whose agent wants the user --
/// today, one blocked on a permission request in a tab that is not in front.
///
/// A second glyph rather than a colour: the head dot already carries the
/// daemon's status, and an agent waiting to be allowed a tool is still
/// perfectly healthy. The tab's tooltip says what it is waiting for.
QString attentionDot() {
    return QStringLiteral(" ") + QString(QChar(0x2022));
}

/// A `QString` as the borrowed UTF-8 slice the script's fixture builders take.
/// The `QByteArray` is the owner and must outlive the call, so it is named.
::rust::Str rustStr(const QByteArray& utf8) {
    return ::rust::Str(utf8.constData(), static_cast<std::size_t>(utf8.size()));
}

/// The fixture the script built, as a `QString`. Empty means the script found
/// nothing to rearrange, and the seam then installs nothing rather than
/// emptying the model.
QString menuTestFixture(const ::rust::String& state) {
    return QString::fromUtf8(state.data(), static_cast<qsizetype>(state.size()));
}

/// The steps in `BS_MENU_TEST`. **Test-only**: the variable is unset in every
/// ordinary run, this is then empty, and nothing the seam reaches executes.
///
/// It stands in for a user at a context menu: a menu is opened over a named tab
/// or group, the model is changed while it is up -- the daemon event that the
/// resolve-before-`exec` rule in the two menus below exists for -- and an item
/// is chosen. What the bar emitted then reaches `tests/smoke.rs` as a line on
/// the IDE's stdout, which is the only way to see it: the confirmations these
/// menus lead to are modal, and an automated run has nobody to answer them.
QStringList menuTestSteps() {
    // Two variables and not one. `BS_MENU_TEST` on its own would turn every
    // destroy and every close-group into a silent no-op for anyone who happened
    // to have it set, which is a sharper edge than a test hook should have.
    // `BS_SMOKE_SCRIPT` is the IDE's existing "this is an automated run" switch
    // and is never set by a user; the seam is inert without it. `MainWindow`
    // gates its half of the seam on the same pair.
    if (qEnvironmentVariableIsEmpty("BS_SMOKE_SCRIPT")) {
        return QStringList();
    }
    return qEnvironmentVariable("BS_MENU_TEST").split(QLatin1Char(','), Qt::SkipEmptyParts);
}

} // namespace

GroupBar::GroupBar(GroupModel* model, QWidget* parent) : QWidget(parent), m_model(model) {
    // A fill of its own, so the two rows read as one band of agents rather than
    // as whatever happens to sit above the editor.
    setAutoFillBackground(true);
    // Derived from the unwashed palette, and kept for the captions below: once
    // the band colour is installed, `palette()` would hand back the wash.
    const QPalette basePalette = palette();
    QPalette barPalette = basePalette;
    barPalette.setColor(QPalette::Window, theme::band(basePalette));
    setPalette(barPalette);

    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 2, 8, 2);
    layout->setSpacing(0);

    auto* captionRow = new QHBoxLayout();
    captionRow->setContentsMargins(0, 0, 0, 0);
    captionRow->setSpacing(8);
    auto* caption = new QLabel(QStringLiteral("Agents"), this);
    caption->setObjectName(QStringLiteral("GroupBarCaption"));
    QFont captionFont = caption->font();
    captionFont.setCapitalization(QFont::SmallCaps);
    captionFont.setBold(true);
    caption->setFont(captionFont);
    QPalette captionPalette = caption->palette();
    captionPalette.setColor(QPalette::WindowText, theme::muted(basePalette));
    caption->setPalette(captionPalette);
    captionRow->addWidget(caption, 0, Qt::AlignVCenter);

    m_groupTabs = new QTabBar(this);
    m_groupTabs->setObjectName(QStringLiteral("GroupBarGroupTabs"));
    m_groupTabs->setExpanding(false);
    m_groupTabs->setContextMenuPolicy(Qt::CustomContextMenu);
    captionRow->addWidget(m_groupTabs, 0, Qt::AlignBottom);
    captionRow->addStretch(1);
    layout->addLayout(captionRow);

    auto* agentRow = new QHBoxLayout();
    agentRow->setContentsMargins(0, 0, 0, 0);
    agentRow->setSpacing(6);
    m_agentTabs = new QTabBar(this);
    m_agentTabs->setObjectName(QStringLiteral("GroupBarAgentTabs"));
    m_agentTabs->setExpanding(false);
    m_agentTabs->setDocumentMode(true);
    m_agentTabs->setContextMenuPolicy(Qt::CustomContextMenu);
    // Taller and wider than a document tab: this is the row the user is meant
    // to find first, and a dot with a name and a branch behind it needs the
    // room. A stylesheet, because QTabBar takes its tab metrics from the style
    // rather than from the widget.
    m_agentTabs->setStyleSheet(
        QStringLiteral("QTabBar::tab { padding: 6px 14px; min-height: 32px; }"));
    agentRow->addWidget(m_agentTabs, 0);

    m_emptyLabel = new QLabel(QStringLiteral("No agents in this group yet"), this);
    m_emptyLabel->setObjectName(QStringLiteral("GroupBarEmptyLabel"));
    QPalette emptyPalette = m_emptyLabel->palette();
    emptyPalette.setColor(QPalette::WindowText, theme::muted(basePalette));
    m_emptyLabel->setPalette(emptyPalette);
    m_emptyLabel->setVisible(false);
    agentRow->addWidget(m_emptyLabel, 0, Qt::AlignVCenter);

    m_addButton = new QToolButton(this);
    m_addButton->setObjectName(QStringLiteral("GroupBarNewAgentButton"));
    m_addButton->setText(QStringLiteral("New agent"));
    m_addButton->setIcon(style()->standardIcon(QStyle::SP_FileDialogNewFolder));
    m_addButton->setToolButtonStyle(Qt::ToolButtonTextBesideIcon);
    m_addButton->setToolTip(QStringLiteral("Create a workspace and start an agent in it"));
    m_addButton->setAutoRaise(true);
    agentRow->addWidget(m_addButton, 0, Qt::AlignVCenter);
    agentRow->addStretch(1);
    layout->addLayout(agentRow);

    QObject::connect(m_addButton, &QToolButton::clicked, this, &GroupBar::newAgentRequested);
    QObject::connect(m_groupTabs, &QTabBar::currentChanged, this, &GroupBar::onGroupCurrentChanged);
    QObject::connect(m_agentTabs, &QTabBar::currentChanged, this, &GroupBar::onAgentCurrentChanged);
    QObject::connect(m_groupTabs, &QWidget::customContextMenuRequested, this, &GroupBar::showGroupMenu);
    QObject::connect(m_agentTabs, &QWidget::customContextMenuRequested, this, &GroupBar::showAgentMenu);
    // `changed` and not `arrangementChanged`: a rebuild repaints the status dot
    // and the cost beside every tab as well as laying the tabs out, and those
    // move without the arrangement moving at all.
    QObject::connect(m_model, &GroupModel::changed, this, &GroupBar::rebuild);

    m_menuTestSteps = menuTestSteps();

    rebuild();
}

QString GroupBar::currentGroupName() const {
    const int index = m_groupTabs->currentIndex();
    return index >= 0 ? m_model->groupName(index) : QString();
}

void GroupBar::rebuild() {
    m_rebuilding = true;

    const int groups = m_model->groupCount();
    while (m_groupTabs->count() > groups) {
        m_groupTabs->removeTab(m_groupTabs->count() - 1);
    }
    while (m_groupTabs->count() < groups) {
        m_groupTabs->addTab(QString());
    }
    for (int i = 0; i < groups; ++i) {
        m_groupTabs->setTabText(i, m_model->groupName(i));
    }

    // Follow the model when it moved the active group itself (a tab was added,
    // or a session was restored); otherwise keep showing what the user picked,
    // which is the only way an empty group can stay on screen.
    const int modelGroup = m_model->activeGroupIndex();
    if (modelGroup != m_modelGroup) {
        m_modelGroup = modelGroup;
        m_displayGroup = modelGroup;
    }
    if (m_displayGroup >= groups) {
        m_displayGroup = groups - 1;
    }
    if (m_displayGroup >= 0) {
        m_groupTabs->setCurrentIndex(m_displayGroup);
    }

    const int tabs = m_model->tabCount(m_displayGroup);
    while (m_agentTabs->count() > tabs) {
        m_agentTabs->removeTab(m_agentTabs->count() - 1);
    }
    while (m_agentTabs->count() < tabs) {
        m_agentTabs->addTab(QString());
    }
    const QJsonArray tabsJson = displayedTabsJson();
    // Once for the whole row: the palette does not change between two tabs of
    // the same rebuild, and `isDark` reads and compares colours.
    const bool dark = theme::isDark(palette());
    for (int i = 0; i < tabs; ++i) {
        const QJsonObject tabJson = i < tabsJson.size() ? tabsJson.at(i).toObject() : QJsonObject();
        const QString name = tabJson.value(QStringLiteral("name")).toString();
        const QString branch = tabJson.value(QStringLiteral("branch")).toString();
        // `tabLabel` is the fallback, not the source: it already carries the
        // model's own glyph and name, so a state JSON that does not describe
        // this tab still leaves the tab named.
        QString label = name.isEmpty() ? m_model->tabLabel(m_displayGroup, i)
                                       : statusDot() + QLatin1Char(' ') + name;
        if (!name.isEmpty() && !branch.isEmpty()) {
            label += separator() + branch;
        }
        // Last, so it is the same distance from the tab's edge whether or not
        // the branch is spelled out beside the name.
        if (!tabJson.value(QStringLiteral("attention")).toString().isEmpty()) {
            label += attentionDot();
        }
        m_agentTabs->setTabText(i, label);
        m_agentTabs->setTabToolTip(i, m_model->tabTooltip(m_displayGroup, i));
        m_agentTabs->setTabTextColor(i,
                                     statusColour(m_model->tabStatus(m_displayGroup, i), dark));
    }
    // An empty group is a strip with nothing on it; the label says which of the
    // two it is, where the tabs would have been.
    m_agentTabs->setVisible(tabs > 0);
    m_emptyLabel->setVisible(tabs == 0);

    const int activeTab = m_model->activeTabIndex();
    if (m_displayGroup == modelGroup && activeTab >= 0 && activeTab < tabs) {
        m_agentTabs->setCurrentIndex(activeTab);
    }

    m_rebuilding = false;
    armMenuTest();
}

QJsonArray GroupBar::displayedTabsJson() const {
    const QJsonObject state = QJsonDocument::fromJson(m_model->getStateJson().toUtf8()).object();
    const QJsonArray groups = state.value(QStringLiteral("groups")).toArray();
    if (m_displayGroup < 0 || m_displayGroup >= groups.size()) {
        return QJsonArray();
    }
    return groups.at(m_displayGroup).toObject().value(QStringLiteral("tabs")).toArray();
}

void GroupBar::onGroupCurrentChanged(int index) {
    if (m_rebuilding || index < 0) {
        return;
    }
    m_displayGroup = index;
    // Selecting a group selects its first tab. An empty group has none, so the
    // model answers false and keeps its previous active tab; the rebuild below
    // still swaps the agent row over to the empty group.
    m_model->setActive(index, 0);
    rebuild();
}

void GroupBar::onAgentCurrentChanged(int index) {
    if (m_rebuilding || index < 0) {
        return;
    }
    m_model->setActive(m_displayGroup, index);
}

void GroupBar::showGroupMenu(const QPoint& pos) {
    openGroupMenu(m_groupTabs->tabAt(pos), m_groupTabs->mapToGlobal(pos));
}

void GroupBar::showAgentMenu(const QPoint& pos) {
    openAgentMenu(m_agentTabs->tabAt(pos), m_agentTabs->mapToGlobal(pos));
}

void GroupBar::openGroupMenu(int index, const QPoint& globalPos) {
    // Read off the tab that was clicked, before the menu takes over the event
    // loop. Anything the daemon says while it is up goes through the model, so
    // `index` may name a different group by the time an item is chosen -- and
    // the item behind it closes every workspace in one.
    const QString groupName = index >= 0 ? m_model->groupName(index) : QString();

    QMenu menu(this);
    QAction* addAction = menu.addAction("New group…");
    QAction* renameAction = groupName.isEmpty() ? nullptr : menu.addAction("Rename group…");
    QAction* closeAction = nullptr;
    if (!groupName.isEmpty()) {
        menu.addSeparator();
        closeAction = menu.addAction("Close group…");
    }

    QAction* chosen = execMenu(menu, globalPos);
    if (chosen == nullptr) {
        return;
    }
    if (chosen == closeAction) {
        emit closeGroupRequested(groupName);
        return;
    }
    if (chosen == addAction) {
        bool ok = false;
        const QString name =
            QInputDialog::getText(this, "New group", "Group name:", QLineEdit::Normal, QString(), &ok);
        if (ok && !name.trimmed().isEmpty()) {
            m_model->addGroup(name.trimmed());
        }
    } else if (chosen == renameAction) {
        bool ok = false;
        const QString name = QInputDialog::getText(this, "Rename group", "Group name:", QLineEdit::Normal,
                                                   groupName, &ok);
        if (!ok || name.trimmed().isEmpty()) {
            return;
        }
        // The input dialog is a second nested event loop, so the group is
        // looked up again by the name resolved at the click. Renaming whatever
        // sits at the old index now is the same bug one layer down.
        const int target = groupIndexOf(groupName);
        if (target < 0) {
            // Closed, or renamed by something else, while the input dialog was
            // up. Told rather than dropped, for the same reason as the refusal
            // below: the user typed a new name and pressed OK, and a menu item
            // that silently does nothing reads as a bug in the IDE.
            QMessageBox::information(this, QStringLiteral("Rename group"),
                                     QStringLiteral("There is no longer a group called \"%1\".")
                                         .arg(groupName));
            return;
        }
        if (!m_model->renameGroup(target, name.trimmed())) {
            // The one way a rename is refused is a name another group already
            // has, and a rename that silently did nothing would read as a bug.
            QMessageBox::information(this, QStringLiteral("Rename group"),
                                     QStringLiteral("There is already a group called \"%1\".")
                                         .arg(name.trimmed()));
        }
    }
}

void GroupBar::openAgentMenu(int index, const QPoint& globalPos) {
    // Both read before the menu runs, for the reason spelled out in
    // `openGroupMenu`: the id is what gets destroyed and the name is what the
    // window's confirmation asks about, so neither can drift onto the tab that
    // happens to hold this index once the menu closes.
    const QString workspaceId =
        index >= 0 ? m_model->tabWorkspaceId(m_displayGroup, index) : QString();
    const QString workspaceName =
        workspaceId.isEmpty() ? QString() : m_model->workspaceName(workspaceId);

    QMenu menu(this);
    QAction* newAction = menu.addAction("New agent…");
    QAction* destroyAction =
        workspaceId.isEmpty() ? nullptr : menu.addAction("Destroy workspace…");

    QAction* chosen = execMenu(menu, globalPos);
    if (chosen == nullptr) {
        return;
    }
    if (chosen == newAction) {
        emit newAgentRequested();
    } else if (chosen == destroyAction) {
        emit destroyRequested(workspaceId, workspaceName);
    }
}

int GroupBar::groupIndexOf(const QString& name) const {
    if (name.isEmpty()) {
        return -1;
    }
    for (int i = 0; i < m_model->groupCount(); ++i) {
        if (m_model->groupName(i) == name) {
            return i;
        }
    }
    return -1;
}

QAction* GroupBar::execMenu(QMenu& menu, const QPoint& globalPos) {
    if (m_menuTestChoice.isEmpty()) {
        return menu.exec(globalPos);
    }
    // The seam, and the whole of what it does: the model moves while the menu
    // is up, exactly as a daemon event would move it, and then an item is
    // chosen. Nothing here runs unless `BS_MENU_TEST` armed it.
    if (!m_menuTestState.isEmpty()) {
        // Empty means the script found nothing to rearrange. Loading it would
        // empty the model, and the step would then pass for the wrong reason.
        m_model->loadState(m_menuTestState);
    }
    for (QAction* action : menu.actions()) {
        if (action->text() == m_menuTestChoice) {
            return action;
        }
    }
    return nullptr;
}

void GroupBar::armMenuTest() {
    if (m_menuTestArmed || m_menuTestSteps.isEmpty()) {
        return;
    }
    // Both menus need something to act on, and the change worth making is one
    // that moves an index onto a different target: two groups, and two tabs in
    // the one on show.
    if (m_model->groupCount() < 2 || m_model->tabCount(m_displayGroup) < 2) {
        return;
    }
    m_menuTestArmed = true;
    // Not from inside `rebuild`: a step installs a state of its own, which
    // rebuilds the bar again.
    QTimer::singleShot(0, this, &GroupBar::runMenuTests);
}

void GroupBar::runMenuTests() {
    const QByteArray stateUtf8 = m_model->getStateJson().toUtf8();
    for (const QString& step : m_menuTestSteps) {
        bool ran = true;
        if (step == QLatin1String("destroy")) {
            // The second tab of the group on show, with that group's tabs
            // reversed under the menu.
            m_menuTestState = menuTestFixture(
                bsMenuTestStateWithTabsReversed(rustStr(stateUtf8), m_displayGroup));
            m_menuTestChoice = QStringLiteral("Destroy workspace…");
            openAgentMenu(1, QPoint());
        } else if (step == QLatin1String("destroy-gone")) {
            // The same tab, taken out of the model altogether under the menu:
            // the workspace was destroyed while the question was being read.
            // The bar still emits the id and the name it resolved before the
            // menu, and what the window does with a workspace it can no longer
            // find is the whole of this step.
            m_menuTestState = menuTestFixture(
                bsMenuTestStateWithoutTab(rustStr(stateUtf8), m_displayGroup, 1));
            m_menuTestChoice = QStringLiteral("Destroy workspace…");
            openAgentMenu(1, QPoint());
        } else if (step == QLatin1String("close-group")) {
            // The second group, brought to the front under the menu.
            m_menuTestState = menuTestFixture(bsMenuTestStateWithGroupsRotated(rustStr(stateUtf8)));
            m_menuTestChoice = QStringLiteral("Close group…");
            openGroupMenu(1, QPoint());
        } else {
            // A word for one of the seam's other reports, which are armed in
            // the window and have nothing to do with a menu.
            ran = false;
        }
        m_menuTestChoice.clear();
        m_menuTestState.clear();
        if (ran) {
            // After the menu, not inside it: the window's handler runs within
            // `openAgentMenu`, so a step reported here has already printed
            // whatever it was going to print -- including the nothing that
            // `destroy-gone` expects. This is what the script's `quit` step
            // waits for instead of a fixed delay.
            const QByteArray name = step.toUtf8();
            bsMenuTestReported(rustStr(name));
        }
    }
}
