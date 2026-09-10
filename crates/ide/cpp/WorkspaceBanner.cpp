#include "WorkspaceBanner.h"
#include "CodeView.h"
#include "Theme.h"
#include <QChar>
#include <QFont>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QHBoxLayout>
#include <QLabel>
#include <QPalette>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QSizePolicy>
#include <QToolButton>
#include <QVBoxLayout>

namespace {

/// How many lines of stderr are visible before the box scrolls. Enough for a
/// git error's usual three or four, without the banner taking the pane over.
constexpr int kStderrRows = 8;

/// U+25B6 and U+25BC, the collapsed and expanded disclosure arrows, as code
/// points rather than as characters in a literal: the file is compiled without
/// a byte order mark and MSVC would otherwise read them in the system code
/// page.
QString arrow(bool expanded) {
    return QString(QChar(expanded ? 0x25BC : 0x25B6));
}

} // namespace

WorkspaceBanner::WorkspaceBanner(QWidget* parent) : QFrame(parent) {
    setObjectName(QStringLiteral("WorkspaceBanner"));
    setFrameShape(QFrame::StyledPanel);
    setAutoFillBackground(true);
    // The same red wash the Run panel's blocked-host toast uses: this is a
    // report of something that did not happen, not a question waiting on an
    // answer.
    QPalette colours = palette();
    colours.setColor(QPalette::Window, codeview::wash(palette().base().color(), theme::removed(),
                                                      codeview::kWashAmount));
    setPalette(colours);

    auto* outer = new QVBoxLayout(this);
    outer->setContentsMargins(8, 6, 8, 6);
    outer->setSpacing(4);

    auto* head = new QHBoxLayout();
    head->setContentsMargins(0, 0, 0, 0);
    head->setSpacing(8);

    auto* lines = new QVBoxLayout();
    lines->setContentsMargins(0, 0, 0, 0);
    lines->setSpacing(0);
    m_title = new QLabel(this);
    m_title->setObjectName(QStringLiteral("WorkspaceBannerTitle"));
    // Plain text throughout: every string here comes from the daemon or from a
    // git command's output, and neither is markup.
    m_title->setTextFormat(Qt::PlainText);
    m_title->setWordWrap(true);
    QFont titleFont = m_title->font();
    titleFont.setBold(true);
    m_title->setFont(titleFont);
    m_title->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    lines->addWidget(m_title);

    m_detail = new QLabel(this);
    m_detail->setObjectName(QStringLiteral("WorkspaceBannerDetail"));
    m_detail->setTextFormat(Qt::PlainText);
    m_detail->setWordWrap(true);
    m_detail->setTextInteractionFlags(Qt::TextSelectableByMouse);
    m_detail->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    m_detail->hide();
    lines->addWidget(m_detail);
    head->addLayout(lines, 1);

    m_disclose = new QToolButton(this);
    m_disclose->setObjectName(QStringLiteral("WorkspaceBannerDetailsButton"));
    m_disclose->setAutoRaise(true);
    m_disclose->setToolButtonStyle(Qt::ToolButtonTextOnly);
    m_disclose->setToolTip(QStringLiteral("Show what git printed"));
    m_disclose->hide();
    head->addWidget(m_disclose, 0, Qt::AlignTop);

    m_dismiss = new QPushButton(QStringLiteral("Dismiss"), this);
    m_dismiss->setObjectName(QStringLiteral("WorkspaceBannerDismissButton"));
    head->addWidget(m_dismiss, 0, Qt::AlignTop);
    outer->addLayout(head);

    m_stderr = new QPlainTextEdit(this);
    m_stderr->setObjectName(QStringLiteral("WorkspaceBannerStderr"));
    m_stderr->setReadOnly(true);
    m_stderr->setLineWrapMode(QPlainTextEdit::NoWrap);
    QFont mono = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    mono.setStyleHint(QFont::Monospace);
    m_stderr->setFont(mono);
    m_stderr->hide();
    outer->addWidget(m_stderr);

    QObject::connect(m_disclose, &QToolButton::clicked, this,
                     [this] { setExpanded(!m_stderr->isVisible()); });
    QObject::connect(m_dismiss, &QPushButton::clicked, this, [this] {
        reset();
        emit dismissed();
    });

    QFrame::hide();
}

void WorkspaceBanner::showError(const QString& title, const QString& detail,
                                const QString& stderrText) {
    m_title->setText(title);
    m_title->setToolTip(title);
    m_detail->setText(detail);
    m_detail->setVisible(!detail.isEmpty());
    m_stderr->setPlainText(stderrText);
    m_disclose->setVisible(!stderrText.isEmpty());
    // Every raise starts folded: the sentence is the news, and a wall of git
    // output unfolding by itself would push the pane down each time.
    setExpanded(false);
    QFrame::show();
}

void WorkspaceBanner::reset() {
    m_title->clear();
    m_detail->clear();
    m_detail->hide();
    m_stderr->clear();
    m_disclose->hide();
    setExpanded(false);
    QFrame::hide();
}

void WorkspaceBanner::setExpanded(bool expanded) {
    const bool haveStderr = !m_stderr->toPlainText().isEmpty();
    const bool open = expanded && haveStderr;
    m_stderr->setVisible(open);
    if (open) {
        const QFontMetrics metrics(m_stderr->font());
        m_stderr->setFixedHeight(kStderrRows * metrics.lineSpacing() +
                                 2 * static_cast<int>(m_stderr->frameWidth()));
    }
    m_disclose->setText(arrow(open) + QStringLiteral(" Details"));
}
