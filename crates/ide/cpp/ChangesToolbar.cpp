#include "ChangesToolbar.h"
#include "PrDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QAction>
#include <QInputDialog>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLineEdit>
#include <QMessageBox>
#include <QStringList>
#include <cstdint>
#include <utility>

namespace {

/// The daemon's own words for the three modes, as `MergeMode` serialises them.
const char* const kModeMerge = "merge";
const char* const kModeRebase = "rebase";

/// How many conflicting paths a banner names before it counts the rest. A merge
/// that touched forty files should say so in a sentence, not in a paragraph.
constexpr int kMaxNamedConflicts = 6;

/// The `data.reason` values the daemon sends with a refusal, so the banner can
/// say what to do about it rather than repeating the daemon's own sentence.
const char* const kReasonBaseDirty = "base_dirty";
const char* const kReasonObjectsStranded = "objects_stranded";

/// What the Discard confirmation says is about to be lost.
///
/// A count the daemon could not give (-1) is left out rather than guessed at:
/// "its 0 changed files will be lost" is the one wording that would talk a user
/// into a discard they did not mean.
QString discardWarning(int files) {
    if (files < 0) {
        return QStringLiteral("Its changed files and any unmerged commits will be lost.");
    }
    return QStringLiteral("Its %1 changed file%2 and any unmerged commits will be lost.")
        .arg(files)
        .arg(files == 1 ? QString() : QStringLiteral("s"));
}

} // namespace

QString changestoolbar::conflictList(const QString& conflictsJson) {
    const QJsonArray paths = QJsonDocument::fromJson(conflictsJson.toUtf8()).array();
    QStringList named;
    int usable = 0;
    for (const QJsonValue& value : paths) {
        usable += value.toString().isEmpty() ? 0 : 1;
    }
    for (const QJsonValue& value : paths) {
        const QString path = value.toString();
        if (path.isEmpty()) {
            continue;
        }
        if (named.size() == kMaxNamedConflicts) {
            // Counted from the paths this phrase could have named, not from the
            // raw array: an empty entry was never going to be listed, and
            // counting it would claim conflicts that are not there.
            named.append(QStringLiteral("and %1 more").arg(usable - kMaxNamedConflicts));
            break;
        }
        named.append(path);
    }
    return named.join(QStringLiteral(", "));
}

