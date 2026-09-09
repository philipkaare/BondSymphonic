#pragma once
#include <QColor>
#include <QPlainTextEdit>
#include <QVector>
#include <QWidget>
#include <functional>

class CodeView;
class QEvent;
class QPaintEvent;
class QResizeEvent;
class QSize;

namespace codeview {
/// `tint` mixed into `base` by `amount` (0..1).
///
/// A palette role picked for a *fill* is often far too strong to put behind
/// text: on this machine's dark theme `AlternateBase` is a saturated navy over
/// a near-black `Base`. Washing it into the background keeps the hint and the
/// legibility, and degrades the same way on a light palette.
QColor wash(const QColor& base, const QColor& tint, qreal amount);

/// How much of a palette role a background wash keeps.
inline constexpr qreal kWashAmount = 0.15;
} // namespace codeview

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

/// A code pane: monospaced, unwrapped, with a gutter, a current-line band and
/// optional per-line background tints.
///
/// It knows nothing about files, documents or diffs. What the gutter says comes
/// from a provider the owner installs, and the tints are a vector the owner
/// hands over, so the same widget serves the editor and the side-by-side diff.
/// Syntax colouring is not its business either: that is a `QSyntaxHighlighter`
/// attached to its document.
///
/// A read-only pane gets no current-line band and no caret. The band would sit
/// on top of the row tint of whichever row the invisible caret happened to be
/// on, and there is nothing being typed for it to mark.
class CodeView : public QPlainTextEdit {
    Q_OBJECT
public:
    explicit CodeView(QWidget* parent = nullptr);

    /// What the gutter prints beside block `n`, zero-based. The default prints
    /// `n + 1`. Replacing it repaints the gutter and re-measures its width.
    void setLineNumberProvider(std::function<QString(int block)> provider);

    /// A background colour per block, indexed by block number. An empty vector
    /// clears every tint, and an invalid colour leaves that one block untinted.
    void setRowTints(const QVector<QColor>& perBlock);

    /// The gutter's width in pixels, including its padding.
    int gutterWidth() const;

protected:
    void resizeEvent(QResizeEvent* event) override;
    /// Watches for `QEvent::ReadOnlyChange`, which is how the pane learns that
    /// its caret and its current-line band have just become someone else's
    /// answer. Nothing else tells it: `setReadOnly` is not virtual and has no
    /// signal, and an owner should not have to remember a second call.
    bool event(QEvent* event) override;

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
