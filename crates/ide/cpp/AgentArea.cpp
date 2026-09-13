#include "AgentArea.h"
#include "TerminalWidget.h"
#include "TranscriptView.h"
#include "WorkspaceBanner.h"
#include "WorkspaceLabel.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/transcript_model.cxxqt.h"
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QVBoxLayout>

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
        TranscriptView* view = ensureTranscript(workspaceId, agentId);
        setCurrentWidget(m_pages.value(workspaceId, view));
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
        addWidget(makePage(workspaceId, terminal));
        terminal->openSession(workspaceId, command);
    }
    setCurrentWidget(m_pages.value(workspaceId, terminal));
}

QWidget* AgentArea::makePage(const QString& workspaceId, QWidget* body) {
    auto* page = new QWidget(this);
    auto* layout = new QVBoxLayout(page);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);
    auto* banner = new WorkspaceBanner(page);
    layout->addWidget(banner);
    body->setParent(page);
    layout->addWidget(body, 1);
    m_pages.insert(workspaceId, page);
    m_banners.insert(workspaceId, banner);
    QObject::connect(banner, &WorkspaceBanner::dismissed, this,
                     [this, workspaceId] { emit bannerDismissed(workspaceId); });
    // A failure reported before this pane existed: raise it now, which is the
    // first moment the user could have seen it.
    const QStringList pending = m_pendingBanners.take(workspaceId);
    if (pending.size() == 3) {
        banner->showError(pending.at(0), pending.at(1), pending.at(2));
    }
    return page;
}

void AgentArea::showBanner(const QString& workspaceId, const QString& title,
                           const QString& detail, const QString& stderrText) {
    if (WorkspaceBanner* banner = m_banners.value(workspaceId)) {
        banner->showError(title, detail, stderrText);
        return;
    }
    // No pane yet. Held rather than dropped: the tab is about to go red, and
    // opening it has to explain why.
    m_pendingBanners.insert(workspaceId, { title, detail, stderrText });
}

void AgentArea::clearBanner(const QString& workspaceId) {
    m_pendingBanners.remove(workspaceId);
    if (WorkspaceBanner* banner = m_banners.value(workspaceId)) {
        banner->reset();
    }
}

void AgentArea::setOptionsJson(const QString& workspaceId, const QString& optionsJson) {
    TranscriptView* view = m_transcripts.value(workspaceId);
    TranscriptModel* model = view == nullptr ? nullptr : view->model();
    if (model == nullptr || optionsJson.isEmpty()) {
        return;
    }
    model->setOptionsJson(optionsJson);
}

void AgentArea::setWelcome(const QString& workspaceId, const QString& tabJson) {
    if (workspaceId.isEmpty() || tabJson.isEmpty()) {
        return;
    }
    m_welcomes.insert(workspaceId, tabJson);
    if (TranscriptView* view = m_transcripts.value(workspaceId)) {
        applyWelcome(workspaceId, view);
    }
}

void AgentArea::applyWelcome(const QString& workspaceId, TranscriptView* view) {
    const QJsonObject tab =
        QJsonDocument::fromJson(m_welcomes.value(workspaceId).toUtf8()).object();
    if (tab.isEmpty()) {
        return;
    }
    view->setWelcome(tab.value(QStringLiteral("name")).toString(), workspacelabel::origin(tab),
                     tab.value(QStringLiteral("worktree_path")).toString());
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
        addWidget(makePage(workspaceId, view));
        QObject::connect(view, &TranscriptView::startAgentRequested, this,
                         [this, workspaceId] { emit startAgentRequested(workspaceId); });
        QObject::connect(view, &TranscriptView::loginRequested, this,
                         &AgentArea::loginRequested);
        QObject::connect(view, &TranscriptView::optionsChanged, this,
                         [this, workspaceId](const QString& optionsJson) {
                             emit agentOptionsChanged(workspaceId, optionsJson);
                         });
        // A start that began before this pane existed -- which is every
        // workspace created with an agent, since the tab is shown first.
        view->setStarting(m_starting.contains(workspaceId));
        // Built with the gate the area already knows about, rather than with a
        // composer that is taken away a moment later.
        view->setClaudeLoggedIn(m_claudeLoggedIn);
        // And with what the window said about this workspace before the pane
        // existed, which for a workspace just created is all of it.
        applyWelcome(workspaceId, view);
        view->setShowMeta(m_showMeta);
    }
    setAgent(workspaceId, agentId);
    return view;
}

void AgentArea::setClaudeLoggedIn(bool loggedIn) {
    m_claudeLoggedIn = loggedIn;
    for (TranscriptView* view : m_transcripts) {
        if (view != nullptr) {
            view->setClaudeLoggedIn(loggedIn);
        }
    }
}

void AgentArea::setShowMeta(bool show) {
    m_showMeta = show;
    for (TranscriptView* view : m_transcripts) {
        if (view != nullptr) {
            view->setShowMeta(show);
        }
    }
}

void AgentArea::showPlaceholder() {
    m_placeholder->setText(m_placeholderText);
    setCurrentWidget(m_placeholder);
}

void AgentArea::removeWorkspace(const QString& workspaceId) {
    m_welcomes.remove(workspaceId);
    m_attached.remove(workspaceId);
    m_starting.remove(workspaceId);
    m_banners.remove(workspaceId);
    m_pendingBanners.remove(workspaceId);
    // The page owns the banner and the pane; taking it out of the stack takes
    // both with it, so the panes below only have to deal with what they own
    // beyond the widget tree.
    QWidget* page = m_pages.take(workspaceId);
    if (page != nullptr) {
        removeWidget(page);
    }
    if (TranscriptView* view = m_transcripts.take(workspaceId)) {
        TranscriptModel* model = view->model();
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
        terminal->deleteLater();
        if (session != nullptr) {
            session->close();
            session->deleteLater();
        }
    }
    if (page != nullptr) {
        // After its children have been asked to go, so nothing paints from a
        // half-deleted pane; deferred deletion runs in the order asked for.
        page->deleteLater();
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
