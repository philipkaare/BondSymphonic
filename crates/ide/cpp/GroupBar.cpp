#include "GroupBar.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QAction>
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
#include <QMenu>
#include <QPalette>
#include <QStyle>
#include <QTabBar>
#include <QToolButton>
#include <QVBoxLayout>

namespace {

/// The one mapping table C++ is allowed to hold: `GroupModel::tabStatus` codes
/// onto tab text colours. The status words come from `statusWord` and the tab
/// glyph from `tabLabel`, so nothing else here switches on the code.
QColor statusColour(int status) {
    switch (status) {
    case 0:
        return QColor(0x88, 0x88, 0x88); // idle
    case 1:
        return QColor(0x2f, 0x80, 0xed); // working
    case 2:
        return QColor(0xf2, 0xa9, 0x00); // waiting for permission
    case 3:
        return QColor(0xeb, 0x57, 0x57); // error
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
    QObject::connect(m_model, &GroupModel::changed, this, &GroupBar::rebuild);

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
        m_agentTabs->setTabText(i, label);
        m_agentTabs->setTabToolTip(i, m_model->tabTooltip(m_displayGroup, i));
        m_agentTabs->setTabTextColor(i, statusColour(m_model->tabStatus(m_displayGroup, i)));
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
    const int index = m_groupTabs->tabAt(pos);
    QMenu menu(this);
    QAction* addAction = menu.addAction("New group…");
    QAction* renameAction = index >= 0 ? menu.addAction("Rename group…") : nullptr;
    QAction* closeAction = nullptr;
    if (index >= 0) {
        menu.addSeparator();
        closeAction = menu.addAction("Close group…");
    }

    QAction* chosen = menu.exec(m_groupTabs->mapToGlobal(pos));
    if (chosen == nullptr) {
        return;
    }
    if (chosen == closeAction) {
        emit closeGroupRequested(index);
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
                                                   m_model->groupName(index), &ok);
        if (ok && !name.trimmed().isEmpty()) {
            m_model->renameGroup(index, name.trimmed());
        }
    }
}

void GroupBar::showAgentMenu(const QPoint& pos) {
    const int index = m_agentTabs->tabAt(pos);
    QMenu menu(this);
    QAction* newAction = menu.addAction("New agent…");
    QAction* destroyAction = index >= 0 ? menu.addAction("Destroy workspace…") : nullptr;

    QAction* chosen = menu.exec(m_agentTabs->mapToGlobal(pos));
    if (chosen == nullptr) {
        return;
    }
    if (chosen == newAction) {
        emit newAgentRequested();
    } else if (chosen == destroyAction) {
        const QString id = m_model->tabWorkspaceId(m_displayGroup, index);
        if (!id.isEmpty()) {
            emit destroyRequested(id);
        }
    }
}
