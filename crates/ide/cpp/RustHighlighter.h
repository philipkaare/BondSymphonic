#pragma once
#include <QString>
#include <QSyntaxHighlighter>
#include <functional>

class QTextDocument;

/// Paints the spans a document computed for a line.
///
/// It parses no source and knows no keywords: the whole colouring decision is
/// made by the tree-sitter pass behind `spansProvider`, which answers with
/// `[{"s":0,"l":2,"fg":"#a626a4","b":true,"i":false}, ...]` for a block, or an
/// empty array for a line with no spans (an unrecognised language, or a file
/// too large to highlight). Named for the language the milestone ships; it
/// renders whatever the provider reports.
class RustHighlighter : public QSyntaxHighlighter {
    Q_OBJECT
public:
    RustHighlighter(QTextDocument* doc, std::function<QString(int block)> spansProvider);

    /// Re-runs `highlightBlock` for blocks `from`..`to` inclusive, zero-based
    /// and clamped to the document. The order of the two does not matter.
    void rehighlightLines(int from, int to);

protected:
    void highlightBlock(const QString& text) override;

private:
    std::function<QString(int)> m_spans;
};
