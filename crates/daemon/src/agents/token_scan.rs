//! Picking the long-lived token out of the `claude setup-token` terminal.
//!
//! The CLI prints the token once, in its own full-screen UI, and says it will
//! never show it again. A host setup terminal's output passes through a
//! [`TokenCapture`] on its way to the IDE (see [`crate::pty::OutputTap`]), so the
//! daemon can store the token with [`super::token::write`] without the user
//! having to copy and paste a secret anywhere.
//!
//! What makes this more than a regex is how the token reaches the terminal.
//! Captured from CLI 2.1.263 at 91 columns, it is 108 bytes -- the prefix, then
//! 95 of `[A-Za-z0-9_-]`, ending in `AA` -- printed wrapped: a colour, the first
//! 90 bytes, then a *cursor jump* to the next row with no newline, the last 18
//! bytes, another colour, another jump, and then `Store this token securely`.
//! Stripping every escape and joining glues `Store` onto the token; ending the
//! token at any escape stores a 90-byte prefix. Both would be stored as if they
//! were whole. So [`TokenScanner`] parses escape sequences, treats colours and
//! cursor movement (and `\r`, `\n`, space, for a renderer that wraps with a
//! newline and an indent) as soft -- skipped, the token goes on -- and stops at
//! exactly 108 bytes ending in `AA`. Anything else ends the candidate, which is
//! then thrown away: a token is captured whole or not at all.
//!
//! Nothing here ever logs the token or any part of it.

use crate::pty::OutputTap;
use std::path::PathBuf;
use tracing::{info, warn};

/// Every long-lived token starts with this.
const PREFIX: &[u8] = b"sk-ant-oat01-";

/// The length of a `claude setup-token` token, prefix included. Stricter by
/// design than the store's 40..512 shape check: the store accepts what a user
/// might hand it, while this decides where an unterminated run on a screen
/// full of other text stops.
const TOKEN_LEN: usize = 108;

/// How every token observed so far ends.
const SUFFIX: &[u8] = b"AA";

/// Where the escape-sequence parser is. Only the final byte of a sequence is
/// ever needed, so nothing of a sequence is buffered and a sequence split
/// across two reads costs nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Esc {
    /// Printed text.
    Ground,
    /// After `ESC`.
    Escape,
    /// After `ESC` and one or more intermediate bytes (`ESC ( B` and the like).
    Intermediate,
    /// Inside `ESC [ …`, waiting for the final byte.
    Csi,
    /// Inside a string sequence (`ESC ]`, `ESC P`, `ESC _`, `ESC ^`, `ESC X`),
    /// which ends with BEL or `ESC \`.
    Str,
    /// After an `ESC` inside a string sequence.
    StrEscape,
}

/// Recognises the token in a setup terminal's output. Feed it every chunk the
/// terminal produces, in order; it survives any chunk boundary.
pub struct TokenScanner {
    esc: Esc,
    /// How many bytes of [`PREFIX`] the printed text has matched so far, while
    /// idle.
    matched: usize,
    /// The token being collected, prefix included, once the prefix has been
    /// seen. Never longer than [`TOKEN_LEN`], which is what bounds this scanner's
    /// memory whatever the terminal prints.
    candidate: Option<Vec<u8>>,
    /// Set once a token has been returned; the scanner is done for good, so a
    /// redraw of the same screen is never stored a second time.
    found: bool,
}

impl Default for TokenScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenScanner {
    pub fn new() -> Self {
        Self {
            esc: Esc::Ground,
            matched: 0,
            candidate: None,
            found: false,
        }
    }

    /// Whether a token has been returned by [`push`](Self::push).
    pub fn found(&self) -> bool {
        self.found
    }

    /// Scans the next chunk of output. Returns the token the first time a whole
    /// one has been seen, and `None` otherwise -- including every call after
    /// that first one.
    pub fn push(&mut self, bytes: &[u8]) -> Option<String> {
        for &b in bytes {
            if self.found {
                return None;
            }
            if let Some(token) = self.step(b) {
                self.found = true;
                return Some(token);
            }
        }
        None
    }

