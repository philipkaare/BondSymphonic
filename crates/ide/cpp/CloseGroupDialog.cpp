#include "CloseGroupDialog.h"
#include "ChangesToolbar.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QComboBox>
#include <QCoreApplication>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QLabel>
#include <QPushButton>
#include <QVBoxLayout>
#include <cstdint>

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
            // An in-place workspace has nothing to merge, and its "discard"
            // closes it without touching the checkout, so it is worded as that.
            if (choice.inPlace && action == CloseGroupAction::Merge) {
                continue;
            }
            combo->addItem(choice.inPlace && action == CloseGroupAction::Discard
                               ? QStringLiteral("Close (files are kept)")
                               : actionLabel(action),
                           static_cast<int>(action));
        }
        combo->setCurrentIndex(0);
        if (choice.busy) {
            // Pinned to Keep rather than merely refused later: the run is
            // sequential and stops at the first refusal, so a row that cannot
            // succeed would abandon every workspace after it.
            combo->setEnabled(false);
            combo->setToolTip(
                QStringLiteral("This workspace has a merge, pull request or discard running."));
        }
        m_combos.append(combo);
        form->addRow(choice.busy ? choice.name + QStringLiteral("  (busy)") : choice.name, combo);
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
    // The plain destroy an in-place step sends reports its failures on the
    // leaner signal instead, and a run that heard neither would wait for an
    // answer that never comes. Only one of the two is emitted per failure, and
    // `stepDone` has set `m_current` to -1 by the time any second one could
    // arrive.
    QObject::connect(c, &AppController::workspaceOpFailed, this,
                     [this](const QString& workspaceId, const QString&, const QString& message) {
                         if (m_current >= 0 && m_steps.at(m_current).workspaceId == workspaceId) {
                             stepDone(message);
                         }
                     });
    // A refusal is neither a success nor a failure on the wire, and the plain
    // destroy above is the only step that can meet one. The daemon does not
    // refuse an in-place workspace -- nothing of the user's is removed, so
    // there is nothing to weigh -- but a tab whose `kind` is stale would send
    // this call to a worktree workspace, and a run that heard nothing back
    // would wait for an answer that is never coming. It stops instead, with
    // the group intact, which is what every other refusal here does.
    QObject::connect(c, &AppController::workspaceDestroyRefused, this,
                     [this](const QString& workspaceId, bool, bool) {
                         if (m_current >= 0 && m_steps.at(m_current).workspaceId == workspaceId) {
                             stepDone(QStringLiteral(
                                 "the daemon would not close it without discarding work"));
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
        // Before anything else can run: `deleteLater` disconnects nothing, and
        // every one of the three handlers below indexes `m_steps` at
        // `m_current` after checking only that it is not negative. An answer
        // for some other workspace -- a merge the user started from the
        // toolbar, finishing in this same pass -- would otherwise read one
        // past the end. -1 is the "no step is running" the handlers already
        // understand, and the failure path has always set it.
        m_current = -1;
        emit finished(true, QString(), QString());
        deleteLater();
        return;
    }
    const CloseGroupChoice& step = m_steps.at(m_current);
    if (m_controller.isNull()) {
        stepDone(QStringLiteral("the controller is gone"));
        return;
    }
    // Re-checked at the moment of the step, not only when the dialog opened: the
    // steps run one after another, and the user can start a merge from the
    // Changes toolbar while an earlier one of these is still going.
    if (m_controller->isWorkspaceBusy(step.workspaceId)) {
        stepDone(QStringLiteral("it has a merge, pull request or discard running"));
        return;
    }
    if (step.action == CloseGroupAction::Merge) {
        m_controller->mergeWorkspace(step.workspaceId, QStringLiteral("merge"), QString());
        return;
    }
    if (step.inPlace) {
        // "Close (files are kept)" is the plain destroy the tab menu sends, not
        // a discard: nothing of the user's is removed, so there is nothing to
        // force. The daemon ignores `force` for this kind, which makes this
        // defence in depth rather than a fix -- but a forced destroy is the one
        // request that would delete a checkout if it were ever routed to a
        // workspace of the other kind by mistake.
        m_controller->destroyWorkspace(step.workspaceId, false);
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

// The offscreen widget checks. See the note in `EditorArea.cpp`; `build.rs`
// defines `BS_WIDGET_TESTS` for every profile but `release`, and
// `bs_widget_test_begin` must have run first.
#if defined(BS_WIDGET_TESTS)

/// The dialog offers an in-place workspace "Close (files are kept)" and no
/// merge, and the run behind that row sends a plain destroy rather than the
/// forced one a discard is -- and finishes when that call fails, which is the
/// signal a destroy reports on.
extern "C" std::int32_t bs_widget_test_close_group_closes_an_in_place_workspace_plainly() {
    AppController controller;
    GroupModel model;
    const QString inPlaceId = QStringLiteral("ws_inplace");
    const QString worktreeId = QStringLiteral("ws_worktree");

    QList<CloseGroupChoice> rows;
    CloseGroupChoice inPlace;
    inPlace.workspaceId = inPlaceId;
    inPlace.name = QStringLiteral("checkout");
    inPlace.inPlace = true;
    rows.append(inPlace);
    CloseGroupDialog dialog(QStringLiteral("here"), rows);
    auto* combo = dialog.findChild<QComboBox*>(QStringLiteral("CloseGroupChoice_") + inPlaceId);
    if (combo == nullptr || combo->count() != 2) {
        return 1;
    }
    if (combo->itemText(1) != QLatin1String("Close (files are kept)")) {
        return 2;
    }

    // What the runner does with that row. There is no daemon behind this
    // controller, so both calls fail at once -- on different signals, which is
    // what tells them apart: a discard is a workspace operation and reports on
    // `workspaceOperationFailed`, a destroy on `workspaceOpFailed`.
    int destroys = 0;
    int discards = 0;
    QObject::connect(&controller, &AppController::workspaceOpFailed,
                     [&destroys](const QString&, const QString& op, const QString&) {
                         if (op == QLatin1String("workspace.destroy")) {
                             ++destroys;
                         }
                     });
    QObject::connect(&controller, &AppController::workspaceOperationFailed,
                     [&discards](const QString&, const QString& op, const QString&,
                                 const QString&) {
                         if (op == QLatin1String("workspace.destroy")) {
                             ++discards;
                         }
                     });

    QList<CloseGroupChoice> steps;
    CloseGroupChoice closing = inPlace;
    closing.action = CloseGroupAction::Discard;
    steps.append(closing);
    auto* runner = new CloseGroupRunner(&controller, &model, QStringLiteral("here"), steps);
    int finished = 0;
    bool ok = true;
    QObject::connect(runner, &CloseGroupRunner::finished,
                     [&finished, &ok](bool succeeded, const QString&, const QString&) {
                         ++finished;
                         ok = succeeded;
                     });
    runner->start();
    QCoreApplication::processEvents();
    if (destroys != 1 || discards != 0) {
        return 3;
    }
    // The run must not be left waiting on an answer that already came.
    if (finished != 1 || ok) {
        return 4;
    }

    // A refusal stops the run rather than leaving it waiting. It cannot happen
    // for a workspace the daemon agrees is in place, but a stale `kind` is what
    // would send the plain destroy to a workspace that has work to weigh.
    int refusedFinished = 0;
    bool refusedOk = true;
    QString refusedMessage;
    auto* refused = new CloseGroupRunner(&controller, &model, QStringLiteral("here"), steps);
    QObject::connect(refused, &CloseGroupRunner::finished,
                     [&](bool succeeded, const QString&, const QString& message) {
                         ++refusedFinished;
                         refusedOk = succeeded;
                         refusedMessage = message;
                     });
    refused->start();
    controller.workspaceDestroyRefused(inPlaceId, true, false);
    QCoreApplication::processEvents();
    if (refusedFinished != 1 || refusedOk ||
        !refusedMessage.contains(QLatin1String("without discarding work"))) {
        return 6;
    }

    // A worktree workspace still goes through the discard, which is the forced
    // destroy its confirmation named.
    destroys = 0;
    discards = 0;
    QList<CloseGroupChoice> worktreeSteps;
    CloseGroupChoice discarding;
    discarding.workspaceId = worktreeId;
    discarding.name = QStringLiteral("feature");
    discarding.action = CloseGroupAction::Discard;
    worktreeSteps.append(discarding);
    auto* second = new CloseGroupRunner(&controller, &model, QStringLiteral("here"), worktreeSteps);
    int secondFinished = 0;
    QObject::connect(second, &CloseGroupRunner::finished,
                     [&secondFinished](bool, const QString&, const QString&) { ++secondFinished; });
    second->start();
    QCoreApplication::processEvents();
    if (discards != 1 || destroys != 0 || secondFinished != 1) {
        return 5;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
