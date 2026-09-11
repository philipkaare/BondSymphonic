#pragma once
#include <QColor>
#include <QPalette>

/// The colours the IDE means by "added", "removed", "changed" and "renamed",
/// and the one rule for telling a dark palette from a light one.
///
/// A diff row's background and a status word in the Changes tab are the same
/// judgement rendered two ways, so they read the same accent from here rather
/// than each writing out its own hex. Whatever themes the IDE later, this is
/// the set of values it has to change.
///
/// Nothing here is a widget or a fill: an accent becomes a background through
/// `codeview::wash` and text through `theme::ink`.
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

} // namespace theme