    /// One byte of output: the escape parser first, then text.
    fn step(&mut self, b: u8) -> Option<String> {
        const ESC: u8 = 0x1b;
        const BEL: u8 = 0x07;
        match self.esc {
            Esc::Ground if b == ESC => self.esc = Esc::Escape,
            Esc::Ground => return self.text(b),
            Esc::Escape => {
                self.esc = match b {
                    b'[' => Esc::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => Esc::Str,
                    ESC => Esc::Escape,
                    0x20..=0x2f => Esc::Intermediate,
                    // A two-byte sequence (`ESC 7`, `ESC =`, …).
                    _ => {
                        self.hard();
                        Esc::Ground
                    }
                };
            }
            Esc::Intermediate => match b {
                0x20..=0x2f => {}
                _ => {
                    self.hard();
                    self.esc = Esc::Ground;
                }
            },
            Esc::Csi => match b {
                // Parameter and intermediate bytes.
                0x20..=0x3f => {}
                // The final byte. A colour or a cursor movement is how the CLI
                // styles and wraps the token, so it leaves a token going on.
                b'm' | b'H' | b'f' | b'A' | b'B' | b'C' | b'D' | b'G' | b'd' => {
                    self.esc = Esc::Ground
                }
                0x40..=0x7e => {
                    self.hard();
                    self.esc = Esc::Ground;
                }
                // Not a well-formed sequence: abandon it and read the byte as if
                // the sequence had never begun.
                _ => {
                    self.hard();
                    self.esc = Esc::Ground;
                    return self.step(b);
                }
            },
            Esc::Str => match b {
                BEL => {
                    self.hard();
                    self.esc = Esc::Ground;
                }
                ESC => self.esc = Esc::StrEscape,
                _ => {}
            },
            Esc::StrEscape => {
                self.hard();
                // `ESC \` ends the string; any other `ESC x` both ends it and
                // begins a new sequence.
                self.esc = Esc::Escape;
                if b == b'\\' {
                    self.esc = Esc::Ground;
                } else {
                    return self.step(b);
                }
            }
        }
        None
    }

    /// One byte of printed text.
    fn text(&mut self, b: u8) -> Option<String> {
        let Some(candidate) = self.candidate.as_mut() else {
            // Idle: look for the prefix. `s` occurs in it only at the start, so
            // a mismatch falls back to "just saw `s`" or to nothing.
            if b == PREFIX[self.matched] {
                self.matched += 1;
                if self.matched == PREFIX.len() {
                    self.matched = 0;
                    self.candidate = Some(PREFIX.to_vec());
                }
            } else {
                self.matched = usize::from(b == PREFIX[0]);
            }
            return None;
        };
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => {
                candidate.push(b);
                if candidate.len() < TOKEN_LEN {
                    return None;
                }
                let whole = self.candidate.take()?;
                // `from_utf8` cannot fail on this alphabet, and the store's own
                // shape check is applied too, so the scanner never hands on
                // something `token::write` would refuse.
                String::from_utf8(whole)
                    .ok()
                    .filter(|t| t.as_bytes().ends_with(SUFFIX))
                    .filter(|t| super::token::is_token_shaped(t))
            }
            // A wrap the renderer made with a newline and an indent.
            b'\r' | b'\n' | b' ' => None,
            _ => {
                self.hard();
                None
            }
        }
    }

    /// Something that cannot be inside a token: whatever was being collected is
    /// thrown away, and so is any partial prefix.
    fn hard(&mut self) {
        self.candidate = None;
        self.matched = 0;
    }
}

/// A [`TokenScanner`] tied to the file the token belongs in: the
/// `claude setup-token` terminal's [`OutputTap`](crate::pty::OutputTap).
///
/// Dropping it -- which [`finish`](OutputTap::finish) does, ahead of the
/// terminal's exit -- logs once if no token was ever seen, so a login that
/// finished without the daemon storing anything (the user closed the terminal
/// early, or the CLI changed how it prints the token) is visible in the log
/// rather than only as agents that later fail to authenticate.
pub struct TokenCapture {
    scanner: TokenScanner,
    path: PathBuf,
    /// The store started by [`feed`](OutputTap::feed), until `finish` has
    /// waited for it. There is at most one: the scanner returns a token once.
    pending: Option<tokio::task::JoinHandle<()>>,
}

