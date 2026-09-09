use bondsymphonic_ide::model::diff::{align, counts, rows_json, DiffRow, RowKind};

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
