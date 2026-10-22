#include "PromptInput.h"
#include <QEvent>
#include <QFontMetrics>
#include <QKeyEvent>
#include <QPalette>
#include <QFocusEvent>
#include <QJsonObject>
#include <QListWidget>
#include <QScreen>
#include <QRegularExpression>
#include <QTextCursor>

namespace {

/// What the box says when it is ready for a prompt.
const char* kIdlePlaceholder = "Message the agent…  (Enter to send, Shift+Enter for a new line)";

/// What it says while no agent can accept input, unless the caller supplies its own.
const char* kBusyPlaceholder = "The agent is unavailable…";
const char* kQueuePlaceholder = "Message the agent…  (Enter to queue, Shift+Enter for a new line)";

/// How many lines of prompt are visible before the box scrolls.
constexpr int kVisibleLines = 3;

} // namespace

PromptInput::PromptInput(QWidget* parent) : QPlainTextEdit(parent) {
    setVerticalScrollBarPolicy(Qt::ScrollBarAsNeeded);
    setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
    setLineWrapMode(QPlainTextEdit::WidgetWidth);
    setTabChangesFocus(true);
    applyHeight();
    m_idlePlaceholderColor = palette().color(QPalette::PlaceholderText);
    refreshPlaceholder();
    m_commandsPopup = new QListWidget(this);
    m_commandsPopup->setObjectName(QStringLiteral("PromptCommandPopup"));
    m_commandsPopup->setWindowFlags(Qt::ToolTip);
    m_commandsPopup->setFocusPolicy(Qt::NoFocus);
    m_commandsPopup->setHorizontalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
    connect(m_commandsPopup, &QListWidget::itemClicked, this, [this] { completeCommand(); });
    connect(this, &QPlainTextEdit::textChanged, this, [this] {
        m_completionDismissed = false;
        refreshCommands();
    });
    connect(this, &QPlainTextEdit::cursorPositionChanged, this, &PromptInput::refreshCommands);
}

void PromptInput::applyHeight() {
    // A few lines tall and no taller: the transcript above is what the pane is
    // for, and a box that grew with the text would eat it.
    const QFontMetrics metrics(font());
    const int frame = 2 * static_cast<int>(frameWidth()) + 2 * static_cast<int>(document()->documentMargin());
    setMaximumHeight(kVisibleLines * metrics.lineSpacing() + frame);
}

void PromptInput::changeEvent(QEvent* event) {
    QPlainTextEdit::changeEvent(event);
    // A new text size: three lines are a different height.
    if (event->type() == QEvent::FontChange) {
        applyHeight();
    }
}

void PromptInput::setBusy(bool busy, const QString& message) {
    m_busy = busy;
    m_busyMessage = message;
    // Read-only rather than disabled: the user can still select and copy what
    // they typed while the answer to it is being written.
    setReadOnly(busy);
    if (busy && m_commandsPopup) { m_commandsPopup->hide(); }
    refreshPlaceholder();
}

bool PromptInput::isBusy() const { return m_busy; }

void PromptInput::setQueueing(bool queueing) {
    if (m_queueing == queueing) { return; }
    m_queueing = queueing;
    refreshPlaceholder();
}

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

bool PromptInput::event(QEvent* event) {
    // QPlainTextEdit consumes Tab for focus traversal before keyPressEvent
    // when setTabChangesFocus(true). Completion takes precedence while shown.
    if (event->type() == QEvent::KeyPress && m_commandsPopup && m_commandsPopup->isVisible()) {
        auto* key = static_cast<QKeyEvent*>(event);
        if (key->key() == Qt::Key_Tab && (key->modifiers() & Qt::ShiftModifier) == 0) {
            completeCommand();
            event->accept();
            return true;
        }
    }
    return QPlainTextEdit::event(event);
}