impl TokenCapture {
    pub fn new(path: PathBuf) -> Self {
        Self {
            scanner: TokenScanner::new(),
            path,
            pending: None,
        }
    }
}

impl OutputTap for TokenCapture {
    /// Scans `bytes`, and when they complete the token, starts storing it at
    /// the capture's path.
    ///
    /// The store happens on the blocking pool: the caller is the terminal's
    /// pump task on a tokio worker, and [`super::token::write`] ends in an
    /// `fsync`, which on a slow disk would stall every other task on that
    /// worker. [`finish`](OutputTap::finish) is what waits for it. Must be
    /// called from within a tokio runtime.
    fn feed(&mut self, bytes: &[u8]) {
        let Some(token) = self.scanner.push(bytes) else {
            return;
        };
        let path = self.path.clone();
        self.pending = Some(tokio::task::spawn_blocking(move || {
            match super::token::write(&path, &token) {
                Ok(()) => info!(path = %path.display(), "long-lived claude token stored"),
                Err(e) => warn!(path = %path.display(), "long-lived claude token not stored: {e}"),
            }
        }));
    }

    /// Waits for the store `feed` started, if any. `claude setup-token` exits
    /// straight after printing the token, so without this the IDE's re-check
    /// on `pty.exit` could run before the file exists and still report the
    /// login as the plain one. The pump bounds the wait.
    fn finish(mut self: Box<Self>) -> futures::future::BoxFuture<'static, ()> {
        let pending = self.pending.take();
        Box::pin(async move {
            if let Some(write) = pending {
                // A panic in the write is the only error, and it has nothing
                // to add to what the write itself logs.
                let _ = write.await;
            }
            // `self` drops here, after the write: the missed-token warning in
            // `Drop` lands ahead of the exit too.
            drop(self);
        })
    }
}

impl Drop for TokenCapture {
    fn drop(&mut self) {
        if !self.scanner.found() {
            warn!("the setup-token terminal ended without a token being captured");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/setup-token.raw");
    const EXPECTED: &str = include_str!("../../tests/fixtures/setup-token.expected");

    /// The cursor jump that wraps the token onto its second row in the capture.
    const JUMP: &[u8] = b"\x1b[27;2H";

    /// Every token `push` returned, over `chunks`.
    fn scan<'a>(s: &mut TokenScanner, chunks: impl IntoIterator<Item = &'a [u8]>) -> Vec<String> {
        chunks.into_iter().filter_map(|c| s.push(c)).collect()
    }

    fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        let at = haystack
            .windows(from.len())
            .position(|w| w == from)
            .expect("pattern in fixture");
        [&haystack[..at], to, &haystack[at + from.len()..]].concat()
    }

    /// Token-alphabet filler of length `n`, ending in `end`.
    fn body(n: usize, end: &str) -> String {
        let fill: String = "Ab9_-xY".chars().cycle().take(n - end.len()).collect();
        format!("{fill}{end}")
    }

    #[test]
    fn the_fixture_in_one_push_yields_the_token() {
        assert_eq!(EXPECTED.len(), TOKEN_LEN);
        let mut s = TokenScanner::new();
        assert_eq!(s.push(FIXTURE).as_deref(), Some(EXPECTED));
        assert!(s.found());
    }

    #[test]
    fn the_fixture_in_small_chunks_yields_the_token_exactly_once() {
        for size in [1, 7] {
            let mut s = TokenScanner::new();
            assert_eq!(
                scan(&mut s, FIXTURE.chunks(size)),
                [EXPECTED],
                "chunks of {size}"
            );
        }
    }

