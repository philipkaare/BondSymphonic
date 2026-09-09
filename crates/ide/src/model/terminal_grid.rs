//! Terminal emulation for agent panes.
//!
//! A [`TerminalGrid`] wraps `alacritty_terminal` and exposes exactly what a Qt
//! widget needs to paint: the visible rows as text plus attribute spans, the
//! cursor position, scrollback control, and the window title. No Qt types are
//! used here; the Qt key and modifier values arrive as plain integers (see the
//! [`qt`] module) so that the mapping to PTY bytes stays testable in pure Rust.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use serde::Serialize;

/// Lines of scrollback kept per terminal.
const SCROLLBACK_LINES: usize = 10_000;

/// A run of cells on one row that share the same attributes.
///
/// `fg` and `bg` are `#rrggbb` strings, or empty when the cell uses the
/// terminal's default foreground/background (the widget's palette decides).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub len: usize,
    pub fg: String,
    pub bg: String,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// One visible row: its text plus the attribute spans covering it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub text: String,
    pub spans: Vec<Span>,
}

/// Captures the terminal events we care about (currently just the title).
#[derive(Clone, Default)]
struct Listener {
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => *self.title.lock().unwrap() = Some(title),
            TermEvent::ResetTitle => *self.title.lock().unwrap() = None,
            _ => {}
        }
    }
}

/// Terminal dimensions handed to `alacritty_terminal`.
struct Size {
    cols: u16,
    rows: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize + SCROLLBACK_LINES
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// A terminal screen driven by PTY bytes.
pub struct TerminalGrid {
    term: Term<Listener>,
    parser: Processor,
    cols: u16,
    rows: u16,
    listener: Listener,
}

impl TerminalGrid {
    /// Create a terminal of `cols` x `rows` with 10,000 lines of scrollback.
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let listener = Listener::default();
        let term = Term::new(config, &Size { cols, rows }, listener.clone());
        Self {
            term,
            parser: Processor::new(),
            cols,
            rows,
            listener,
        }
    }

    /// Feed raw PTY output through the ANSI parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the screen, keeping the scrollback.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(2);
        self.rows = rows.max(1);
        self.term.resize(Size {
            cols: self.cols,
            rows: self.rows,
        });
    }

    /// Current `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// The visible rows at the current display offset.
    pub fn rows(&self) -> Vec<Row> {
        let grid = self.term.grid();
        let colors = self.term.colors();
        let offset = grid.display_offset() as i32;
        let columns = grid.columns();
        (0..grid.screen_lines())
            .map(|screen_line| {
                let line = Line(screen_line as i32 - offset);
                let mut text = String::with_capacity(columns);
                let mut spans: Vec<Span> = Vec::new();
                for col in 0..columns {
                    let cell = &grid[line][Column(col)];
                    // The trailing half of a wide character carries a dummy
                    // glyph; render it as a blank so column indices stay aligned.
                    let ch = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        ' '
                    } else {
                        cell.c
                    };
                    text.push(ch);
                    let fg = color_hex(cell.fg, colors);
                    let bg = color_hex(cell.bg, colors);
                    let bold = cell.flags.contains(Flags::BOLD);
                    let italic = cell.flags.contains(Flags::ITALIC);
                    let underline = cell.flags.intersects(Flags::ALL_UNDERLINES);
                    let inverse = cell.flags.contains(Flags::INVERSE);
                    match spans.last_mut() {
                        Some(last)
                            if last.fg == fg
                                && last.bg == bg
                                && last.bold == bold
                                && last.italic == italic
                                && last.underline == underline
                                && last.inverse == inverse =>
                        {
                            last.len += 1;
                        }
                        _ => spans.push(Span {
                            start: col,
                            len: 1,
                            fg,
                            bg,
                            bold,
                            italic,
                            underline,
                            inverse,
                        }),
                    }
                }
                Row { text, spans }
            })
            .collect()
    }

    /// The visible rows as JSON (an array of [`Row`]).
    pub fn rows_json(&self) -> String {
        serde_json::to_string(&self.rows()).unwrap_or_default()
    }

    /// `(col, row, visible)` with `row` relative to the visible area. The
    /// cursor is hidden while scrolled into history or when the application
    /// turned it off.
    pub fn cursor(&self) -> (u16, u16, bool) {
        let grid = self.term.grid();
        let point = grid.cursor.point;
        let visible =
            grid.display_offset() == 0 && self.term.mode().contains(TermMode::SHOW_CURSOR);
        let col = point.column.0.min(u16::MAX as usize) as u16;
        let row = point.line.0.clamp(0, u16::MAX as i32) as u16;
        (col, row, visible)
    }

    /// Scroll the viewport; positive `delta_lines` moves towards history.
    pub fn scroll(&mut self, delta_lines: i32) {
        self.term.scroll_display(Scroll::Delta(delta_lines));
    }

    /// Jump back to the live end of the output.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// The title set by the application via OSC 0/2, if any.
    pub fn title(&self) -> Option<String> {
        self.listener.title.lock().unwrap().clone()
    }

    /// Write an inverse-video marker line into the output stream, used for
    /// notices such as `[output dropped]`.
    ///
    /// Deliberately does not scroll to the bottom: a reader who has scrolled up
    /// into history keeps their position.
    pub fn insert_marker(&mut self, text: &str) {
        self.feed(format!("\r\n\x1b[7m{text}\x1b[0m\r\n").as_bytes());
    }

    /// Whether the application asked for application cursor keys (DECCKM).
    pub fn app_cursor_keys(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }
}

