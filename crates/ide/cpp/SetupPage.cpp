#include "SetupPage.h"
#include "TerminalWidget.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/terminal_session.cxxqt.h"
#include <QClipboard>
#include <QColor>
#include <QDesktopServices>
#include <QFont>
#include <QFontMetrics>
#include <QGuiApplication>
#include <QFrame>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPalette>
#include <QPushButton>
#include <QResizeEvent>
#include <QScrollArea>
#include <QSizePolicy>
#include <QTimer>
#include <QUrl>
#include <QVBoxLayout>
#include <cstdint>
#include <utility>

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

/// How long "Link copied" stays on the sign-in row.
constexpr int kCopiedFeedbackMs = 3000;

/// The narrowest the URL label will elide to. Below this the elision is all
/// ellipsis and says nothing; the tooltip and the buttons still work.
constexpr int kMinLinkWidth = 40;

} // namespace

SetupPage::SetupPage(AppController* controller, QWidget* parent)
    : QWidget(parent), m_controller(controller) {
    m_closePty = [this](const QString& ptyId) {
        if (m_session == nullptr) {
            return;
        }
        // The session learns its own id a turn of the event loop after
        // `attach`, and a page being torn down has no turn left, so it is told
        // here before being asked to close.
        m_session->setPtyId(ptyId);
        m_session->close();
        // Told once. The session closes whatever id it is still holding when it
        // is destroyed a moment after this, and a second `pty.close` for a PTY
        // the daemon has already reaped comes back an error.
        m_session->setPtyId(QString());
    };
    auto* outer = new QVBoxLayout(this);

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

    // The sign-in row, under the terminal and hidden until a login URL is
    // printed. It exists because opening the browser is the one step of the
    // login the IDE cannot verify: nothing comes back to say a browser took
    // the URL, and without this row the only copy of it is wrapped across
    // eighty columns of terminal.
    m_linkRow = new QWidget(this);
    auto* linkLayout = new QHBoxLayout(m_linkRow);
    linkLayout->setContentsMargins(0, 0, 0, 0);
    auto* linkTitle = new QLabel("Sign-in link:", m_linkRow);
    QFont linkTitleFont = linkTitle->font();
    linkTitleFont.setBold(true);
    linkTitle->setFont(linkTitleFont);
    linkLayout->addWidget(linkTitle);
    m_linkLabel = new QLabel(m_linkRow);
    // Rich text so the URL reads as a link, but Qt never follows it itself:
    // `QDesktopServices::openUrl` is the one place in the IDE a URL reaches the
    // system browser, and this one was printed by a terminal.
    m_linkLabel->setTextFormat(Qt::RichText);
    m_linkLabel->setOpenExternalLinks(false);
    m_linkLabel->setTextInteractionFlags(Qt::TextBrowserInteraction);
    // Ignored, and a minimum of one pixel: the label is given whatever width
    // the row has left and elides to it. Without this the label asks for the
    // whole URL and the dialog grows to fit a query string.
    m_linkLabel->setMinimumWidth(1);
    m_linkLabel->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    linkLayout->addWidget(m_linkLabel, 1);
    m_linkFeedback = new QLabel(m_linkRow);
    m_linkFeedback->setTextFormat(Qt::PlainText);
    m_linkFeedback->setEnabled(false);
    m_linkFeedback->setVisible(false);
    linkLayout->addWidget(m_linkFeedback);
    m_copyLink = new QPushButton("Copy", m_linkRow);
    m_copyLink->setToolTip("Put the sign-in link on the clipboard");
    m_openLink = new QPushButton("Open in browser", m_linkRow);
    m_openLink->setToolTip("Open the sign-in link in your desktop browser");
    linkLayout->addWidget(m_copyLink);
    linkLayout->addWidget(m_openLink);
    m_linkRow->setVisible(false);
    outer->addWidget(m_linkRow);

    m_linkFeedbackTimer = new QTimer(this);
    m_linkFeedbackTimer->setSingleShot(true);
    m_linkFeedbackTimer->setInterval(kCopiedFeedbackMs);

    auto* buttons = new QHBoxLayout();
    m_recheckButton = new QPushButton("Re-check", this);
    m_recheckButton->setToolTip("Ask the daemon about these again");
    buttons->addWidget(m_recheckButton);
    buttons->addStretch(1);
    outer->addLayout(buttons);

    QObject::connect(m_recheckButton, &QPushButton::clicked, this,
                     [this] { m_controller->recheckPrereqs(); });
    QObject::connect(m_controller, &AppController::prereqsChecked, this, &SetupPage::applyPrereqs);
    QObject::connect(m_controller, &AppController::setupPtyOpened, this,
                     &SetupPage::onSetupPtyOpened);
    QObject::connect(m_controller, &AppController::operationFailed, this,
                     &SetupPage::onOperationFailed);
    QObject::connect(m_session, &TerminalSession::linkDetected, this, &SetupPage::onLinkDetected);
    QObject::connect(m_session, &TerminalSession::exitedSignal, this, &SetupPage::onTerminalExited);
    QObject::connect(m_linkFeedbackTimer, &QTimer::timeout, this,
                     [this] { m_linkFeedback->setVisible(false); });
    QObject::connect(m_copyLink, &QPushButton::clicked, this, &SetupPage::copyLink);
    QObject::connect(m_openLink, &QPushButton::clicked, this, &SetupPage::openLink);
    QObject::connect(m_linkLabel, &QLabel::linkActivated, this,
                     [this](const QString&) { useLink(); });

    // The page is built when the Settings dialog opens, long after the check it
    // draws. Drawing the last answer straight away is what stops the section
    // being an empty box until something happens to trigger another check;
    // "Re-check" is beneath it for anything that has changed since.
    const QString last = m_controller->prereqsJson();
    if (!last.isEmpty()) {
        applyPrereqs(last);
    }
}

