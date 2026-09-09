#pragma once
#include <QFrame>
#include <QJsonObject>
#include <QString>

class QLabel;
class QPlainTextEdit;
class QResizeEvent;
class QToolButton;

/// One tool call in a transcript: a header that always shows and a body that
/// unfolds.
///
/// The card holds no state of its own beyond what it was last handed. Whether
/// it is collapsed is the transcript model's answer, not the card's: the toggle
/// reports the click and the model sends the item back with the new value.
class ToolCard : public QFrame {
    Q_OBJECT
public:
    /// `item` is one `tool_use` frame from `TranscriptModel::itemJson`.
    explicit ToolCard(const QJsonObject& item, QWidget* parent = nullptr);

    /// `QWidget::update()` is still reachable; the overload below only adds to
    /// it. Without this the repaint slot would be hidden by name lookup.
    using QFrame::update;

    /// Repaints the card from a newer copy of the same item, in place.
    void update(const QJsonObject& item);

    bool isCollapsed() const;

Q_SIGNALS:
    /// The user worked the toggle. The model decides what happens next.
    void collapsedChanged(bool collapsed);

protected:
    /// Re-elides the summary, which has no width of its own to elide against
    /// until the card is laid out.
    void resizeEvent(QResizeEvent* event) override;

private:
    /// Fills one of the two body panes and sizes it to what it holds. A pane
    /// the item has nothing for at all is hidden along with its caption.
    static void fillPane(QPlainTextEdit* pane, QLabel* caption, const QString& text, bool visible);
    void applyCollapsed(bool collapsed);
    void elideSummary();

    QToolButton* m_toggle = nullptr;
    QLabel* m_name = nullptr;
    QLabel* m_summary = nullptr;
    QLabel* m_status = nullptr;
    QWidget* m_body = nullptr;
    QLabel* m_inputCaption = nullptr;
    QPlainTextEdit* m_input = nullptr;
    QLabel* m_resultCaption = nullptr;
    QPlainTextEdit* m_result = nullptr;
    /// The unelided summary, so a resize can elide the full text again rather
    /// than eliding what is already elided.
    QString m_summaryText;
    bool m_collapsed = true;
};
