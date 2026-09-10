#include "CloseGroupDialog.h"
#include "ChangesToolbar.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QLabel>
#include <QPushButton>
#include <QVBoxLayout>

namespace {

/// The three choices, in the order they are offered. Keep is first and is the
/// default: closing a group must not be the fast path to destroying work.
const CloseGroupAction kActions[] = { CloseGroupAction::Keep, CloseGroupAction::Merge,
                                      CloseGroupAction::Discard };

QString actionLabel(CloseGroupAction action) {
    switch (action) {
    case CloseGroupAction::Merge:
        return QStringLiteral("Merge into its base");
    case CloseGroupAction::Discard:
        return QStringLiteral("Discard (destroy it)");
    case CloseGroupAction::Keep:
    default:
        return QStringLiteral("Keep (move to Unsorted)");
    }
}

} // namespace

CloseGroupDialog::CloseGroupDialog(const QString& groupName,
                                   const QList<CloseGroupChoice>& workspaces, QWidget* parent)
    : QDialog(parent), m_choices(workspaces) {
    setWindowTitle(QStringLiteral("Close group"));
    setModal(true);

    auto* outer = new QVBoxLayout(this);
    auto* intro = new QLabel(this);
    intro->setTextFormat(Qt::PlainText);
    intro->setWordWrap(true);
    intro->setText(
        workspaces.isEmpty()
            ? QStringLiteral("Close the group \"%1\"? It has no workspaces.").arg(groupName)
            : QStringLiteral("Close the group \"%1\". Choose what happens to each of its %2 "
                             "workspaces; anything kept moves to Unsorted.")
                  .arg(groupName)
                  .arg(workspaces.size()));
    outer->addWidget(intro);

    auto* form = new QFormLayout();
    outer->addLayout(form);
    for (const CloseGroupChoice& choice : workspaces) {
        auto* combo = new QComboBox(this);
        combo->setObjectName(QStringLiteral("CloseGroupChoice_") + choice.workspaceId);
        for (const CloseGroupAction action : kActions) {
            combo->addItem(actionLabel(action), static_cast<int>(action));
        }
        combo->setCurrentIndex(0);
        m_combos.append(combo);
        form->addRow(choice.name, combo);
    }

    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText(QStringLiteral("Close group"));
    outer->addWidget(m_buttons);
    QObject::connect(m_buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    QObject::connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
}

QList<CloseGroupChoice> CloseGroupDialog::choices() const {
    QList<CloseGroupChoice> answers = m_choices;
    for (int i = 0; i < answers.size() && i < m_combos.size(); ++i) {
        answers[i].action = static_cast<CloseGroupAction>(m_combos.at(i)->currentData().toInt());
    }
    return answers;
}

CloseGroupRunner::CloseGroupRunner(AppController* controller, GroupModel* model,
                                   const QString& groupName,
                                   const QList<CloseGroupChoice>& choices, QObject* parent)
    : QObject(parent), m_controller(controller), m_model(model), m_groupName(groupName),
      m_steps(choices) {
    if (m_controller.isNull()) {
        return;
    }
    AppController* c = m_controller;
    QObject::connect(c, &AppController::mergeFinished, this,
                     [this](const QString& workspaceId, bool ok, const QString& conflictsJson,
                            const QString&) {
                         if (m_current < 0 || m_steps.at(m_current).workspaceId != workspaceId) {
                             return;
                         }
                         if (ok) {
                             stepDone(QString());
                             return;
                         }
                         const QString paths = changestoolbar::conflictList(conflictsJson);
                         stepDone(paths.isEmpty()
                                      ? QStringLiteral("the merge stopped on conflicts")
                                      : QStringLiteral("the merge stopped on conflicts in %1")
                                            .arg(paths));
                     });
    QObject::connect(c, &AppController::workspaceDestroyed, this, [this](const QString& id) {
        if (m_current >= 0 && m_steps.at(m_current).workspaceId == id) {
            stepDone(QString());
        }
    });
    QObject::connect(c, &AppController::workspaceOperationFailed, this,
                     [this](const QString& workspaceId, const QString&, const QString& message,
                            const QString&) {
                         if (m_current >= 0 && m_steps.at(m_current).workspaceId == workspaceId) {
                             stepDone(message);
                         }
                     });
}

void CloseGroupRunner::start() {
    m_current = -1;
    next();
}

void CloseGroupRunner::next() {
    ++m_current;
    while (m_current < m_steps.size() && m_steps.at(m_current).action == CloseGroupAction::Keep) {
        // Keeping asks the daemon for nothing: the tab moves to "Unsorted" when
        // the group goes, which `GroupModel::removeGroup` does at the end.
        ++m_current;
    }
    if (m_current >= m_steps.size()) {
        if (!m_model.isNull()) {
            m_model->removeGroup(m_groupName);
        }
        emit finished(true, QString(), QString());
        deleteLater();
        return;
    }
    const CloseGroupChoice& step = m_steps.at(m_current);
    if (m_controller.isNull()) {
        stepDone(QStringLiteral("the controller is gone"));
        return;
    }
    if (step.action == CloseGroupAction::Merge) {
        m_controller->mergeWorkspace(step.workspaceId, QStringLiteral("merge"), QString());
        return;
    }
    m_controller->discardWorkspace(step.workspaceId);
}

void CloseGroupRunner::stepDone(const QString& message) {
    if (message.isEmpty()) {
        next();
        return;
    }
    // Stopped, and the group stays: everything after this workspace is
    // untouched and still in it, which is what makes the failure recoverable by
    // opening the dialog again.
    const CloseGroupChoice step = m_steps.at(m_current);
    m_current = -1;
    emit finished(false, step.workspaceId,
                  QStringLiteral("%1: %2").arg(step.name, message));
    deleteLater();
}
