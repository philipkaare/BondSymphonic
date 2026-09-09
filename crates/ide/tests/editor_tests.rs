use bondsymphonic_ide::highlight::languages::Language;
use bondsymphonic_ide::highlight::theme::{StyleId, Theme};
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
