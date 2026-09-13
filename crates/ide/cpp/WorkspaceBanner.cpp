#include "WorkspaceBanner.h"
#include "CodeView.h"
#include "Theme.h"
#include <QChar>
#include <QCoreApplication>
#include <QFont>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QHBoxLayout>
#include <QLabel>
#include <QPalette>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QSizePolicy>
#include <QToolButton>
#include <QVBoxLayout>
#include <cstdint>

namespace {

/// How many lines of stderr are visible before the box scrolls. Enough for a
/// git error's usual three or four, without the banner taking the pane over.
constexpr int kStderrRows = 8;

/// U+25B6 and U+25BC, the collapsed and expanded disclosure arrows, as code
/// points rather than as characters in a literal: the file is compiled without
/// a byte order mark and MSVC would otherwise read them in the system code
/// page.
QString arrow(bool expanded) {
    return QString(QChar(expanded ? 0x25BC : 0x25B6));
}

} // namespace

WorkspaceBanner::WorkspaceBanner(QWidget* parent) : QFrame(parent) {
    setObjectName(QStringLiteral("WorkspaceBanner"));
    setFrameShape(QFrame::StyledPanel);
    setAutoFillBackground(true);
    applyWash();

    auto* outer = new QVBoxLayout(this);
    outer->setContentsMargins(8, 6, 8, 6);
    outer->setSpacing(4);

    auto* head = new QHBoxLayout();
    head->setContentsMargins(0, 0, 0, 0);
    head->setSpacing(8);

    auto* lines = new QVBoxLayout();
    lines->setContentsMargins(0, 0, 0, 0);
    lines->setSpacing(0);
    m_title = new QLabel(this);
    m_title->setObjectName(QStringLiteral("WorkspaceBannerTitle"));
    // Plain text throughout: every string here comes from the daemon or from a
    // git command's output, and neither is markup.
    m_title->setTextFormat(Qt::PlainText);
    m_title->setWordWrap(true);
    QFont titleFont = m_title->font();
    titleFont.setBold(true);
    m_title->setFont(titleFont);
    m_title->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    lines->addWidget(m_title);

    m_detail = new QLabel(this);
    m_detail->setObjectName(QStringLiteral("WorkspaceBannerDetail"));
    m_detail->setTextFormat(Qt::PlainText);
    m_detail->setWordWrap(true);
    m_detail->setTextInteractionFlags(Qt::TextSelectableByMouse);
    m_detail->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    m_detail->hide();
    lines->addWidget(m_detail);
    head->addLayout(lines, 1);

    m_disclose = new QToolButton(this);
    m_disclose->setObjectName(QStringLiteral("WorkspaceBannerDetailsButton"));
    m_disclose->setAutoRaise(true);
    m_disclose->setToolButtonStyle(Qt::ToolButtonTextOnly);
    m_disclose->setToolTip(QStringLiteral("Show what git printed"));
    m_disclose->hide();
    head->addWidget(m_disclose, 0, Qt::AlignTop);

    // Before Dismiss, because it is the one that acts: dismissing an agent that
    // died without restarting it leaves a pane that can do nothing, and the
    // button that fixes that should not be the second one read.
    m_restart = new QPushButton(QStringLiteral("Restart agent"), this);
    m_restart->setObjectName(QStringLiteral("WorkspaceBannerRestartButton"));
    m_restart->setToolTip(
        QStringLiteral("Start this workspace's agent again, resuming the conversation."));
    m_restart->hide();
    head->addWidget(m_restart, 0, Qt::AlignTop);

    m_dismiss = new QPushButton(QStringLiteral("Dismiss"), this);
    m_dismiss->setObjectName(QStringLiteral("WorkspaceBannerDismissButton"));
    head->addWidget(m_dismiss, 0, Qt::AlignTop);
    outer->addLayout(head);

    m_stderr = new QPlainTextEdit(this);
    m_stderr->setObjectName(QStringLiteral("WorkspaceBannerStderr"));
    m_stderr->setReadOnly(true);
    m_stderr->setLineWrapMode(QPlainTextEdit::NoWrap);
    QFont mono = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    mono.setStyleHint(QFont::Monospace);
    m_stderr->setFont(mono);
    m_stderr->hide();
    outer->addWidget(m_stderr);

    QObject::connect(m_disclose, &QToolButton::clicked, this,
                     [this] { setExpanded(!m_stderr->isVisible()); });
    QObject::connect(m_dismiss, &QPushButton::clicked, this, [this] {
        reset();
        emit dismissed();
    });
    // The banner stays up. What the restart does next -- succeed, or fail again
    // with something new to say -- is what takes it down or replaces it, and a
    // banner that vanished on the click would leave nothing on screen while the
    // start is in flight.
    QObject::connect(m_restart, &QPushButton::clicked, this,
                     [this] { emit restartRequested(); });

    QFrame::hide();
}

void WorkspaceBanner::showError(const QString& title, const QString& detail,
                                const QString& stderrText) {
    m_title->setText(title);
    m_title->setToolTip(title);
    m_detail->setText(detail);
    m_detail->setVisible(!detail.isEmpty());
    m_stderr->setPlainText(stderrText);
    m_disclose->setVisible(!stderrText.isEmpty());
    // Every raise starts folded: the sentence is the news, and a wall of git
    // output unfolding by itself would push the pane down each time.
    setExpanded(false);
    QFrame::show();
}

