#include "ToolCard.h"
#include "CodeView.h"
#include "Theme.h"
#include <QChar>
#include <QColor>
#include <QFont>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QHBoxLayout>
#include <QJsonDocument>
#include <QJsonParseError>
#include <QJsonValue>
#include <QLabel>
#include <QPalette>
#include <QPlainTextEdit>
#include <QResizeEvent>
#include <QScrollBar>
#include <QSizePolicy>
#include <QToolButton>
#include <QVBoxLayout>

namespace {

/// Tallest either body pane grows before it scrolls. A tool that printed a
/// thousand lines must not push the rest of the transcript off the screen.
constexpr int kPaneMaxHeight = 200;

/// A call that has not answered yet, one that has, and one that failed.
///
/// Written as code points rather than as literals: the sources carry no BOM,
/// so a compiler reading them in the system code page would mangle a glyph
/// whose bytes it cannot map. HOURGLASS WITH FLOWING SAND, CHECK MARK, BALLOT X.
constexpr char16_t kGlyphPending = 0x23F3;
constexpr char16_t kGlyphOk = 0x2713;
constexpr char16_t kGlyphError = 0x2717;

/// `input_json` is stored on one line; the card shows it indented.
QString prettyJson(const QString& compact) {
    if (compact.isEmpty()) {
        return QString();
    }
    QJsonParseError error{};
    const QJsonDocument doc = QJsonDocument::fromJson(compact.toUtf8(), &error);
    if (error.error != QJsonParseError::NoError) {
        // Not valid JSON is still worth showing: it is what the daemon sent.
        return compact;
    }
    return QString::fromUtf8(doc.toJson(QJsonDocument::Indented)).trimmed();
}

/// A read-only monospace pane for a tool's input or output.
QPlainTextEdit* makePane(QWidget* parent) {
    auto* pane = new QPlainTextEdit(parent);
    QFont font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    font.setStyleHint(QFont::Monospace);
    font.setFixedPitch(true);
    pane->setFont(font);
    pane->setReadOnly(true);
    pane->setLineWrapMode(QPlainTextEdit::NoWrap);
    return pane;
}

/// The height a pane needs for what is in it, capped.
///
/// Left to its own size hint a `QPlainTextEdit` asks for the same tall box
/// whether it holds two lines or two hundred, and a card with three lines of
/// JSON in a 200 px well reads as a bug. The horizontal scroll bar's strip is
/// always reserved: the lines are not wrapped, so it can appear at any width.
int paneHeight(const QPlainTextEdit* pane) {
    const QFontMetrics metrics(pane->font());
    const int chrome = 2 * static_cast<int>(pane->frameWidth()) +
                       2 * static_cast<int>(pane->document()->documentMargin()) +
                       pane->horizontalScrollBar()->sizeHint().height();
    return qMin(pane->document()->blockCount() * metrics.lineSpacing() + chrome, kPaneMaxHeight);
}

/// A caption above a body pane, in the small grey the transcript uses.
QLabel* makeCaption(const QString& text, QWidget* parent) {
    auto* label = new QLabel(text, parent);
    QFont font = label->font();
    font.setPointSizeF(font.pointSizeF() * 0.85);
    label->setFont(font);
    label->setEnabled(false);
    return label;
}

} // namespace

