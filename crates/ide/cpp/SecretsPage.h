#pragma once
#include <QDialog>
#include <QString>
#include <QStringList>
#include <QWidget>

class AppController;
class QComboBox;
class QDialogButtonBox;
class QLabel;
class QLineEdit;
class QPushButton;
class QTreeWidget;

/// Adds a secret, or gives one a new value: its name, its value (never shown
/// again once saved) and whether it is for every repository or one.
///
/// No `Q_OBJECT`, like `CommitDialog`: wired with lambdas.
class SecretDialog : public QDialog {
public:
    /// `name` and `repo` fill the fields for a new value of an existing
    /// secret, and are then fixed; empty for a new one. `repos` are the
    /// repositories the scope offers, as the user would type them.
    SecretDialog(const QString& name, const QString& repo, const QStringList& repos,
                 QWidget* parent = nullptr);
    QString name() const;
    QString value() const;
    /// The repository as typed or chosen, or empty for every repository.
    QString repo() const;

    /// Whether `name` is one the daemon accepts, said the same way. Empty
    /// when it is.
    static QString nameProblem(const QString& name);

private:
    void updateEnabled();
    QLineEdit* m_name;
    QLineEdit* m_value;
    QComboBox* m_scope;
    QLabel* m_problem;
    QDialogButtonBox* m_buttons;
};

/// Settings › Secrets: the environment variables every agent's sandbox gets,
/// for every repository or for one. Lists names and scopes; the values stay
/// with the daemon.
class SecretsPage : public QWidget {
public:
    explicit SecretsPage(AppController* controller, QWidget* parent = nullptr);

    /// Shows the daemon's list (a JSON array of `SecretInfo`). Public for
    /// the tests.
    void showListing(const QString& json);

private:
    void addSecret();
    void changeSelected();
    void removeSelected();
    void updateButtons();
    QStringList knownRepos() const;

    AppController* m_controller;
    QTreeWidget* m_list;
    QPushButton* m_add;
    QPushButton* m_change;
    QPushButton* m_remove;
    QLabel* m_status;
};