void WorkspaceBanner::reset() {
    m_title->clear();
    m_detail->clear();
    m_detail->hide();
    m_stderr->clear();
    m_disclose->hide();
    m_restart->hide();
    setExpanded(false);
    QFrame::hide();
}

void WorkspaceBanner::setRestartOffered(bool offered) {
    m_restart->setVisible(offered);
}

void WorkspaceBanner::applyWash() {
    // Installing a palette raises the change event that brought us here, so the
    // guard is what stops this calling itself. Same shape as `PermissionBar`.
    if (m_mixing) {
        return;
    }
    m_mixing = true;
    // Cleared first so that `palette()` below answers with the pane behind the
    // banner rather than with whatever this widget was last given. It is
    // defensive rather than load-bearing as the code stands -- the mix reads
    // `base()` and installs only `Window`, so there is nothing yet for a second
    // mix to compound with -- but the day someone washes a second role in here,
    // the version without this line drifts a shade further from the window on
    // every theme change and the drift is invisible until it is bad.
    setPalette(QPalette());
    // The same red wash the Run panel's blocked-host toast uses: this is a
    // report of something that did not happen, not a question waiting on an
    // answer.
    QPalette colours;
    colours.setColor(QPalette::Window, codeview::wash(palette().base().color(), theme::removed(),
                                                      codeview::kWashAmount));
    setPalette(colours);
    m_mixing = false;
}

void WorkspaceBanner::changeEvent(QEvent* event) {
    QFrame::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        applyWash();
    }
}

void WorkspaceBanner::setExpanded(bool expanded) {
    const bool haveStderr = !m_stderr->toPlainText().isEmpty();
    const bool open = expanded && haveStderr;
    m_stderr->setVisible(open);
    if (open) {
        const QFontMetrics metrics(m_stderr->font());
        m_stderr->setFixedHeight(kStderrRows * metrics.lineSpacing() +
                                 2 * static_cast<int>(m_stderr->frameWidth()));
    }
    m_disclose->setText(arrow(open) + QStringLiteral(" Details"));
}

// The offscreen widget checks. See the note in `EditorArea.cpp`; `build.rs`
// defines `BS_WIDGET_TESTS` for every profile but `release`, and
// `bs_widget_test_begin` must have run first.
#if defined(BS_WIDGET_TESTS)

extern "C" std::int32_t bs_widget_test_banner_offers_restart_only_to_a_dead_agent() {
    QWidget host;
    auto* banner = new WorkspaceBanner(&host);
    auto* restart = banner->findChild<QPushButton*>(QStringLiteral("WorkspaceBannerRestartButton"));
    if (restart == nullptr) {
        return 1;
    }

    // A merge that conflicted: the agent behind it is alive, and restarting it
    // would interrupt rather than repair.
    banner->showError(QStringLiteral("Merge failed"), QStringLiteral("two files conflict"),
                      QString());
    if (!restart->isHidden()) {
        return 2;
    }

    banner->setRestartOffered(true);
    if (restart->isHidden()) {
        return 3;
    }
    // And back: a workspace whose agent came up again is a workspace whose
    // banner must stop offering to start it. Asserted in both directions
    // because only one of them is the constructor's `hide()` doing the work --
    // a `setRestartOffered` that ignored its argument would pass the other.
    banner->setRestartOffered(false);
    if (!restart->isHidden()) {
        return 8;
    }
    banner->setRestartOffered(true);
    // A second failure on the same dead agent still offers it: `showError` says
    // what happened, not whether the agent is running.
    banner->showError(QStringLiteral("Agent exited"), QStringLiteral("status 1"), QString());
    if (restart->isHidden()) {
        return 4;
    }

    bool asked = false;
    QObject::connect(banner, &WorkspaceBanner::restartRequested, [&asked] { asked = true; });
    restart->click();
    if (!asked) {
        return 5;
    }
    // The banner stays up: what the restart does next is what replaces it, and
    // a banner that vanished on the click would leave nothing on screen.
    if (banner->isHidden()) {
        return 6;
    }

    banner->reset();
    if (!restart->isHidden()) {
        return 7;
    }
    return 0;
}

extern "C" std::int32_t bs_widget_test_banner_remixes_its_red_for_a_new_palette() {
    QWidget host;
    QPalette dark;
    dark.setColor(QPalette::Base, QColor(0x1e, 0x1e, 0x1e));
    dark.setColor(QPalette::Window, QColor(0x25, 0x25, 0x26));
    host.setPalette(dark);

    auto* banner = new WorkspaceBanner(&host);
    const QColor onDark = banner->palette().color(QPalette::Window);

    QPalette light;
    light.setColor(QPalette::Base, QColor(0xff, 0xff, 0xff));
    light.setColor(QPalette::Window, QColor(0xf0, 0xf0, 0xf0));
    host.setPalette(light);
    QCoreApplication::processEvents();

    const QColor onLight = banner->palette().color(QPalette::Window);
    if (onLight == onDark) {
        return 1;
    }
    if (theme::isDark(banner->palette())) {
        return 2;
    }

    // Back again, and the red must land exactly where it started rather than a
    // shade further out: the wash is mixed into the pane, not into itself.
    host.setPalette(dark);
    QCoreApplication::processEvents();
    if (banner->palette().color(QPalette::Window) != onDark) {
        return 3;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
