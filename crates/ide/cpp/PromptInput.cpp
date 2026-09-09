#include "PromptInput.h"
#include <QFontMetrics>
#include <QKeyEvent>
#include <QPalette>

namespace {

/// What the box says when it is ready for a prompt.
const char* kIdlePlaceholder = "Message Claude…  (Enter to send, Shift+Enter for a new line)";

/// What it says while the agent is working, unless the caller supplies its own.
const char* kBusyPlaceholder = "Claude is working…";

/// How many lines of prompt are visible before the box scrolls.
constexpr int kVisibleLines = 3;

} // namespace

PromptInput::PromptInput(QWidget* parent) : QPlainTextEdit(parent) {
    setVerticalScrollBarPolicy(Qt::ScrollBarAsNeeded);
    setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
    setLineWrapMode(QPlainTextEdit::WidgetWidth);
    setTabChangesFocus(true);
    // A few lines tall and no taller: the transcript above is what the pane is
    // for, and a box that grew with the text would eat it.
    const QFontMetrics metrics(font());
    const int frame = 2 * static_cast<int>(frameWidth()) + 2 * static_cast<int>(document()->documentMargin());
    setMaximumHeight(kVisibleLines * metrics.lineSpacing() + frame);
    m_idlePlaceholderColor = palette().color(QPalette::PlaceholderText);
    refreshPlaceholder();
}

void PromptInput::setBusy(bool busy, const QString& message) {
    m_busy = busy;
    m_busyMessage = message;
    // Read-only rather than disabled: the user can still select and copy what
    // they typed while the answer to it is being written.
    setReadOnly(busy);
    refreshPlaceholder();
}

bool PromptInput::isBusy() const { return m_busy; }

void PromptInput::submit() {
    if (m_busy) {
        return;
    }
    const QString text = toPlainText().trimmed();
    if (text.isEmpty()) {
        // Nothing to send, but the whitespace that was typed goes: an empty
        // box is what "Enter did nothing" should look like.
        clear();
        return;
    }
    clear();
    Q_EMIT submitted(text);
}

void PromptInput::keyPressEvent(QKeyEvent* event) {
    const bool isReturn = event->key() == Qt::Key_Return || event->key() == Qt::Key_Enter;
    if (isReturn && (event->modifiers() & Qt::ShiftModifier) == 0) {
        // Any other modifier (Ctrl, Alt) sends too: only Shift is spelled out
        // as "I meant a newline".
        submit();
        event->accept();
        return;
    }
    QPlainTextEdit::keyPressEvent(event);
}

void PromptInput::refreshPlaceholder() {
    if (m_busy) {
        setPlaceholderText(m_busyMessage.isEmpty() ? QString::fromUtf8(kBusyPlaceholder) : m_busyMessage);
    } else {
        setPlaceholderText(QString::fromUtf8(kIdlePlaceholder));
    }
    // Greyed while busy: the placeholder is the only thing in the box saying
    // the prompt is not going anywhere yet.
    QPalette pal = palette();
    pal.setColor(QPalette::PlaceholderText,
                 m_busy ? pal.color(QPalette::Disabled, QPalette::Text) : m_idlePlaceholderColor);
    setPalette(pal);
}
