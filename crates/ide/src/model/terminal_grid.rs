//! Terminal emulation for agent panes.
//!
//! A [`TerminalGrid`] wraps `alacritty_terminal` and exposes exactly what a Qt
//! widget needs to paint: the visible rows as text plus attribute spans, the
//! cursor position, scrollback control, and the window title. No Qt types are
//! used here; the Qt key and modifier values arrive as plain integers (see the
//! [`qt`] module) so that the mapping to PTY bytes stays testable in pure Rust.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use serde::Serialize;

/// A colour as the emulator spells one. Re-exported because [`Appearance`] is
/// made of them and the callers that build one live in Qt land.
pub use alacritty_terminal::vte::ansi::Rgb;

/// Lines of scrollback kept per terminal.
const SCROLLBACK_LINES: usize = 10_000;

/// The foreground and background a terminal reports when nobody has told it
/// what the widget paints with: the traditional pair. Overwritten by
/// [`TerminalGrid::set_appearance`] as soon as a widget exists, so these are
/// what a grid built by a test answers with.
const DEFAULT_FG: Rgb = Rgb {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};
const DEFAULT_BG: Rgb = Rgb { r: 0, g: 0, b: 0 };

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
    /// Whether the user has this run selected. Painted by swapping the two
    /// colours, which is what a selection has looked like on a terminal since
    /// before any of them had a colour to swap.
    pub selected: bool,
}

/// One visible row: its text plus the attribute spans covering it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub text: String,
    pub spans: Vec<Span>,
}

/// An answer the terminal owes the program, waiting for the grid to spell it.
///
/// The emulator hands three kinds of reply to its listener. Two of them arrive
/// as a formatter rather than as text, because only the frontend knows what to
/// put in them -- which colour a palette entry holds, how big a cell is in
/// pixels -- so they are resolved in [`TerminalGrid::take_replies`], where the
/// terminal and its appearance are both in reach.
enum Reply {
    /// Ready-made bytes: a cursor-position report, a device-attributes answer.
    Bytes(String),
    /// A colour the program asked for, by palette index.
    Color(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send>),
    /// The size of the text area, in cells and in pixels.
    Size(Arc<dyn Fn(WindowSize) -> String + Sync + Send>),
}

/// Captures the terminal events we care about: the title, and every answer the
/// program is waiting for.
#[derive(Clone, Default)]
struct Listener {
    title: Arc<Mutex<Option<String>>>,
    /// Replies in the order the program asked for them. Drained by
    /// [`TerminalGrid::take_replies`] and written to the PTY by the session.
    replies: Arc<Mutex<Vec<Reply>>>,
}

impl Listener {
    fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push(reply);
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => *self.title.lock().unwrap() = Some(title),
            TermEvent::ResetTitle => *self.title.lock().unwrap() = None,
            // A query, and the program is blocked until it is answered. This
            // is not decoration: `gh auth login` asks where the cursor is
            // (`ESC [ 6 n`) before every one of its yes/no prompts and reads
            // nothing until the report comes back, so a terminal that drops
            // these looks to the user like a question that will not take an
            // answer.
            TermEvent::PtyWrite(text) => self.push(Reply::Bytes(text)),
            TermEvent::ColorRequest(index, format) => self.push(Reply::Color(index, format)),
            TermEvent::TextAreaSizeRequest(format) => self.push(Reply::Size(format)),
            // `ClipboardLoad` is OSC 52 asking to *read* the clipboard, and it
            // is deliberately left unanswered: the clipboard belongs to the
            // person at the keyboard, and a program in a workspace -- or a
            // build script that printed the sequence -- has no business being
            // handed whatever they last copied. `ClipboardStore` is the same
            // boundary from the other side. Both are dropped rather than
            // refused, which is what a terminal without clipboard access
            // looks like.
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
    /// What the widget paints a default cell with, and how big one cell is in
    /// pixels. Not used for painting -- the widget has its own palette -- but a
    /// program may ask the terminal any of it, and an answer of white on black
    /// in a light pane is how a CLI ends up picking a theme nobody can read.
    appearance: Appearance,
}

