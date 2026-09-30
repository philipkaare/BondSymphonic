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
    if (adapter == kClaudeAdapter || adapter == QLatin1String("codex")) {
        // Backend identity is set before options and login state are applied.
        TranscriptView* view = ensureTranscript(workspaceId, agentId);
        view->setBackend(adapter);
        view->setClaudeLoggedIn(adapter == "claude" ? m_claudeLoggedIn
                                                    : m_backendLoggedIn.value(adapter, false));
        view->setClaudeAuthFailure(adapter == "claude" ? m_claudeAuthFailure
                                                       : m_backendAuthFailure.value(adapter));
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
    // The banner's Restart is the same request the pane used to make with its
    // own button, so it goes out as the same signal and the window answers it
    // the same way.
    QObject::connect(banner, &WorkspaceBanner::restartRequested, this,
                     [this, workspaceId] { emit startAgentRequested(workspaceId); });
    QObject::connect(banner, &WorkspaceBanner::retryRequested, this,
                     [this, workspaceId] { emit retryWorkspaceRequested(workspaceId); });
    QObject::connect(banner, &WorkspaceBanner::removeRequested, this,
                     [this, workspaceId] { emit removeWorkspaceRequested(workspaceId); });
    // A failure reported before this pane existed: raise it now, which is the
    // first moment the user could have seen it.
    if (m_pendingBanners.contains(workspaceId)) {
        const PendingBanner pending = m_pendingBanners.take(workspaceId);
        banner->showError(pending.title, pending.detail, pending.stderrText);
    }
    // And whatever was said about the agent while this pane did not exist. An
    // agent that died before its tab was ever opened is exactly the one that
    // needs the button, and it has nothing to do with whether a failure was
    // held above.
    applyRestartOffer(workspaceId);
    // And whether the workspace can run at all, which outranks both.
    applyWorkspaceProblem(workspaceId);
    return page;
}

void AgentArea::setWorkspaceProblem(const QString& workspaceId, const QString& title,
                                    const QString& detail, bool inPlace,
                                    const QString& whatChanged) {
    if (workspaceId.isEmpty()) {
        return;
    }
    const auto found = m_problems.constFind(workspaceId);
    const bool sameReason = found != m_problems.constEnd() && found->title == title &&
                            found->detail == detail && found->inPlace == inPlace;
    if (sameReason && found->whatChanged == whatChanged) {
        return;
    }
    // A new reason is the answer to whatever Retry was in flight, so the
    // button is pressable again. The same reason with the daemon's explanation
    // now attached -- the two arrive as separate events -- is not an answer to
    // anything, and must not re-enable a Retry that is still out.
    m_problems.insert(workspaceId,
                      { title, detail, sameReason && found->retrying, inPlace, whatChanged });
    applyWorkspaceProblem(workspaceId);
}

void AgentArea::clearWorkspaceProblem(const QString& workspaceId) {
    if (m_problems.remove(workspaceId) == 0) {
        return;
    }
    if (WorkspaceBanner* banner = m_banners.value(workspaceId)) {
        banner->clearWorkspaceProblem();
    }
    // Whatever was said about the agent while the problem was up -- a
    // `clearBanner` meanwhile takes the banner's own copy down with it.
    applyRestartOffer(workspaceId);
    if (TranscriptView* view = m_transcripts.value(workspaceId)) {
        view->setWorkspaceDown(false);
    }
}

void AgentArea::setRetrying(const QString& workspaceId, bool retrying) {
    auto found = m_problems.find(workspaceId);
    if (found == m_problems.end()) {
        return;
    }
    found->retrying = retrying;
    applyWorkspaceProblem(workspaceId);
}

void AgentArea::applyWorkspaceProblem(const QString& workspaceId) {
    const auto found = m_problems.constFind(workspaceId);
    if (found == m_problems.constEnd()) {
        return;
    }
    if (WorkspaceBanner* banner = m_banners.value(workspaceId)) {
        banner->setInPlace(found->inPlace);
        banner->showWorkspaceProblem(found->title, found->detail, found->whatChanged);
        banner->setRetrying(found->retrying);
    }
    if (TranscriptView* view = m_transcripts.value(workspaceId)) {
        view->setWorkspaceDown(true);
    }
}

void AgentArea::showBanner(const QString& workspaceId, const QString& title,
                           const QString& detail, const QString& stderrText) {
    if (WorkspaceBanner* banner = m_banners.value(workspaceId)) {
        banner->showError(title, detail, stderrText);
        // The offer is re-stated rather than assumed: a banner that was reset
        // between the two failures hid its button on the way down, and the
        // agent this one is about may well still be the dead one.
        applyRestartOffer(workspaceId);
        return;
    }
    // No pane yet. Held rather than dropped: the tab is about to go red, and
    // opening it has to explain why.
    m_pendingBanners.insert(workspaceId, { title, detail, stderrText });
}