ChangesToolbar::ChangesToolbar(AppController* controller, QWidget* parent)
    : QToolBar(parent), m_controller(controller) {
    m_ask = [this](Ask ask, const QString& workspaceId) {
        return askModal(ask, workspaceId);
    };
    setObjectName(QStringLiteral("ChangesToolbar"));
    setMovable(false);
    setToolButtonStyle(Qt::ToolButtonTextOnly);
    // U+2026, the ellipsis, as a code point rather than as a character in a
    // literal: the file is compiled without a byte order mark and MSVC would
    // otherwise read it in the system code page. It marks the three actions
    // that ask something before they act.
    const QString ellipsis = QString(QChar(0x2026));

    m_merge = addAction(QStringLiteral("Merge"));
    m_merge->setObjectName(QStringLiteral("ChangesMergeAction"));
    m_merge->setToolTip(QStringLiteral("Merge this workspace's branch into its base"));
    m_rebase = addAction(QStringLiteral("Rebase"));
    m_rebase->setObjectName(QStringLiteral("ChangesRebaseAction"));
    m_rebase->setToolTip(QStringLiteral("Replay this workspace's commits onto its base"));
    m_squash = addAction(QStringLiteral("Squash") + ellipsis);
    m_squash->setObjectName(QStringLiteral("ChangesSquashAction"));
    m_squash->setToolTip(QStringLiteral("Land this workspace's work as one commit"));
    m_pr = addAction(QStringLiteral("Create PR") + ellipsis);
    m_pr->setObjectName(QStringLiteral("ChangesCreatePrAction"));
    m_pr->setToolTip(QStringLiteral("Push the branch and open a pull request"));
    addSeparator();
    m_discard = addAction(QStringLiteral("Discard") + ellipsis);
    m_discard->setObjectName(QStringLiteral("ChangesDiscardAction"));
    m_discard->setToolTip(QStringLiteral("Destroy the workspace and everything unmerged in it"));

    QObject::connect(m_merge, &QAction::triggered, this,
                     [this] { onMerge(QString::fromUtf8(kModeMerge)); });
    QObject::connect(m_rebase, &QAction::triggered, this,
                     [this] { onMerge(QString::fromUtf8(kModeRebase)); });
    QObject::connect(m_squash, &QAction::triggered, this, &ChangesToolbar::onSquash);
    QObject::connect(m_pr, &QAction::triggered, this, &ChangesToolbar::onCreatePr);
    QObject::connect(m_discard, &QAction::triggered, this, &ChangesToolbar::onDiscard);

    if (!m_controller.isNull()) {
        AppController* c = m_controller;
        QObject::connect(c, &AppController::mergeFinished, this, &ChangesToolbar::onMergeFinished);
        QObject::connect(c, &AppController::prCreated, this, &ChangesToolbar::onPrCreated);
        QObject::connect(c, &AppController::workspaceOperationFailed, this,
                         &ChangesToolbar::onOperationFailed);
        QObject::connect(c, &AppController::workspaceSummarized, this,
                         &ChangesToolbar::onSummarized);
        // A workspace that has gone takes its bookkeeping with it, so a later
        // workspace reusing the id could never inherit stale numbers.
        QObject::connect(c, &AppController::workspaceDestroyed, this, [this](const QString& id) {
            m_changedFiles.remove(id);
            m_branchOf.remove(id);
            updateActions();
        });
        // Which workspaces have an operation out is the controller's, not this
        // toolbar's: the tab context menu and the close-group runner start them
        // too, and a set kept here would not see either.
        QObject::connect(c, &AppController::workspaceBusyChanged, this,
                         [this](const QString&, bool) { updateActions(); });
    }
    updateActions();
}

void ChangesToolbar::setWorkspace(const QString& workspaceId, const QString& name,
                                  const QString& branch, const QString& baseBranch) {
    const bool moved = workspaceId != m_workspaceId;
    m_workspaceId = workspaceId;
    m_name = name;
    m_branch = branch;
    m_baseBranch = baseBranch;
    noteBranches(workspaceId, branch, baseBranch);
    updateActions();
    if (moved) {
        refreshSummary();
    }
}

void ChangesToolbar::refreshSummary() {
    requestSummary(m_workspaceId);
}

void ChangesToolbar::requestSummary(const QString& workspaceId) {
    if (workspaceId.isEmpty() || m_controller.isNull()) {
        return;
    }
    m_controller->workspaceSummary(workspaceId);
}

int ChangesToolbar::changedFilesFor(const QString& workspaceId) const {
    return m_changedFiles.value(workspaceId, -1);
}

void ChangesToolbar::noteBranches(const QString& workspaceId, const QString& branch,
                                  const QString& baseBranch) {
    if (!workspaceId.isEmpty()) {
        m_branchOf.insert(workspaceId, { branch, baseBranch });
    }
}

bool ChangesToolbar::busy(const QString& workspaceId) const {
    return !m_controller.isNull() && !workspaceId.isEmpty() &&
           m_controller->isWorkspaceBusy(workspaceId);
}

void ChangesToolbar::updateActions() {
    const bool live = !m_workspaceId.isEmpty() && !busy(m_workspaceId);
    for (QAction* action : { m_merge, m_rebase, m_squash, m_pr, m_discard }) {
        action->setEnabled(live);
    }
}

bool ChangesToolbar::beginOperation(const QString& workspaceId) {
    // Only a pre-check, and deliberately not a booking: the controller books the
    // workspace in when the call reaches it and refuses a second one itself, so
    // this is what stops the confirmation being *asked* rather than what makes
    // the interlock hold. `workspaceBusyChanged` greys the actions out a moment
    // later.
    return !workspaceId.isEmpty() && !m_controller.isNull() && !busy(workspaceId);
}