/// What a program is told when it asks what the terminal looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appearance {
    pub fg: Rgb,
    pub bg: Rgb,
    /// One cell in pixels. Zero means "not known yet", which is what a grid
    /// with no widget in front of it reports.
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            cell_width: 0,
            cell_height: 0,
        }
    }
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
            appearance: Appearance::default(),
        }
    }

    /// Tells the terminal what the widget in front of it paints with, so that
    /// a program asking about colours or pixel sizes is told the truth.
    pub fn set_appearance(&mut self, appearance: Appearance) {
        self.appearance = appearance;
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
        // Resolved once for the screen rather than per cell: the range is in
        // the same buffer coordinates the rows below are read in, so a
        // selection made before a scroll stays on the text it was made on.
        let selection = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term));
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
                    let selected = selection
                        .is_some_and(|range| range.contains(Point::new(line, Column(col))));
                    match spans.last_mut() {
                        Some(last)
                            if last.fg == fg
                                && last.bg == bg
                                && last.bold == bold
                                && last.italic == italic
                                && last.underline == underline
                                && last.inverse == inverse
                                && last.selected == selected =>
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
                            selected,
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

    /// Starts a selection at the cell under the pointer.
    ///
    /// `right_half` says which side of that cell the pointer is on, which is
    /// what decides whether the cell itself is in or out once the drag goes
    /// the other way. `word` picks out the word under the pointer instead --
    /// what a double-click does -- and keeps expanding by words as the drag
    /// continues.
    pub fn begin_selection(&mut self, col: u16, row: u16, right_half: bool, word: bool) {
        let kind = if word {
            SelectionType::Semantic
        } else {
            SelectionType::Simple
        };
        let point = self.point_at(col, row);
        self.term.selection = Some(Selection::new(kind, point, side(right_half)));
    }

    /// Moves the loose end of the selection to the cell under the pointer.
    /// Nothing happens without a selection to extend.
    pub fn extend_selection(&mut self, col: u16, row: u16, right_half: bool) {
        let point = self.point_at(col, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side(right_half));
        }
    }

    /// Drops the selection, so nothing is highlighted and there is nothing to
    /// copy.
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The selected text, empty when there is no selection.
    ///
    /// The terminal's own rendering of it: trailing blanks on each line are
    /// dropped and a line that only wrapped is joined to the next, so what is
    /// copied is what was written rather than the shape of the screen it
    /// landed on.
    pub fn selection_text(&self) -> String {
        self.term.selection_to_string().unwrap_or_default()
    }

    /// Whether anything is selected. Cheaper than asking for the text, which
    /// is what a menu about to be shown wants to know.
    pub fn has_selection(&self) -> bool {
        self.term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term))
            .is_some()
    }

    /// The buffer point under a cell of the visible screen. Columns past the
    /// end of the row land on its last cell, which is what a drag off the right
    /// edge means.
    fn point_at(&self, col: u16, row: u16) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let line = Line(i32::from(row) - offset);
        let column = Column(usize::from(col).min(self.cols.saturating_sub(1).into()));
        Point::new(line, column)
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

    /// The bytes the terminal owes the program, and forgets once handed over.
    ///
    /// Every escape sequence that is a *question* -- where is the cursor, what
    /// are you, how big is your window, what colour is entry 11 -- is parsed by
    /// the emulator and turned into one of these. They have to reach the PTY in
    /// the order they were asked, which is the order they are queued in, and
    /// they have to reach it promptly: the program that asked is not reading
    /// its input until the answer arrives.
    pub fn take_replies(&mut self) -> Vec<u8> {
        let queued: Vec<Reply> = std::mem::take(&mut *self.listener.replies.lock().unwrap());
        let mut out = Vec::new();
        for reply in queued {
            let text = match reply {
                Reply::Bytes(text) => text,
                Reply::Color(index, format) => format(self.palette_rgb(index)),
                Reply::Size(format) => format(WindowSize {
                    num_lines: self.rows,
                    num_cols: self.cols,
                    cell_width: self.appearance.cell_width,
                    cell_height: self.appearance.cell_height,
                }),
            };
            out.extend_from_slice(text.as_bytes());
        }
        out
    }

    /// The colour behind a palette index, for a program that asked for it.
    ///
    /// A colour the program itself set with OSC 4 wins, because it did set it.
    /// Otherwise the xterm defaults answer for the 256-colour palette and the
    /// widget's own pair answers for the three that have no fixed value:
    /// foreground, background and cursor.
    fn palette_rgb(&self, index: usize) -> Rgb {
        const FOREGROUND: usize = NamedColor::Foreground as usize;
        const BACKGROUND: usize = NamedColor::Background as usize;
        const CURSOR: usize = NamedColor::Cursor as usize;
        const DIM_BLACK: usize = NamedColor::DimBlack as usize;
        const DIM_WHITE: usize = NamedColor::DimWhite as usize;
        const BRIGHT_FOREGROUND: usize = NamedColor::BrightForeground as usize;
        const DIM_FOREGROUND: usize = NamedColor::DimForeground as usize;
        // Past the end of the terminal's own table, which would panic. The
        // emulator only ever asks about entries it has, so this is a guard and
        // not a case.
        if index > DIM_FOREGROUND {
            return self.appearance.fg;
        }
        if let Some(rgb) = self.term.colors()[index] {
            return rgb;
        }
        match index {
            0..=255 => ansi_rgb(index),
            BACKGROUND => self.appearance.bg,
            FOREGROUND | CURSOR | BRIGHT_FOREGROUND | DIM_FOREGROUND => self.appearance.fg,
            DIM_BLACK..=DIM_WHITE => ansi_rgb(index - DIM_BLACK),
            _ => self.appearance.fg,
        }
    }

    /// Whether the application asked for bracketed paste (DECSET 2004).
    ///
    /// A program that asks for it wants to be told where a paste begins and
    /// ends, so that it can take the whole block as data rather than as the
    /// keystrokes it looks like. Claude Code's prompt and every modern shell
    /// ask for it; [`paste_bytes`] is what answers.
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }
}

