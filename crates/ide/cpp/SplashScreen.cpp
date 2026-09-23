#include "SplashScreen.h"
#include "Branding.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QEvent>
#include <QFont>
#include <QGuiApplication>
#include <QLabel>
#include <QMouseEvent>
#include <QPainter>
#include <QProgressBar>
#include <QScreen>
#include <QTimer>
#include <QVBoxLayout>

namespace {

/// The controller's `connectionState` codes this widget reads. They mirror
/// `ConnectionState::as_i32` in `model/app_state.rs`; the window keeps its own
/// copy of `Connected` for the same reason, and the splash is built before the
/// window and does not include it.
constexpr int kConnectionDisconnected = 0;
constexpr int kConnectionLaunching = 1;
constexpr int kConnectionConnecting = 2;
constexpr int kConnectionConnected = 3;
constexpr int kConnectionLost = 4;
constexpr int kConnectionError = 5;
constexpr int kConnectionReconnecting = 6;

/// The longest the splash stays up whatever the controller says. The launch-
/// time requests it is waiting on are themselves capped at 30 s, so a splash
/// that outlived them would be covering a failure the window's status bar and
/// Setup page already report -- and a splash the user cannot get past is the
/// empty Setup page all over again, with the page hidden as well.
constexpr int kSplashCapMs = 30'000;

/// The logo at exactly half its 256 px source, so nearest-neighbour drops
/// every other pixel evenly rather than doubling some and not others.
constexpr int kLogoPx = 128;

/// Room around the content. Generous for a widget this small, because the
/// splash is frameless and the padding is the only frame it has.
constexpr int kPadding = 28;

/// Wide enough for the longest status line in the title's font with room
/// either side, so the splash does not resize as the phases go by.
constexpr int kMinWidth = 360;

/// The title's size relative to the application font. Large enough to read
/// as a name rather than a label, and no larger: the logo is the picture.
constexpr int kTitlePointSizeDelta = 6;

/// What the status line says for a connection state, before the daemon has
/// answered anything and after it has answered the prerequisites. The
/// controller's `statusMessage` is status-bar shorthand ("daemon: connecting")
/// and is deliberately not shown here.
QString phaseText(int state, bool prereqsAnswered) {
    switch (state) {
    case kConnectionConnected:
        return prereqsAnswered ? QStringLiteral("Loading your workspaces…")
                               : QStringLiteral("Checking the prerequisites…");
    case kConnectionConnecting:
        return QStringLiteral("Connecting…");
    case kConnectionReconnecting:
        return QStringLiteral("Reconnecting…");
    case kConnectionDisconnected:
    case kConnectionLaunching:
    default:
        return QStringLiteral("Launching the daemon…");
    }
}

} // namespace

