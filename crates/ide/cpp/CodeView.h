#pragma once
#include <QColor>
#include <QPlainTextEdit>
#include <QVector>
#include <QWidget>
#include <functional>

class CodeView;
class QPaintEvent;
class QResizeEvent;
class QSize;

/// The gutter down the left edge of a `CodeView`.
///
/// A bare child widget: it owns nothing and decides nothing, it only hands its
/// paint event back to the view, which is where the block geometry lives.
class LineNumberArea : public QWidget {
public:
    explicit LineNumberArea(CodeView* view);

    QSize sizeHint() const override;

protected:
    void paintEvent(QPaintEvent* event) override;

private:
    CodeView* m_view;
};

/// A read-write code pane: monospaced, unwrapped, with a gutter, a current-line
/// highlight and optional per-line background tints.
///
/// It knows nothing about files, documents or diffs. What the gutter says comes
/// from a provider the owner installs, and the tints are a vector the owner
/// hands over, so the same widget serves the editor and the side-by-side diff.
/// Syntax colouring is not its business either: that is a `QSyntaxHighlighter`
/// attached to its document.
class CodeView : public QPlainTextEdit {
    Q_OBJECT
public:
    explicit CodeView(QWidget* parent = nullptr);

    /// What the gutter prints beside block `n`, zero-based. The default prints
    /// `n + 1`. Replacing it repaints the gutter and re-measures its width.
    void setLineNumberProvider(std::function<QString(int block)> provider);

    /// A background colour per block, indexed by block number. An empty vector
    /// clears every tint, and an invalid colour leaves that one block untinted.
    /// The tints sit behind the current-line highlight.
    void setRowTints(const QVector<QColor>& perBlock);

    /// The gutter's width in pixels, including its padding.
    int gutterWidth() const;

protected:
    void resizeEvent(QResizeEvent* event) override;

private:
    friend class LineNumberArea;

    void paintGutter(QPaintEvent* event);
    /// Re-measures the gutter and moves it over the viewport margin.
    void updateGutterGeometry();
    void updateExtraSelections();

    LineNumberArea* m_gutter = nullptr;
    /// Never null: the constructor installs the default numbering.
    std::function<QString(int)> m_provider;
    QVector<QColor> m_rowTints;
};
