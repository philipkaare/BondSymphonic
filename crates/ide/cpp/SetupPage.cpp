#include "SetupPage.h"
#include "TerminalWidget.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include <QDesktopServices>
#include <QFont>
#include <QFrame>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPushButton>
#include <QScrollArea>
#include <QTimer>
#include <QUrl>
#include <QVBoxLayout>

namespace {

/// How many rows of terminal the fix pane shows.
constexpr int kTerminalRows = 24;
/// The fallback grid, used only if the pane is asked for its size before it
/// has ever been laid out.
constexpr int kFallbackCols = 80;
/// How many rows the list is sized for. The daemon reports eight; a ninth
/// would scroll rather than shrink the terminal, which is the right way round.
constexpr int kPrereqCount = 8;

const QString kOk = QStringLiteral("✓");
const QString kBad = QStringLiteral("✗");

} // namespace

SetupPage::SetupPage(AppController* controller, QWidget* parent)
    : QWidget(parent), m_controller(controller) {
    auto* outer = new QVBoxLayout(this);

    auto* title = new QLabel("Set up BondSymphonic", this);
    QFont titleFont = title->font();
    titleFont.setPointSize(titleFont.pointSize() + 6);
    titleFont.setBold(true);
    title->setFont(titleFont);
    outer->addWidget(title);

    auto* subtitle = new QLabel(
        "BondSymphonic runs its agents inside a sandbox in WSL. These are what it needs.", this);
    subtitle->setWordWrap(true);
    subtitle->setEnabled(false);
    outer->addWidget(subtitle);

    // Scrolled: eight rows fit, but a detail line long enough to wrap twice
    // must not push the buttons off the bottom of a short window.
    auto* scroll = new QScrollArea(this);
    scroll->setWidgetResizable(true);
    scroll->setFrameShape(QFrame::NoFrame);
    m_rowsHost = new QWidget(scroll);
    m_rowsLayout = new QVBoxLayout(m_rowsHost);
    m_rowsLayout->setContentsMargins(0, 0, 0, 0);
    scroll->setWidget(m_rowsHost);
    // All eight prerequisites fit in an ordinary window; the scroll area is
    // for a short one. Without a floor the terminal's own minimum height wins
    // the whole layout and squeezes the list down to three rows.
    scroll->setMinimumHeight(kPrereqCount * fontMetrics().lineSpacing() * 2);
    outer->addWidget(scroll, 1);

    // The fix pane, hidden until there is something in it. A terminal opened
    // into a hidden pane would start the program at whatever size an unshown
    // widget happens to have, so nothing is opened until this is on screen.
    m_terminalHost = new QWidget(this);
    auto* terminalLayout = new QVBoxLayout(m_terminalHost);
    terminalLayout->setContentsMargins(0, 0, 0, 0);
    m_terminalLabel = new QLabel(m_terminalHost);
    m_terminalLabel->setEnabled(false);
    terminalLayout->addWidget(m_terminalLabel);
    m_session = new TerminalSession(this);
    m_terminal = new TerminalWidget(m_session, m_terminalHost);
    m_terminal->setMinimumHeight(m_terminal->sizeHint().height());
    terminalLayout->addWidget(m_terminal, 1);
    m_terminalHost->setVisible(false);
    outer->addWidget(m_terminalHost, 1);

    auto* buttons = new QHBoxLayout();
    m_recheckButton = new QPushButton("Re-check", this);
    m_continueButton = new QPushButton("Continue anyway", this);
    // The rule is "no blocking failure has been reported", not "a check
    // succeeded". A check that never answers -- a daemon that is down, which is
    // exactly when someone opens this page from the Help menu -- reports no
    // failure, and the page must not become a room with no door. Nothing is
    // lost by being wrong here: continuing with a broken sandbox fails loudly
    // at the first workspace, whereas a window that cannot be dismissed has to
    // be restarted.
    buttons->addWidget(m_recheckButton);
    buttons->addStretch(1);
    buttons->addWidget(m_continueButton);
    outer->addLayout(buttons);

    QObject::connect(m_recheckButton, &QPushButton::clicked, this,
                     [this] { m_controller->recheckPrereqs(); });
    QObject::connect(m_continueButton, &QPushButton::clicked, this, &SetupPage::completed);
    QObject::connect(m_controller, &AppController::prereqsChecked, this, &SetupPage::applyPrereqs);
    QObject::connect(m_controller, &AppController::setupPtyOpened, this,
                     &SetupPage::onSetupPtyOpened);
    QObject::connect(m_controller, &AppController::operationFailed, this,
                     &SetupPage::onOperationFailed);
    QObject::connect(m_session, &TerminalSession::linkDetected, this, &SetupPage::onLinkDetected);
    QObject::connect(m_session, &TerminalSession::exitedSignal, this, &SetupPage::onTerminalExited);
}

