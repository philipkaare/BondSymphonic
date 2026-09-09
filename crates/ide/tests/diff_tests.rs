use bondsymphonic_ide::model::diff::{
    align, align_with_deadline, counts, rows_json, DiffRow, RowKind,
};
use bondsymphonic_ide::model::editor_buffer::EditorBuffer;
use std::time::Duration;

fn kinds(rows: &[DiffRow]) -> Vec<RowKind> {
    rows.iter().map(|r| r.kind).collect()
}

#[test]
fn identical_texts_are_all_equal_rows_with_both_numbers() {
    let rows = align("a\nb\n", "a\nb\n");
    assert_eq!(kinds(&rows), vec![RowKind::Equal, RowKind::Equal]);
    assert_eq!(
        rows[1],
        DiffRow {
            left_no: Some(2),
            left_text: "b".into(),
            right_no: Some(2),
            right_text: "b".into(),
            kind: RowKind::Equal
        }
    );
    assert_eq!(counts(&rows), (0, 0));
}

#[test]
fn pure_insert_and_delete_rows_are_one_sided() {
    let rows = align("a\nc\n", "a\nb\nc\n");
    assert_eq!(
        kinds(&rows),
        vec![RowKind::Equal, RowKind::Insert, RowKind::Equal]
    );
    assert_eq!(rows[1].left_no, None);
    assert_eq!(rows[1].left_text, "");
    assert_eq!(rows[1].right_no, Some(2));
    assert_eq!(counts(&rows), (1, 0));

    let rows = align("a\nb\nc\n", "a\nc\n");
    assert_eq!(
        kinds(&rows),
        vec![RowKind::Equal, RowKind::Delete, RowKind::Equal]
    );
    assert_eq!(rows[1].right_no, None);
    assert_eq!(counts(&rows), (0, 1));
}

#[test]
fn replace_pairs_lines_and_spills_the_remainder() {
    // 2 old lines replaced by 3 new lines: two Replace rows + one Insert.
    let rows = align("x\nold1\nold2\ny\n", "x\nnew1\nnew2\nnew3\ny\n");
    assert_eq!(
        kinds(&rows),
        vec![
            RowKind::Equal,
            RowKind::Replace,
            RowKind::Replace,
            RowKind::Insert,
            RowKind::Equal
        ]
    );
    assert_eq!(
        (rows[1].left_text.as_str(), rows[1].right_text.as_str()),
        ("old1", "new1")
    );
    assert_eq!((rows[3].left_no, rows[3].right_no), (None, Some(4)));
    assert_eq!(counts(&rows), (3, 2));
}

#[test]
fn empty_base_is_all_inserts_and_missing_trailing_newline_is_kept_as_text() {
    let rows = align("", "a\nb");
    assert_eq!(kinds(&rows), vec![RowKind::Insert, RowKind::Insert]);
    assert_eq!(rows[1].right_text, "b");
}

#[test]
fn rows_json_uses_lowercase_kinds_and_null_numbers() {
    let rows = align("a\n", "b\n");
    let v: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    assert_eq!(v[0]["kind"], "replace");
    let rows = align("", "a\n");
    let v: serde_json::Value = serde_json::from_str(&rows_json(&rows)).unwrap();
    assert!(v[0]["left_no"].is_null());
    assert_eq!(v[0]["right_no"], 1);
}

#[test]
fn crlf_lines_are_aligned_and_carriage_returns_are_stripped() {
    let rows = align("a\r\nb\r\n", "a\r\nc\r\n");
    assert_eq!(kinds(&rows), vec![RowKind::Equal, RowKind::Replace]);
    assert_eq!(rows[0].left_text, "a");
    assert_eq!(rows[0].right_text, "a");
    assert_eq!(rows[1].left_text, "b");
    assert_eq!(rows[1].right_text, "c");
    assert!(rows.iter().all(|r| !r.left_text.contains('\r')
        && !r.right_text.contains('\r')
        && !r.left_text.contains('\n')
        && !r.right_text.contains('\n')));
    assert_eq!(counts(&rows), (1, 1));
}

