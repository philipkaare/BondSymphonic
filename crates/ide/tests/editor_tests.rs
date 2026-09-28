use bondsymphonic_ide::highlight::languages::Language;
use bondsymphonic_ide::highlight::theme::{StyleId, Theme, STYLE_NAMES};
use bondsymphonic_ide::model::editor_buffer::{EditorBuffer, Span};

#[test]
fn language_is_detected_from_the_extension() {
    assert_eq!(Language::from_path("src/main.rs"), Some(Language::Rust));
    assert_eq!(Language::from_path("web/app.tsx"), Some(Language::Tsx));
    assert_eq!(Language::from_path("a/b/c.yml"), Some(Language::Yaml));
    assert_eq!(Language::from_path("include/x.hpp"), Some(Language::Cpp));
    assert_eq!(Language::from_path("Makefile"), None);
    assert_eq!(Language::from_path("notes.unknownext"), None);
}

#[test]
fn rust_keywords_strings_and_comments_get_spans_per_line() {
    let src = "fn main() {\n    let s = \"hi\"; // greet\n}\n";
    let mut buf = EditorBuffer::new("x.rs", src);
    assert_eq!(buf.language(), Some(Language::Rust));
    assert_eq!(buf.line_count(), 4);
    let l0 = buf.spans_for_line(0).to_vec();
    assert!(
        l0.contains(&Span {
            start: 0,
            len: 2,
            style: StyleId::Keyword
        }),
        "{l0:?}"
    );
    let l1 = buf.spans_for_line(1).to_vec();
    assert!(
        l1.iter()
            .any(|s| s.style == StyleId::Keyword && s.start == 4 && s.len == 3),
        "let: {l1:?}"
    );
    assert!(
        l1.iter()
            .any(|s| s.style == StyleId::String && s.start == 12 && s.len == 4),
        "\"hi\": {l1:?}"
    );
    assert!(
        l1.iter()
            .any(|s| s.style == StyleId::Comment && s.start == 18),
        "comment: {l1:?}"
    );
    assert!(buf.spans_for_line(3).is_empty());
}

#[test]
fn multi_line_comment_spans_are_split_per_line() {
    let src = "/* a\n b */ fn f() {}\n";
    let mut buf = EditorBuffer::new("x.rs", src);
    let l0 = buf.spans_for_line(0).to_vec();
    let l1 = buf.spans_for_line(1).to_vec();
    assert_eq!(
        l0,
        vec![Span {
            start: 0,
            len: 4,
            style: StyleId::Comment
        }]
    );
    assert_eq!(
        l1[0],
        Span {
            start: 0,
            len: 5,
            style: StyleId::Comment
        }
    );
    assert!(l1
        .iter()
        .any(|s| s.style == StyleId::Keyword && s.start == 6));
}

#[test]
fn an_edit_keeps_spans_identical_to_a_fresh_parse() {
    let mut buf = EditorBuffer::new("x.rs", "fn a() {}\nfn b() {}\n");
    // A view paints spans before the user types, so the cache is warm and the
    // reported range is a real diff rather than the whole buffer.
    let _ = buf.spans_for_line(0);
    // Insert a string literal into the second line: "fn b() { \"x\" }".
    let pos = buf.text().find("b() {}").unwrap() + "b() {".len();
    let (from, to) = buf.apply_edit(pos, 0, " \"x\" ");
    assert_eq!((from, to), (1, 1));
    let fresh = EditorBuffer::new("x.rs", &buf.text());
    let mut fresh = fresh;
    for n in 0..buf.line_count() {
        assert_eq!(buf.spans_for_line(n), fresh.spans_for_line(n), "line {n}");
    }
}