void SetupPage::applyPrereqs(const QString& json) {
    clearRows();
    const QJsonArray items = QJsonDocument::fromJson(json.toUtf8()).array();
    bool allOk = true;
    for (const QJsonValue& value : items) {
        const QJsonObject item = value.toObject();
        const bool ok = item.value("ok").toBool();
        allOk = allOk && ok;
        addRow(item.value("name").toString(), ok, item.value("detail").toString(),
               item.value("fix_hint").toString());
    }
    m_rowsLayout->addStretch(1);
    // "Continue anyway" is about the four the IDE cannot work around; a missing
    // CLI or login costs the user Claude Code and nothing else.
    m_continueButton->setEnabled(!m_controller->prereqsBlock(json));
    if (allOk && !items.isEmpty()) {
        // Nothing left to do here. The page says so by getting out of the way.
        emit completed();
    }
}

void SetupPage::setActionsEnabled(bool enabled) {
    for (QPushButton* button : m_actionButtons) {
        if (button != nullptr) {
            button->setEnabled(enabled);
        }
    }
}

void SetupPage::clearRows() {
    m_actionButtons.clear();
    QLayoutItem* item = nullptr;
    while ((item = m_rowsLayout->takeAt(0)) != nullptr) {
        if (QWidget* widget = item->widget()) {
            widget->deleteLater();
        }
        delete item;
    }
}

QString SetupPage::actionFor(const QString& name) {
    if (name == QLatin1String("claude")) {
        return QStringLiteral("install_claude");
    }
    if (name == QLatin1String("claude_auth")) {
        return QStringLiteral("claude_login");
    }
    if (name == QLatin1String("gh")) {
        return QStringLiteral("install_gh");
    }
    if (name == QLatin1String("gh_auth")) {
        return QStringLiteral("gh_login");
    }
    // git, bwrap, userns and the sandbox itself: the daemon has no command for
    // these, so the row shows the one a person would type.
    return QString();
}

QString SetupPage::buttonTextFor(const QString& action) {
    if (action == QLatin1String("install_claude")) {
        return QStringLiteral("Install Claude Code");
    }
    if (action == QLatin1String("claude_login")) {
        return QStringLiteral("Log in to Claude Code");
    }
    if (action == QLatin1String("install_gh")) {
        return QStringLiteral("Install GitHub CLI");
    }
    return QStringLiteral("Log in to GitHub");
}