#[test]
fn empty_work_is_all_deletes_and_two_empty_texts_yield_no_rows() {
    let rows = align("a\nb\n", "");
    assert_eq!(kinds(&rows), vec![RowKind::Delete, RowKind::Delete]);
    assert_eq!(rows[1].left_no, Some(2));
    assert_eq!(rows[1].right_no, None);
    assert_eq!(counts(&rows), (0, 2));

    assert!(align("", "").is_empty());
    assert_eq!(counts(&[]), (0, 0));
}

#[test]
fn replace_spills_the_remainder_as_deletes_when_the_base_is_longer() {
    // 3 old lines replaced by 2 new lines: two Replace rows + one Delete.
    let rows = align("x\nold1\nold2\nold3\ny\n", "x\nnew1\nnew2\ny\n");
    assert_eq!(
        kinds(&rows),
        vec![
            RowKind::Equal,
            RowKind::Replace,
            RowKind::Replace,
            RowKind::Delete,
            RowKind::Equal
        ]
    );
    assert_eq!((rows[3].left_no, rows[3].right_no), (Some(4), None));
    assert_eq!(rows[3].left_text, "old3");
    assert_eq!(rows[3].right_text, "");
    assert_eq!(counts(&rows), (2, 3));
}

/// The deadline form is what `DiffDocument` runs, so it has to agree with
/// `align` whenever the budget is not actually spent: same rows, and nothing
/// reported as cut short.
#[test]
fn a_budget_that_is_not_spent_yields_the_same_rows_as_align() {
    let base = "a\nb\nc\nd\n";
    let (rows, truncated) = align_with_deadline(base, base, Duration::from_secs(2));
    assert!(!truncated);
    assert_eq!(rows, align(base, base));
    assert_eq!(counts(&rows), (0, 0));

    let work = "a\nB\nc\ne\nd\n";
    let (rows, truncated) = align_with_deadline(base, work, Duration::from_secs(2));
    assert!(!truncated);
    assert_eq!(rows, align(base, work));
}

/// A bare carriage return is an ordinary character, not a line break.
///
/// `similar`'s own line splitter ends a line at it; `EditorBuffer` and
/// `QTextDocument` do not. If the two disagreed, a file containing a lone CR
/// would produce more rows than the buffer has lines, and from that point on
/// every row's `left_no`/`right_no` would fetch the wrong line's spans.
#[test]
fn a_lone_carriage_return_does_not_start_a_row() {
    let text = "a\rb\n";
    let rows = align(text, text);
    assert_eq!(kinds(&rows), vec![RowKind::Equal]);
    assert_eq!(rows[0].left_text, "a\rb");
    assert_eq!(rows[0].right_text, "a\rb");
    assert_eq!(rows[0].left_no, Some(1));
    assert_eq!(rows[0].right_no, Some(1));

    // The buffer counts a trailing newline as opening one more, empty line;
    // the rows describe every line before it. Both therefore say "one line".
    let buffer = EditorBuffer::new("x.txt", text);
    assert_eq!(rows.len(), buffer.line_count() - 1);
    assert_eq!(buffer.line(0), "a\rb");

    // The same through the deadline form, which the diff view actually calls.
    let (rows, truncated) = align_with_deadline(text, text, Duration::from_secs(2));
    assert!(!truncated);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].left_text, "a\rb");
}

/// A carriage return that is not part of a `\r\n` and not at the end of the
/// line survives into the row text, while the terminator itself is stripped.
#[test]
fn only_the_terminating_crlf_is_stripped_from_a_row() {
    let rows = align("a\r\r\n", "b\r\r\n");
    assert_eq!(kinds(&rows), vec![RowKind::Replace]);
    assert_eq!(rows[0].left_text, "a\r");
    assert_eq!(rows[0].right_text, "b\r");
    assert_eq!(
        rows[0].left_text,
        EditorBuffer::new("x.txt", "a\r\r\n").line(0)
    );
}
