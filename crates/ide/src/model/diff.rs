//! Side-by-side alignment of two texts into rows, for the diff view.
//!
//! One row is one line of the rendered diff. Equal and Replace rows carry both
//! sides; Insert and Delete rows carry one side and leave the other blank, so
//! the two panes always scroll in step. A replaced block pairs old lines with
//! new lines one for one and spills the longer side into Delete or Insert rows.

use serde::Serialize;
use similar::{DiffOp, DiffTag, TextDiff};
use std::time::{Duration, Instant};

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
///
/// Takes as long as it takes. Anything driven by a UI wants
/// [`align_with_deadline`] instead: Myers' algorithm is O(n·d), and two
/// megabyte-sized texts that share almost nothing can run for tens of seconds.
pub fn align(base: &str, work: &str) -> Vec<DiffRow> {
    let diff = TextDiff::from_lines(base, work);
    rows_from(diff.old_slices(), diff.new_slices(), diff.ops())
}

/// [`align`], but giving up on an exact diff after `budget` and approximating
/// the rest. The flag says whether the budget ran out, so a view can tell the
/// user the diff it is showing is coarser than the file deserves.
///
/// The rows are always a valid alignment of the two texts; only their
/// minimality is at stake. A budget that is not spent produces exactly what
/// [`align`] produces.
pub fn align_with_deadline(base: &str, work: &str, budget: Duration) -> (Vec<DiffRow>, bool) {
    let started = Instant::now();
    let diff = TextDiff::configure()
        .deadline(started + budget)
        .diff_lines(base, work);
    // Measured before the rows are built: turning ops into rows is linear and
    // has nothing to do with whether the algorithm approximated.
    let truncated = started.elapsed() >= budget;
    (
        rows_from(diff.old_slices(), diff.new_slices(), diff.ops()),
        truncated,
    )
}

/// Turns one diff's ops into side-by-side rows. Shared by both entry points so
/// the deadline can never change the shape of the result, only its minimality.
fn rows_from(old: &[&str], new: &[&str], ops: &[DiffOp]) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for op in ops {
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
