#include "AgentArea.h"
#include "TerminalWidget.h"
#include "TranscriptView.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/transcript_model.cxxqt.h"
#include <QLabel>

namespace {

/// The two adapters with a pane. Anything else gets the placeholder until it
/// grows one.
const QString kTerminalAdapter = QStringLiteral("terminal");
const QString kClaudeAdapter = QStringLiteral("claude");

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

void AgentArea::showWorkspace(const QString& workspaceId, const QString& adapter, const QString& command,
                              const QString& agentId) {
    if (workspaceId.isEmpty()) {
        showPlaceholder();
        return;
    }
    if (adapter == kClaudeAdapter) {
        setCurrentWidget(ensureTranscript(workspaceId, agentId));
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

void AgentArea::setStarting(const QString& workspaceId, bool starting) {
    if (workspaceId.isEmpty()) {
        return;
    }
    if (starting) {
        m_starting.insert(workspaceId);
    } else {
        m_starting.remove(workspaceId);
    }
    if (TranscriptView* view = m_transcripts.value(workspaceId)) {
        view->setStarting(starting);
    }
}

void AgentArea::clearStarting() {
    const QList<QString> pending = m_starting.values();
    for (const QString& workspaceId : pending) {
        setStarting(workspaceId, false);
    }
}

void AgentArea::setAgent(const QString& workspaceId, const QString& agentId) {
    TranscriptView* view = m_transcripts.value(workspaceId);
    if (view == nullptr || agentId.isEmpty()) {
        // No pane yet: `showWorkspace` carries the id in when the tab is next
        // shown, and the tab model is where it was recorded meanwhile.
        return;
    }
    if (m_attached.value(workspaceId) == agentId) {
        return;
    }
    m_attached.insert(workspaceId, agentId);
    if (TranscriptModel* model = view->model()) {
        model->attach(workspaceId, agentId);
    }
}

TranscriptModel* AgentArea::transcriptModel(const QString& workspaceId) const {
    TranscriptView* view = m_transcripts.value(workspaceId);
    return view == nullptr ? nullptr : view->model();
}

TranscriptView* AgentArea::ensureTranscript(const QString& workspaceId, const QString& agentId) {
    TranscriptView* view = m_transcripts.value(workspaceId);
    if (view == nullptr) {
        // Parented to the area for the same reason the terminal's session is:
        // the view borrows the pointer and both die together.
        auto* model = new TranscriptModel(this);
        view = new TranscriptView(model, this);
        m_transcripts.insert(workspaceId, view);
        addWidget(view);
        QObject::connect(view, &TranscriptView::startAgentRequested, this,
                         [this, workspaceId] { emit startAgentRequested(workspaceId); });
        // A start that began before this pane existed -- which is every
        // workspace created with an agent, since the tab is shown first.
        view->setStarting(m_starting.contains(workspaceId));
    }
    setAgent(workspaceId, agentId);
    return view;
}

void AgentArea::showPlaceholder() {
    m_placeholder->setText(m_placeholderText);
    setCurrentWidget(m_placeholder);
}

void AgentArea::removeWorkspace(const QString& workspaceId) {
    m_attached.remove(workspaceId);
    m_starting.remove(workspaceId);
    if (TranscriptView* view = m_transcripts.take(workspaceId)) {
        TranscriptModel* model = view->model();
        removeWidget(view);
        // The view goes first, so it can never paint from a model that is gone;
        // deferred deletion runs in the order it was asked for.
        view->deleteLater();
        if (model != nullptr) {
            // Detaching, not stopping: the agent belongs to the workspace, and
            // `workspace.destroy` is what reaps it.
            model->attach(QString(), QString());
            model->deleteLater();
        }
    }
    if (TerminalWidget* terminal = m_terminals.take(workspaceId)) {
        TerminalSession* session = terminal->session();
        removeWidget(terminal);
        terminal->deleteLater();
        if (session != nullptr) {
            session->close();
            session->deleteLater();
        }
    }
    if ((m_terminals.isEmpty() && m_transcripts.isEmpty()) || currentWidget() == nullptr) {
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
