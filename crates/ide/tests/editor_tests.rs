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