SetupPage::~SetupPage() {
    // Nothing from the session may reach this page any more: it is half
    // destroyed, and the close below can make the session emit.
    if (m_session != nullptr) {
        QObject::disconnect(m_session, nullptr, this, nullptr);
    }
    if (m_ptyId.isEmpty()) {
        return;
    }
    const QString ptyId = m_ptyId;
    m_ptyId.clear();
    m_closePty(ptyId);
}

void SetupPage::setPtyCloser(std::function<void(const QString& ptyId)> close) {
    if (close) {
        m_closePty = std::move(close);
    }
}

void SetupPage::applyPrereqs(const QString& json) {
    clearRows();
    const QJsonArray items = QJsonDocument::fromJson(json.toUtf8()).array();
    for (const QJsonValue& value : items) {
        const QJsonObject item = value.toObject();
        addRow(item.value("name").toString(), item.value("ok").toBool(),
               item.value("detail").toString(), item.value("fix_hint").toString());
    }
    m_rowsLayout->addStretch(1);
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
    // The IDE's two judgements, read from `theme` rather than written out
    // again here: a prerequisite that is in place is the same green as a line
    // that was added, and one that is missing the same red as a line that is
    // gone, which is the red `GroupBar` already puts on a failed agent.
    //
    // Through `ink`, because the glyph is text. The theme's accents are picked
    // as fills for a light background and sit too close to a dark one.
    const QColor glyphColour =
        theme::ink(ok ? theme::added() : theme::removed(), theme::isDark(palette()));
    glyph->setStyleSheet(QStringLiteral("color:%1").arg(glyphColour.name()));
    glyph->setFixedWidth(glyph->fontMetrics().horizontalAdvance(kBad) * 2);
    layout->addWidget(glyph);

    auto* label = new QLabel(name, row);
    QFont nameFont = label->font();
    nameFont.setBold(true);
    label->setFont(nameFont);
    label->setMinimumWidth(label->fontMetrics().horizontalAdvance(QStringLiteral("claude_auth  ")));
    layout->addWidget(label);

    auto* detailLabel = new QLabel(detail, row);
    // The daemon wrote this sentence, and it names commands and paths. Left at
    // `AutoText` a detail that happens to contain angle brackets is guessed to
    // be HTML: the tags vanish from the row and what is left reads as a
    // different sentence, and an `<img src=...>` in one would make the page
    // fetch it.
    detailLabel->setTextFormat(Qt::PlainText);
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
    // Whatever the last terminal printed belongs to the last terminal. A row
    // left standing here would offer the previous login's URL beside the new
    // one's output.
    clearLink();
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
    // Before `attach`, and kept here rather than read back off the session: the
    // session learns its own id a turn of the event loop later, so a page torn
    // down in between would have nothing to close and would leave a login
    // prompt running in the distro with nobody attached. `attach` closes
    // whatever PTY the session was holding before this one.
    m_ptyId = ptyId;
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
    m_linkUrl = url;
    m_linkFeedbackTimer->stop();
    m_linkFeedback->setVisible(false);
    m_linkRow->setVisible(true);
    updateLinkElide();
    // Unchanged: the first thing that happens to a detected link is still that
    // the browser is asked to open it. `LinkScanner` reports each URL once, so
    // this is one open per login rather than one per chunk of output.
    QDesktopServices::openUrl(QUrl(url));
}