void AgentArea::setRestartOffered(const QString& workspaceId, bool offered) {
    if (workspaceId.isEmpty()) {
        return;
    }
    m_restartOffered.insert(workspaceId, offered);
    applyRestartOffer(workspaceId);
}

void AgentArea::applyRestartOffer(const QString& workspaceId) {
    if (WorkspaceBanner* banner = m_banners.value(workspaceId)) {
        banner->setRestartOffered(m_restartOffered.value(workspaceId, false));
    }
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
    auto options = QJsonDocument::fromJson(optionsJson.toUtf8()).object();
    options.insert("adapter", view->backend());
    model->setOptionsJson(QString::fromUtf8(QJsonDocument(options).toJson(QJsonDocument::Compact)));
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
        view->setPrereqsAnswered(m_prereqsAnswered);
        view->setClaudeAuthFailure(m_claudeAuthFailure);
        // And with what the window said about this workspace before the pane
        // existed, which for a workspace just created is all of it.
        applyWelcome(workspaceId, view);
        view->setShowMeta(m_showMeta);
    }
    setAgent(workspaceId, agentId);
    return view;
}

void AgentArea::setBackendAuth(const QString &backend, bool loggedIn, const QString &failure) {
    m_backendLoggedIn.insert(backend, loggedIn);
    m_backendAuthFailure.insert(backend, failure);
    for (auto *view : m_transcripts)
        if (view && view->backend() == backend) {
            view->setClaudeLoggedIn(loggedIn);
            view->setClaudeAuthFailure(failure);
        }
}

void AgentArea::setClaudeLoggedIn(bool loggedIn) {
    m_claudeLoggedIn = loggedIn;
    for (TranscriptView* view : m_transcripts) {
        if (view != nullptr && view->backend() == "claude") {
            view->setClaudeLoggedIn(loggedIn);
        }
    }
}

void AgentArea::setPrereqsAnswered(bool answered) {
    m_prereqsAnswered = answered;
    for (TranscriptView* view : m_transcripts) {
        if (view != nullptr) {
            view->setPrereqsAnswered(answered);
        }
    }
}

void AgentArea::setClaudeAuthFailure(const QString& sentence) {
    m_claudeAuthFailure = sentence;
    for (TranscriptView* view : m_transcripts) {
        if (view != nullptr && view->backend() == "claude") {
            view->setClaudeAuthFailure(sentence);
        }
    }
}

