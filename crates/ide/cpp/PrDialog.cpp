#include "PrDialog.h"
#include <QCheckBox>
#include <QDialogButtonBox>
#include <QFontMetrics>
#include <QFormLayout>
#include <QLabel>
#include <QLineEdit>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QVBoxLayout>

namespace {

/// How many lines of body are visible before the box scrolls.
constexpr int kBodyRows = 8;

} // namespace

PrDialog::PrDialog(const QString& workspaceName, const QString& branch, const QString& baseBranch,
                   QWidget* parent)
    : QDialog(parent) {
    setWindowTitle(QStringLiteral("Create pull request"));
    setModal(true);

    auto* outer = new QVBoxLayout(this);

    auto* what = new QLabel(this);
    what->setObjectName(QStringLiteral("PrDialogTarget"));
    // Plain text: the two branch names are the daemon's, and one of them is a
    // repository's own.
    what->setTextFormat(Qt::PlainText);
    what->setWordWrap(true);
    what->setText(QStringLiteral("Push %1 and open a pull request against %2.")
                      .arg(branch, baseBranch));
    outer->addWidget(what);

    auto* form = new QFormLayout();
    outer->addLayout(form);

    m_title = new QLineEdit(workspaceName, this);
    m_title->setObjectName(QStringLiteral("PrDialogTitle"));
    m_title->selectAll();
    form->addRow(QStringLiteral("Title:"), m_title);

    m_body = new QPlainTextEdit(this);
    m_body->setObjectName(QStringLiteral("PrDialogBody"));
    m_body->setPlaceholderText(QStringLiteral("What changed, and why."));
    const QFontMetrics metrics(m_body->font());
    m_body->setFixedHeight(kBodyRows * metrics.lineSpacing() +
                           2 * static_cast<int>(m_body->frameWidth()) +
                           2 * static_cast<int>(m_body->document()->documentMargin()));
    form->addRow(QStringLiteral("Body:"), m_body);

    m_draft = new QCheckBox(QStringLiteral("Open as a draft"), this);
    m_draft->setObjectName(QStringLiteral("PrDialogDraft"));
    form->addRow(QString(), m_draft);

    m_buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel, this);
    m_buttons->button(QDialogButtonBox::Ok)->setText(QStringLiteral("Create"));
    outer->addWidget(m_buttons);

    QObject::connect(m_buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    QObject::connect(m_buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    QObject::connect(m_title, &QLineEdit::textChanged, this, &PrDialog::updateOkEnabled);
    updateOkEnabled();
}

QString PrDialog::title() const {
    return m_title->text().trimmed();
}

QString PrDialog::body() const {
    return m_body->toPlainText();
}

bool PrDialog::draft() const {
    return m_draft->isChecked();
}

void PrDialog::updateOkEnabled() {
    m_buttons->button(QDialogButtonBox::Ok)->setEnabled(!title().isEmpty());
}