SplashScreen::SplashScreen(AppController* controller, QWidget* parent)
    : QWidget(parent), m_controller(controller) {
    setObjectName(QStringLiteral("SplashScreen"));
    // A splash-type top-level: frameless, no taskbar entry, and kept above
    // the window that is shown behind it a moment later.
    setWindowFlags(Qt::SplashScreen | Qt::WindowStaysOnTopHint);
    setAutoFillBackground(true);
    setMinimumWidth(kMinWidth);

    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(kPadding, kPadding, kPadding, kPadding);
    layout->setSpacing(8);

    auto* logo = new QLabel(this);
    logo->setPixmap(branding::logo(kLogoPx));
    logo->setAlignment(Qt::AlignHCenter);
    layout->addWidget(logo);

    auto* title = new QLabel(QStringLiteral("BondSymphonic"), this);
    title->setObjectName(QStringLiteral("SplashTitle"));
    title->setAlignment(Qt::AlignHCenter);
    QFont titleFont = title->font();
    titleFont.setPointSize(titleFont.pointSize() + kTitlePointSizeDelta);
    titleFont.setBold(true);
    title->setFont(titleFont);
    layout->addWidget(title);

    m_version = new QLabel(branding::version(), this);
    m_version->setObjectName(QStringLiteral("SplashVersion"));
    m_version->setAlignment(Qt::AlignHCenter);
    layout->addWidget(m_version);

    layout->addSpacing(kPadding / 2);

    m_status = new QLabel(this);
    m_status->setObjectName(QStringLiteral("SplashStatus"));
    m_status->setAlignment(Qt::AlignHCenter);
    layout->addWidget(m_status);

    // Indeterminate: nothing here knows how long a check or a list takes,
    // and a bar that pretended to would sit at 90% for the slow one.
    auto* busy = new QProgressBar(this);
    busy->setObjectName(QStringLiteral("SplashBusy"));
    busy->setRange(0, 0);
    busy->setTextVisible(false);
    layout->addWidget(busy);

    restyle();

    m_cap = new QTimer(this);
    m_cap->setObjectName(QStringLiteral("SplashCap"));
    m_cap->setSingleShot(true);
    m_cap->setInterval(kSplashCapMs);
    connect(m_cap, &QTimer::timeout, this, &SplashScreen::finish);
    m_cap->start();

    connect(m_controller, &AppController::connectionStateChanged, this, &SplashScreen::refresh);
    connect(m_controller, &AppController::prereqsAnsweredChanged, this, &SplashScreen::refresh);
    connect(m_controller, &AppController::workspacesListed, this, [this](const QString&) {
        m_workspacesListed = true;
        refresh();
    });
    // A check the daemon could not answer on a live connection is followed by
    // the controller's own retry five seconds later (`PREREQ_RETRY_DELAY`), so
    // the splash keeps waiting for that. With no connection there is no retry
    // coming, and the window's status bar owns what happens next.
    connect(m_controller, &AppController::prereqsCheckFailed, this, [this](const QString&) {
        if (m_controller->getConnectionState() != kConnectionConnected) {
            finish();
        }
    });

    // Built before `AppController::start`, so the state read here is the
    // resting one and the first decision is made on the first change.
    refresh();

    adjustSize();
    if (QScreen* screen = QGuiApplication::primaryScreen()) {
        move(screen->availableGeometry().center() - rect().center());
    }
}

bool SplashScreen::isDone() const {
    return m_done;
}

void SplashScreen::refresh() {
    if (m_done) {
        return;
    }
    const int state = m_controller->getConnectionState();
    // A launch that is not going to connect: the status bar and the Setup
    // page tell that story, and a splash over them helps nobody.
    if (state == kConnectionLost || state == kConnectionError) {
        finish();
        return;
    }
    const bool prereqsAnswered = m_controller->getPrereqsAnswered();
    if (state == kConnectionConnected && prereqsAnswered && m_workspacesListed) {
        finish();
        return;
    }
    m_status->setText(phaseText(state, prereqsAnswered));
}

void SplashScreen::finish() {
    if (m_done) {
        return;
    }
    m_done = true;
    m_cap->stop();
    hide();
    emit finished();
}

void SplashScreen::mousePressEvent(QMouseEvent* event) {
    event->accept();
    finish();
}

void SplashScreen::changeEvent(QEvent* event) {
    QWidget::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        restyle();
    }
}

void SplashScreen::restyle() {
    // The version recedes like a caption; everything else is the window's
    // own ink, so the splash is right in either polarity without a colour
    // of its own.
    QPalette faded = m_version->palette();
    faded.setColor(QPalette::WindowText, theme::muted(palette()));
    m_version->setPalette(faded);
    update();
}

void SplashScreen::paintEvent(QPaintEvent* event) {
    QWidget::paintEvent(event);
    // A frameless window in the window colour over a window in the window
    // colour needs an edge, or it reads as a hole in the IDE. One line of the
    // caption ink, which is the quietest thing that still separates them.
    QPainter painter(this);
    painter.setPen(theme::muted(palette()));
    painter.drawRect(rect().adjusted(0, 0, -1, -1));
}

// --- offscreen test entries --------------------------------------------------
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.
#if defined(BS_WIDGET_TESTS)
#include <QCoreApplication>
#include <cstdint>

namespace {

/// Fires the cap now, if it is running, without waiting the thirty seconds
/// out: the interval goes to zero and the pending timer event is delivered,
/// which runs the real timeout connection. Same device as `ExplorerDock.cpp`'s
/// `fireGrace`.
void fireCap(QTimer* cap) {
    if (cap->isActive()) {
        cap->setInterval(0);
    }
    QCoreApplication::processEvents();
}

QLabel* statusOf(const SplashScreen& splash) {
    return splash.findChild<QLabel*>(QStringLiteral("SplashStatus"));
}

QTimer* capOf(const SplashScreen& splash) {
    return splash.findChild<QTimer*>(QStringLiteral("SplashCap"));
}

} // namespace