void SetupPage::copyLink() {
    if (m_linkUrl.isEmpty()) {
        return;
    }
    // The URL as printed, never the label's text: the label shows an elided
    // rendering, and pasting an ellipsis into a browser is worse than nothing.
    QGuiApplication::clipboard()->setText(m_linkUrl);
    m_linkFeedback->setText("Link copied");
    m_linkFeedback->setVisible(true);
    m_linkFeedbackTimer->start();
}

void SetupPage::openLink() {
    if (m_linkUrl.isEmpty()) {
        return;
    }
    QDesktopServices::openUrl(QUrl(m_linkUrl));
}

void SetupPage::useLink() {
    // Both halves, because a click on the URL itself is the user saying "I want
    // this link" without saying what for, and either one alone is a guess.
    copyLink();
    openLink();
}

void SetupPage::updateLinkElide() {
    if (m_linkUrl.isEmpty()) {
        m_linkLabel->clear();
        m_linkLabel->setToolTip(QString());
        return;
    }
    // Elided in the middle: the host says which login this is and the tail
    // carries the state, so a URL cut at either end says less than one cut in
    // the middle. The href is always the whole URL.
    const int room = qMax(m_linkLabel->width(), kMinLinkWidth);
    const QString shown = m_linkLabel->fontMetrics().elidedText(m_linkUrl, Qt::ElideMiddle, room);
    m_linkLabel->setText(QStringLiteral("<a href=\"%1\">%2</a>")
                             .arg(m_linkUrl.toHtmlEscaped(), shown.toHtmlEscaped()));
    m_linkLabel->setToolTip(m_linkUrl);
}

void SetupPage::clearLink() {
    m_linkUrl.clear();
    m_linkFeedbackTimer->stop();
    m_linkFeedback->setVisible(false);
    m_linkLabel->clear();
    m_linkLabel->setToolTip(QString());
    m_linkRow->setVisible(false);
}

void SetupPage::resizeEvent(QResizeEvent* event) {
    QWidget::resizeEvent(event);
    // The label's width is only real once a layout has run, so the elision is
    // recomputed here rather than at the moment the URL arrives.
    updateLinkElide();
}

