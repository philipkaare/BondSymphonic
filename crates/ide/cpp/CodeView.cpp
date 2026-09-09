#include "CodeView.h"
#include <QEvent>
#include <QFont>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QFontMetricsF>
#include <QLatin1Char>
#include <QPainter>
#include <QPaintEvent>
#include <QPalette>
#include <QResizeEvent>
#include <QSize>
#include <QTextBlock>
#include <QTextDocument>
#include <QTextEdit>
#include <QtGlobal>
#include <utility>

namespace {

/// Space on either side of the gutter text.
constexpr int kGutterPadding = 6;
/// How faint the gutter numbers are against the pane's text colour.
constexpr int kGutterAlpha = 140;
/// A tab is this many `M` advances wide.
constexpr int kTabWidthInChars = 4;

} // namespace

namespace codeview {

QColor wash(const QColor& base, const QColor& tint, qreal amount) {
    const qreal keep = 1.0 - amount;
    return QColor::fromRgbF(base.redF() * keep + tint.redF() * amount,
                            base.greenF() * keep + tint.greenF() * amount,
                            base.blueF() * keep + tint.blueF() * amount);
}

} // namespace codeview

LineNumberArea::LineNumberArea(CodeView* view) : QWidget(view), m_view(view) {}

QSize LineNumberArea::sizeHint() const {
    return QSize(m_view->gutterWidth(), 0);
}

void LineNumberArea::paintEvent(QPaintEvent* event) {
    m_view->paintGutter(event);
}

CodeView::CodeView(QWidget* parent) : QPlainTextEdit(parent) {
    m_provider = [](int block) { return QString::number(block + 1); };

    QFont font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    font.setPointSize(10);
    font.setStyleHint(QFont::Monospace);
    font.setFixedPitch(true);
    setFont(font);
    // Fractional: a tab stop rounded to whole pixels drifts away from the
    // character grid over a deeply indented line.
    const QFontMetricsF metrics(font);
    setTabStopDistance(kTabWidthInChars * metrics.horizontalAdvance(QLatin1Char('M')));
    setLineWrapMode(QPlainTextEdit::NoWrap);

    m_gutter = new LineNumberArea(this);

    QObject::connect(this, &QPlainTextEdit::blockCountChanged, this,
                     [this](int) { updateGutterGeometry(); });
    QObject::connect(this, &QPlainTextEdit::updateRequest, this, [this](const QRect& rect, int dy) {
        // Scrolling moves the gutter's pixels with the text; anything else
        // repaints the strip the viewport asked for.
        if (dy != 0) {
            m_gutter->scroll(0, dy);
        } else {
            m_gutter->update(0, rect.y(), m_gutter->width(), rect.height());
        }
        if (rect.contains(viewport()->rect())) {
            updateGutterGeometry();
        }
    });
    QObject::connect(this, &QPlainTextEdit::cursorPositionChanged, this,
                     &CodeView::updateExtraSelections);

    updateGutterGeometry();
    updateExtraSelections();
}

void CodeView::setLineNumberProvider(std::function<QString(int)> provider) {
    if (!provider) {
        return;
    }
    m_provider = std::move(provider);
    updateGutterGeometry();
    m_gutter->update();
}

void CodeView::setRowTints(const QVector<QColor>& perBlock) {
    m_rowTints = perBlock;
    updateExtraSelections();
}

bool CodeView::event(QEvent* event) {
    const bool handled = QPlainTextEdit::event(event);
    if (event->type() == QEvent::ReadOnlyChange) {
        updateExtraSelections();
    }
    return handled;
}

int CodeView::gutterWidth() const {
    // The provider is monotone in width for both numbering schemes it is used
    // with (one column of line numbers, or a diff's two), so the first and last
    // blocks bracket the widest label without walking the document.
    const QFontMetrics metrics(font());
    const int last = qMax(0, blockCount() - 1);
    const int text = qMax(metrics.horizontalAdvance(m_provider(0)),
                          metrics.horizontalAdvance(m_provider(last)));
    return text + 2 * kGutterPadding;
}

void CodeView::updateGutterGeometry() {
    const int width = gutterWidth();
    setViewportMargins(width, 0, 0, 0);
    const QRect area = contentsRect();
    m_gutter->setGeometry(QRect(area.left(), area.top(), width, area.height()));
}

void CodeView::resizeEvent(QResizeEvent* event) {
    QPlainTextEdit::resizeEvent(event);
    updateGutterGeometry();
}

void CodeView::paintGutter(QPaintEvent* event) {
    QPainter painter(m_gutter);
    painter.fillRect(event->rect(), palette().window());
    QColor ink = palette().text().color();
    ink.setAlpha(kGutterAlpha);
    painter.setPen(ink);
    painter.setFont(font());

    QTextBlock block = firstVisibleBlock();
    int top = qRound(blockBoundingGeometry(block).translated(contentOffset()).top());
    const int lineHeight = fontMetrics().height();
    while (block.isValid() && top <= event->rect().bottom()) {
        const int bottom = top + qRound(blockBoundingRect(block).height());
        if (block.isVisible() && bottom >= event->rect().top()) {
            painter.drawText(0, top, m_gutter->width() - kGutterPadding, lineHeight,
                             Qt::AlignRight | Qt::AlignTop, m_provider(block.blockNumber()));
        }
        block = block.next();
        top = bottom;
    }
}

void CodeView::updateExtraSelections() {
    // A pane nobody can type into shows no caret; Qt keeps drawing one for the
    // keyboard selection a read-only text edit still supports.
    const int caret = isReadOnly() ? 0 : 1;
    if (cursorWidth() != caret) {
        setCursorWidth(caret);
    }

    QList<QTextEdit::ExtraSelection> selections;
    // Tints first: extra selections are painted in order, so the current-line
    // band appended last is the one on top.
    for (int n = 0; n < m_rowTints.size(); ++n) {
        const QColor tint = m_rowTints.at(n);
        if (!tint.isValid()) {
            continue;
        }
        const QTextBlock block = document()->findBlockByNumber(n);
        if (!block.isValid()) {
            break;
        }
        QTextEdit::ExtraSelection row;
        row.format.setBackground(tint);
        row.format.setProperty(QTextFormat::FullWidthSelection, true);
        row.cursor = QTextCursor(block);
        row.cursor.clearSelection();
        selections.append(row);
    }

    // Last, so it wins over a tint on the same row -- and only on a pane that
    // is being typed into, so the diff's tints are never covered.
    if (!isReadOnly()) {
        QTextEdit::ExtraSelection current;
        current.format.setBackground(codeview::wash(palette().base().color(),
                                                    palette().alternateBase().color(),
                                                    codeview::kWashAmount));
        current.format.setProperty(QTextFormat::FullWidthSelection, true);
        current.cursor = textCursor();
        current.cursor.clearSelection();
        selections.append(current);
    }

    setExtraSelections(selections);
}
