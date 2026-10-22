#pragma once
#include <QDialog>
#include <QString>

class QComboBox;
class QDialogButtonBox;
class QPlainTextEdit;

/// Chooses one daemon-generated, precisely previewed .gitignore rule.
class IgnoreDialog : public QDialog {
public:
    IgnoreDialog(const QString& workspaceName, const QString& optionsJson, QWidget* parent = nullptr);
    QString path() const;
    QString target() const;

private:
    void updatePreview();
    QComboBox* m_options;
    QPlainTextEdit* m_preview;
    QDialogButtonBox* m_buttons;
};
