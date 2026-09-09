//! Side-by-side alignment of two texts into rows, for the diff view.
//!
//! One row is one line of the rendered diff. Equal and Replace rows carry both
//! sides; Insert and Delete rows carry one side and leave the other blank, so
//! the two panes always scroll in step. A replaced block pairs old lines with
//! new lines one for one and spills the longer side into Delete or Insert rows.

use serde::Serialize;
use similar::{DiffTag, TextDiff};

/// What one row of the side-by-side view represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RowKind {
    /// The same line on both sides.
    Equal,
    /// A line present only in the working text.
    Insert,
    /// A line present only in the base text.
    Delete,
    /// A base line paired with the working line that took its place.
    Replace,
}

/// One line of the side-by-side view.
///
/// Line numbers are 1-based and `None` on a side the row does not occupy.
/// Texts never contain their line terminator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiffRow {
    pub left_no: Option<usize>,
    pub left_text: String,
    pub right_no: Option<usize>,
    pub right_text: String,
    pub kind: RowKind,
}

/// Drops the line terminator, `\n` or `\r\n`, that the line splitter kept.
fn strip(line: &str) -> String {
    line.trim_end_matches(['\n', '\r']).to_string()
}

/// Aligns `base` against `work` into rows of the side-by-side view.
///
/// Two empty texts yield no rows; a missing trailing newline is immaterial,
/// the final line is a row like any other.
pub fn align(base: &str, work: &str) -> Vec<DiffRow> {
    let diff = TextDiff::from_lines(base, work);
    let old: Vec<&str> = diff.old_slices().to_vec();
    let new: Vec<&str> = diff.new_slices().to_vec();
    let mut rows = Vec::new();
    for op in diff.ops() {
        let (tag, old_range, new_range) = (op.tag(), op.old_range(), op.new_range());
        match tag {
            DiffTag::Equal => {
                for (o, n) in old_range.zip(new_range) {
                    rows.push(DiffRow {
                        left_no: Some(o + 1),
                        left_text: strip(old[o]),
                        right_no: Some(n + 1),
                        right_text: strip(new[n]),
                        kind: RowKind::Equal,
                    });
                }
            }
            DiffTag::Delete => {
                for o in old_range {
                    rows.push(delete_row(o, old[o]));
                }
            }
            DiffTag::Insert => {
                for n in new_range {
                    rows.push(insert_row(n, new[n]));
                }
            }
            DiffTag::Replace => {
                // Pair the two sides line for line, then spill whichever side
                // is longer into one-sided rows.
                let mut olds = old_range;
                let mut news = new_range;
                loop {
                    match (olds.next(), news.next()) {
                        (Some(o), Some(n)) => rows.push(DiffRow {
                            left_no: Some(o + 1),
                            left_text: strip(old[o]),
                            right_no: Some(n + 1),
                            right_text: strip(new[n]),
                            kind: RowKind::Replace,
                        }),
                        (Some(o), None) => rows.push(delete_row(o, old[o])),
                        (None, Some(n)) => rows.push(insert_row(n, new[n])),
                        (None, None) => break,
                    }
                }
            }
        }
    }
    rows
}

fn delete_row(o: usize, line: &str) -> DiffRow {
    DiffRow {
        left_no: Some(o + 1),
        left_text: strip(line),
        right_no: None,
        right_text: String::new(),
        kind: RowKind::Delete,
    }
}

fn insert_row(n: usize, line: &str) -> DiffRow {
    DiffRow {
        left_no: None,
        left_text: String::new(),
        right_no: Some(n + 1),
        right_text: strip(line),
        kind: RowKind::Insert,
    }
}

/// Additions and deletions, as a diff stat counts them: a Replace row is both
/// one addition and one deletion.
pub fn counts(rows: &[DiffRow]) -> (usize, usize) {
    let adds = rows
        .iter()
        .filter(|r| matches!(r.kind, RowKind::Insert | RowKind::Replace))
        .count();
    let dels = rows
        .iter()
        .filter(|r| matches!(r.kind, RowKind::Delete | RowKind::Replace))
        .count();
    (adds, dels)
}

/// The rows as a JSON array, for the widget layer to render.
pub fn rows_json(rows: &[DiffRow]) -> String {
    serde_json::to_string(rows).expect("rows serialise")
}