bool ChangesToolbar::stillOn(const QString& workspaceId) const {
    return workspaceId == m_workspaceId;
}

void ChangesToolbar::reportCancelled(const QString& what) {
    // Not sent to either one. The confirmation named the workspace it was
    // about, so the user has not agreed to anything happening to the one the
    // toolbar is pointed at now; and re-sending it to the workspace they did
    // agree about is just as wrong, because they are no longer looking at it
    // and the toolbar will report the answer under the wrong heading.
    emit statusMessage(QStringLiteral("Workspace changed, %1 cancelled").arg(what), QString());
}

ChangesToolbar::Answer ChangesToolbar::askModal(Ask ask, const QString& workspaceId) {
    Answer answer;
    switch (ask) {
    case Ask::Merge:
    case Ask::Rebase: {
        const bool rebase = ask == Ask::Rebase;
        QMessageBox box(this);
        box.setIcon(QMessageBox::Question);
        box.setWindowTitle(rebase ? QStringLiteral("Rebase workspace")
                                  : QStringLiteral("Merge workspace"));
        box.setText(rebase ? QStringLiteral("Replay %1 (%2) onto %3 and fast-forward %3?")
                                 .arg(m_name, m_branch, m_baseBranch)
                           : QStringLiteral("Merge %1 (%2) into %3?")
                                 .arg(m_name, m_branch, m_baseBranch));
        box.setInformativeText(
            QStringLiteral("The workspace and its branch stay; only %1 moves.").arg(m_baseBranch));
        box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
        box.setDefaultButton(QMessageBox::Cancel);
        answer.accepted = box.exec() == QMessageBox::Yes;
        return answer;
    }
    case Ask::Squash: {
        bool ok = false;
        // Empty is a real answer, not a cancelled one: the daemon then takes
        // the subject of the workspace's last commit, which is usually the
        // right line.
        const QString summary = QInputDialog::getText(
            this, QStringLiteral("Squash workspace"),
            QStringLiteral("Summary line for the squashed commit on %1\n(leave empty to use the "
                           "workspace's last commit subject):")
                .arg(m_baseBranch),
            QLineEdit::Normal, QString(), &ok);
        answer.accepted = ok;
        answer.summary = summary.trimmed();
        return answer;
    }
    case Ask::Pr: {
        PrDialog dialog(m_name, m_branch, m_baseBranch, this);
        answer.accepted = dialog.exec() == QDialog::Accepted;
        answer.title = dialog.title();
        answer.body = dialog.body();
        answer.draft = dialog.draft();
        return answer;
    }
    case Ask::Discard: {
        // The count this box names is the one thing that talks a user out of a
        // discard they did not mean, so it is asked for here -- rather than on
        // every `changesLoaded`, which put one `workspace.summary` on the wire
        // per burst of agent output for a number nothing was showing.
        requestSummary(workspaceId);

        QMessageBox box(this);
        box.setIcon(QMessageBox::Warning);
        box.setWindowTitle(QStringLiteral("Discard workspace"));
        box.setText(QStringLiteral("Discard %1?").arg(m_name));
        box.setInformativeText(discardWarning(changedFilesFor(workspaceId)));
        box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
        box.setDefaultButton(QMessageBox::Cancel);
        // `exec` runs its own event loop, so the answer to the request above
        // arrives while the box is up and the sentence is rewritten under the
        // user rather than being one tab switch out of date. The connection is
        // scoped to the box, so it is gone the moment the box is.
        if (!m_controller.isNull()) {
            QObject::connect(m_controller, &AppController::workspaceSummarized, &box,
                             [this, &box, workspaceId](const QString& answered, const QString&) {
                                 // `onSummarized` is connected first and has
                                 // already recorded the count by the time this
                                 // runs.
                                 if (answered == workspaceId) {
                                     box.setInformativeText(
                                         discardWarning(changedFilesFor(answered)));
                                 }
                             });
        }
        answer.accepted = box.exec() == QMessageBox::Yes;
        return answer;
    }
    }
    return answer;
}