void PromptInput::keyPressEvent(QKeyEvent* event) {
    if (m_commandsPopup->isVisible()) {
        switch (event->key()) {
        case Qt::Key_Up:
        case Qt::Key_Down: {
            const int direction = event->key() == Qt::Key_Up ? -1 : 1;
            const int count = m_commandsPopup->count();
            m_commandsPopup->setCurrentRow((m_commandsPopup->currentRow() + direction + count) % count);
            event->accept();
            return;
        }
        case Qt::Key_Tab:
        case Qt::Key_Return:
        case Qt::Key_Enter:
            if ((event->modifiers() & Qt::ShiftModifier) == 0) {
                completeCommand();
                event->accept();
                return;
            }
            break;
        case Qt::Key_Escape:
            m_completionDismissed = true;
            m_commandsPopup->hide();
            event->accept();
            return;
        default: break;
        }
    }
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
    } else if (m_queueing) {
        setPlaceholderText(QString::fromUtf8(kQueuePlaceholder));
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

#ifdef BS_WIDGET_TESTS
#include <QApplication>
#include <QJsonArray>
#include <QJsonObject>
#include <QListWidget>
#include <QLineEdit>
#include <QVBoxLayout>
#include <cstdint>

extern "C" std::int32_t bs_widget_test_prompt_command_completion() {
    QWidget window;
    QVBoxLayout layout(&window);
    PromptInput input(&window);
    QLineEdit next(&window);
    layout.addWidget(&input);
    layout.addWidget(&next);
    window.resize(500, 160);
    window.show();
    window.activateWindow();
    input.setFocus();
    QApplication::processEvents();
    input.setCommands(QJsonArray{
        QJsonObject{{"name", "compact"}, {"description", "Compact conversation"}},
        QJsonObject{{"name", "status"}, {"description", "Show session status"}}
    });
    QStringList sent;
    QObject::connect(&input, &PromptInput::submitted, &input,
                     [&sent](const QString& text) { sent.append(text); });
    input.insertPlainText(QStringLiteral("/co"));
    auto* popup = input.findChild<QListWidget*>(QStringLiteral("PromptCommandPopup"));
    if (!popup || !popup->isVisible() || popup->count() != 1 ||
        !popup->item(0)->text().contains(QLatin1String("Compact conversation"))) { return 1; }
    QKeyEvent enter(QEvent::KeyPress, Qt::Key_Return, Qt::NoModifier);
    QApplication::sendEvent(&input, &enter);
    if (!sent.isEmpty() || input.toPlainText() != QLatin1String("/compact ")) { return 2; }
    QApplication::sendEvent(&input, &enter);
    if (sent != QStringList{QStringLiteral("/compact")}) { return 3; }
    input.insertPlainText(QStringLiteral("/"));
    QKeyEvent down(QEvent::KeyPress, Qt::Key_Down, Qt::NoModifier);
    QApplication::sendEvent(&input, &down);
    QKeyEvent tab(QEvent::KeyPress, Qt::Key_Tab, Qt::NoModifier);
    QApplication::sendEvent(&input, &tab);
    if (input.toPlainText() != QLatin1String("/status ") || sent.size() != 1 || next.hasFocus()) { return 4; }
    input.setPlainText(QStringLiteral("/compact argument"));
    if (popup->isVisible()) { return 5; }
    input.clear();
    input.insertPlainText(QStringLiteral("/"));
    QKeyEvent escape(QEvent::KeyPress, Qt::Key_Escape, Qt::NoModifier);
    QApplication::sendEvent(&input, &escape);
    if (popup->isVisible()) { return 6; }
    input.setCommands(QJsonArray{});
    input.clear();
    input.insertPlainText(QStringLiteral("/compact"));
    if (popup->isVisible()) { return 7; }
    return 0;
}
#endif

void PromptInput::setCommands(const QJsonArray& commands) {
    m_commands = commands;
    m_commandsPopup->hide();
    m_completionDismissed = false;
    refreshCommands();
}

void PromptInput::refreshCommands() {
    if (m_completing) { return; }
    const QTextCursor cursor = textCursor();
    const QString prefix = toPlainText().left(cursor.position());
    if (m_busy || !isVisible() || m_completionDismissed || cursor.hasSelection() ||
        !prefix.startsWith(QLatin1Char('/')) || prefix.contains(QRegularExpression(QStringLiteral("\\s")))) {
        m_commandsPopup->hide();
        return;
    }
    m_commandsPopup->clear();
    for (const QJsonValue& value : m_commands) {
        const QJsonObject command = value.toObject();
        const QString name = QStringLiteral("/") + command.value("name").toString();
        if (!name.startsWith(prefix, Qt::CaseInsensitive)) { continue; }
        QString label = name;
        const QString hint = command.value("argument_hint").toString();
        const QString description = command.value("description").toString();
        if (!hint.isEmpty()) { label += QStringLiteral(" ") + hint; }
        if (!description.isEmpty()) { label += QStringLiteral("\n") + description; }
        auto* item = new QListWidgetItem(label, m_commandsPopup);
        item->setData(Qt::UserRole, name);
        item->setToolTip(description);
    }
    if (m_commandsPopup->count() == 0) { m_commandsPopup->hide(); return; }
    m_commandsPopup->setCurrentRow(0);
    const int height = qMin(m_commandsPopup->count(), 7) * (fontMetrics().lineSpacing() * 2 + 8) + 4;
    const int width = qMax(300, qMin(this->width(), 640));
    QPoint position = mapToGlobal(cursorRect().bottomLeft());
    if (screen() && position.y() + height > screen()->availableGeometry().bottom()) {
        position.setY(mapToGlobal(cursorRect().topLeft()).y() - height);
    }
    if (screen()) {
        position.setX(qMax(screen()->availableGeometry().left(),
            qMin(position.x(), screen()->availableGeometry().right() - width + 1)));
    }
    m_commandsPopup->setFont(font());
    m_commandsPopup->resize(width, height);
    m_commandsPopup->move(position);
    m_commandsPopup->show();
}

void PromptInput::completeCommand() {
    auto* item = m_commandsPopup->currentItem();
    if (!item) { return; }
    const QString command = item->data(Qt::UserRole).toString();
    const QString text = toPlainText();
    int end = 0;
    while (end < text.size() && !text.at(end).isSpace()) { ++end; }
    QTextCursor cursor = textCursor();
    cursor.setPosition(0);
    cursor.setPosition(end, QTextCursor::KeepAnchor);
    m_completing = true;
    cursor.insertText(command + (end == text.size() ? QStringLiteral(" ") : QString()));
    setTextCursor(cursor);
    m_completing = false;
    m_completionDismissed = true;
    m_commandsPopup->hide();
    setFocus();
}

void PromptInput::focusOutEvent(QFocusEvent* event) {
    m_commandsPopup->hide();
    QPlainTextEdit::focusOutEvent(event);
}
