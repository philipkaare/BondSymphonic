#include "AgentArea.h"
#include "TerminalWidget.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include <QLabel>

namespace {

/// The one adapter with a terminal pane. Other adapters get the placeholder
/// until they grow one.
const QString kTerminalAdapter = QStringLiteral("terminal");

} // namespace

AgentArea::AgentArea(QWidget* parent) : QStackedWidget(parent) {
    m_placeholderText = QStringLiteral("No agent selected");
    m_placeholder = new QLabel(m_placeholderText, this);
    m_placeholder->setAlignment(Qt::AlignCenter);
    m_placeholder->setEnabled(false);
    addWidget(m_placeholder);
}

void AgentArea::setPlaceholderText(const QString& text) {
    m_placeholderText = text;
    if (currentWidget() == m_placeholder) {
        m_placeholder->setText(text);
    }
}

void AgentArea::showWorkspace(const QString& workspaceId, const QString& adapter, const QString& command) {
    if (workspaceId.isEmpty()) {
        showPlaceholder();
        return;
    }
    if (adapter != kTerminalAdapter) {
        m_placeholder->setText(QStringLiteral("The %1 adapter has no pane yet").arg(adapter));
        setCurrentWidget(m_placeholder);
        return;
    }
    TerminalWidget* terminal = m_terminals.value(workspaceId);
    if (terminal == nullptr) {
        // Parented to the area, not to the widget: the widget only borrows the
        // pointer, and both are torn down together in `removeWorkspace`.
        auto* session = new TerminalSession(this);
        terminal = new TerminalWidget(session, this);
        m_terminals.insert(workspaceId, terminal);
        addWidget(terminal);
        terminal->openSession(workspaceId, command);
    }
    setCurrentWidget(terminal);
}

void AgentArea::showPlaceholder() {
    m_placeholder->setText(m_placeholderText);
    setCurrentWidget(m_placeholder);
}

void AgentArea::removeWorkspace(const QString& workspaceId) {
    TerminalWidget* terminal = m_terminals.take(workspaceId);
    if (terminal == nullptr) {
        return;
    }
    TerminalSession* session = terminal->session();
    removeWidget(terminal);
    // The widget goes first, so it can never paint from a session that is gone;
    // deferred deletion runs in the order it was asked for.
    terminal->deleteLater();
    if (session != nullptr) {
        session->close();
        session->deleteLater();
    }
    if (m_terminals.isEmpty() || currentWidget() == nullptr) {
        showPlaceholder();
    }
}

QList<TerminalSession*> AgentArea::sessions() const {
    QList<TerminalSession*> result;
    result.reserve(m_terminals.size());
    for (TerminalWidget* terminal : m_terminals) {
        if (TerminalSession* session = terminal->session()) {
            result.append(session);
        }
    }
    return result;
}

TerminalSession* AgentArea::sessionFor(const QString& workspaceId) const {
    TerminalWidget* terminal = m_terminals.value(workspaceId);
    return terminal != nullptr ? terminal->session() : nullptr;
}
