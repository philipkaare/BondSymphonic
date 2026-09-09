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

/// Collects everything a new workspace needs: which repository and base branch
/// to fork, what to call the agent, which adapter runs in it, and which group
/// its tab belongs in.
///
/// The adapter decides which half of the form is on show. `terminal` asks for a
/// command; `claude` asks for a model, a permission mode and an opening prompt,
/// and hands them over as `optionsJson` and `initialPrompt` rather than as
/// separate arguments, because the daemon's start options are what they are.
///
/// The branch list is filled asynchronously: editing the repository path asks
/// the daemon to inspect it, and the answer arrives on `repoInspected`.
class NewAgentDialog : public QDialog {
    Q_OBJECT
public:
    NewAgentDialog(AppController* controller, GroupModel* model, QWidget* parent = nullptr);

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

private:
    void browse();
    void inspectRepo();
    void onRepoInspected(const QString& path, const QString& infoJson);
    void onInspectFailed(const QString& op, const QString& message);
    void onGroupChanged(int index);
    /// Shows the fields the selected adapter has and hides the rest.
    void onAdapterChanged();
    void updateOkEnabled();

    AppController* m_controller;
    GroupModel* m_model;
    QFormLayout* m_form = nullptr;
    QLineEdit* m_repoPath = nullptr;
    QComboBox* m_baseBranch = nullptr;
    QLineEdit* m_name = nullptr;
    QComboBox* m_adapter = nullptr;
    QLineEdit* m_command = nullptr;
    QLineEdit* m_claudeModel = nullptr;
    QComboBox* m_permissionMode = nullptr;
    QPlainTextEdit* m_initialPrompt = nullptr;
    QComboBox* m_group = nullptr;
    QLineEdit* m_newGroup = nullptr;
    QLabel* m_newGroupLabel = nullptr;
    QLabel* m_status = nullptr;
    QDialogButtonBox* m_buttons = nullptr;
    /// The distro path of the inspection in flight, so a late answer for a path
    /// the user has since edited away from is ignored.
    QString m_pendingPath;
};
