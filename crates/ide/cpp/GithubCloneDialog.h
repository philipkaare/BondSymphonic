#pragma once
#include <QDialog>
#include <QString>

class AppController;
class QDialogButtonBox;
class QLabel;
class QLineEdit;
class QListWidget;
class QPushButton;

/// New Agent › From GitHub…: pick one of the user's GitHub repositories (or
/// type `owner/name` or a URL) and clone it fresh into a new folder, through
/// `gh` on the host. Accepted once the clone is on disk; `clonedPath()` is
/// then the distro path to inspect.
///
/// No `Q_OBJECT`: everything is wired with lambdas, like `CommitDialog`.
class GithubCloneDialog : public QDialog {
public:
    /// `parentDir` is the folder offered to clone into, as the user would
    /// type it (a Windows path on Windows).
    GithubCloneDialog(AppController* controller, const QString& parentDir,
                      QWidget* parent = nullptr);

    /// The distro path of the fresh checkout, once accepted.
    QString clonedPath() const { return m_clonedPath; }

    /// `repo` and the folder it would land in, by the same rules as the
    /// daemon: the last part of `owner/name` or a URL, without `.git`. Empty
    /// for something that names no repository. Exposed for the tests.
    static QString targetName(const QString& repo);

private:
    void onListed(const QString& json);
    void onListFailed(const QString& message);
    void applyFilter();
    void browse();
    void startClone();
    void updateEnabled();
    void setBusy(bool busy);

    AppController* m_controller;
    QLineEdit* m_filter;
    QListWidget* m_list;
    QLineEdit* m_repo;
    QLineEdit* m_parentDir;
    QPushButton* m_browse;
    QLabel* m_status;
    QDialogButtonBox* m_buttons;
    /// The repository being cloned, while a clone is out; answers for any
    /// other are not this dialog's.
    QString m_cloning;
    QString m_clonedPath;
};