/// Render a cell colour as `#rrggbb`, or empty for the terminal defaults.
fn color_hex(color: Color, colors: &Colors) -> String {
    match color {
        Color::Spec(rgb) => hex(rgb),
        Color::Named(named) => match colors[named] {
            Some(rgb) => hex(rgb),
            None => named_default(named),
        },
        Color::Indexed(index) => match colors[index as usize] {
            Some(rgb) => hex(rgb),
            None => ansi_default(index as usize),
        },
    }
}

fn hex(Rgb { r, g, b }: Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Fallback for a named colour the terminal has not overridden. The defaults
/// (foreground, background, cursor and the dim/bright variants of those) render
/// as empty so the widget's palette decides.
fn named_default(named: NamedColor) -> String {
    let index = named as usize;
    match named {
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor
        | NamedColor::BrightForeground
        | NamedColor::DimForeground => String::new(),
        // Dim variants of the 8 base colours; approximate them with the base.
        NamedColor::DimBlack
        | NamedColor::DimRed
        | NamedColor::DimGreen
        | NamedColor::DimYellow
        | NamedColor::DimBlue
        | NamedColor::DimMagenta
        | NamedColor::DimCyan
        | NamedColor::DimWhite => ansi_default(index - NamedColor::DimBlack as usize),
        _ => ansi_default(index),
    }
}

/// The xterm default 256-colour palette: 16 system colours, a 6x6x6 cube, and
/// a 24-step grey ramp.
fn ansi_default(index: usize) -> String {
    const SYSTEM: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    let (r, g, b) = if index < 16 {
        SYSTEM[index]
    } else if index < 232 {
        let i = index - 16;
        let level = |v: usize| -> u8 {
            if v == 0 {
                0
            } else {
                (55 + 40 * v) as u8
            }
        };
        (level(i / 36), level((i / 6) % 6), level(i % 6))
    } else if index < 256 {
        let v = (8 + 10 * (index - 232)) as u8;
        (v, v, v)
    } else {
        return String::new();
    };
    hex(Rgb { r, g, b })
}

/// `Qt::Key` values and `Qt::KeyboardModifiers` bits used by [`key_to_bytes`].
pub mod qt {
    pub const KEY_ESCAPE: i32 = 0x0100_0000;
    pub const KEY_TAB: i32 = 0x0100_0001;
    pub const KEY_BACKTAB: i32 = 0x0100_0002;
    pub const KEY_BACKSPACE: i32 = 0x0100_0003;
    pub const KEY_RETURN: i32 = 0x0100_0004;
    pub const KEY_ENTER: i32 = 0x0100_0005;
    pub const KEY_INSERT: i32 = 0x0100_0006;
    pub const KEY_DELETE: i32 = 0x0100_0007;
    pub const KEY_HOME: i32 = 0x0100_0010;
    pub const KEY_END: i32 = 0x0100_0011;
    pub const KEY_LEFT: i32 = 0x0100_0012;
    pub const KEY_UP: i32 = 0x0100_0013;
    pub const KEY_RIGHT: i32 = 0x0100_0014;
    pub const KEY_DOWN: i32 = 0x0100_0015;
    pub const KEY_PAGEUP: i32 = 0x0100_0016;
    pub const KEY_PAGEDOWN: i32 = 0x0100_0017;
    pub const KEY_F1: i32 = 0x0100_0030;
    pub const MOD_SHIFT: u32 = 0x0200_0000;
    pub const MOD_CTRL: u32 = 0x0400_0000;
    pub const MOD_ALT: u32 = 0x0800_0000;
}

/// Qt key value + `Qt::KeyboardModifiers` bits + the event's text, translated
/// into the bytes a PTY expects. Empty when the key produces nothing (a bare
/// modifier press, or an unmapped key with no text).
pub fn key_to_bytes(qt_key: i32, modifiers: u32, text: &str, app_cursor_keys: bool) -> Vec<u8> {
    if is_modifier_key(qt_key) {
        return Vec::new();
    }
    let ctrl = modifiers & qt::MOD_CTRL != 0;
    let alt = modifiers & qt::MOD_ALT != 0;
    let shift = modifiers & qt::MOD_SHIFT != 0;

    // Windows reports AltGr as Ctrl+Alt, and on layouts such as Danish or
    // German that is how @ { } [ ] are typed. When both modifiers are set and
    // the event carries a printable character, the keyboard layout has already
    // composed it, so send that character rather than folding it into a
    // control code. Plain Ctrl and plain Alt are unaffected.
    if ctrl && alt && starts_with_printable(text) {
        return text.as_bytes().to_vec();
    }

    // Cursor and edit keys use SS3 (`ESC O`) in application mode, CSI otherwise.
    let intro = if app_cursor_keys { b'O' } else { b'[' };

    let mut out: Vec<u8> = match qt_key {
        qt::KEY_RETURN | qt::KEY_ENTER => b"\r".to_vec(),
        qt::KEY_BACKSPACE => {
            if ctrl {
                b"\x08".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        qt::KEY_TAB => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        qt::KEY_BACKTAB => b"\x1b[Z".to_vec(),
        qt::KEY_ESCAPE => b"\x1b".to_vec(),
        qt::KEY_UP => vec![0x1b, intro, b'A'],
        qt::KEY_DOWN => vec![0x1b, intro, b'B'],
        qt::KEY_RIGHT => vec![0x1b, intro, b'C'],
        qt::KEY_LEFT => vec![0x1b, intro, b'D'],
        qt::KEY_HOME => vec![0x1b, intro, b'H'],
        qt::KEY_END => vec![0x1b, intro, b'F'],
        qt::KEY_INSERT => b"\x1b[2~".to_vec(),
        qt::KEY_DELETE => b"\x1b[3~".to_vec(),
        qt::KEY_PAGEUP => b"\x1b[5~".to_vec(),
        qt::KEY_PAGEDOWN => b"\x1b[6~".to_vec(),
        key if (qt::KEY_F1..=qt::KEY_F1 + 11).contains(&key) => function_key(key - qt::KEY_F1),
        _ if ctrl => ctrl_bytes(qt_key, text),
        _ => text.as_bytes().to_vec(),
    };

    // Alt sends the key sequence prefixed with ESC (meta-sends-escape).
    if alt && !out.is_empty() && out[0] != 0x1b {
        out.insert(0, 0x1b);
    }
    out
}

/// Whether the event's text begins with a printable character rather than a
/// C0 control code or DEL.
fn starts_with_printable(text: &str) -> bool {
    match text.as_bytes().first() {
        Some(&byte) => byte >= 0x20 && byte != 0x7f,
        None => false,
    }
}

/// F1-F12, as sent by xterm.
fn function_key(offset: i32) -> Vec<u8> {
    match offset {
        0 => b"\x1bOP".to_vec(),
        1 => b"\x1bOQ".to_vec(),
        2 => b"\x1bOR".to_vec(),
        3 => b"\x1bOS".to_vec(),
        4 => b"\x1b[15~".to_vec(),
        5 => b"\x1b[17~".to_vec(),
        6 => b"\x1b[18~".to_vec(),
        7 => b"\x1b[19~".to_vec(),
        8 => b"\x1b[20~".to_vec(),
        9 => b"\x1b[21~".to_vec(),
        10 => b"\x1b[23~".to_vec(),
        _ => b"\x1b[24~".to_vec(),
    }
}

/// Ctrl + an ASCII key, folded into a C0 control code.
fn ctrl_bytes(qt_key: i32, text: &str) -> Vec<u8> {
    if let Some(ch) = u8::try_from(qt_key).ok().map(|b| b as char) {
        let upper = ch.to_ascii_uppercase();
        // Ctrl+A..Ctrl+Z and Ctrl+@ [ \ ] ^ _ all mask off the top bits.
        if upper.is_ascii_uppercase() || matches!(upper, '@' | '[' | '\\' | ']' | '^' | '_') {
            return vec![(upper as u8) & 0x1f];
        }
        match upper {
            ' ' => return vec![0x00],
            '?' => return vec![0x7f],
            _ => {}
        }
    }
    text.as_bytes().to_vec()
}

/// Bare modifier and lock keys produce no PTY bytes.
fn is_modifier_key(qt_key: i32) -> bool {
    const KEY_SHIFT: i32 = 0x0100_0020;
    const KEY_SCROLLLOCK: i32 = 0x0100_0026;
    const KEY_SUPER_L: i32 = 0x0100_0053;
    const KEY_SUPER_R: i32 = 0x0100_0054;
    const KEY_ALTGR: i32 = 0x0100_1103;
    (KEY_SHIFT..=KEY_SCROLLLOCK).contains(&qt_key)
        || qt_key == KEY_SUPER_L
        || qt_key == KEY_SUPER_R
        || qt_key == KEY_ALTGR
}