ToolCard::ToolCard(const QJsonObject& item, QWidget* parent) : QFrame(parent) {
    setFrameShape(QFrame::StyledPanel);
    setAutoFillBackground(true);
    QPalette cardPalette = palette();
    // The same wash the editor's notice bar uses, so a card reads as a panel
    // rather than as another paragraph of the conversation.
    cardPalette.setColor(QPalette::Window,
                         codeview::wash(palette().base().color(), palette().alternateBase().color(),
                                        codeview::kWashAmount));
    setPalette(cardPalette);

    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(6, 4, 6, 4);
    layout->setSpacing(4);

    auto* header = new QHBoxLayout();
    header->setSpacing(6);
    m_toggle = new QToolButton(this);
    m_toggle->setAutoRaise(true);
    m_toggle->setArrowType(Qt::RightArrow);
    m_toggle->setToolTip(QStringLiteral("Show the call's input and result"));
    header->addWidget(m_toggle, 0);

    m_name = new QLabel(this);
    QFont nameFont = m_name->font();
    nameFont.setBold(true);
    m_name->setFont(nameFont);
    m_name->setTextFormat(Qt::PlainText);
    header->addWidget(m_name, 0);

    m_summary = new QLabel(this);
    m_summary->setTextFormat(Qt::PlainText);
    // The summary is what gives way when the pane is narrow; the tool's name
    // and its status glyph are never worth eliding.
    m_summary->setMinimumWidth(0);
    m_summary->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    header->addWidget(m_summary, 1);

    m_status = new QLabel(this);
    m_status->setTextFormat(Qt::PlainText);
    header->addWidget(m_status, 0);
    layout->addLayout(header);

    m_body = new QWidget(this);
    auto* bodyLayout = new QVBoxLayout(m_body);
    bodyLayout->setContentsMargins(0, 0, 0, 0);
    bodyLayout->setSpacing(2);
    m_inputCaption = makeCaption(QStringLiteral("Input"), m_body);
    m_input = makePane(m_body);
    m_resultCaption = makeCaption(QStringLiteral("Result"), m_body);
    m_result = makePane(m_body);
    bodyLayout->addWidget(m_inputCaption);
    bodyLayout->addWidget(m_input);
    bodyLayout->addWidget(m_resultCaption);
    bodyLayout->addWidget(m_result);
    layout->addWidget(m_body);

    QObject::connect(m_toggle, &QToolButton::clicked, this, [this] {
        // Reported, not applied: the model owns the flag and sends the item
        // back. Applying it here as well would fold the card twice on a fast
        // double click, once from the click and once from the answer.
        Q_EMIT collapsedChanged(!m_collapsed);
    });

    update(item);
}

void ToolCard::update(const QJsonObject& item) {
    m_name->setText(item.value(QStringLiteral("name")).toString());
    m_summaryText = item.value(QStringLiteral("summary")).toString();
    elideSummary();
    m_summary->setToolTip(m_summaryText);

    const QJsonValue result = item.value(QStringLiteral("result"));
    const bool hasResult = result.isString();
    const bool isError = item.value(QStringLiteral("is_error")).toBool();
    const bool dark = theme::isDark(palette());
    if (!hasResult) {
        m_status->setText(QChar(kGlyphPending));
        m_status->setToolTip(QStringLiteral("Waiting for the result"));
        m_status->setStyleSheet(QString());
    } else {
        m_status->setText(QChar(isError ? kGlyphError : kGlyphOk));
        m_status->setToolTip(isError ? QStringLiteral("The call failed") : QStringLiteral("Done"));
        const QColor accent = theme::ink(isError ? theme::removed() : theme::added(), dark);
        m_status->setStyleSheet(QStringLiteral("color: %1").arg(accent.name()));
    }

    fillPane(m_input, m_inputCaption, prettyJson(item.value(QStringLiteral("input_json")).toString()),
             true);
    // Shown on `hasResult`, not on the text: a tool that answered with nothing
    // has answered, and an absent pane would say it had not.
    fillPane(m_result, m_resultCaption, result.toString(), hasResult);

    applyCollapsed(item.value(QStringLiteral("collapsed")).toBool(true));
}

bool ToolCard::isCollapsed() const { return m_collapsed; }

void ToolCard::resizeEvent(QResizeEvent* event) {
    QFrame::resizeEvent(event);
    elideSummary();
}

void ToolCard::fillPane(QPlainTextEdit* pane, QLabel* caption, const QString& text, bool visible) {
    pane->setPlainText(text);
    pane->setFixedHeight(paneHeight(pane));
    pane->setVisible(visible);
    caption->setVisible(visible);
}

void ToolCard::applyCollapsed(bool collapsed) {
    m_collapsed = collapsed;
    m_body->setVisible(!collapsed);
    m_toggle->setArrowType(collapsed ? Qt::RightArrow : Qt::DownArrow);
}

void ToolCard::elideSummary() {
    const QFontMetrics metrics(m_summary->font());
    const int available = m_summary->width();
    if (available <= 0) {
        m_summary->setText(m_summaryText);
        return;
    }
    m_summary->setText(metrics.elidedText(m_summaryText, Qt::ElideRight, available));
}