void ChangesToolbar::setConfirmPrompt(std::function<Answer(Ask, const QString&)> ask) {
    if (ask) {
        m_ask = std::move(ask);
    }
}

// Every one of the four below reads the workspace once, before it asks, and
// uses that id for the rest of the call. The confirmation runs a nested event
// loop: the Explorer can switch tabs in it, and `setWorkspace` then points the
// toolbar somewhere else while the box is still on screen. Reading
// `m_workspaceId` after the answer is how a "Discard alpha?" that the user said
// yes to destroys beta.

void ChangesToolbar::onMerge(const QString& mode) {
    const QString workspaceId = m_workspaceId;
    if (workspaceId.isEmpty()) {
        return;
    }
    const bool rebase = mode == QString::fromUtf8(kModeRebase);
    if (!m_ask(rebase ? Ask::Rebase : Ask::Merge, workspaceId).accepted) {
        return;
    }
    if (!stillOn(workspaceId)) {
        reportCancelled(rebase ? QStringLiteral("rebase") : QStringLiteral("merge"));
        return;
    }
    if (!beginOperation(workspaceId)) {
        return;
    }
    m_controller->mergeWorkspace(workspaceId, mode, QString());
}

void ChangesToolbar::onSquash() {
    const QString workspaceId = m_workspaceId;
    if (workspaceId.isEmpty()) {
        return;
    }
    const Answer answer = m_ask(Ask::Squash, workspaceId);
    if (!answer.accepted) {
        return;
    }
    if (!stillOn(workspaceId)) {
        reportCancelled(QStringLiteral("squash"));
        return;
    }
    if (!beginOperation(workspaceId)) {
        return;
    }
    m_controller->mergeWorkspace(workspaceId, QStringLiteral("squash"), answer.summary);
}

void ChangesToolbar::onCreatePr() {
    const QString workspaceId = m_workspaceId;
    if (workspaceId.isEmpty()) {
        return;
    }
    const Answer answer = m_ask(Ask::Pr, workspaceId);
    if (!answer.accepted) {
        return;
    }
    if (!stillOn(workspaceId)) {
        reportCancelled(QStringLiteral("pull request"));
        return;
    }
    if (!beginOperation(workspaceId)) {
        return;
    }
    m_controller->createPr(workspaceId, answer.title, answer.body, answer.draft);
}

void ChangesToolbar::onDiscard() {
    const QString workspaceId = m_workspaceId;
    if (workspaceId.isEmpty()) {
        return;
    }
    if (!m_ask(Ask::Discard, workspaceId).accepted) {
        return;
    }
    if (!stillOn(workspaceId)) {
        reportCancelled(QStringLiteral("discard"));
        return;
    }
    if (!beginOperation(workspaceId)) {
        return;
    }
    m_controller->discardWorkspace(workspaceId);
}

void ChangesToolbar::onMergeFinished(const QString& workspaceId, bool ok,
                                     const QString& conflictsJson, const QString& reason) {
    // Read out of the map rather than off the toolbar's own fields: a merge on
    // a large repository answers long after the user may have moved to another
    // tab, and the message has to name the branches that actually moved.
    const QStringList branches = m_branchOf.value(workspaceId);
    const QString branch = branches.value(0);
    const QString base = branches.value(1);
    if (ok) {
        emit workspaceRecovered(workspaceId);
        // Silent rather than "Merged  into ": a workspace this toolbar has
        // never shown and nobody seeded has no branch names to report, and a
        // line with two holes in it says less than none.
        if (!branch.isEmpty() && !base.isEmpty()) {
            emit statusMessage(QStringLiteral("Merged %1 into %2").arg(branch, base), QString());
        }
        requestSummary(workspaceId);
        return;
    }
    const QString paths = changestoolbar::conflictList(conflictsJson);
    // U+2014, the em dash, as a code point rather than as a character in a
    // literal, so no compiler's idea of this file's source encoding can change
    // what it means.
    const QString dash = QStringLiteral(" ") + QString(QChar(0x2014)) + QStringLiteral(" ");
    const QString tail =
        QStringLiteral("the workspace is untouched; ask the agent to rebase");
    QString title = paths.isEmpty() ? QStringLiteral("Merge stopped") + dash + tail
                                    : QStringLiteral("Merge stopped: conflicts in %1%2%3")
                                          .arg(paths, dash, tail);
    // `conflict` is the only reason the daemon sends with a result today. A
    // newer daemon's word is appended rather than swallowed or substituted:
    // the same answer still carried the file list, and an IDE that does not
    // recognise the word must not throw away the paths that came with it.
    if (!reason.isEmpty() && reason != QLatin1String("conflict")) {
        title += QStringLiteral(" (%1)").arg(reason);
    }
    emit workspaceError(workspaceId, title, QString(), QString());
}