void SetupPage::addRow(const QString& name, bool ok, const QString& detail,
                       const QString& fixHint) {
    auto* row = new QWidget(m_rowsHost);
    auto* layout = new QHBoxLayout(row);
    layout->setContentsMargins(0, 2, 0, 2);

    auto* glyph = new QLabel(ok ? kOk : kBad, row);
    glyph->setStyleSheet(ok ? "color:#4caf50" : "color:#eb5757");
    glyph->setFixedWidth(glyph->fontMetrics().horizontalAdvance(kBad) * 2);
    layout->addWidget(glyph);

    auto* label = new QLabel(name, row);
    QFont nameFont = label->font();
    nameFont.setBold(true);
    label->setFont(nameFont);
    label->setMinimumWidth(label->fontMetrics().horizontalAdvance(QStringLiteral("claude_auth  ")));
    layout->addWidget(label);

    auto* detailLabel = new QLabel(detail, row);
    detailLabel->setWordWrap(true);
    // Selectable so a failure can be pasted into a bug report.
    detailLabel->setTextInteractionFlags(Qt::TextSelectableByMouse | Qt::TextSelectableByKeyboard);
    layout->addWidget(detailLabel, 1);

    if (ok) {
        m_rowsLayout->addWidget(row);
        return;
    }
    const QString action = actionFor(name);
    if (action.isEmpty()) {
        if (!fixHint.isEmpty()) {
            // Copyable: this is a command the user runs in the distro, and
            // retyping it from a screenshot is how typos get made.
            auto* hint = new QLabel(fixHint, row);
            hint->setTextInteractionFlags(Qt::TextSelectableByMouse |
                                          Qt::TextSelectableByKeyboard);
            hint->setTextFormat(Qt::PlainText);
            hint->setStyleSheet("font-family:monospace");
            layout->addWidget(hint);
        }
        m_rowsLayout->addWidget(row);
        return;
    }
    auto* button = new QPushButton(buttonTextFor(action), row);
    QObject::connect(button, &QPushButton::clicked, this, [this, action] { runAction(action); });
    // A re-check can rebuild the rows while a request is still in flight, and
    // the new buttons must be as dead as the ones they replaced.
    button->setEnabled(m_pendingAction.isEmpty());
    m_actionButtons.append(button);
    layout->addWidget(button);
    m_rowsLayout->addWidget(row);
}

void SetupPage::runAction(const QString& action) {
    if (!m_pendingAction.isEmpty()) {
        // A second request while the first is being answered would orphan the
        // first one's process: `setupPtyOpened` carries the pty id, so until it
        // arrives there is nothing to close.
        return;
    }
    m_pendingAction = action;
    setActionsEnabled(false);
    // The previous action's terminal, if there is one, is released by the
    // `attach` below: `TerminalSession::begin` tears the old subscription down
    // and closes the PTY it held. What that cannot cover is a *second* request
    // made before the first is answered, because the reply is the only thing
    // that names the pty id -- which is what the guard above refuses.
    m_terminalLabel->setText(QString("Running %1. Answer its questions here.").arg(action));
    m_terminal->setVisible(true);
    m_terminalHost->setVisible(true);
    // Deferred by one turn of the event loop: the pane has only just been
    // shown, so the layout that gives its terminal a real width and height has
    // not run yet, and a terminal opened at the size of an unshown widget
    // starts the program in a few columns.
    QTimer::singleShot(0, this, [this, action] {
        m_controller->openSetupPty(action, terminalCols(), terminalRows());
    });
}

void SetupPage::onSetupPtyOpened(const QString& action, const QString& ptyId) {
    if (action != m_pendingAction) {
        return;
    }
    m_pendingAction.clear();
    setActionsEnabled(true);
    m_session->attach(ptyId, terminalCols(), terminalRows());
    m_terminal->setFocus();
}

void SetupPage::onOperationFailed(const QString& op, const QString& message) {
    if (op != QLatin1String("setup") || m_pendingAction.isEmpty()) {
        return;
    }
    const QString action = m_pendingAction;
    m_pendingAction.clear();
    setActionsEnabled(true);
    // The pane header says what went wrong, over a hidden terminal: nothing was
    // opened, so an empty black rectangle would only read as a hang.
    m_terminalLabel->setText(QString("%1 could not start: %2").arg(action, message));
    m_terminal->setVisible(false);
    m_terminalHost->setVisible(true);
}

void SetupPage::onLinkDetected(const QString& url) {
    QDesktopServices::openUrl(QUrl(url));
}

void SetupPage::onTerminalExited() {
    m_terminalLabel->setText("Finished. Re-checking…");
    // The reply to `system.setup_pty` came back the moment the terminal
    // opened; the fix is only done now.
    m_controller->recheckPrereqs();
}

int SetupPage::terminalCols() const {
    const int cols = m_session == nullptr ? 0 : m_session->getCols();
    return cols > 1 ? cols : kFallbackCols;
}

int SetupPage::terminalRows() const {
    const int rows = m_session == nullptr ? 0 : m_session->getRows();
    return rows > 0 ? rows : kTerminalRows;
}