#[test]
fn opening_a_block_comment_reports_every_affected_line() {
    let mut buf = EditorBuffer::new("x.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    let _ = buf.spans_for_line(0); // warm the cache, so this is a real diff
    let (from, to) = buf.apply_edit(0, 0, "/* ");
    assert_eq!(from, 0);
    assert!(
        to >= 2,
        "lines 1 and 2 turned into comment text, got to={to}"
    );
}

#[test]
fn utf16_offsets_map_to_char_offsets() {
    let buf = EditorBuffer::new("x.txt", "a😀b");
    assert_eq!(buf.utf16_to_char(0), 0);
    assert_eq!(buf.utf16_to_char(1), 1);
    assert_eq!(buf.utf16_to_char(3), 2); // after the surrogate pair
}

#[test]
fn unknown_language_has_no_spans_and_spans_json_is_well_formed() {
    let mut buf = EditorBuffer::new("x.unknown", "let x = 1;\n");
    assert!(buf.spans_for_line(0).is_empty());
    assert_eq!(buf.spans_json(0, Theme::light()), "[]");
    let mut rs = EditorBuffer::new("x.rs", "let x = 1;\n");
    let json = rs.spans_json(0, Theme::dark());
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let first = &v.as_array().unwrap()[0];
    assert_eq!(first["s"], 0);
    assert_eq!(first["l"], 3);
    assert!(first["fg"].as_str().unwrap().starts_with('#'));
    assert!(first["b"].is_boolean() && first["i"].is_boolean());
}

/// Every bundled grammar loads, compiles its query, and produces at least one
/// span for a one-line sample of that language.
#[test]
fn every_bundled_language_produces_spans_for_a_sample_line() {
    let samples: [(&str, &str); 15] = [
        ("a.rs", "fn main() {}"),
        ("a.js", "const x = 1;"),
        ("a.ts", "const x: number = 1;"),
        ("a.tsx", "const x = 1;"),
        ("a.py", "def f():\n    return 1"),
        ("a.json", "{\"a\": 1}"),
        ("a.toml", "a = 1"),
        ("a.yaml", "a: 1"),
        ("a.html", "<p>hi</p>"),
        ("a.css", "a { color: red; }"),
        ("a.md", "# Title"),
        ("a.sh", "echo hi"),
        ("a.c", "int main() { return 0; }"),
        ("a.cpp", "int main() { return 0; }"),
        ("a.go", "package main"),
    ];
    for (path, src) in samples {
        let mut buf = EditorBuffer::new(path, src);
        assert!(buf.language().is_some(), "{path} has no language");
        let total: usize = (0..buf.line_count())
            .map(|n| buf.spans_for_line(n).len())
            .sum();
        assert!(total > 0, "{path}: no spans for {src:?}");
    }
}

/// Spans stop at the last visible character: a highlighted region that runs to
/// the end of a line never covers the newline itself.
#[test]
fn spans_never_cover_the_newline() {
    let src = "// one\n// two\n";
    let mut buf = EditorBuffer::new("x.rs", src);
    for n in 0..buf.line_count() {
        let visible = buf.line(n).chars().count();
        for s in buf.spans_for_line(n) {
            assert!(
                s.start + s.len <= visible,
                "line {n}: span {s:?} exceeds {visible} visible chars"
            );
        }
    }
    assert_eq!(
        buf.spans_for_line(0),
        [Span {
            start: 0,
            len: 6,
            style: StyleId::Comment
        }]
    );
}

/// `StyleId`, `STYLE_NAMES` and `StyleId::from_index` encode the same order in
/// three places; `Theme::style` indexes the palette by the discriminant while
/// tree-sitter hands back an index into `STYLE_NAMES`.
#[test]
fn style_ids_and_style_names_stay_in_lockstep() {
    assert_eq!(STYLE_NAMES.len(), 12);
    for i in 0..STYLE_NAMES.len() {
        let id = StyleId::from_index(i).unwrap_or_else(|| panic!("no style at index {i}"));
        assert_eq!(id as usize, i, "{id:?} is not at index {i}");
        // Every id has a palette entry in both themes.
        assert!(Theme::light().style(id).fg.starts_with('#'));
        assert!(Theme::dark().style(id).fg.starts_with('#'));
    }
    assert_eq!(StyleId::from_index(12), None);
    assert_eq!(StyleId::from_index(usize::MAX), None);
}

/// Highlighting can be switched off for files too large to re-highlight on
/// every keystroke, without hiding what language the file is.
#[test]
fn highlighting_can_be_switched_off_without_hiding_the_language() {
    let mut buf = EditorBuffer::new("x.rs", "fn a() {}\nfn b() {}\n");
    assert!(buf.highlighting());
    assert!(!buf.spans_for_line(0).is_empty());

    buf.set_highlighting(false);
    assert!(!buf.highlighting());
    assert_eq!(buf.language(), Some(Language::Rust), "language is kept");
    assert!(buf.spans_for_line(0).is_empty());
    assert_eq!(buf.spans_json(0, Theme::light()), "[]");

    // An edit still applies to the text and reports only the edited line.
    let pos = buf.text().find("b()").unwrap();
    assert_eq!(buf.apply_edit(pos, 0, "big_"), (1, 1));
    assert_eq!(buf.text(), "fn a() {}\nfn big_b() {}\n");
    assert!(buf.spans_for_line(1).is_empty());

    buf.set_highlighting(true);
    assert!(buf
        .spans_for_line(1)
        .iter()
        .any(|s| s.style == StyleId::Keyword));
}

/// With no spans cached there is nothing to diff against, so the first edit
/// reports the whole buffer rather than paying for a second highlight pass.
#[test]
fn an_edit_on_a_cold_cache_reports_the_whole_buffer() {
    let mut buf = EditorBuffer::new("x.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    let last = buf.line_count() - 1;
    assert_eq!(buf.apply_edit(0, 0, "// "), (0, last));
    // Warm now: a second edit on the same line reports just that line.
    assert_eq!(buf.apply_edit(0, 0, "  "), (0, 0));
}

/// Deleting lines must not report a line number the buffer no longer has.
#[test]
fn the_reported_range_never_exceeds_the_last_line() {
    let mut buf = EditorBuffer::new("x.rs", "// a\n// b\n// c\n");
    let _ = buf.spans_for_line(0); // warm the cache
    let len = buf.text().chars().count();
    let (from, to) = buf.apply_edit(0, len, "fn f() {}");
    assert_eq!(buf.line_count(), 1);
    assert_eq!(
        (from, to),
        (0, 0),
        "range must be clamped to the one line left"
    );
}

/// Columns are chars, not bytes: a multibyte comment must not shift the
/// columns of the code that follows it.
#[test]
fn columns_are_char_offsets_after_multibyte_text() {
    let src = "// ünïcode\nlet ünï = \"x\";\n";
    let mut buf = EditorBuffer::new("x.rs", src);
    assert_eq!(
        buf.spans_for_line(0),
        [Span {
            start: 0,
            len: 10,
            style: StyleId::Comment
        }],
        "the comment is 10 chars, not 13 bytes"
    );
    let l1 = buf.spans_for_line(1).to_vec();
    assert!(
        l1.iter()
            .any(|s| s.style == StyleId::Keyword && s.start == 0 && s.len == 3),
        "let: {l1:?}"
    );
    // `"x"` starts at char column 10 even though it starts at byte 13.
    assert!(
        l1.iter()
            .any(|s| s.style == StyleId::String && s.start == 10 && s.len == 3),
        "string: {l1:?}"
    );
}

/// On a CRLF file no span reaches the carriage return, and the rope counts
/// CRLF as one break, the way `QTextDocument` counts blocks.
#[test]
fn crlf_lines_count_once_and_spans_stop_before_the_carriage_return() {
    let mut buf = EditorBuffer::new("x.rs", "// one\r\n// two\r\n");
    assert_eq!(buf.line_count(), 3);
    assert_eq!(buf.line(0), "// one");
    for n in 0..buf.line_count() {
        let visible = buf.line(n).chars().count();
        for s in buf.spans_for_line(n) {
            assert!(
                s.start + s.len <= visible,
                "line {n}: span {s:?} reaches the carriage return"
            );
        }
    }
    assert_eq!(
        buf.spans_for_line(1),
        [Span {
            start: 0,
            len: 6,
            style: StyleId::Comment
        }]
    );
}

/// The rope must break lines where `QTextDocument` breaks blocks: on a
/// newline, and not on a form feed, vertical tab or NEL.
#[test]
fn only_newlines_start_a_new_line() {
    let buf = EditorBuffer::new("x.txt", "a\u{0c}b\u{0b}c\u{85}d\n");
    assert_eq!(buf.line_count(), 2, "one line of text plus the empty tail");
    assert_eq!(buf.line(0), "a\u{0c}b\u{0b}c\u{85}d");

    let crlf = EditorBuffer::new("x.txt", "a\r\nb\r\n");
    assert_eq!(crlf.line_count(), 3);
    assert_eq!(crlf.line(1), "b");
}

// ---------------------------------------------------------------------------
// Review fixes 2026-09-11, task 7: line endings (IQ1), the watch across a
// reconnect (IQ2) and an own save not being an external change (IQ3).
// ---------------------------------------------------------------------------

mod review_fixes_task_7 {
    use bondsymphonic_ide::client::router::EventRouter;
    use bondsymphonic_ide::client::DaemonClient;
    use bondsymphonic_ide::model::editor_buffer::{EditorBuffer, LineEnding};
    use bondsymphonic_ide::qobjects::app_controller::{publish_shared, Shared};
    use bondsymphonic_ide::qobjects::editor_document::{
        content_hash, disk_verdict, watch_file, DiskState, DiskVerdict, ReadOutcome, WatchAction,
        WatchNotice,
    };
    use bondsymphonic_proto::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    /// `QTextDocument` collapses every `\r\n` to one block separator, so the
    /// position it reports for "between c and d" in `ab⏎cd` is 4. A rope
    /// still holding the `\r` puts unit 4 in front of the `c`, one early, and
    /// every edit below line 1 lands one unit early per preceding CRLF.
    /// Uses only the API that existed before the fix, so it fails on
    /// behaviour rather than on a missing name.
    #[test]
    fn the_view_position_after_a_crlf_lands_on_the_right_char() {
        let mut buf = EditorBuffer::new("x.txt", "ab\r\ncd");
        let from = buf.utf16_to_char(4);
        buf.apply_edit(from, 0, "X");
        assert!(
            buf.text().ends_with("cXd"),
            "the X must land between c and d, got {:?}",
            buf.text()
        );
    }

    /// The buffer holds LF text (what the view is given) and remembers the
    /// file's ending, which `text_for_save` puts back.
    #[test]
    fn a_crlf_file_is_edited_at_qt_positions_and_saved_with_its_endings() {
        let mut buf = EditorBuffer::new("x.txt", "ab\r\ncd");
        assert_eq!(buf.line_ending(), LineEnding::Crlf);
        assert_eq!(buf.text(), "ab\ncd", "the view gets LF text");
        assert_eq!(buf.line_count(), 2);
        let from = buf.utf16_to_char(4);
        buf.apply_edit(from, 0, "X");
        assert_eq!(buf.text(), "ab\ncXd");
        assert_eq!(buf.text_for_save(), "ab\r\ncXd");
    }

    /// Three CRLF lines: an edit on line 3 is two units off with the old rope.
    #[test]
    fn an_edit_on_the_third_crlf_line_lands_where_the_view_put_it() {
        let mut buf = EditorBuffer::new("x.txt", "l1\r\nl2\r\nl3");
        // "l1" ¶ "l2" ¶ "l3": the `3` is at unit 7.
        let from = buf.utf16_to_char(7);
        buf.apply_edit(from, 0, "X");
        assert_eq!(buf.text(), "l1\nl2\nlX3");
        assert_eq!(buf.text_for_save(), "l1\r\nl2\r\nlX3");
    }

    /// Backspacing over a line break removes one unit in the view and one
    /// char in the buffer; the saved file must not keep a stray `\r`.
    #[test]
    fn deleting_a_line_break_leaves_no_stray_carriage_return() {
        let mut buf = EditorBuffer::new("x.txt", "ab\r\ncd\r\n");
        let from = buf.utf16_to_char(2);
        let to = buf.utf16_to_char(3);
        buf.apply_edit(from, to - from, "");
        assert_eq!(buf.text(), "abcd\n");
        assert_eq!(buf.text_for_save(), "abcd\r\n");
        assert_eq!(buf.text_for_save().matches('\r').count(), 1);
    }

    /// A lone carriage return is content, not a break: only the `\r\n` pair
    /// collapses, and saving an untouched file must return every byte of it.
    #[test]
    fn a_lone_carriage_return_before_a_break_survives_the_round_trip() {
        let original = "a\r\r\n";
        let buf = EditorBuffer::new("x.txt", original);
        assert_eq!(buf.line_ending(), LineEnding::Crlf);
        assert_eq!(buf.text(), "a\r\n", "only the CRLF collapsed");
        assert_eq!(
            buf.text_for_save(),
            original,
            "saving a file nobody edited must not lose a character"
        );
        assert_eq!(buf.line(0), "a\r", "the lone CR is part of the line");
    }

    /// Mixed endings: the majority wins, a tie goes to LF, and the file is
    /// saved consistently in the chosen ending.
    #[test]
    fn mixed_endings_normalise_to_the_majority_and_save_consistently() {
        let crlf_majority = EditorBuffer::new("x.txt", "a\r\nb\r\nc\nd");
        assert_eq!(crlf_majority.line_ending(), LineEnding::Crlf);
        assert_eq!(crlf_majority.text(), "a\nb\nc\nd");
        assert_eq!(crlf_majority.text_for_save(), "a\r\nb\r\nc\r\nd");

        let lf_majority = EditorBuffer::new("x.txt", "a\nb\nc\r\nd");
        assert_eq!(lf_majority.line_ending(), LineEnding::Lf);
        assert_eq!(lf_majority.text_for_save(), "a\nb\nc\nd");

        let tie = EditorBuffer::new("x.txt", "a\r\nb\nc");
        assert_eq!(tie.line_ending(), LineEnding::Lf);
        assert_eq!(tie.text_for_save(), "a\nb\nc");

        let no_breaks = EditorBuffer::new("x.txt", "abc");
        assert_eq!(no_breaks.line_ending(), LineEnding::Lf);
        assert_eq!(no_breaks.text_for_save(), "abc");

        // A file that starts out LF and is saved stays byte-identical.
        let lf = EditorBuffer::new("x.txt", "a\nb\n");
        assert_eq!(lf.text_for_save(), "a\nb\n");
    }

    /// Spans and line lookups keep working over the LF-only rope.
    #[test]
    fn crlf_files_still_highlight_per_line() {
        let mut buf = EditorBuffer::new("x.rs", "// one\r\n// two\r\n");
        assert_eq!(buf.line_count(), 3);
        assert_eq!(buf.line(1), "// two");
        assert_eq!(buf.spans_for_line(1).len(), 1);
        assert_eq!(buf.spans_for_line(1)[0].len, 6);
    }

    // -- IQ3: an own save is not an external change ------------------------

    /// The document remembers what it last knew to be on disk (by hash). An
    /// `fs.changed` that reads back exactly that is its own write, or a touch,
    /// and changes nothing: a keystroke typed between the save and the event
    /// stays in the buffer, and no `externalChange` is raised.
    #[test]
    fn a_change_that_reads_back_the_saved_text_is_ignored() {
        let saved = "fn main() {}\r\n";
        let known = Some(content_hash(saved));
        // Dirty again: the user typed after the save was issued.
        assert_eq!(disk_verdict(saved, known, true, 3, 4), DiskVerdict::Ignore);
        // Still clean: the same answer, nothing to install.
        assert_eq!(disk_verdict(saved, known, false, 4, 4), DiskVerdict::Ignore);
    }

    /// The two halves together: what `save` writes is `text_for_save`, so the
    /// hash it records is over the file's *own* bytes, CRLF and all. The
    /// `fs.changed` that write produces reads those bytes back and is ignored
    /// even though the user has typed since -- which is the keystroke
    /// surviving the save.
    #[test]
    fn the_saved_bytes_are_what_the_next_read_is_measured_against() {
        let mut buf = EditorBuffer::new("x.rs", "fn main() {}\r\n");
        let written = buf.text_for_save();
        assert_eq!(written, "fn main() {}\r\n", "the file keeps its endings");
        let known = Some(content_hash(&written));

        // The user types while the write is in flight: dirty again, content
        // generation moved on.
        let at = buf.utf16_to_char(12);
        buf.apply_edit(at, 0, "\n");
        assert_eq!(buf.text(), "fn main() {}\n\n");

        // The daemon reports the write. The file holds the written bytes.
        assert_eq!(
            disk_verdict(&written, known, true, 7, 8),
            DiskVerdict::Ignore
        );
        // And what the user typed is still there to be saved next time.
        assert_eq!(buf.text_for_save(), "fn main() {}\r\n\r\n");
    }

    /// Something else wrote the file: with local edits the user is asked,
    /// without them the disk text is installed, unless the buffer moved on
    /// while the read was in flight.
    #[test]
    fn a_change_that_reads_back_other_text_is_external() {
        let known = Some(content_hash("fn main() {}\n"));
        let other = "fn main() { changed }\n";
        assert_eq!(
            disk_verdict(other, known, true, 3, 3),
            DiskVerdict::ExternalChange
        );
        assert_eq!(
            disk_verdict(other, known, false, 3, 3),
            DiskVerdict::Install
        );
        // A save overtook the read: its content is newer than the read's.
        assert_eq!(disk_verdict(other, known, false, 3, 4), DiskVerdict::Drop);
        // Nothing known yet (the first read never landed): install.
        assert_eq!(disk_verdict(other, None, false, 3, 3), DiskVerdict::Install);
    }

    // -- IQ2/IQ3 through the document's own decisions ----------------------

    /// The path the brief names for IQ2, one layer below the QObject: the
    /// daemon restarts, the watch re-attaches and reports `Reconnected`, the
    /// document re-reads, and because it has unsaved edits and the file no
    /// longer holds what it knew, it raises `externalChange`.
    ///
    /// `EditorDocument::on_watch_notice` is `DiskState::on_notice` plus a
    /// generation check, and `apply_disk_read` is `DiskState::on_read` plus
    /// `external_change()`; this drives both halves in order.
    #[test]
    fn a_reconnect_that_finds_other_bytes_raises_an_external_change() {
        let mut disk = DiskState::default();
        // The open: the first read installs whatever is there.
        assert_eq!(
            disk.on_notice(WatchNotice::Subscribed, 0),
            WatchAction::ReadAndInstall
        );
        disk.record(content_hash("fn main() {}\n"));

        // The user types (content generation 1), then the daemon restarts and
        // the watch re-attaches on the new router.
        assert_eq!(
            disk.on_notice(WatchNotice::Reconnected, 1),
            WatchAction::ReadAndJudge(1)
        );
        // An agent rewrote the file while the IDE had nobody listening.
        assert_eq!(
            disk.on_read("fn main() { agent }\n", true, 1, 1),
            ReadOutcome::RaiseExternalChange
        );
        assert!(disk.external_pending, "the bar is up");

        // A `git checkout` touching the file again must not stack a second
        // prompt behind the first.
        assert_eq!(
            disk.on_notice(WatchNotice::Changed, 1),
            WatchAction::Nothing
        );

        // "Keep mine": the bar goes down and the next change is a fresh
        // question, but the bytes the user already saw are no longer news.
        disk.keep_local();
        assert_eq!(
            disk.on_notice(WatchNotice::Changed, 1),
            WatchAction::ReadAndJudge(1)
        );
        assert_eq!(
            disk.on_read("fn main() { agent }\n", true, 1, 1),
            ReadOutcome::Nothing,
            "the same disk bytes must not prompt twice"
        );
    }

    /// The same path for IQ3, with a real buffer on the other end: the write
    /// goes out as `text_for_save`, the user types before the daemon reports
    /// it, and the document must neither prompt nor lose the keystroke.
    #[test]
    fn an_own_save_never_prompts_and_the_keystroke_survives_it() {
        let mut buf = EditorBuffer::new("x.rs", "fn main() {}\r\n");
        let mut disk = DiskState::default();
        disk.record(content_hash(&buf.text_for_save()));

        // Ctrl+S at content generation 7: these are the bytes that go out.
        let written = buf.text_for_save();
        // The user types while the write is in flight: generation 8, dirty.
        let at = buf.utf16_to_char(12);
        buf.apply_edit(at, 0, "\n");
        // The write lands and the document records what is now on disk.
        disk.record(content_hash(&written));

        // The daemon reports the write this document made.
        assert_eq!(
            disk.on_notice(WatchNotice::Changed, 8),
            WatchAction::ReadAndJudge(8)
        );
        assert_eq!(
            disk.on_read(&written, true, 8, 8),
            ReadOutcome::Nothing,
            "our own write is not an external change"
        );
        assert!(!disk.external_pending, "no bar over our own write");
        assert_eq!(buf.text(), "fn main() {}\n\n", "the keystroke survives");
        assert_eq!(buf.text_for_save(), "fn main() {}\r\n\r\n");
    }

    /// An unmodified document quietly takes whatever else wrote the file, and a
    /// read overtaken by a save is dropped rather than undoing the save.
    #[test]
    fn a_clean_document_installs_and_a_stale_read_is_dropped() {
        let mut disk = DiskState::default();
        disk.record(content_hash("one\n"));
        assert_eq!(
            disk.on_read("two\n", false, 4, 4),
            ReadOutcome::InstallDiskText
        );
        assert_eq!(disk.known, Some(content_hash("two\n")));

        // A read issued at generation 4 landing after a save moved it to 5.
        assert_eq!(disk.on_read("three\n", false, 4, 5), ReadOutcome::Nothing);
        assert_eq!(
            disk.known,
            Some(content_hash("two\n")),
            "a stale read must not be recorded as what is on disk"
        );
    }

    // -- IQ2: the watch follows the router across a reconnect ----------------

    /// A daemon just real enough for the watch task: it answers `hello`,
    /// acknowledges `fs.watch`, and accepts any number of connections so a
    /// second `connect` is a reconnect.
    async fn fake_daemon(token: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut r = BufReader::new(r);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match r.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let ClientMessage::Request { id, request } =
                            codec::decode(line.trim_end()).unwrap();
                        let msg = match request {
                            Request::Hello(p) if p.token == token => ServerMessage::ok(
                                id,
                                &HelloResult {
                                    daemon_version: "9.9.9".into(),
                                    capabilities: Capabilities {
                                        backends: vec![],
                                        sandbox_backend: "noop".into(),
                                        git_protect: false,
                                        adapters: vec![],
                                    },
                                    protocol_version: Some(PROTOCOL_VERSION),
                                },
                            ),
                            Request::Hello(_) => ServerMessage::err(id, RpcError::unauthorized()),
                            Request::FsWatch(_) => ServerMessage::ok(id, &serde_json::json!({})),
                            other => ServerMessage::err(
                                id,
                                RpcError::internal(format!(
                                    "not implemented: {}",
                                    other.method_name()
                                )),
                            ),
                        };
                        if w.write_all(codec::encode(&msg).as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn fs_changed(ws: &str, path: &str) -> (Option<WorkspaceId>, Event) {
        (
            Some(ws.into()),
            Event::FsChanged {
                paths: vec![path.into()],
            },
        )
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<WatchNotice>) -> Option<WatchNotice> {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the watch task must answer within 5 s")
    }

    /// `publish_shared` writes one process-wide slot and bumps one
    /// process-wide generation, and cargo runs the tests in this binary in
    /// parallel. A second test publishing during the first test's quiet window
    /// would hand it a spurious `Reconnected`, or attach its watch to the wrong
    /// router. Every test that publishes holds this for its whole body.
    static GLOBAL_CONNECTION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn nothing(rx: &mut mpsc::UnboundedReceiver<WatchNotice>) {
        let got = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(got.is_err(), "expected no notice, got {:?}", got.unwrap());
    }

    /// Every connect builds a fresh router, so a subscription taken on the
    /// previous one is dead. The watch task re-subscribes on the router that
    /// is live after the reconnect and asks again for `fs.watch`; a change
    /// published there reaches the document.
    #[tokio::test]
    async fn the_watch_follows_the_router_across_a_reconnect() {
        let _connection = GLOBAL_CONNECTION.lock().await;
        let addr = fake_daemon("secret").await;
        let (c1, _hello, _events1) = DaemonClient::connect(addr, "secret", "0.1.0")
            .await
            .unwrap();
        let r1 = EventRouter::new();
        publish_shared(Shared {
            client: c1,
            router: r1.clone(),
        });

        let (tx, mut notices) = mpsc::unbounded_channel();
        let task = tokio::spawn(watch_file(
            "ws_a".to_owned(),
            "src/main.rs".to_owned(),
            move |notice| tx.send(notice).is_ok(),
        ));
        assert_eq!(next(&mut notices).await, Some(WatchNotice::Subscribed));

        let (ws, ev) = fs_changed("ws_a", "src/main.rs");
        r1.dispatch(ws, ev);
        assert_eq!(next(&mut notices).await, Some(WatchNotice::Changed));
        // Another path, and another workspace: not this document's business.
        let (ws, ev) = fs_changed("ws_a", "src/other.rs");
        r1.dispatch(ws, ev);
        let (ws, ev) = fs_changed("ws_b", "src/main.rs");
        r1.dispatch(ws, ev);
        nothing(&mut notices).await;

        // The reconnect: a new connection, a new router, published the way
        // `AppController` publishes one.
        let (c2, _hello, _events2) = DaemonClient::connect(addr, "secret", "0.1.0")
            .await
            .unwrap();
        let r2 = EventRouter::new();
        publish_shared(Shared {
            client: c2,
            router: r2.clone(),
        });
        assert_eq!(next(&mut notices).await, Some(WatchNotice::Reconnected));

        // The old router is history; the new one is what the document hears.
        let (ws, ev) = fs_changed("ws_a", "src/main.rs");
        r1.dispatch(ws, ev);
        nothing(&mut notices).await;
        let (ws, ev) = fs_changed("ws_a", "src/main.rs");
        r2.dispatch(ws, ev);
        assert_eq!(next(&mut notices).await, Some(WatchNotice::Changed));

        task.abort();
    }

    /// A closed tab, or a document re-opened on another file: the sink refuses
    /// the notice and the task ends rather than watching a file nobody is
    /// showing for the rest of the session.
    #[tokio::test]
    async fn the_watch_ends_when_its_document_stops_listening() {
        let _connection = GLOBAL_CONNECTION.lock().await;
        let addr = fake_daemon("secret").await;
        let (client, _hello, _events) = DaemonClient::connect(addr, "secret", "0.1.0")
            .await
            .unwrap();
        let router = EventRouter::new();
        publish_shared(Shared {
            client,
            router: router.clone(),
        });

        let (tx, mut notices) = mpsc::unbounded_channel();
        let task = tokio::spawn(watch_file(
            "ws_gone".to_owned(),
            "src/main.rs".to_owned(),
            move |notice| {
                // The document is gone: the queue onto its Qt thread fails,
                // which is what `false` stands for here.
                let _ = tx.send(notice);
                false
            },
        ));
        assert_eq!(next(&mut notices).await, Some(WatchNotice::Subscribed));
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the watch task must end once its sink refuses")
            .expect("and end by returning, not by panicking");
    }
}
