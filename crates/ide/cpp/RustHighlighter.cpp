#include "RustHighlighter.h"
#include <QColor>
#include <QFont>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QList>
#include <QTextBlock>
#include <QTextCharFormat>
#include <QTextDocument>
#include <QtGlobal>
#include <utility>

namespace {

/// Whether `text` holds anything outside the BMP, which is the only case in
/// which a character index and a `QString` index differ.
bool hasSurrogates(const QString& text) {
    for (const QChar c : text) {
        if (c.isHighSurrogate()) {
            return true;
        }
    }
    return false;
}

/// Maps character index -> UTF-16 offset in `text`, plus a final sentinel.
QList<int> charOffsets(const QString& text) {
    QList<int> offsets;
    offsets.reserve(text.size() + 1);
    for (int i = 0; i < text.size();) {
        offsets.append(i);
        const bool pair = text.at(i).isHighSurrogate() && i + 1 < text.size() &&
                          text.at(i + 1).isLowSurrogate();
        i += pair ? 2 : 1;
    }
    offsets.append(text.size());
    return offsets;
}

} // namespace

RustHighlighter::RustHighlighter(QTextDocument* doc, std::function<QString(int)> spansProvider)
    : QSyntaxHighlighter(doc), m_spans(std::move(spansProvider)) {}

void RustHighlighter::rehighlightLines(int from, int to) {
    QTextDocument* doc = document();
    if (doc == nullptr) {
        return;
    }
    const int last = doc->blockCount() - 1;
    if (last < 0) {
        return;
    }
    const int first = qBound(0, qMin(from, to), last);
    const int end = qBound(0, qMax(from, to), last);
    for (int n = first; n <= end; ++n) {
        const QTextBlock block = doc->findBlockByNumber(n);
        if (block.isValid()) {
            rehighlightBlock(block);
        }
    }
}

void RustHighlighter::highlightBlock(const QString& text) {
    if (!m_spans) {
        return;
    }
    const QString json = m_spans(currentBlock().blockNumber());
    const QJsonArray spans = QJsonDocument::fromJson(json.toUtf8()).array();
    if (spans.isEmpty()) {
        return;
    }
    // The spans count characters, `setFormat` counts UTF-16 units. They agree
    // on every line that stays inside the BMP, which is nearly every line, so
    // the mapping table is only built when one does not.
    const bool wide = hasSurrogates(text);
    const QList<int> offsets = wide ? charOffsets(text) : QList<int>();
    const int chars = wide ? static_cast<int>(offsets.size()) - 1 : static_cast<int>(text.size());

    for (const QJsonValue& value : spans) {
        const QJsonObject span = value.toObject();
        const int start = span.value(QStringLiteral("s")).toInt();
        const int len = span.value(QStringLiteral("l")).toInt();
        if (start < 0 || len <= 0 || start >= chars) {
            continue;
        }
        const int end = qMin(start + len, chars);
        QTextCharFormat format;
        const QColor colour = QColor::fromString(span.value(QStringLiteral("fg")).toString());
        if (colour.isValid()) {
            format.setForeground(colour);
        }
        if (span.value(QStringLiteral("b")).toBool()) {
            format.setFontWeight(QFont::Bold);
        }
        if (span.value(QStringLiteral("i")).toBool()) {
            format.setFontItalic(true);
        }
        const int at = wide ? offsets.at(start) : start;
        const int stop = wide ? offsets.at(end) : end;
        setFormat(at, stop - at, format);
    }
}
