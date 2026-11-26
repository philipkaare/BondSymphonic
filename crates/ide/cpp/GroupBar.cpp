#include "GroupBar.h"
#include "Theme.h"
#include "WorkspaceLabel.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/smoke.cxx.h"
#include <QAction>
#include <QByteArray>
#include <QChar>
#include <QColor>
#include <QCoreApplication>
#include <QEvent>
#include <QFont>
#include <QFontMetrics>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QDialogButtonBox>
#include <QDrag>
#include <QDragMoveEvent>
#include <QDropEvent>
#include <QMimeData>
#include <QMouseEvent>
#include <QPixmap>
#include <QPushButton>
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
#include <algorithm>

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
/// The narrowest a task line under an agent's title is cut to, in pixels.
constexpr int kMinTaskWidth = 160;

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

/// What one agent tab is called, after its status dot: the agent's name, then
/// the repository it is working in and the branch it forked from. The tab's own
/// branch is deliberately absent; see `WorkspaceLabel.h`.
///
/// Empty when the tab JSON names nothing, which is the caller's signal to fall
/// back on the model's own label.
QString workspaceLabel(const QJsonObject& tab) {
    const QString name = tab.value(QStringLiteral("name")).toString();
    const QString origin = workspacelabel::origin(tab);
    if (name.isEmpty() || origin.isEmpty()) {
        return name;
    }
    return name + separator() + origin;
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

    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 2, 8, 2);
    layout->setSpacing(0);

    auto* captionRow = new QHBoxLayout();
    captionRow->setContentsMargins(0, 0, 0, 0);
    captionRow->setSpacing(8);
    auto* caption = new QLabel(QStringLiteral("Agents"), this);
    m_caption = caption;
    caption->setObjectName(QStringLiteral("GroupBarCaption"));
    QFont captionFont = caption->font();
    captionFont.setCapitalization(QFont::SmallCaps);
    captionFont.setBold(true);
    caption->setFont(captionFont);
    captionRow->addWidget(caption, 0, Qt::AlignVCenter);

    m_groupTabs = new QTabBar(this);
    m_groupTabs->setObjectName(QStringLiteral("GroupBarGroupTabs"));
    m_groupTabs->setExpanding(false);
    m_groupTabs->setContextMenuPolicy(Qt::CustomContextMenu);
    // Drag a group along the row to reorder it, and drop an agent tab on one
    // to move the agent there.
    m_groupTabs->setMovable(true);
    m_groupTabs->setAcceptDrops(true);
    m_groupTabs->installEventFilter(this);
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
    // Drag along the row to reorder; pull a tab up out of the row to drop it
    // on another group.
    m_agentTabs->setMovable(true);
    m_agentTabs->installEventFilter(this);
    // Taller and wider than a document tab: this is the row the user is meant
    // to find first, and a dot with a name and a branch behind it needs the
    // room. A stylesheet, because QTabBar takes its tab metrics from the style
    // rather than from the widget.
    m_agentTabs->setStyleSheet(
        QStringLiteral("QTabBar::tab { padding: 6px 14px; min-height: 32px; }"));
    agentRow->addWidget(m_agentTabs, 0);

    m_emptyLabel = new QLabel(QStringLiteral("No agents in this group yet"), this);
    m_emptyLabel->setObjectName(QStringLiteral("GroupBarEmptyLabel"));
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
    QObject::connect(m_agentTabs, &QTabBar::tabMoved, this, &GroupBar::onAgentTabMoved);
    QObject::connect(m_groupTabs, &QTabBar::tabMoved, this, &GroupBar::onGroupTabMoved);
    QObject::connect(m_groupTabs, &QWidget::customContextMenuRequested, this, &GroupBar::showGroupMenu);
    QObject::connect(m_agentTabs, &QWidget::customContextMenuRequested, this, &GroupBar::showAgentMenu);
    // `changed` and not `arrangementChanged`: a rebuild repaints the status dot
    // and the cost beside every tab as well as laying the tabs out, and those
    // move without the arrangement moving at all.
    QObject::connect(m_model, &GroupModel::changed, this, &GroupBar::rebuild);

    m_menuTestSteps = menuTestSteps();

    applyBand();
    rebuild();
}