void ChangesToolbar::onPrCreated(const QString& workspaceId, const QString& url) {
    emit workspaceRecovered(workspaceId);
    emit statusMessage(QStringLiteral("PR: %1").arg(url), url);
}

void ChangesToolbar::onOperationFailed(const QString& workspaceId, const QString& op,
                                       const QString& message, const QString& dataJson) {
    // Not `data`: `QWidget` has a member of that name, and a local hiding it
    // is a warning this build treats as one to fix.
    const QJsonObject payload = QJsonDocument::fromJson(dataJson.toUtf8()).object();
    const QString reason = payload.value(QStringLiteral("reason")).toString();
    const QString stderrText = payload.value(QStringLiteral("stderr")).toString();

    QString title = op;
    QString detail = message;
    if (reason == QLatin1String(kReasonBaseDirty)) {
        title = QStringLiteral("The base repository has uncommitted work");
        // The daemon's sentence names the repository and the branch; what it
        // cannot say is what the user should do next.
        detail = message + QStringLiteral("\nCommit or stash your changes there first.");
    } else if (reason == QLatin1String(kReasonObjectsStranded)) {
        title = QStringLiteral("The work landed but could not be copied out");
        detail = message + QStringLiteral("\nDo not destroy this workspace.");
    } else if (op == QLatin1String("workspace.merge")) {
        title = QStringLiteral("Merge failed");
    } else if (op == QLatin1String("workspace.create_pr")) {
        title = QStringLiteral("Pull request failed");
    } else if (op == QLatin1String("workspace.destroy")) {
        title = QStringLiteral("Discard failed");
    }
    emit workspaceError(workspaceId, title, detail, stderrText);
}

void ChangesToolbar::onSummarized(const QString& workspaceId, const QString& json) {
    const QJsonObject summary = QJsonDocument::fromJson(json.toUtf8()).object();
    // Absent is -1 as well: an object without the key says as little as one the
    // daemon could not build.
    m_changedFiles.insert(workspaceId,
                          summary.value(QStringLiteral("changed_files")).toInt(-1));
}

// --- offscreen test entries --------------------------------------------------
//
// Compiled only into a development build: this is test code -- it builds
// widgets, leaks a QApplication and asserts -- and a shipped IDE has no caller
// for any of it. `build.rs` defines `BS_WIDGET_TESTS` for every profile but
// `release`, which is the one the packaged executable is built with.
#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`: the widget checks live beside the widget
// and answer a code. `bs_widget_test_begin` must have run first.
//
// The stand-in for a daemon is a real `AppController` that was never started.
// `discardWorkspace` books the workspace in before it discovers it has no
// connection, and un-books it through the Qt event loop, which is not running
// here -- so `isWorkspaceBusy` says, synchronously and without a daemon,
// exactly which workspace the toolbar acted on.

