#pragma once
#include <QApplication>
#include <QColor>
#include <QCoreApplication>
#include <QEvent>
#include <QFont>
#include <QFontDatabase>
#include <QPalette>
#include <QString>
#include <QStyleHints>
#include <QVariant>
#include <QWidget>
#include <QWidgetList>

/// The colours the IDE means by "added", "removed", "changed" and "renamed",
/// the one rule for telling a dark palette from a light one, and the palette
/// the application wears.
///
/// A diff row's background and a status word in the Changes tab are the same
/// judgement rendered two ways, so they read the same accent from here rather
/// than each writing out its own hex. This is the set of values a retheme has to
/// change, and the file that installs the palette they are all read against.
///
/// Nothing here is an accent's own widget or fill: an accent becomes a
/// background through `codeview::wash` and text through `theme::ink`. The one
/// thing here that does paint is [`apply`], at the other end of the scale --
/// it dresses the whole application, and every colour below then follows from
/// the palette it installed.
namespace theme {

/// A palette whose `Base` is darker than this is a dark palette. The pane's own
/// background is the only theme signal there is.
inline constexpr int kDarkLightnessCutoff = 128;

/// How much an accent is lifted before it is drawn as text on a dark palette,
/// as a `QColor::lighter` percentage. The flat accents are chosen to read on a
/// light background and sit too close to a dark one.
inline constexpr int kDarkLift = 135;

/// Whether `palette` is a dark one.
inline bool isDark(const QPalette& palette) {
    return palette.base().color().lightness() < kDarkLightnessCutoff;
}

/// A line or a file that is new: green. Also the IDE's one "in place", which
/// is the same judgement told about something that is not a line: the tick on
/// a prerequisite the daemon found.
///
/// A fill, like every accent here. Drawn as text it goes through `ink` first.
inline QColor added() {
    return QColor(0x2e, 0xa0, 0x43);
}

/// A line or a file that is gone: red. Also the IDE's one "wrong", which is
/// the same judgement told about something that is not a line: an agent that
/// failed, a prerequisite that is missing, a terminal that could not be
/// reached. There is deliberately no separate error colour -- a second red
/// would only be this one drifting.
///
/// A fill, like every accent here. Drawn as text it goes through `ink` first.
inline QColor removed() {
    return QColor(0xd0, 0x39, 0x33);
}

/// A line or a file that was rewritten in place: amber.
inline QColor changed() {
    return QColor(0xc9, 0x96, 0x2a);
}

/// A file that moved: blue.
inline QColor renamed() {
    return QColor(0x3b, 0x7d, 0xd8);
}

/// An accent as ink rather than as a fill, legible on either palette.
inline QColor ink(const QColor& accent, bool dark) {
    return dark ? accent.lighter(kDarkLift) : accent;
}

/// How far a band's fill is lifted off a dark window, and pushed under a light
/// one, as `QColor::lighter`/`darker` percentages. Two different numbers
/// because the same step reads much louder going up from near-black than it
/// does going down from near-white.
inline constexpr int kBandLift = 132;
inline constexpr int kBandSink = 108;

/// The fill of a strip that has to read as its own band -- the Explorer's
/// workspace header, the agent bar -- rather than as more of the window behind
/// it. A wash of the window colour itself, so it follows whatever palette Qt
/// hands the application and never becomes a colour of its own.
inline QColor band(const QPalette& palette) {
    const QColor window = palette.window().color();
    return isDark(palette) ? window.lighter(kBandLift) : window.darker(kBandSink);
}

/// Text that labels a band rather than saying anything -- the "Agents" caption,
/// the Explorer's branch line. The window's own foreground, faded towards the
/// band it sits on, so it recedes on either palette.
inline QColor muted(const QPalette& palette) {
    const QColor text = palette.windowText().color();
    const QColor fill = band(palette);
    return QColor((text.red() + fill.red()) / 2, (text.green() + fill.green()) / 2,
                  (text.blue() + fill.blue()) / 2);
}

/// Which palette the application wears. `System` is not "dark or light decided
/// once": it is whatever the desktop is doing, and it changes under the
/// application when Windows changes at dusk.
enum class Choice { System, Light, Dark };

/// The word `AppController::theme()` answers with, as a choice. Anything
/// unknown is `System`, because the settings file is the only writer and a word
/// this does not know could only come from a hand edit.
inline Choice choiceFromName(const QString& name) {
    if (name == QLatin1String("light")) {
        return Choice::Light;
    }
    if (name == QLatin1String("dark")) {
        return Choice::Dark;
    }
    return Choice::System;
}

/// The word to store for a choice.
inline QString nameOfChoice(Choice choice) {
    switch (choice) {
    case Choice::Light:
        return QStringLiteral("light");
    case Choice::Dark:
        return QStringLiteral("dark");
    case Choice::System:
        break;
    }
    return QStringLiteral("system");
}

/// A flat Fusion palette in the given polarity. Only the roles Fusion actually
/// reads are set; everything else is derived by the style, which is what keeps
/// a light palette from needing a second set of hand-picked greys.
inline QPalette explicitPalette(bool dark) {
    QPalette p;
    const QColor window = dark ? QColor(0x25, 0x25, 0x26) : QColor(0xf0, 0xf0, 0xf0);
    const QColor base = dark ? QColor(0x1e, 0x1e, 0x1e) : QColor(0xff, 0xff, 0xff);
    const QColor text = dark ? QColor(0xdc, 0xdc, 0xdc) : QColor(0x1e, 0x1e, 0x1e);
    const QColor disabled = dark ? QColor(0x7f, 0x7f, 0x7f) : QColor(0x9a, 0x9a, 0x9a);
    const QColor highlight = dark ? QColor(0x2d, 0x5c, 0x8a) : QColor(0x30, 0x8c, 0xc6);
    p.setColor(QPalette::Window, window);
    p.setColor(QPalette::WindowText, text);
    p.setColor(QPalette::Base, base);
    p.setColor(QPalette::AlternateBase, dark ? window.lighter(110) : window.darker(103));
    p.setColor(QPalette::Text, text);
    p.setColor(QPalette::Button, window);
    p.setColor(QPalette::ButtonText, text);
    p.setColor(QPalette::ToolTipBase, base);
    p.setColor(QPalette::ToolTipText, text);
    p.setColor(QPalette::Highlight, highlight);
    // White on either polarity: both highlights are picked dark enough to carry
    // it, and a selected row that changed its ink with the theme would be the
    // one place the two palettes disagreed about what selection means.
    p.setColor(QPalette::HighlightedText, QColor(0xff, 0xff, 0xff));
    p.setColor(QPalette::Disabled, QPalette::Text, disabled);
    p.setColor(QPalette::Disabled, QPalette::WindowText, disabled);
    p.setColor(QPalette::Disabled, QPalette::ButtonText, disabled);
    return p;
}

/// Tells every widget in the application that the palette moved under it.
///
/// `QApplication::setPalette` alone does not: a widget that has never set a
/// palette of its own silently picks the new colours up and is sent no
/// `QEvent::PaletteChange` at all, which is exactly the widget whose
/// palette-derived colours are stale -- the Explorer's header band and the
/// transcript's error banner among them. Only a widget that already carries
/// its own palette is told, which is too few to repaint a window with. Sending
/// it by hand costs one pass over the widget list per theme change and makes
/// `changeEvent` mean what every widget here assumes it means.
inline void announcePaletteChange() {
    QEvent changed(QEvent::PaletteChange);
    const QWidgetList widgets = QApplication::allWidgets();
    for (QWidget* widget : widgets) {
        QCoreApplication::sendEvent(widget, &changed);
    }
}

/// Dresses the application. All three choices install an explicit palette;
/// `System` differs only in reading the polarity off the desktop rather than
/// off the user.
inline void apply(QApplication& app, Choice choice) {
    switch (choice) {
    case Choice::Light:
        app.setPalette(explicitPalette(false));
        announcePaletteChange();
        return;
    case Choice::Dark:
        app.setPalette(explicitPalette(true));
        announcePaletteChange();
        return;
    case Choice::System:
        break;
    }
    const bool dark = QApplication::styleHints()->colorScheme() == Qt::ColorScheme::Dark;
    app.setPalette(explicitPalette(dark));
    announcePaletteChange();
}

// --- text sizes ----------------------------------------------------------------
//
// Two sizes, both settings: the interface's own text, which is the application
// font and reaches every widget that does not set a size of its own, and the
// code font, which everything code-shaped wears. A widget that sizes its text
// any other way -- a fraction of the interface size, or the code font -- says
// so through `useCodeFont` or `useScaledFont`, and is then re-sized by
// `applyFonts` with everything else. A widget that caches metrics from its font
// redoes them on `QEvent::FontChange`, which `applyFonts` sends to every widget.

/// The bounds and defaults `settings.rs` holds as `MIN_FONT_SIZE`,
/// `MAX_FONT_SIZE`, `DEFAULT_UI_FONT_SIZE` and `DEFAULT_CODE_FONT_SIZE`. The
/// defaults are only what a widget built before `applyFonts` (a test's) wears;
/// the application applies the stored sizes before it builds a window.
inline constexpr int kMinFontPointSize = 8;
inline constexpr int kMaxFontPointSize = 24;
inline constexpr int kDefaultUiPointSize = 10;
inline constexpr int kDefaultCodePointSize = 11;

/// How much smaller than the interface's text the small grey lines are: a
/// transcript's result and system lines, a tool card's captions.
inline constexpr qreal kSmallTextScale = 0.85;

/// The dynamic properties a widget's text size is recorded under.
inline constexpr char kCodeFontProperty[] = "bsCodeFont";
inline constexpr char kFontScaleProperty[] = "bsFontScale";

/// The code font's size in points, as last applied.
inline int& codePointSizeSlot() {
    static int size = kDefaultCodePointSize;
    return size;
}

/// The fixed-pitch font at the chosen size.
inline QFont codeFont() {
    QFont font = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    font.setStyleHint(QFont::Monospace);
    font.setFixedPitch(true);
    font.setPointSize(codePointSizeSlot());
    return font;
}

/// Dresses `widget` in the code font, and keeps it there when the size moves.
inline void useCodeFont(QWidget* widget) {
    widget->setProperty(kCodeFontProperty, true);
    widget->setFont(codeFont());
}

/// Sizes `widget`'s text at `scale` times the interface's, and keeps it at that
/// fraction when the size moves. Whatever else the widget's font says -- italic,
/// bold -- is left alone.
inline void useScaledFont(QWidget* widget, qreal scale) {
    widget->setProperty(kFontScaleProperty, scale);
    QFont font = widget->font();
    font.setPointSizeF(QApplication::font().pointSizeF() * scale);
    widget->setFont(font);
}

/// A size the IDE can lay out: the nearest one to `points`.
inline int boundedPointSize(int points) {
    return qBound(kMinFontPointSize, points, kMaxFontPointSize);
}

/// Sets both text sizes and re-sizes every widget already on screen.
///
/// The application font alone would not: it reaches only the widgets that never
/// set a font, so the code panes and the small grey lines would stay at the old
/// size, and a widget that measured its rows once would go on measuring them in
/// the old font. So the two kinds of tagged widget are re-dressed by hand, and
/// every widget is then sent `QEvent::FontChange`, for the same reason
/// `announcePaletteChange` sends its event.
inline void applyFonts(QApplication& app, int uiPoints, int codePoints) {
    uiPoints = boundedPointSize(uiPoints);
    codePoints = boundedPointSize(codePoints);
    codePointSizeSlot() = codePoints;
    QFont ui = QFontDatabase::systemFont(QFontDatabase::GeneralFont);
    ui.setPointSize(uiPoints);
    app.setFont(ui);
    const QWidgetList widgets = QApplication::allWidgets();
    // `setFont` tells the widgets itself only once the event loop is running,
    // and then only the top-level ones. Said to every widget here, so the
    // sizes are in place before the loop has started too -- which is when a
    // test, and the IDE's own startup, applies them.
    // After the tagged widgets have their new sizes, so what a child inherits
    // is the new size rather than the one its parent is about to lose.
    for (QWidget* widget : widgets) {
        if (widget->property(kCodeFontProperty).toBool()) {
            QFont font = widget->font();
            font.setPointSize(codePoints);
            widget->setFont(font);
        }
        const QVariant scale = widget->property(kFontScaleProperty);
        if (scale.isValid()) {
            QFont font = widget->font();
            font.setPointSizeF(uiPoints * scale.toReal());
            widget->setFont(font);
        }
    }
    QEvent appFontChanged(QEvent::ApplicationFontChange);
    for (QWidget* widget : widgets) {
        QCoreApplication::sendEvent(widget, &appFontChanged);
    }
    QEvent changed(QEvent::FontChange);
    for (QWidget* widget : widgets) {
        QCoreApplication::sendEvent(widget, &changed);
    }
}

} // namespace theme
