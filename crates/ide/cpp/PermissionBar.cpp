#include "PermissionBar.h"
#include "CodeView.h"
#include "Theme.h"
#include <QCheckBox>
#include <QColor>
#include <QEvent>
#include <QFont>
#include <QHBoxLayout>
#include <QLabel>
#include <QPalette>
#include <QPushButton>
#include <QSizePolicy>
#if defined(BS_WIDGET_TESTS)
#include <QCoreApplication>
#include <cstdint>
#endif

PermissionBar::PermissionBar(QWidget* parent) : QFrame(parent) {
    setFrameShape(QFrame::StyledPanel);
    setAutoFillBackground(true);
    applyWash();

    auto* layout = new QHBoxLayout(this);
    layout->setContentsMargins(6, 4, 6, 4);
    layout->setSpacing(8);

    m_prompt = new QLabel(this);
    m_prompt->setTextFormat(Qt::PlainText);
    m_prompt->setWordWrap(true);
    m_prompt->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
    layout->addWidget(m_prompt, 1);

    m_always = new QCheckBox(QStringLiteral("Always allow this tool for this session"), this);
    layout->addWidget(m_always, 0);

    auto* allow = new QPushButton(QStringLiteral("Allow"), this);
    allow->setDefault(true);
    auto* deny = new QPushButton(QStringLiteral("Deny"), this);
    layout->addWidget(allow, 0);
    layout->addWidget(deny, 0);

    QObject::connect(allow, &QPushButton::clicked, this,
                     [this] { Q_EMIT allowed(m_always->isChecked()); });
    QObject::connect(deny, &QPushButton::clicked, this, [this] { Q_EMIT denied(); });

    hide();
}

void PermissionBar::show(const QJsonObject& pending) {
    const QString requestId = pending.value(QStringLiteral("request_id")).toString();
    const QString tool = pending.value(QStringLiteral("tool_name")).toString();
    const QString summary = pending.value(QStringLiteral("summary")).toString();
    if (requestId != m_requestId) {
        // A different request: whatever was ticked was ticked about the last
        // one, and carrying it over would allowlist a tool nobody agreed to.
        m_always->setChecked(false);
        m_requestId = requestId;
    }
    m_prompt->setText(summary.isEmpty() || summary == tool
                          ? QStringLiteral("Allow %1?").arg(tool)
                          : QStringLiteral("Allow %1: %2?").arg(tool, summary));
    m_prompt->setToolTip(pending.value(QStringLiteral("input_json")).toString());
    QFrame::show();
}

void PermissionBar::clear() {
    m_requestId.clear();
    m_always->setChecked(false);
    m_prompt->clear();
    m_prompt->setToolTip(QString());
    hide();
}

QString PermissionBar::requestId() const { return m_requestId; }

void PermissionBar::applyWash() {
    if (m_mixing) {
        return;
    }
    m_mixing = true;
    // Clearing first, because the palette installed below is the bar's own
    // from then on: `palette()` would otherwise answer with the last mix
    // instead of with the pane behind it, and amber washed into amber drifts
    // a shade further from the window on every theme change.
    setPalette(QPalette());
    // Amber: the bar is asking, not reporting a failure, and "changed" is the
    // one accent the theme reserves for something waiting on the user.
    QPalette barPalette;
    barPalette.setColor(QPalette::Window, codeview::wash(palette().base().color(),
                                                        theme::changed(), codeview::kWashAmount));
    setPalette(barPalette);
    m_mixing = false;
}

void PermissionBar::changeEvent(QEvent* event) {
    QFrame::changeEvent(event);
    if (event->type() == QEvent::PaletteChange || event->type() == QEvent::StyleChange) {
        applyWash();
    }
}

#if defined(BS_WIDGET_TESTS)
//
// See the note in `EditorArea.cpp`. `bs_widget_test_begin` must have run first.

extern "C" std::int32_t bs_widget_test_permission_bar_follows_the_palette() {
    QWidget host;
    QPalette dark = host.palette();
    dark.setColor(QPalette::Base, QColor(0x1e, 0x1e, 0x1e));
    dark.setColor(QPalette::Window, QColor(0x25, 0x25, 0x25));
    host.setPalette(dark);

    auto* bar = new PermissionBar(&host);
    const QColor onDark = bar->palette().color(QPalette::Window);

    QPalette light = host.palette();
    light.setColor(QPalette::Base, QColor(0xff, 0xff, 0xff));
    light.setColor(QPalette::Window, QColor(0xf0, 0xf0, 0xf0));
    host.setPalette(light);
    QCoreApplication::processEvents();

    const QColor onLight = bar->palette().color(QPalette::Window);
    if (onLight == onDark) {
        // The amber wash is mixed into the pane's own background. A bar that
        // kept the dark mix on a light palette is a bar that read the colour
        // once, at construction, and never heard that it changed.
        return 1;
    }
    if (theme::isDark(bar->palette())) {
        return 2;
    }
    return 0;
}

#endif // BS_WIDGET_TESTS
