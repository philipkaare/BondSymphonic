#include "CommitDialog.h"
#include <QDialogButtonBox>
#include <QFontMetrics>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QSet>
#include <QTreeWidget>
#include <QVBoxLayout>

CommitDialog::CommitDialog(const QString& workspaceName, const QString& preparedJson,
                           const QStringList& selectedPaths, QWidget* parent)
    : QDialog(parent) {
    setObjectName(QStringLiteral("CommitDialog"));
    setWindowTitle(QStringLiteral("Commit — %1").arg(workspaceName));
    setModal(true);
    resize(620, 520);
    auto* layout = new QVBoxLayout(this);
    auto* explanation = new QLabel(QStringLiteral(
        "Review the files to commit. Other staged files are left alone."), this);
    explanation->setTextFormat(Qt::PlainText);
    explanation->setWordWrap(true);
    layout->addWidget(explanation);
    m_files = new QTreeWidget(this);
    m_files->setObjectName(QStringLiteral("CommitFiles"));
    m_files->setHeaderLabels({QStringLiteral("File"), QStringLiteral("Status")});
    m_files->setRootIsDecorated(false);
    m_files->setColumnWidth(0, 440);
    const QJsonArray files = QJsonDocument::fromJson(preparedJson.toUtf8())
                                 .object().value("files").toArray();
    QSet<QString> seen;
    for (const auto& value : files) {
        const auto file = value.toObject();
        const QString path = file.value("path").toString();
        if (path.isEmpty() || seen.contains(path)) { continue; }
        seen.insert(path);
        bool selected = selectedPaths.isEmpty();
        for (QString candidate : selectedPaths) {
            while (candidate.endsWith(QLatin1Char('/'))) { candidate.chop(1); }
            if (candidate == QLatin1String(".") || path == candidate ||
                path.startsWith(candidate + QLatin1Char('/'))) { selected = true; }
        }
        auto* item = new QTreeWidgetItem(m_files, {path, file.value("status").toString()});
        item->setData(0, Qt::UserRole, path);
        item->setCheckState(0, selected ? Qt::Checked : Qt::Unchecked);
        item->setToolTip(0, path);
    }
    layout->addWidget(m_files, 1);
    layout->addWidget(new QLabel(QStringLiteral("Commit message:"), this));
    m_message = new QPlainTextEdit(this);
    m_message->setObjectName(QStringLiteral("CommitMessage"));
    m_message->setPlaceholderText(QStringLiteral("Describe these changes"));
    m_message->setMaximumHeight(fontMetrics().lineSpacing() * 6);
    layout->addWidget(m_message);
    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText(QStringLiteral("Commit"));
    layout->addWidget(m_buttons);
    connect(m_buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    connect(m_message, &QPlainTextEdit::textChanged, this, &CommitDialog::updateEnabled);
    connect(m_files, &QTreeWidget::itemChanged, this, [this] { updateEnabled(); });
    updateEnabled();
    m_message->setFocus();
}

QString CommitDialog::message() const { return m_message->toPlainText().trimmed(); }

QStringList CommitDialog::paths() const {
    QStringList paths;
    for (int index = 0; index < m_files->topLevelItemCount(); ++index) {
        auto* item = m_files->topLevelItem(index);
        if (item->checkState(0) == Qt::Checked) { paths.append(item->data(0, Qt::UserRole).toString()); }
    }
    return paths;
}

void CommitDialog::updateEnabled() {
    m_buttons->button(QDialogButtonBox::Ok)->setEnabled(!message().isEmpty() && !paths().isEmpty());
}

#ifdef BS_WIDGET_TESTS
#include <cstdint>
extern "C" std::int32_t bs_widget_test_commit_dialog_selects_pending_paths() {
    const QString pending = QStringLiteral(R"({"files":[
        {"path":"src/main.rs","status":"modified"},
        {"path":"src/new.rs","status":"untracked"},
        {"path":"notes.txt","status":"deleted"}
    ],"ahead":0})");
    CommitDialog dialog(QStringLiteral("alpha"), pending, {QStringLiteral("src")});
    auto* files = dialog.findChild<QTreeWidget*>(QStringLiteral("CommitFiles"));
    auto* message = dialog.findChild<QPlainTextEdit*>(QStringLiteral("CommitMessage"));
    auto* buttons = dialog.findChild<QDialogButtonBox*>();
    if (!files || !message || !buttons || files->topLevelItemCount() != 3) { return 1; }
    if (buttons->button(QDialogButtonBox::Ok)->isEnabled()) { return 2; }
    if (dialog.paths() != QStringList{QStringLiteral("src/main.rs"), QStringLiteral("src/new.rs")}) { return 3; }
    message->setPlainText(QStringLiteral("  Improve login  "));
    if (dialog.message() != QLatin1String("Improve login") || !buttons->button(QDialogButtonBox::Ok)->isEnabled()) { return 4; }
    for (int i = 0; i < files->topLevelItemCount(); ++i) { files->topLevelItem(i)->setCheckState(0, Qt::Unchecked); }
    if (buttons->button(QDialogButtonBox::Ok)->isEnabled()) { return 5; }
    CommitDialog all(QStringLiteral("alpha"), pending, {});
    if (all.paths().size() != 3) { return 6; }
    return 0;
}
#endif
