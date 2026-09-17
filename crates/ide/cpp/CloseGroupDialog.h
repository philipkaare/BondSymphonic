#pragma once
#include <QDialog>
#include <QList>
#include <QObject>
#include <QPointer>
#include <QString>

class AppController;
class GroupModel;
class QComboBox;
class QDialogButtonBox;

/// What "Close group" is to do with one of the group's workspaces.
enum class CloseGroupAction {
    /// Move the tab to "Unsorted" and leave the workspace alone.
    Keep,
    /// Merge its branch into its base, then leave it in "Unsorted".
    Merge,
    /// Destroy it and everything unmerged in it.
    Discard,
};

/// One row of the dialog, and one step of the run that follows it.
struct CloseGroupChoice {
    QString workspaceId;
    QString name;
    CloseGroupAction action = CloseGroupAction::Keep;
    /// This workspace already had a merge, pull request, discard or destroy out
    /// when the dialog opened. Such a row is pinned to Keep and greyed: merging
    /// or discarding a workspace whose merge is still absorbing objects out of
    /// it is what leaves the base branch pointing at commits that are gone.
    bool busy = false;
    /// The workspace works in its checkout: it cannot be merged, and
    /// discarding it only closes it.
    bool inPlace = false;
};

/// Asks what to do with each workspace in a group before the group is closed.
///
/// One row per workspace, defaulting to Keep, because closing a group is a
/// decision about the group and the workspaces in it are still live work. The
/// dialog only collects the answers; `CloseGroupRunner` carries them out.
class CloseGroupDialog : public QDialog {
    Q_OBJECT
public:
    /// `workspaces` is the group's tabs in order, as `{workspaceId, name}`
    /// pairs with the action they start on.
    CloseGroupDialog(const QString& groupName, const QList<CloseGroupChoice>& workspaces,
                     QWidget* parent = nullptr);

    /// The rows as the user left them, in the order they will run.
    QList<CloseGroupChoice> choices() const;

private:
    QList<CloseGroupChoice> m_choices;
    QList<QComboBox*> m_combos;
    QDialogButtonBox* m_buttons = nullptr;
};

/// Runs a `CloseGroupDialog`'s answers, one workspace at a time, and removes
/// the group when every one of them has succeeded.
///
/// Sequential rather than parallel, and stopping at the first refusal: a merge
/// that conflicts leaves that workspace exactly as it was, and carrying on
/// would destroy the *next* workspace on the list while the user is being told
/// the previous one needs their attention. The group survives a stop, so
/// nothing is half-closed: whatever already merged or was discarded stays that
/// way, and the rest are untouched and still in the group.
///
/// Deletes itself when it finishes, so the caller starts it and forgets it.
class CloseGroupRunner : public QObject {
    Q_OBJECT
public:
    CloseGroupRunner(AppController* controller, GroupModel* model, const QString& groupName,
                     const QList<CloseGroupChoice>& choices, QObject* parent = nullptr);

    /// Starts the first step. Answers arrive on the controller's signals.
    void start();

signals:
    /// The run ended. `ok` is whether the group was closed; `message` is empty
    /// then, and otherwise says which workspace stopped it and why.
    /// `workspaceId` is that workspace, or empty on success.
    void finished(bool ok, const QString& workspaceId, const QString& message);

private:
    /// Runs the next step, or finishes when there are none left.
    void next();
    /// A step answered. `message` empty means it worked.
    void stepDone(const QString& message);

    QPointer<AppController> m_controller;
    QPointer<GroupModel> m_model;
    QString m_groupName;
    QList<CloseGroupChoice> m_steps;
    /// The step being run, so an answer for another workspace (another tab's
    /// merge, finishing at the same time) is ignored rather than counted here.
    int m_current = -1;
};
