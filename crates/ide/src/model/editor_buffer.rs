//! The text of one open file and its highlight spans. Pure Rust: the widget
//! sends edits in and reads spans per line out.
//!
//! Highlighting re-runs over the whole buffer after an edit, lazily on the
//! next span query. tree-sitter parses a 4 MiB file in tens of milliseconds
//! and typical files in under one, so incremental tree edits are deferred
//! until profiling asks for them (IDE spec section 6 describes the
//! incremental form).

use crate::highlight::languages::Language;
use crate::highlight::theme::{StyleId, Theme};
use ropey::Rope;
use tree_sitter_highlight::{HighlightEvent, Highlighter};

/// Every character `ropey` treats as ending a line. Trailing occurrences are
/// stripped from [`EditorBuffer::line`] so that no span ever covers them.
const LINE_BREAKS: [char; 7] = [
    '\n', '\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}',
];

/// A run of characters on one line that share a style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Column of the first character, in chars from the start of the line.
    pub start: usize,
    /// Length in chars.
    pub len: usize,
    pub style: StyleId,
}

/// One open file: its text, its language, and its per-line highlight spans.
pub struct EditorBuffer {
    rope: Rope,
    language: Option<Language>,
    /// Spans per line; `None` until computed or after the text is replaced.
    spans: Option<Vec<Vec<Span>>>,
}

impl EditorBuffer {
    pub fn new(path: &str, text: &str) -> EditorBuffer {
        EditorBuffer {
            rope: Rope::from_str(text),
            language: Language::from_path(path),
            spans: None,
        }
    }

    pub fn language(&self) -> Option<Language> {
        self.language
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Lines as the editor counts them: a trailing newline yields a final
    /// empty line.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Line `n` without its line terminator; empty past the end of the file.
    pub fn line(&self, n: usize) -> String {
        if n >= self.rope.len_lines() {
            return String::new();
        }
        let line = self.rope.line(n);
        line.slice(..self.visible_len(n)).to_string()
    }

    /// The char offset for a UTF-16 code-unit offset, as Qt reports positions.
    pub fn utf16_to_char(&self, utf16: usize) -> usize {
        self.rope
            .utf16_cu_to_char(utf16.min(self.rope.len_utf16_cu()))
    }

    /// Applies one edit in char units and returns the inclusive line range
    /// whose spans differ from before, so a view can re-highlight exactly
    /// those lines. Always includes the edited line.
    pub fn apply_edit(
        &mut self,
        char_pos: usize,
        removed_chars: usize,
        inserted: &str,
    ) -> (usize, usize) {
        let before = self.take_spans();
        let pos = char_pos.min(self.rope.len_chars());
        let end = (pos + removed_chars).min(self.rope.len_chars());
        self.rope.remove(pos..end);
        self.rope.insert(pos, inserted);
        let edited_line = self.rope.char_to_line(pos);
        let after = self.compute_spans();
        let (from, to) = changed_range(&before, &after, edited_line);
        self.spans = Some(after);
        (from, to)
    }

    /// Replaces the whole text, discarding the cached spans.
    pub fn replace_all(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
        self.spans = None;
    }

    /// The spans on line `n`, highlighting the buffer first if needed.
    pub fn spans_for_line(&mut self, n: usize) -> &[Span] {
        if self.spans.is_none() {
            self.spans = Some(self.compute_spans());
        }
        self.spans
            .as_ref()
            .expect("spans computed above")
            .get(n)
            .map_or(&[][..], Vec::as_slice)
    }

    /// The spans on line `n` as the JSON the widget consumes:
    /// `[{"s":0,"l":2,"fg":"#a626a4","b":true,"i":false}, ...]`.
    pub fn spans_json(&mut self, n: usize, theme: &Theme) -> String {
        let items: Vec<serde_json::Value> = self
            .spans_for_line(n)
            .iter()
            .map(|s| {
                let st = theme.style(s.style);
                serde_json::json!({"s": s.start, "l": s.len, "fg": st.fg, "b": st.bold, "i": st.italic})
            })
            .collect();
        serde_json::Value::Array(items).to_string()
    }

    /// The cached spans, computing them first when the cache is cold.
    fn take_spans(&mut self) -> Vec<Vec<Span>> {
        match self.spans.take() {
            Some(s) => s,
            None => self.compute_spans(),
        }
    }

    /// Chars on line `n` before its line terminator.
    fn visible_len(&self, n: usize) -> usize {
        let line = self.rope.line(n);
        let mut len = line.len_chars();
        while len > 0 && LINE_BREAKS.contains(&line.char(len - 1)) {
            len -= 1;
        }
        len
    }

    /// Full highlight pass: byte ranges from tree-sitter become per-line char
    /// spans, split at line boundaries.
    fn compute_spans(&self) -> Vec<Vec<Span>> {
        let mut lines: Vec<Vec<Span>> = vec![Vec::new(); self.rope.len_lines()];
        let Some(lang) = self.language else {
            return lines;
        };
        let text = self.rope.to_string();
        let mut highlighter = Highlighter::new();
        let Ok(events) = highlighter.highlight(lang.config(), text.as_bytes(), None, |_| None)
        else {
            return lines;
        };
        let mut stack: Vec<StyleId> = Vec::new();
        for ev in events.flatten() {
            match ev {
                HighlightEvent::HighlightStart(h) => {
                    stack.push(StyleId::from_index(h.0).unwrap_or(StyleId::Punctuation));
                }
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let Some(&style) = stack.last() else { continue };
                    let start_c = self.rope.byte_to_char(start);
                    let end_c = self.rope.byte_to_char(end);
                    let mut c = start_c;
                    while c < end_c {
                        let line = self.rope.char_to_line(c);
                        let line_start = self.rope.line_to_char(line);
                        let line_end = if line + 1 < self.rope.len_lines() {
                            self.rope.line_to_char(line + 1)
                        } else {
                            self.rope.len_chars()
                        };
                        let stop = end_c.min(line_end);
                        // Exclude the line terminator so spans never cover it.
                        let visible_end = stop.min(line_start + self.visible_len(line));
                        if visible_end > c {
                            lines[line].push(Span {
                                start: c - line_start,
                                len: visible_end - c,
                                style,
                            });
                        }
                        c = stop.max(c + 1);
                    }
                }
            }
        }
        lines
    }
}

/// The inclusive range of lines whose spans differ, widened to always cover
/// the edited line.
fn changed_range(before: &[Vec<Span>], after: &[Vec<Span>], edited_line: usize) -> (usize, usize) {
    let mut from = edited_line;
    let mut to = edited_line;
    let n = before.len().max(after.len());
    for i in 0..n {
        let same = before.get(i).map_or(&[][..], Vec::as_slice)
            == after.get(i).map_or(&[][..], Vec::as_slice);
        if !same {
            from = from.min(i);
            to = to.max(i);
        }
    }
    (from, to)
}
