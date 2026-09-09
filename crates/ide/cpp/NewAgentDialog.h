#pragma once
#include <QDialog>
#include <QString>

class AppController;
class GroupModel;
class QComboBox;
class QDialogButtonBox;
class QLabel;
class QLineEdit;

/// Collects everything `AppController::createWorkspace` needs: which repository
/// and base branch to fork, what to call the agent, which adapter and command to
/// run it with, and which group its tab belongs in.
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
    /// The adapter as the daemon spells it, e.g. "terminal".
    QString adapter() const;
    /// The command to run, or empty for the adapter's default.
    QString command() const;
    QString group() const;

private:
    void browse();
    void inspectRepo();
    void onRepoInspected(const QString& path, const QString& infoJson);
    void onInspectFailed(const QString& op, const QString& message);
    void onGroupChanged(int index);
    void updateOkEnabled();

    AppController* m_controller;
    GroupModel* m_model;
    QLineEdit* m_repoPath = nullptr;
    QComboBox* m_baseBranch = nullptr;
    QLineEdit* m_name = nullptr;
    QComboBox* m_adapter = nullptr;
    QLineEdit* m_command = nullptr;
    QComboBox* m_group = nullptr;
    QLineEdit* m_newGroup = nullptr;
    QLabel* m_newGroupLabel = nullptr;
    QLabel* m_status = nullptr;
    QDialogButtonBox* m_buttons = nullptr;
    /// The distro path of the inspection in flight, so a late answer for a path
    /// the user has since edited away from is ignored.
    QString m_pendingPath;
};
