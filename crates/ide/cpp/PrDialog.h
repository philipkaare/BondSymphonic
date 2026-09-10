#pragma once
#include <QDialog>
#include <QString>

class QCheckBox;
class QDialogButtonBox;
class QLineEdit;
class QPlainTextEdit;

/// Collects the title, body and draft flag for `workspace.create_pr`.
///
/// The title is prefilled with the workspace's name, which is the one thing the
/// user already chose that reads as a pull request title, and Create is refused
/// while it is empty: `gh pr create` takes a title and there is nothing
/// sensible to invent for it. Everything else is optional.
class PrDialog : public QDialog {
    Q_OBJECT
public:
    /// `workspaceName` prefills the title; `branch` and `baseBranch` are named
    /// in the strip above the fields so the user can see what is about to be
    /// pushed, and where it is going.
    PrDialog(const QString& workspaceName, const QString& branch, const QString& baseBranch,
             QWidget* parent = nullptr);

    QString title() const;
    QString body() const;
    bool draft() const;

private:
    void updateOkEnabled();

    QLineEdit* m_title = nullptr;
    QPlainTextEdit* m_body = nullptr;
    QCheckBox* m_draft = nullptr;
    QDialogButtonBox* m_buttons = nullptr;
};