void AgentArea::refillModels() {
    for (TranscriptView* view : m_transcripts) {
        if (view != nullptr) {
            view->refillModels();
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
    m_restartOffered.remove(workspaceId);
    m_problems.remove(workspaceId);
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

// --- offscreen test entries --------------------------------------------------
//
// Compiled only into a development build: this is test code -- it builds
// widgets, leaks a QApplication and asserts -- and a shipped IDE has no caller
// for any of it. `build.rs` defines `BS_WIDGET_TESTS` for every profile but
// `release`, which is the one the packaged executable is built with.
#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.
#include <QPushButton>
#include <cstdint>

namespace {

/// The banner's Restart, for the one page a check has built.
QPushButton* restartButton(const AgentArea& area) {
    return area.findChild<QPushButton*>(QStringLiteral("WorkspaceBannerRestartButton"));
}

/// Builds `workspaceId`'s Claude pane, which is what replays a held banner.
void openPane(AgentArea& area, const QString& workspaceId) {
    area.showWorkspace(workspaceId, QStringLiteral("claude"), QString());
}

} // namespace

/// The offer to restart survives a pane that does not exist yet, and a failure
/// that has nothing to do with it.
///
/// Whether to offer a Restart is a fact about the agent -- is it down? -- and a
/// banner is a fact about the latest failure. Carrying the first on the second
/// is wrong in both directions: a merge that fails over a still-dead agent
/// would take away the one way back the pane has left now that agents start
/// themselves, and an agent that came back would keep a button that kills a
/// working conversation to resume it.
extern "C" std::int32_t bs_widget_test_agent_area_holds_a_restartable_banner() {
    const QString workspace = QStringLiteral("ws_held");
    {
        AgentArea area;
        // The agent died before the tab was ever opened, so there is nothing
        // yet to put the offer on.
        area.setRestartOffered(workspace, true);
        area.showBanner(workspace, QStringLiteral("agent exited"), QStringLiteral("status 1"),
                        QString());
        if (restartButton(area) != nullptr) {
            return 1;
        }
        openPane(area, workspace);
        QPushButton* restart = restartButton(area);
        if (restart == nullptr) {
            return 2;
        }
        if (restart->isHidden()) {
            // The offer did not survive the hold.
            return 3;
        }
        QString asked;
        QObject::connect(&area, &AgentArea::startAgentRequested, &area,
                         [&asked](const QString& id) { asked = id; });
        restart->click();
        if (asked != workspace) {
            // The banner is the only restart left, so its button has to reach
            // the window as the same request the pane's own used to make.
            return 4;
        }

        // A second failure that says nothing about the agent. The agent is
        // still down, so the way back is still there.
        area.showBanner(workspace, QStringLiteral("merge failed"), QStringLiteral("conflict"),
                        QString());
        if (restartButton(area)->isHidden()) {
            return 5;
        }

        // And the agent coming back is what takes it away, from the one caller
        // that knows it did.
        area.setRestartOffered(workspace, false);
        if (!restartButton(area)->isHidden()) {
            return 6;
        }
    }

    {
        // The same two facts arriving the other way round, on a pane that
        // already exists.
        AgentArea area;
        openPane(area, workspace);
        if (restartButton(area) == nullptr || !restartButton(area)->isHidden()) {
            // A workspace nobody has said anything about offers nothing.
            return 7;
        }
        // Raised with no banner up at all, which is a state the window reaches:
        // an agent can exit without anything having failed.
        area.setRestartOffered(workspace, true);
        if (restartButton(area)->isHidden()) {
            return 8;
        }
        // A failure raised afterwards leaves it alone, and so does one raised
        // after the banner has been taken down and put back up.
        area.clearBanner(workspace);
        area.showBanner(workspace, QStringLiteral("merge failed"), QString(), QString());
        if (restartButton(area)->isHidden()) {
            return 9;
        }
    }
    return 0;
}

/// A workspace that cannot run is said before its pane exists and shown when
/// it is built; its Retry and Remove reach the window as the workspace's own
/// requests; and recovery takes the problem down without touching anything
/// else the banner had to say.
extern "C" std::int32_t bs_widget_test_agent_area_holds_a_workspace_problem() {
    const QString workspace = QStringLiteral("ws_problem");
    AgentArea area;
    area.setWorkspaceProblem(workspace, QStringLiteral("This workspace could not be started"),
                             QStringLiteral("worktree registration is missing"));
    // A Retry for a pane nobody has opened is remembered too.
    area.setRetrying(workspace, true);
    if (area.banner(workspace) != nullptr) {
        return 1;
    }
    openPane(area, workspace);
    WorkspaceBanner* banner = area.banner(workspace);
    if (banner == nullptr || banner->isHidden() || !banner->hasWorkspaceProblem()) {
        return 2;
    }
    auto* retry = banner->findChild<QPushButton*>(QStringLiteral("WorkspaceBannerRetryButton"));
    auto* remove = banner->findChild<QPushButton*>(QStringLiteral("WorkspaceBannerRemoveButton"));
    if (retry == nullptr || remove == nullptr || retry->isEnabled()) {
        return 3;
    }

    // The same problem again is no news, and must not end the Retry.
    area.setWorkspaceProblem(workspace, QStringLiteral("This workspace could not be started"),
                             QStringLiteral("worktree registration is missing"));
    if (retry->isEnabled()) {
        return 4;
    }
    // A different reason is the Retry's answer.
    area.setWorkspaceProblem(workspace, QStringLiteral("This workspace could not be started"),
                             QStringLiteral("bwrap: permission denied"));
    if (!retry->isEnabled()) {
        return 5;
    }

    QString retried;
    QString removed;
    QObject::connect(&area, &AgentArea::retryWorkspaceRequested, &area,
                     [&retried](const QString& id) { retried = id; });
    QObject::connect(&area, &AgentArea::removeWorkspaceRequested, &area,
                     [&removed](const QString& id) { removed = id; });
    retry->click();
    remove->click();
    if (retried != workspace || removed != workspace) {
        return 6;
    }

    // A merge failure raised meanwhile waits under the problem and is what is
    // left when the workspace comes back.
    area.showBanner(workspace, QStringLiteral("merge failed"), QStringLiteral("conflict"),
                    QString());
    if (!banner->hasWorkspaceProblem()) {
        return 7;
    }
    area.clearWorkspaceProblem(workspace);
    if (banner->hasWorkspaceProblem() || banner->isHidden()) {
        return 8;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
