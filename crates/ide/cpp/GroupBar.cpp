#include "GroupBar.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QAction>
#include <QColor>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QLineEdit>
#include <QMenu>
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

} // namespace

GroupBar::GroupBar(GroupModel* model, QWidget* parent) : QWidget(parent), m_model(model) {
    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    m_groupTabs = new QTabBar(this);
    m_groupTabs->setExpanding(false);
    m_groupTabs->setContextMenuPolicy(Qt::CustomContextMenu);
    layout->addWidget(m_groupTabs);

    auto* agentRow = new QHBoxLayout();
    agentRow->setContentsMargins(0, 0, 0, 0);
    agentRow->setSpacing(0);
    m_agentTabs = new QTabBar(this);
    m_agentTabs->setExpanding(false);
    m_agentTabs->setDocumentMode(true);
    m_agentTabs->setContextMenuPolicy(Qt::CustomContextMenu);
    agentRow->addWidget(m_agentTabs, 1);

    m_addButton = new QToolButton(this);
    m_addButton->setText("+");
    m_addButton->setToolTip("New agent");
    m_addButton->setAutoRaise(true);
    agentRow->addWidget(m_addButton, 0);
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
    for (int i = 0; i < tabs; ++i) {
        m_agentTabs->setTabText(i, m_model->tabLabel(m_displayGroup, i));
        m_agentTabs->setTabToolTip(i, m_model->tabTooltip(m_displayGroup, i));
        m_agentTabs->setTabTextColor(i, statusColour(m_model->tabStatus(m_displayGroup, i)));
    }

    const int activeTab = m_model->activeTabIndex();
    if (m_displayGroup == modelGroup && activeTab >= 0 && activeTab < tabs) {
        m_agentTabs->setCurrentIndex(activeTab);
    }

    m_rebuilding = false;
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

    QAction* chosen = menu.exec(m_groupTabs->mapToGlobal(pos));
    if (chosen == nullptr) {
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