/// The splash names the phase the launch is in and goes away exactly when the
/// IDE is ready -- connected, prerequisites answered, workspaces listed, in
/// either order -- and also on a broken connection, on a failed check with no
/// connection to retry on, on the cap and on a click. It stays through a
/// failed check on a live connection, because the controller retries that.
///
/// Driven through a real `AppController` that was never started: its property
/// setters emit their `Changed` signals, which is all the splash listens to.
/// Never shown: showing a top-level in this suite aborts the run.
extern "C" std::int32_t bs_widget_test_splash_closes_when_the_ide_is_ready() {
    {
        AppController controller;
        SplashScreen splash(&controller);
        QLabel* status = statusOf(splash);
        QTimer* cap = capOf(splash);
        if (status == nullptr || cap == nullptr || !cap->isActive()) {
            return 1;
        }
        int finished = 0;
        QObject::connect(&splash, &SplashScreen::finished, &splash, [&finished] { ++finished; });
        // Before `start`: the daemon is being launched, as far as the user is
        // concerned, and the same line covers the launch itself.
        if (status->text() != QStringLiteral("Launching the daemon…")) {
            return 2;
        }
        controller.setConnectionState(kConnectionLaunching);
        if (status->text() != QStringLiteral("Launching the daemon…")) {
            return 3;
        }
        controller.setConnectionState(kConnectionConnecting);
        if (status->text() != QStringLiteral("Connecting…")) {
            return 4;
        }
        controller.setConnectionState(kConnectionConnected);
        if (status->text() != QStringLiteral("Checking the prerequisites…")) {
            return 5;
        }
        // A failed check on a live connection: the retry is five seconds out,
        // so the splash waits for it.
        controller.prereqsCheckFailed(QStringLiteral("request timed out"));
        if (splash.isDone() || finished != 0) {
            return 6;
        }
        // The list lands before the prerequisites do: still checking, still up.
        controller.workspacesListed(QStringLiteral("[]"));
        if (splash.isDone() || status->text() != QStringLiteral("Checking the prerequisites…")) {
            return 7;
        }
        // The third condition, and the splash is over -- once.
        controller.setPrereqsAnswered(true);
        if (!splash.isDone() || finished != 1 || cap->isActive()) {
            return 8;
        }
        controller.workspacesListed(QStringLiteral("[]"));
        controller.setConnectionState(kConnectionLost);
        if (finished != 1) {
            return 9;
        }
    }
    {
        // The other order: the prerequisites answer first, and the line says
        // what is still being waited for.
        AppController controller;
        SplashScreen splash(&controller);
        controller.setConnectionState(kConnectionConnected);
        controller.setPrereqsAnswered(true);
        if (splash.isDone() || statusOf(splash)->text() != QStringLiteral("Loading your workspaces…")) {
            return 10;
        }
        controller.workspacesListed(QStringLiteral("[]"));
        if (!splash.isDone()) {
            return 11;
        }
    }
    {
        // The connection is not coming: the window's status bar owns it.
        AppController controller;
        SplashScreen splash(&controller);
        controller.setConnectionState(kConnectionLaunching);
        controller.setConnectionState(kConnectionLost);
        if (!splash.isDone()) {
            return 12;
        }
    }
    {
        // A failed check with nothing to retry on.
        AppController controller;
        SplashScreen splash(&controller);
        controller.setConnectionState(kConnectionConnecting);
        controller.prereqsCheckFailed(QStringLiteral("daemon connection lost"));
        if (!splash.isDone()) {
            return 13;
        }
    }
    {
        // The cap, with nothing else having happened at all.
        AppController controller;
        SplashScreen splash(&controller);
        fireCap(capOf(splash));
        if (!splash.isDone()) {
            return 14;
        }
    }
    {
        // A click anywhere.
        AppController controller;
        SplashScreen splash(&controller);
        const QPointF at(1, 1);
        QMouseEvent press(QEvent::MouseButtonPress, at, at, at, Qt::LeftButton, Qt::LeftButton,
                          Qt::NoModifier);
        QCoreApplication::sendEvent(&splash, &press);
        if (!splash.isDone()) {
            return 15;
        }
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
