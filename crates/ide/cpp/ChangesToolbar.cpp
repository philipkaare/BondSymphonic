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

int ChangesToolbar::changedFiles() const {
    return changedFilesFor(m_workspaceId);
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

bool ChangesToolbar::beginOperation() {
    // Only a pre-check, and deliberately not a booking: the controller books the
    // workspace in when the call reaches it and refuses a second one itself, so
    // this is what stops the confirmation being *asked* rather than what makes
    // the interlock hold. `workspaceBusyChanged` greys the actions out a moment
    // later.
    return !m_workspaceId.isEmpty() && !m_controller.isNull() && !busy(m_workspaceId);
}

void ChangesToolbar::onMerge(const QString& mode) {
    if (m_workspaceId.isEmpty()) {
        return;
    }
    const bool rebase = mode == QString::fromUtf8(kModeRebase);
    QMessageBox box(this);
    box.setIcon(QMessageBox::Question);
    box.setWindowTitle(rebase ? QStringLiteral("Rebase workspace") : QStringLiteral("Merge workspace"));
    box.setText(rebase ? QStringLiteral("Replay %1 (%2) onto %3 and fast-forward %3?")
                             .arg(m_name, m_branch, m_baseBranch)
                       : QStringLiteral("Merge %1 (%2) into %3?").arg(m_name, m_branch, m_baseBranch));
    box.setInformativeText(
        QStringLiteral("The workspace and its branch stay; only %1 moves.").arg(m_baseBranch));
    box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    if (box.exec() != QMessageBox::Yes) {
        return;
    }
    if (!beginOperation()) {
        return;
    }
    m_controller->mergeWorkspace(m_workspaceId, mode, QString());
}

void ChangesToolbar::onSquash() {
    if (m_workspaceId.isEmpty()) {
        return;
    }
    bool ok = false;
    // Empty is a real answer, not a cancelled one: the daemon then takes the
    // subject of the workspace's last commit, which is usually the right line.
    const QString summary = QInputDialog::getText(
        this, QStringLiteral("Squash workspace"),
        QStringLiteral("Summary line for the squashed commit on %1\n(leave empty to use the "
                       "workspace's last commit subject):")
            .arg(m_baseBranch),
        QLineEdit::Normal, QString(), &ok);
    if (!ok) {
        return;
    }
    if (!beginOperation()) {
        return;
    }
    m_controller->mergeWorkspace(m_workspaceId, QStringLiteral("squash"), summary.trimmed());
}

void ChangesToolbar::onCreatePr() {
    if (m_workspaceId.isEmpty()) {
        return;
    }
    PrDialog dialog(m_name, m_branch, m_baseBranch, this);
    if (dialog.exec() != QDialog::Accepted) {
        return;
    }
    if (!beginOperation()) {
        return;
    }
    m_controller->createPr(m_workspaceId, dialog.title(), dialog.body(), dialog.draft());
}

void ChangesToolbar::onDiscard() {
    if (m_workspaceId.isEmpty()) {
        return;
    }
    const int files = changedFiles();
    // A count the daemon could not give is left out rather than guessed at:
    // "its 0 changed files will be lost" is the one wording that would talk a
    // user into a discard they did not mean.
    const QString what = files < 0
                             ? QStringLiteral("Its changed files and any unmerged commits will be "
                                              "lost.")
                             : QStringLiteral("Its %1 changed file%2 and any unmerged commits will "
                                              "be lost.")
                                   .arg(files)
                                   .arg(files == 1 ? QString() : QStringLiteral("s"));
    QMessageBox box(this);
    box.setIcon(QMessageBox::Warning);
    box.setWindowTitle(QStringLiteral("Discard workspace"));
    box.setText(QStringLiteral("Discard %1?").arg(m_name));
    box.setInformativeText(what);
    box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    if (box.exec() != QMessageBox::Yes) {
        return;
    }
    if (!beginOperation()) {
        return;
    }
    m_controller->discardWorkspace(m_workspaceId);
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
    const QJsonObject data = QJsonDocument::fromJson(dataJson.toUtf8()).object();
    const QString reason = data.value(QStringLiteral("reason")).toString();
    const QString stderrText = data.value(QStringLiteral("stderr")).toString();

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