/// Which half of a cell the pointer is on, as `alacritty_terminal` names it.
fn side(right_half: bool) -> Side {
    if right_half {
        Side::Right
    } else {
        Side::Left
    }
}

/// The bytes a paste of `text` sends to the PTY.
///
/// Two things happen to the text on the way. Line breaks -- in either
/// convention -- become the carriage return the Enter key sends, because that
/// is the only thing a program reading a terminal understands a new line to
/// be; and every other control character is dropped. The second is what keeps
/// a paste data: the clipboard is filled by whatever the user last copied, and
/// an escape sequence in it would otherwise be obeyed by the terminal rather
/// than read by the program -- including, in `bracketed` mode, a forged
/// `ESC [ 201 ~` that ends the bracket early and hands the rest of the paste
/// to the program as keystrokes.
///
/// Tab survives because it is a character a pasted line legitimately contains.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let mut body = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // CRLF is one line break, not two.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                body.push('\r');
            }
            '\n' => body.push('\r'),
            '\t' => body.push('\t'),
            c if c.is_control() => {}
            c => body.push(c),
        }
    }
    if body.is_empty() {
        return Vec::new();
    }
    if !bracketed {
        return body.into_bytes();
    }
    let mut out = b"\x1b[200~".to_vec();
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
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

/// `#rrggbb` back to a colour, or `None` for anything else.
///
/// The inverse of [`hex`], and the one way a Qt colour reaches this module:
/// the widget names the pane's palette in the same spelling the spans use.
pub fn parse_hex_rgb(text: &str) -> Option<Rgb> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
    Some(Rgb {
        r: byte(0)?,
        g: byte(2)?,
        b: byte(4)?,
    })
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

/// The xterm default 256-colour palette as a hex string, empty past the end.
fn ansi_default(index: usize) -> String {
    if index > 255 {
        return String::new();
    }
    hex(ansi_rgb(index))
}

/// The xterm default 256-colour palette: 16 system colours, a 6x6x6 cube, and
/// a 24-step grey ramp. An index past the end is black, which is what a
/// terminal with nothing to say about a colour says.
fn ansi_rgb(index: usize) -> Rgb {
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
        (0, 0, 0)
    };
    Rgb { r, g, b }
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
