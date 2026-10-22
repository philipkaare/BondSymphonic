#include "IgnoreDialog.h"
#include <QComboBox>
#include <QDialogButtonBox>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QVBoxLayout>

IgnoreDialog::IgnoreDialog(const QString& workspaceName, const QString& optionsJson, QWidget* parent)
    : QDialog(parent) {
    setObjectName(QStringLiteral("IgnoreDialog"));
    setWindowTitle(QStringLiteral("Ignore — %1").arg(workspaceName));
    setModal(true);
    resize(580, 330);
    auto* layout = new QVBoxLayout(this);
    layout->addWidget(new QLabel(QStringLiteral("What should Git ignore?"), this));
    m_options = new QComboBox(this);
    m_options->setObjectName(QStringLiteral("IgnoreTarget"));
    const auto options = QJsonDocument::fromJson(optionsJson.toUtf8()).object().value("options").toArray();
    for (const auto& value : options) {
        const auto option = value.toObject();
        m_options->addItem(option.value("label").toString(), option);
    }
    layout->addWidget(m_options);
    layout->addWidget(new QLabel(QStringLiteral("Rule to add to the workspace's .gitignore:"), this));
    m_preview = new QPlainTextEdit(this);
    m_preview->setObjectName(QStringLiteral("IgnorePatternPreview"));
    m_preview->setReadOnly(true);
    layout->addWidget(m_preview, 1);
    auto* note = new QLabel(QStringLiteral(
        "Files already tracked by Git remain tracked. This does not delete any files."), this);
    note->setTextFormat(Qt::PlainText);
    note->setWordWrap(true);
    layout->addWidget(note);
    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText(QStringLiteral("Ignore"));
    layout->addWidget(m_buttons);
    connect(m_options, &QComboBox::currentIndexChanged, this, [this] { updatePreview(); });
    connect(m_buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    updatePreview();
}

QString IgnoreDialog::path() const { return m_options->currentData().toJsonObject().value("path").toString(); }
QString IgnoreDialog::target() const { return m_options->currentData().toJsonObject().value("target").toString(); }

void IgnoreDialog::updatePreview() {
    const auto option = m_options->currentData().toJsonObject();
    const QString pattern = option.value("pattern").toString();
    m_preview->setPlainText(pattern);
    m_buttons->button(QDialogButtonBox::Ok)->setEnabled(!path().isEmpty() && !target().isEmpty() && !pattern.isEmpty());
}

#ifdef BS_WIDGET_TESTS
#include <cstdint>
extern "C" std::int32_t bs_widget_test_ignore_dialog_previews_selected_rule() {
    const QString json = QStringLiteral(R"({"options":[
        {"label":"This file","path":"logs/debug.log","target":"file","pattern":"/logs/debug.log"},
        {"label":"Directory logs/","path":"logs","target":"directory","pattern":"/logs/"},
        {"label":"Repository root","path":".","target":"directory","pattern":"/*\n!/.gitignore"},
        {"label":"All .log files","path":"logs/debug.log","target":"file_type","pattern":"*.log"}
    ]})");
    IgnoreDialog dialog(QStringLiteral("alpha"), json);
    auto* options = dialog.findChild<QComboBox*>(QStringLiteral("IgnoreTarget"));
    auto* preview = dialog.findChild<QPlainTextEdit*>(QStringLiteral("IgnorePatternPreview"));
    if (!options || !preview || options->count() != 4 || !preview->isReadOnly()) { return 1; }
    if (preview->toPlainText() != QLatin1String("/logs/debug.log") || dialog.target() != QLatin1String("file")) { return 2; }
    options->setCurrentIndex(1);
    if (preview->toPlainText() != QLatin1String("/logs/") || dialog.path() != QLatin1String("logs")) { return 3; }
    options->setCurrentIndex(2);
    if (preview->toPlainText() != QLatin1String("/*\n!/.gitignore")) { return 4; }
    options->setCurrentIndex(3);
    if (preview->toPlainText() != QLatin1String("*.log") || dialog.target() != QLatin1String("file_type")) { return 5; }
    return 0;
}
#endif