void SetupPage::onTerminalExited() {
    // That PTY is spent: the daemon reaped it when the process ended, and
    // closing it on the way out would only ask about a PTY that is gone.
    m_ptyId.clear();
    // The link belonged to the process that has just ended. Whether the login
    // worked or not that URL is spent, and offering it afterwards would send
    // the user to a page that answers with an expired code.
    clearLink();
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

// --- offscreen test entries --------------------------------------------------
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.

namespace {

/// One failing prerequisite the IDE knows how to fix, whose detail reads as
/// markup. The daemon writes these details, and a `<b>` in one is text about a
/// command, never a request for bold.
const char* const kMarkupPrereq =
    "[{\"name\":\"claude\",\"ok\":false,\"detail\":\"<b>x</b>\",\"fix_hint\":\"\"}]";

/// One prerequisite the daemon found and one it did not, so a single payload
/// draws both glyphs.
const char* const kMixedPrereqs =
    "[{\"name\":\"git\",\"ok\":true,\"detail\":\"2.43.0\",\"fix_hint\":\"\"},"
    "{\"name\":\"claude\",\"ok\":false,\"detail\":\"not installed\",\"fix_hint\":\"\"}]";

/// The page's fix button for that row, or null.
QPushButton* fixButton(const SetupPage& page) {
    for (QPushButton* button : page.findChildren<QPushButton*>()) {
        if (button->text() == QLatin1String("Install Claude Code")) {
            return button;
        }
    }
    return nullptr;
}

} // namespace

/// The tick and the cross are the theme's accents, lifted for a dark palette:
/// `theme` picks its accents as fills for a light background, and `ink` is the
/// one way one of them becomes text.
extern "C" std::int32_t bs_widget_test_setup_page_glyphs_are_inked_accents() {
    // Without this the checks below would pass on an implementation that never
    // lifted anything, because the two colours each compares would be equal.
    if (theme::ink(theme::added(), true) == theme::added()) {
        return 1;
    }
    for (const bool dark : { false, true }) {
        AppController controller;
        SetupPage page(&controller);
        QPalette palette = page.palette();
        // `theme::isDark` reads `Base`. Set before the rows are built: each row
        // resolves its glyph colour as it is made.
        palette.setColor(QPalette::Base,
                         dark ? QColor(0x1e, 0x1e, 0x1e) : QColor(0xff, 0xff, 0xff));
        page.setPalette(palette);
        if (theme::isDark(page.palette()) != dark) {
            return dark ? 2 : 3;
        }
        controller.prereqsChecked(QString::fromUtf8(kMixedPrereqs));

        QString okSheet;
        QString badSheet;
        for (QLabel* label : page.findChildren<QLabel*>()) {
            if (label->text() == kOk) {
                okSheet = label->styleSheet();
            } else if (label->text() == kBad) {
                badSheet = label->styleSheet();
            }
        }
        const QString wantOk =
            QStringLiteral("color:%1").arg(theme::ink(theme::added(), dark).name());
        const QString wantBad =
            QStringLiteral("color:%1").arg(theme::ink(theme::removed(), dark).name());
        if (okSheet != wantOk) {
            return dark ? 4 : 5;
        }
        if (badSheet != wantBad) {
            return dark ? 6 : 7;
        }
    }
    return 0;
}

/// A detail the daemon wrote is shown as the text it is.
extern "C" std::int32_t bs_widget_test_setup_page_detail_is_plain_text() {
    AppController controller;
    SetupPage page(&controller);
    controller.prereqsChecked(QString::fromUtf8(kMarkupPrereq));
    for (QLabel* label : page.findChildren<QLabel*>()) {
        if (label->text() == QLatin1String("<b>x</b>")) {
            return label->textFormat() == Qt::PlainText ? 0 : 2;
        }
    }
    // No label carries the detail at all: either the row was not built or the
    // markup was already swallowed by a rich-text render.
    return 1;
}

/// The page closes the host PTY it was given when it goes away.
extern "C" std::int32_t bs_widget_test_setup_page_closes_its_pty() {
    AppController controller;
    QString closed;
    {
        SetupPage page(&controller);
        page.setPtyCloser([&closed](const QString& ptyId) { closed = ptyId; });
        controller.prereqsChecked(QString::fromUtf8(kMarkupPrereq));
        QPushButton* fix = fixButton(page);
        if (fix == nullptr) {
            return 1;
        }
        // The request itself is deferred by a turn of the event loop, which is
        // not running here; what the click does now is mark the action as the
        // one being waited for, which is what makes the reply below this
        // page's.
        fix->click();
        controller.setupPtyOpened(QStringLiteral("install_claude"), QStringLiteral("pty_7"));
    }
    if (closed != QStringLiteral("pty_7")) {
        return 2;
    }
    return 0;
}