    #[test]
    fn a_redraw_of_the_same_screen_is_not_a_second_token() {
        let mut s = TokenScanner::new();
        assert_eq!(s.push(FIXTURE).as_deref(), Some(EXPECTED));
        assert_eq!(s.push(FIXTURE), None);
    }

    #[test]
    fn a_token_wrapped_with_a_newline_and_indent_is_joined() {
        let wrapped = replace(FIXTURE, JUMP, b"\r\n ");
        let mut s = TokenScanner::new();
        assert_eq!(s.push(&wrapped).as_deref(), Some(EXPECTED));
    }

    #[test]
    fn a_truncated_token_is_never_returned() {
        let prefix = &EXPECTED[..90];
        let mut s = TokenScanner::new();
        assert_eq!(s.push(format!(" {prefix}.").as_bytes()), None);
        // And the rest arriving later does not revive it.
        assert_eq!(s.push(&EXPECTED.as_bytes()[90..]), None);
        assert!(!s.found());
    }

    #[test]
    fn a_token_is_not_returned_before_its_last_byte_arrives() {
        let mut s = TokenScanner::new();
        assert_eq!(s.push(format!(" {}", &EXPECTED[..107]).as_bytes()), None);
        assert_eq!(
            s.push(&EXPECTED.as_bytes()[107..]).as_deref(),
            Some(EXPECTED)
        );
    }

    #[test]
    fn a_full_length_run_not_ending_in_aa_is_refused() {
        let t = format!("sk-ant-oat01-{}", body(TOKEN_LEN - PREFIX.len(), "AB"));
        assert_eq!(t.len(), TOKEN_LEN);
        let mut s = TokenScanner::new();
        assert_eq!(s.push(format!(" {t}\r\n").as_bytes()), None);
    }

    #[test]
    fn a_short_run_of_the_token_alphabet_is_refused() {
        let t = format!("sk-ant-oat01-{}", body(40, "AA"));
        let mut s = TokenScanner::new();
        assert_eq!(
            s.push(format!(" \x1b[38;2;1;2;3m{t}\x1b[m\r\nStore").as_bytes()),
            None
        );
    }

    #[test]
    fn ordinary_login_output_is_not_a_token() {
        let login = b"\x1b[38;2;102;102;102mhttps://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d196\x1b[m\r\n\x1b[38;2;102;102;102m2f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=user%3Ainference\x1b[m\r\n Paste code here if prompted >";
        let mut s = TokenScanner::new();
        assert_eq!(s.push(login), None);
        assert!(!s.found());
    }

    #[test]
    fn a_prefix_inside_an_escape_sequence_is_not_text() {
        let t = format!("sk-ant-oat01-{}", body(TOKEN_LEN - PREFIX.len(), "AA"));
        let mut s = TokenScanner::new();
        // A window title is not printed text.
        assert_eq!(s.push(format!("\x1b]0;{t}\x07").as_bytes()), None);
        // The same run printed is.
        assert_eq!(
            s.push(format!(" {t}").as_bytes()).as_deref(),
            Some(t.as_str())
        );
    }

    #[tokio::test]
    async fn capture_stores_the_token_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = super::super::token::token_path(dir.path());
        let mut c = TokenCapture::new(path.clone());
        for chunk in FIXTURE.chunks(7) {
            c.feed(chunk);
        }
        // A redraw starts no second write.
        c.feed(FIXTURE);
        Box::new(c).finish().await;
        assert_eq!(super::super::token::read(&path).as_deref(), Some(EXPECTED));
    }

    #[tokio::test]
    async fn the_token_is_on_disk_by_the_time_finish_resolves() {
        // Fed in one go and finished at once, as when `claude setup-token`
        // prints the token and exits in the same breath: nothing between the
        // two gives the blocking write time to land on its own.
        let dir = tempfile::tempdir().unwrap();
        let path = super::super::token::token_path(dir.path());
        let mut c = TokenCapture::new(path.clone());
        c.feed(FIXTURE);
        Box::new(c).finish().await;
        assert_eq!(super::super::token::read(&path).as_deref(), Some(EXPECTED));
    }
}
