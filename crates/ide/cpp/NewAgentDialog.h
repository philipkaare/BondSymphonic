#pragma once
#include <QDialog>
#include <QString>

class AppController;
class GroupModel;
class QComboBox;
class QDialogButtonBox;
class QFormLayout;
class QLabel;
class QLineEdit;
class QPlainTextEdit;
class QToolButton;

/// Collects everything a new workspace needs: which repository and base branch
/// to fork, what to call the agent, which adapter runs in it, and which group
/// its tab belongs in.
///
/// The adapter decides which half of the form is on show. `terminal` asks for a
/// command; `claude` asks for a model, a permission mode and an opening prompt,
/// and hands them over as `optionsJson` and `initialPrompt` rather than as
/// separate arguments, because the daemon's start options are what they are.
///
/// The branch list and the run-configuration list are filled asynchronously:
/// editing the repository path asks the daemon to inspect it and to detect its
/// run configurations, and the answers arrive on `repoInspected` and
/// `runConfigsDetected`.
class NewAgentDialog : public QDialog {
    Q_OBJECT
public:
    /// `initialPath` is the repository the dialog opens on, as the user would
    /// type it; the window supplies the most recent one. The dialog inspects it
    /// itself, from the constructor, so the branch combo fills in behind a
    /// dialog that is already on screen.
    NewAgentDialog(AppController* controller, GroupModel* model, const QString& initialPath,
                   QWidget* parent = nullptr);

    /// The repository the dialog will open on: the most recently used one, or
    /// empty when there is none. Static because the window needs it *before*
    /// there is a dialog -- it inspects that path first, so the branch list is
    /// filled the moment the dialog appears rather than half a minute later.
    static QString initialRepoPath(AppController* controller);

    /// Preselects a group, adding it to the list if it is not there yet.
    void setGroup(const QString& name);

    /// The repository as the daemon spells it, i.e. a path inside the distro.
    QString repoPath() const;
    QString baseBranch() const;
    QString name() const;
    /// The adapter as the daemon spells it, e.g. "terminal" or "claude".
    QString adapter() const;
    /// The command to run, or empty for the adapter's default. Terminal only.
    QString command() const;
    QString group() const;

    /// `AgentStartOptions` as JSON, with the fields the user left blank left
    /// out, so the daemon's own defaults apply to them. Empty for an adapter
    /// that has no options.
    QString optionsJson() const;
    /// The first prompt to send once the agent is up, or empty for none.
    QString initialPrompt() const;

    /// The run configuration to record on the new tab, so the Run panel opens
    /// on it, or empty when the user chose none. It is a name out of the
    /// daemon's own detection, never something typed.
    QString runConfig() const;

    /// Whether the daemon should create and initialise the folder as a git
    /// repository before making the workspace.
    ///
    /// True only when the last inspection said this path is not a repository,
    /// which is also when the dialog says so in the "Repository" line. The
    /// user is never asked to tick anything: the sentence under the path is
    /// the consent, and pressing Create is the agreement to it.
    bool initIfMissing() const;

private:
    void browse();
    /// Rebuilds the Recent menu from the controller's list, most recent first,
    /// and disables the button when there is nothing to offer. The paths are
    /// the ones a create succeeded with, so this is the list of repositories
    /// the user has actually worked in.
    void buildRecentMenu();
    void inspectRepo();
    void onRepoInspected(const QString& path, const QString& infoJson);
    void onRunConfigsDetected(const QString& path, const QString& json);
    /// Raises or hides the note naming the hosts the repository's own
    /// `bondsymphonic.toml` would add to the new workspace's allowlist.
    void showNetworkAllow(const QString& json);
    void onInspectFailed(const QString& op, const QString& message);
    /// Re-runs the name rule and shows or hides the hint under the field.
    /// Called on every edit, so a name the daemon would refuse is refused
    /// while it is being typed rather than after the dialog has closed and
    /// taken everything else typed into it with it.
    void updateNameHint();
    /// Puts the "Repository" line under the path: nothing for an ordinary
    /// repository, and what is about to be created for anything else.
    void showRepoState(bool isRepo, bool exists);
    void onGroupChanged(int index);
    /// Shows the fields the selected adapter has and hides the rest.
    void onAdapterChanged();
    void updateOkEnabled();

    AppController* m_controller;
    GroupModel* m_model;
    QFormLayout* m_form = nullptr;
    QLineEdit* m_repoPath = nullptr;
    /// Drops down the repositories the user has created workspaces in before.
    QToolButton* m_recent = nullptr;
    QComboBox* m_baseBranch = nullptr;
    QLineEdit* m_name = nullptr;
    QComboBox* m_adapter = nullptr;
    QLineEdit* m_command = nullptr;
    QLineEdit* m_claudeModel = nullptr;
    QComboBox* m_permissionMode = nullptr;
    QComboBox* m_runConfig = nullptr;
    /// Names the hosts this repository's own `bondsymphonic.toml` would add to
    /// the new workspace's network allowlist, or is hidden when it adds none.
    ///
    /// Creating a workspace applies `[network] allow` from a file the user may
    /// never have opened, so this is the one place before Create where that is
    /// visible. Plain text: the repository chose the strings.
    QLabel* m_networkNote = nullptr;
    /// Why the typed name will not do, or hidden when it will. Red, under the
    /// field, and the same sentence the rule itself carries.
    QLabel* m_nameHint = nullptr;
    /// What creating from this path will do to it, when that is anything more
    /// than using a repository that is already there.
    QLabel* m_repoState = nullptr;
    QPlainTextEdit* m_initialPrompt = nullptr;
    QComboBox* m_group = nullptr;
    QLineEdit* m_newGroup = nullptr;
    QLabel* m_newGroupLabel = nullptr;
    QLabel* m_status = nullptr;
    QDialogButtonBox* m_buttons = nullptr;
    /// The distro path of the inspection in flight, so a late answer for a path
    /// the user has since edited away from is ignored.
    QString m_pendingPath;
    /// Whether an inspection for `m_pendingPath` is still out. Create is
    /// refused while it is: the base branch would be whatever was left in the
    /// combo from the previous repository.
    bool m_inspectPending = false;
    /// Whether the last inspection failed. Create is refused then too: the
    /// daemon could not read the repository, so nothing in the branch combo
    /// is known to exist.
    bool m_inspectFailed = false;
    /// See `initIfMissing`. Recomputed from every inspection, so it cannot
    /// survive a move to a path that is a repository.
    bool m_initIfMissing = false;
};
