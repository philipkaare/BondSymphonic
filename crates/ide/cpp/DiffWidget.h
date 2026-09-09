#pragma once
#include <QPointer>
#include <QString>
#include <QVector>
#include <QWidget>

class CodeView;
class DiffDocument;
class QLabel;
class QScrollBar;

/// One file's working copy beside its base: a header and two read-only panes.
///
/// The widget computes nothing about the diff. The rows, the stat counts and
/// the truncation flag all come from the `DiffDocument`; this only turns a row
/// into a line of text, a gutter label and a background tint, and keeps the two
/// panes on the same row. Both sides have exactly one block per row, so
/// scrolling one to a value and the other to the same value lines them up
/// without any measuring.
class DiffWidget : public QWidget {
    Q_OBJECT
public:
    /// Takes ownership of `doc`, so closing the tab ends its work.
    explicit DiffWidget(DiffDocument* doc, QWidget* parent = nullptr);

private:
    /// Fills both panes from `rowsJson`: text, gutter numbers and row tints.
    void onRowsLoaded();
    /// Puts `message` in the header in red.
    void onLoadFailed(const QString& message);
    /// The path, the stat and whatever the header has to say about the diff.
    void updateHeader();
    /// Makes `to` follow `from`, without the answer coming back. `guard` is the
    /// flag for that axis, and the two axes must not share one.
    void link(QScrollBar* from, QScrollBar* to, bool* guard);

    QPointer<DiffDocument> m_doc;
    QLabel* m_path = nullptr;
    QLabel* m_stat = nullptr;
    /// The truncation warning, or the load error in red. Hidden when neither.
    QLabel* m_notice = nullptr;
    CodeView* m_left = nullptr;
    CodeView* m_right = nullptr;
    /// The 1-based line number each block carries on that side, 0 where the row
    /// does not occupy the side. Indexed by block number, which is the row
    /// index: one row is one block.
    QVector<int> m_leftNo;
    QVector<int> m_rightNo;
    /// How many characters every gutter label on that side is padded to.
    /// `CodeView` measures its gutter at the first and last block only, so a
    /// row without a number in between would otherwise be able to make the
    /// widest label one the measurement never saw.
    int m_leftDigits = 0;
    int m_rightDigits = 0;
    /// The last `loadFailed` message, cleared by the next set of rows.
    QString m_loadError;
    /// Set while one scrollbar is moving the other, one flag per axis. They
    /// cannot be a single flag: scrolling vertically can clamp the driven
    /// pane's horizontal value in the same call, and a shared flag would drop
    /// that as an echo and leave the two sides on different columns.
    bool m_syncingVertical = false;
    bool m_syncingHorizontal = false;
};