namespace {

/// One of the five actions: the object name of the `QAction` that starts it,
/// and the line a user is owed when it is called off because the workspace
/// moved under the confirmation.
struct ToolbarAction {
    const char* objectName;
    const char* cancelled;
};

/// All five, because the mistake is the same one in each of them: the
/// confirmation names one workspace and the request is addressed to whichever
/// one the toolbar is pointed at when the answer comes back.
const ToolbarAction kToolbarActions[] = {
    { "ChangesMergeAction", "Workspace changed, merge cancelled" },
    { "ChangesRebaseAction", "Workspace changed, rebase cancelled" },
    { "ChangesSquashAction", "Workspace changed, squash cancelled" },
    { "ChangesCreatePrAction", "Workspace changed, pull request cancelled" },
    { "ChangesDiscardAction", "Workspace changed, discard cancelled" },
};

/// Runs one action to its end with `confirm` answering its modal, and checks
/// what the controller was asked to do afterwards.
///
/// Both checks below are the same walk with a different confirmation, so they
/// share it: a controller and a toolbar of their own per action -- the
/// controller books a workspace in for as long as the request is out and never
/// lets it go without an event loop, so a shared one would refuse the second
/// action for a reason neither check is about.
///
/// `switchTo` non-empty makes the confirmation move the toolbar to that
/// workspace, the way the Explorer does when the active tab changes under a
/// modal. Answers 0, or 100/200/300/400 plus `index` for the assertion that
/// failed.
std::int32_t runToolbarAction(const ToolbarAction& action, std::int32_t index,
                              const QString& switchTo) {
    AppController controller;
    ChangesToolbar toolbar(&controller);
    toolbar.setWorkspace(QStringLiteral("ws_1"), QStringLiteral("alpha"),
                         QStringLiteral("bs/alpha"), QStringLiteral("main"));
    QString status;
    QObject::connect(&toolbar, &ChangesToolbar::statusMessage, &toolbar,
                     [&status](const QString& text, const QString&) { status = text; });
    toolbar.setConfirmPrompt([&toolbar, switchTo](ChangesToolbar::Ask, const QString&) {
        if (!switchTo.isEmpty()) {
            toolbar.setWorkspace(switchTo, QStringLiteral("beta"), QStringLiteral("bs/beta"),
                                 QStringLiteral("main"));
        }
        ChangesToolbar::Answer answer;
        answer.accepted = true;
        return answer;
    });
    auto* trigger = toolbar.findChild<QAction*>(QString::fromUtf8(action.objectName));
    if (trigger == nullptr) {
        return 100 + index;
    }
    trigger->trigger();

    const bool moved = !switchTo.isEmpty();
    // The workspace the confirmation was about: acted on when it is still the
    // one on show, and untouched when it is not.
    if (controller.isWorkspaceBusy(QStringLiteral("ws_1")) == moved) {
        return 200 + index;
    }
    // The one the toolbar moved to never agreed to anything.
    if (moved && controller.isWorkspaceBusy(switchTo)) {
        return 300 + index;
    }
    const QString expected =
        moved ? QString::fromUtf8(action.cancelled) : QString();
    if (status != expected) {
        return 400 + index;
    }
    return 0;
}

} // namespace

/// The workspace moved while the confirmation was up: nothing may be sent, for
/// either workspace, and the user has to be told why their click did nothing.
///
/// A failure answers 100, 200, 300 or 400 plus the index of the action it
/// happened on, so the code says both what went wrong and which of the five it
/// went wrong on.
extern "C" std::int32_t bs_widget_test_changes_toolbar_cancels_a_switched_workspace() {
    std::int32_t index = 0;
    for (const ToolbarAction& action : kToolbarActions) {
        const std::int32_t code = runToolbarAction(action, index, QStringLiteral("ws_2"));
        if (code != 0) {
            return code;
        }
        ++index;
    }
    return 0;
}

/// The control for the check above: an action whose workspace did not move
/// still reaches the daemon, and reaches the workspace that was confirmed.
extern "C" std::int32_t bs_widget_test_changes_toolbar_acts_on_the_confirmed_workspace() {
    std::int32_t index = 0;
    for (const ToolbarAction& action : kToolbarActions) {
        const std::int32_t code = runToolbarAction(action, index, QString());
        if (code != 0) {
            return code;
        }
        ++index;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
