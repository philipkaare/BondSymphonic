#pragma once
#include <QDialog>
#include <QStringList>

class QDialogButtonBox;
class QPlainTextEdit;
class QTreeWidget;

/// Reviews the actual pending files, independently of the branch diff list.
class CommitDialog : public QDialog {
public:
    CommitDialog(const QString& workspaceName, const QString& preparedJson,
                 const QStringList& selectedPaths, QWidget* parent = nullptr);
    QString message() const;
    QStringList paths() const;

private:
    void updateEnabled();
    QTreeWidget* m_files;
    QPlainTextEdit* m_message;
    QDialogButtonBox* m_buttons;
};