void GroupBar::applyBand() {
    if (m_banding) {
        return;
    }
    m_banding = true;
    // Clearing first, because the band installed below is the bar's own from
    // then on: `palette()` would otherwise answer with the last band instead
    // of with the window behind it, and a band lifted off a band walks away
    // from the window a step on every theme change.
    setPalette(QPalette());
    // A fill of its own, so the two rows read as one band of agents rather
    // than as whatever happens to sit above the editor. Derived before it is
    // installed, and the captions with it, because installing it is what makes
    // `palette()` stop answering with the window.
    const QPalette basePalette = palette();
    QPalette barPalette;
    barPalette.setColor(QPalette::Window, theme::band(basePalette));
    QPalette captionPalette;
    captionPalette.setColor(QPalette::WindowText, theme::muted(basePalette));
    setPalette(barPalette);
    m_caption->setPalette(captionPalette);
    m_emptyLabel->setPalette(captionPalette);
    m_banding = false;
}

void GroupBar::changeEvent(QEvent* event) {
    QWidget::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        applyBand();
    }
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
        const QString named = workspaceLabel(tabJson);
        // `tabLabel` is the fallback, not the source: it already carries the
        // model's own glyph and name, so a state JSON that does not describe
        // this tab still leaves the tab named.
        QString label = named.isEmpty() ? m_model->tabLabel(m_displayGroup, i)
                                        : statusDot() + QLatin1Char(' ') + named;
        // Last, so it is the same distance from the tab's edge whether or not
        // the branch is spelled out beside the name.
        if (!tabJson.value(QStringLiteral("attention")).toString().isEmpty()) {
            label += attentionDot();
        }
        // The agent's task on a second line under its title, cut to the
        // title's width (but never narrower than a few words) so a long task
        // does not stretch the tab.
        const QString task = tabJson.value(QStringLiteral("task")).toString().trimmed();
        if (!task.isEmpty()) {
            const QFontMetrics metrics(m_agentTabs->font());
            const int width = std::max(metrics.horizontalAdvance(label), kMinTaskWidth);
            label += QLatin1Char('\n') + metrics.elidedText(task, Qt::ElideRight, width);
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
    // The same group: `QTabBar::moveTab` reports the current tab's new index
    // after a reorder, which `onGroupTabMoved` has already followed. Selecting
    // it again would put the group's first agent in front of the one the user
    // was on.
    if (m_rebuilding || index < 0 || index == m_displayGroup) {
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

namespace {
/// `Workspaces::moved_index` again, for the bar's own two indices: where the
/// item at `i` is once the one at `from` has been moved to `to`.
int movedIndex(int i, int from, int to) {
    if (i == from) {
        return to;
    }
    if (from < i && i <= to) {
        return i - 1;
    }
    if (to <= i && i < from) {
        return i + 1;
    }
    return i;
}

/// How far above or below the agent row the pointer has to go before a tab
/// being dragged leaves the row for the groups, rather than sliding along it.
constexpr int kTearOffPixels = 12;
} // namespace

void GroupBar::onAgentTabMoved(int from, int to) {
    if (m_rebuilding) {
        return;
    }
    m_model->moveTab(m_displayGroup, from, to);
}

void GroupBar::onGroupTabMoved(int from, int to) {
    if (m_rebuilding) {
        return;
    }
    // Both indices first: the model's move rebuilds the bar before it returns,
    // and that rebuild must find the bar already showing the group that moved
    // rather than read the shift as the model choosing another group.
    m_displayGroup = movedIndex(m_displayGroup, from, to);
    m_modelGroup = movedIndex(m_modelGroup, from, to);
    m_model->moveGroup(from, to);
}

bool GroupBar::eventFilter(QObject* watched, QEvent* event) {
    if (watched == m_agentTabs) {
        if (event->type() == QEvent::MouseButtonPress) {
            auto* mouse = static_cast<QMouseEvent*>(event);
            const int index = m_agentTabs->tabAt(mouse->position().toPoint());
            if (mouse->button() == Qt::LeftButton && index >= 0) {
                m_dragWorkspaceId = m_model->tabWorkspaceId(m_displayGroup, index);
                m_dragPressPos = mouse->position().toPoint();
            }
        } else if (event->type() == QEvent::MouseButtonRelease) {
            m_dragWorkspaceId.clear();
        } else if (event->type() == QEvent::MouseMove && !m_dragWorkspaceId.isEmpty()) {
            auto* mouse = static_cast<QMouseEvent*>(event);
            const int y = mouse->position().toPoint().y();
            if ((mouse->buttons() & Qt::LeftButton) &&
                (y < -kTearOffPixels || y > m_agentTabs->height() + kTearOffPixels) &&
                m_model->groupCount() > 1) {
                startGroupDrag();
                return true;
            }
        }
        return false;
    }
    if (watched == m_groupTabs) {
        switch (event->type()) {
        case QEvent::DragEnter:
        case QEvent::DragMove: {
            auto* drag = static_cast<QDragMoveEvent*>(event);
            const int target = dropGroupAt(drag->position().toPoint());
            if (target >= 0) {
                drag->setDropAction(Qt::MoveAction);
                drag->accept();
            } else {
                drag->ignore();
            }
            if (target != m_dropHover) {
                m_dropHover = target;
                // The tab it would land in, shown the way a tab is shown when
                // it is chosen; the displayed group comes back on leave or drop.
                m_rebuilding = true;
                m_groupTabs->setCurrentIndex(target >= 0 ? target : m_displayGroup);
                m_rebuilding = false;
            }
            return true;
        }
        case QEvent::DragLeave:
            m_dropHover = -1;
            m_rebuilding = true;
            m_groupTabs->setCurrentIndex(m_displayGroup);
            m_rebuilding = false;
            return true;
        case QEvent::Drop: {
            auto* drop = static_cast<QDropEvent*>(event);
            const int target = dropGroupAt(drop->position().toPoint());
            const QString id =
                QString::fromUtf8(drop->mimeData()->data(QString::fromLatin1(kWorkspaceMime)));
            m_dropHover = -1;
            m_rebuilding = true;
            m_groupTabs->setCurrentIndex(m_displayGroup);
            m_rebuilding = false;
            if (target < 0 || id.isEmpty()) {
                drop->ignore();
                return true;
            }
            drop->setDropAction(Qt::MoveAction);
            drop->accept();
            m_model->moveTabToGroup(id, m_model->groupName(target));
            return true;
        }
        default:
            return false;
        }
    }
    return QWidget::eventFilter(watched, event);
}

int GroupBar::dropGroupAt(const QPoint& pos) const {
    const int index = m_groupTabs->tabAt(pos);
    if (index < 0) {
        return -1;
    }
    // The agent is already there. The source group is the one the drag
    // started in, which is the one on show.
    if (index == m_displayGroup) {
        return -1;
    }
    return index;
}

void GroupBar::startGroupDrag() {
    const QString id = m_dragWorkspaceId;
    m_dragWorkspaceId.clear();
    // The row's own slide is over: a release where the press was lets it put
    // the tab down, since the real release will go to the drag instead.
    QMouseEvent release(QEvent::MouseButtonRelease, QPointF(m_dragPressPos),
                        m_agentTabs->mapToGlobal(QPointF(m_dragPressPos)), Qt::LeftButton,
                        Qt::NoButton, Qt::NoModifier);
    QCoreApplication::sendEvent(m_agentTabs, &release);

    int index = -1;
    for (int i = 0; i < m_model->tabCount(m_displayGroup); ++i) {
        if (m_model->tabWorkspaceId(m_displayGroup, i) == id) {
            index = i;
            break;
        }
    }
    if (index < 0) {
        return;
    }
    auto* mime = new QMimeData();
    mime->setData(QString::fromLatin1(kWorkspaceMime), id.toUtf8());
    auto* drag = new QDrag(this);
    drag->setMimeData(mime);
    const QRect rect = m_agentTabs->tabRect(index);
    drag->setPixmap(m_agentTabs->grab(rect));
    drag->setHotSpot(m_dragPressPos - rect.topLeft());
    drag->exec(Qt::MoveAction);
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

void GroupBar::setHarnessChoices(const QList<QPair<QString, QString>>& choices) {
    m_harnessChoices = choices;
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
    QAction* renameAction = workspaceId.isEmpty() ? nullptr : menu.addAction("Rename agent…");
    if (renameAction != nullptr) {
        renameAction->setObjectName(QStringLiteral("GroupBarRenameAgentAction"));
    }
    // Every harness but the one running here; a shell tab has no harness.
    QMenu* harnessMenu = nullptr;
    const QString adapter = workspaceId.isEmpty() ? QString() : m_model->workspaceAdapter(workspaceId);
    if (!adapter.isEmpty() && adapter != QLatin1String("terminal") && m_harnessChoices.size() > 1) {
        harnessMenu = menu.addMenu(QStringLiteral("Switch harness"));
        harnessMenu->setObjectName(QStringLiteral("GroupBarSwitchHarnessMenu"));
        for (const auto& [id, label] : m_harnessChoices) {
            QAction* action = harnessMenu->addAction(label);
            action->setData(id);
            action->setCheckable(true);
            action->setChecked(id == adapter);
            action->setEnabled(id != adapter);
        }
    }
    QAction* destroyAction =
        workspaceId.isEmpty()
            ? nullptr
            : menu.addAction(m_model->workspaceInPlace(workspaceId) ? "Close workspace…"
                                                                   : "Destroy workspace…");
    if (destroyAction != nullptr) {
        // The seam finds the item by this name, whichever verb it carries.
        destroyAction->setObjectName(QStringLiteral("GroupBarDestroyAction"));
    }

    QAction* chosen = execMenu(menu, globalPos);
    if (chosen == nullptr) {
        return;
    }
    if (chosen == newAction) {
        emit newAgentRequested();
    } else if (chosen == renameAction) {
        const QString currentName = m_model->workspaceName(workspaceId);
        if (currentName.isEmpty()) {
            QMessageBox::information(this, QStringLiteral("Rename agent"),
                QStringLiteral("This agent is no longer open."));
            return;
        }
        QInputDialog dialog(this);
        dialog.setObjectName(QStringLiteral("RenameAgentDialog"));
        dialog.setWindowTitle(QStringLiteral("Rename agent"));
        dialog.setLabelText(QStringLiteral("Agent name:"));
        dialog.setInputMode(QInputDialog::TextInput);
        dialog.setTextValue(currentName);
        const auto validate = [&dialog](const QString& text) {
            dialog.setOkButtonText(QStringLiteral("Rename"));
            if (auto* buttons = dialog.findChild<QDialogButtonBox*>()) {
                buttons->button(QDialogButtonBox::Ok)->setEnabled(!text.trimmed().isEmpty());
            }
        };
        QObject::connect(&dialog, &QInputDialog::textValueChanged, &dialog, validate);
        validate(dialog.textValue());
        if (dialog.exec() != QDialog::Accepted) { return; }
        if (!m_model->renameAgent(workspaceId, dialog.textValue())) {
            QMessageBox::information(this, QStringLiteral("Rename agent"),
                QStringLiteral("This agent is no longer open, or its name is blank."));
        }
    } else if (chosen == destroyAction) {
        emit destroyRequested(workspaceId, workspaceName);
    } else if (harnessMenu != nullptr && chosen->parent() == harnessMenu) {
        emit switchHarnessRequested(workspaceId, chosen->data().toString());
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
        if (action->text() == m_menuTestChoice ||
            (m_menuTestChoice == QStringLiteral("Destroy workspace…") &&
             action->objectName() == QLatin1String("GroupBarDestroyAction"))) {
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

// --- offscreen test entries --------------------------------------------------
//
// Compiled only into a development build: this is test code -- it builds
// strings and asserts -- and a shipped IDE has no caller for any of it.
// `build.rs` defines `BS_WIDGET_TESTS` for every profile but `release`, which
// is the one the packaged executable is built with.
#if defined(BS_WIDGET_TESTS)
#include <cstdint>

/// A tab names the repository the agent is in, not the branch generated from
/// the agent's own name.
extern "C" std::int32_t bs_widget_test_group_bar_names_the_repository() {
    const QJsonObject tab{
        { QStringLiteral("workspace_id"), QStringLiteral("ws_1") },
        { QStringLiteral("name"), QStringLiteral("agent-4") },
        { QStringLiteral("branch"), QStringLiteral("bs/agent-4/work") },
        { QStringLiteral("base_branch"), QStringLiteral("main") },
        { QStringLiteral("repo_path"), QStringLiteral("C:/git/BondSymphonic") },
        { QStringLiteral("worktree_path"), QStringLiteral("C:/git/.bs/agent-4") },
    };
    const QString label = workspaceLabel(tab);
    if (label.contains(QLatin1String("bs/agent-4/work"))) {
        // The generated branch is the agent's own name spelled a second way.
        return 1;
    }
    if (!label.contains(QLatin1String("agent-4")) ||
        !label.contains(QLatin1String("BondSymphonic")) ||
        !label.contains(QLatin1String("main"))) {
        return 2;
    }
    if (!workspacelabel::detail(tab).contains(QLatin1String("bs/agent-4/work"))) {
        // Still what gets merged, and a user who needs it in a git command
        // needs it exactly.
        return 3;
    }
    // The status bar and the window title have the width the tab has not, so
    // the repository is spelled in full there rather than by its last part.
    if (!workspacelabel::originFull(tab).contains(QLatin1String("C:/git/BondSymphonic"))) {
        return 4;
    }
    return 0;
}

/// The band comes back to where it started after a round trip through the other
/// palette, rather than a step further from the window each time.
///
/// `theme::band` lifts the *window* colour and `applyBand` installs the result
/// as this widget's own window colour, so the next band is lifted off the last
/// one unless the palette is cleared first. Nothing about that is visible in a
/// single switch: the bar simply drifts a shade paler every time the theme
/// changes, and is only obviously wrong after several. That is what makes it
/// worth a check rather than a comment -- removing the clear fails this on the
/// first round trip.
extern "C" std::int32_t bs_widget_test_group_bar_does_not_lift_a_band_off_a_band() {
    QPalette dark;
    dark.setColor(QPalette::Window, QColor(0x25, 0x25, 0x26));
    dark.setColor(QPalette::Base, QColor(0x1e, 0x1e, 0x1e));
    dark.setColor(QPalette::WindowText, QColor(0xdc, 0xdc, 0xdc));
    QPalette light;
    light.setColor(QPalette::Window, QColor(0xf0, 0xf0, 0xf0));
    light.setColor(QPalette::Base, QColor(0xff, 0xff, 0xff));
    light.setColor(QPalette::WindowText, QColor(0x1e, 0x1e, 0x1e));

    QWidget host;
    host.setPalette(dark);
    GroupModel model;
    auto* bar = new GroupBar(&model, &host);
    const QColor first = bar->palette().color(QPalette::Window);

    host.setPalette(light);
    QCoreApplication::processEvents();
    if (bar->palette().color(QPalette::Window) == first) {
        // The band never heard the palette change at all.
        return 1;
    }

    host.setPalette(dark);
    QCoreApplication::processEvents();
    if (bar->palette().color(QPalette::Window) != first) {
        return 2;
    }
    // Twice more, because one round trip of drift can be too small to see and
    // three is not.
    for (int i = 0; i < 2; ++i) {
        host.setPalette(light);
        QCoreApplication::processEvents();
        host.setPalette(dark);
        QCoreApplication::processEvents();
    }
    if (bar->palette().color(QPalette::Window) != first) {
        return 3;
    }
    return 0;
}

namespace {
/// A tab for `id` in the group called `group`, through the model's own entry.
bool addTestTab(GroupModel& model, const QString& id, const QString& group) {
    const QJsonObject info{
        { "id", id },           { "name", id },
        { "repo_path", "/r" },  { "base_branch", "main" },
        { "branch", "bs/" + id + "/work" }, { "worktree_path", "/w/" + id },
        { "created_at", "now" }, { "allowlist", QJsonArray{} },
        { "state", "creating" }, { "agents", QJsonArray{} },
        { "runs", QJsonArray{} },
    };
    return model.addTab(QString::fromUtf8(QJsonDocument(info).toJson()), group,
                        QStringLiteral("terminal"), QString(), QString());
}

/// The workspace ids of group `g`, in order, joined with commas.
QString tabOrder(GroupModel& model, int g) {
    QStringList ids;
    for (int i = 0; i < model.tabCount(g); ++i) {
        ids << model.tabWorkspaceId(g, i);
    }
    return ids.join(QLatin1Char(','));
}
} // namespace

/// Dragging reorders: an agent tab along its row, a group along its row, and
/// the active agent stays the one in front through both. An agent tab dropped
/// on another group's tab moves there; dropped on its own group, nothing moves.
extern "C" std::int32_t bs_widget_test_group_bar_drags_tabs_and_groups() {
    GroupModel model;
    model.loadState(QStringLiteral(
        R"({"groups":[{"id":"g1","name":"One","tabs":[]},{"id":"g2","name":"Two","tabs":[]}],)"
        R"("active_group":0,"active_tab":0})"));
    if (!addTestTab(model, QStringLiteral("a"), QStringLiteral("One")) ||
        !addTestTab(model, QStringLiteral("b"), QStringLiteral("One")) ||
        !addTestTab(model, QStringLiteral("c"), QStringLiteral("Two"))) {
        return 1;
    }
    model.setActive(0, 1); // b
    GroupBar bar(&model);
    auto* agents = bar.findChild<QTabBar*>(QStringLiteral("GroupBarAgentTabs"));
    auto* groups = bar.findChild<QTabBar*>(QStringLiteral("GroupBarGroupTabs"));
    if (agents == nullptr || groups == nullptr || !agents->isMovable() || !groups->isMovable() ||
        !groups->acceptDrops()) {
        return 2;
    }

    // Along the agent row: b to the front.
    agents->moveTab(1, 0);
    if (tabOrder(model, 0) != QLatin1String("b,a") || model.activeTabIndex() != 0 ||
        agents->currentIndex() != 0) {
        return 3;
    }

    // Along the group row: One after Two. b stays in front, and the bar keeps
    // showing One where it now is.
    groups->moveTab(0, 1);
    if (model.groupName(0) != QLatin1String("Two") || model.groupName(1) != QLatin1String("One") ||
        model.activeGroupIndex() != 1 || model.activeTabIndex() != 0 ||
        bar.currentGroupName() != QLatin1String("One") || agents->count() != 2) {
        return 4;
    }

    const auto dropOn = [&](int groupIndex, const QString& id) {
        auto* mime = new QMimeData();
        mime->setData(QString::fromLatin1(GroupBar::kWorkspaceMime), id.toUtf8());
        const QPointF at = QRectF(groups->tabRect(groupIndex)).center();
        QDragEnterEvent enter(at.toPoint(), Qt::MoveAction, mime, Qt::LeftButton, Qt::NoModifier);
        QCoreApplication::sendEvent(groups, &enter);
        const bool accepted = enter.isAccepted();
        QDropEvent drop(at, Qt::MoveAction, mime, Qt::LeftButton, Qt::NoModifier);
        QCoreApplication::sendEvent(groups, &drop);
        delete mime;
        return accepted;
    };
    // On its own group: refused, and nothing moves.
    if (dropOn(1, QStringLiteral("a")) || tabOrder(model, 1) != QLatin1String("b,a")) {
        return 5;
    }
    // On Two: a moves to the end of Two.
    if (!dropOn(0, QStringLiteral("a")) || tabOrder(model, 0) != QLatin1String("c,a") ||
        tabOrder(model, 1) != QLatin1String("b")) {
        return 6;
    }
    // The hover highlight gave the displayed group back.
    if (bar.currentGroupName() != QLatin1String("One")) {
        return 7;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
