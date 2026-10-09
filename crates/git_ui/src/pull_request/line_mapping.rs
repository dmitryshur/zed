//! Translates line numbers between versions of a file. All rows are zero-based; GitHub's
//! one-based line numbers are converted at the API boundary.

use std::ops::Range;

use super::github::DiffSide;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MappedRow {
    Exact(u32),
    /// The row was edited or removed; `nearest` is where its replacement starts.
    Changed {
        nearest: u32,
    },
}

impl MappedRow {
    pub(crate) fn exact(self) -> Option<u32> {
        match self {
            Self::Exact(row) => Some(row),
            Self::Changed { .. } => None,
        }
    }

    pub(crate) fn nearest(self) -> u32 {
        match self {
            Self::Exact(row) | Self::Changed { nearest: row } => row,
        }
    }
}

/// Maps rows between an old and a new version of a text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LineMap {
    /// Row ranges replaced between the versions, in ascending order.
    edits: Vec<(Range<u32>, Range<u32>)>,
}

impl LineMap {
    pub(crate) fn new(old_text: &str, new_text: &str) -> Self {
        if old_text == new_text {
            return Self::default();
        }
        Self {
            edits: language::line_diff(old_text, new_text),
        }
    }

    pub(crate) fn old_to_new(&self, row: u32) -> MappedRow {
        map_row(
            self.edits
                .iter()
                .map(|(old, new)| (old.clone(), new.clone())),
            row,
        )
    }

    pub(crate) fn new_to_old(&self, row: u32) -> MappedRow {
        map_row(
            self.edits
                .iter()
                .map(|(old, new)| (new.clone(), old.clone())),
            row,
        )
    }
}

fn map_row(edits: impl Iterator<Item = (Range<u32>, Range<u32>)>, row: u32) -> MappedRow {
    let mut offset = 0_i64;
    for (from, to) in edits {
        if row < from.start {
            break;
        }
        if from.contains(&row) {
            return MappedRow::Changed { nearest: to.start };
        }
        offset = i64::from(to.end) - i64::from(from.end);
    }
    MappedRow::Exact(u32::try_from(i64::from(row) + offset).unwrap_or(0))
}

/// The rows GitHub accepts comments on: the lines of the pull request's diff hunks, which
/// include a few unchanged lines of context around each change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommentableLines {
    left: Vec<Range<u32>>,
    right: Vec<Range<u32>>,
}

impl CommentableLines {
    /// Reads the hunk headers (`@@ -start,count +start,count @@`) of a unified diff.
    pub(crate) fn from_patch(patch: &str) -> Self {
        let mut lines = Self::default();
        for header in patch.lines().filter(|line| line.starts_with("@@ ")) {
            let mut ranges = header.split_whitespace().skip(1);
            let (Some(left), Some(right)) = (ranges.next(), ranges.next()) else {
                continue;
            };
            if let (Some(left), Some(right)) = (
                left.strip_prefix('-').and_then(parse_hunk_range),
                right.strip_prefix('+').and_then(parse_hunk_range),
            ) {
                lines.left.push(left);
                lines.right.push(right);
            }
        }
        lines
    }

    pub(crate) fn contains(&self, side: DiffSide, row: u32) -> bool {
        let ranges = match side {
            DiffSide::Left => &self.left,
            DiffSide::Right => &self.right,
        };
        ranges.iter().any(|range| range.contains(&row))
    }
}

/// Parses `start,count` (or `start`, meaning one line) into zero-based rows.
fn parse_hunk_range(range: &str) -> Option<Range<u32>> {
    let (start, count) = match range.split_once(',') {
        Some((start, count)) => (start.parse::<u32>().ok()?, count.parse::<u32>().ok()?),
        None => (range.parse::<u32>().ok()?, 1),
    };
    let start = start.saturating_sub(1);
    Some(start..start + count)
}

/// Finds where an outdated comment's line is in `text`, by the lines that precede it in the
/// comment's diff hunk (the hunk ends at the commented line). Prefers the match nearest `hint`.
pub(crate) fn locate_by_diff_hunk(
    diff_hunk: &str,
    side: DiffSide,
    text: &str,
    hint: u32,
) -> Option<u32> {
    let removed_prefix = match side {
        DiffSide::Left => '+',
        DiffSide::Right => '-',
    };
    let mut needle = diff_hunk
        .lines()
        .filter(|line| !line.starts_with("@@") && !line.starts_with(removed_prefix))
        .map(|line| line.get(1..).unwrap_or_default())
        .rev()
        .take(3)
        .collect::<Vec<_>>();
    needle.reverse();
    if needle.iter().all(|line| line.trim().is_empty()) {
        return None;
    }
    let lines = text.lines().collect::<Vec<_>>();
    let last_offset = needle.len().checked_sub(1)?;
    (0..lines.len())
        .filter(|&start| {
            lines
                .get(start..start + needle.len())
                .is_some_and(|window| window == needle.as_slice())
        })
        .filter_map(|start| u32::try_from(start + last_offset).ok())
        .min_by_key(|row| row.abs_diff(hint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_line_map() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nx\ny\nd\n";
        let map = LineMap::new(old, new);
        assert_eq!(map.old_to_new(0), MappedRow::Exact(0));
        assert_eq!(map.old_to_new(1), MappedRow::Changed { nearest: 1 });
        assert_eq!(map.old_to_new(2), MappedRow::Exact(2));
        assert_eq!(map.old_to_new(3), MappedRow::Exact(5));
        assert_eq!(map.old_to_new(4), MappedRow::Changed { nearest: 6 });
        assert_eq!(map.new_to_old(3), MappedRow::Changed { nearest: 3 });
        assert_eq!(map.new_to_old(5), MappedRow::Exact(3));

        let unchanged = LineMap::new(old, old);
        assert_eq!(unchanged.old_to_new(4), MappedRow::Exact(4));
    }

    #[test]
    fn test_commentable_lines_from_patch() {
        let patch = "@@ -7,9 +7,9 @@ line 6\n line 7\n-old\n+new\n@@ -27,9 +27,6 @@ line 26\n line\n@@ -0,0 +1 @@\n+only";
        let lines = CommentableLines::from_patch(patch);
        assert!(lines.contains(DiffSide::Right, 6));
        assert!(lines.contains(DiffSide::Right, 14));
        assert!(!lines.contains(DiffSide::Right, 15));
        assert!(!lines.contains(DiffSide::Right, 5));
        assert!(lines.contains(DiffSide::Left, 34));
        assert!(!lines.contains(DiffSide::Left, 35));
        assert!(lines.contains(DiffSide::Right, 31));
        assert!(!lines.contains(DiffSide::Right, 32));
        assert!(lines.contains(DiffSide::Right, 0));
        assert!(CommentableLines::from_patch("").right.is_empty());
    }

    #[test]
    fn test_locate_by_diff_hunk() {
        let diff_hunk = "@@ -1,4 +1,4 @@\n one\n-two\n+TWO\n three";
        let text = "zero\none\nTWO\nthree\nfour\none\nTWO\nthree\n";
        assert_eq!(
            locate_by_diff_hunk(diff_hunk, DiffSide::Right, text, 1),
            Some(3)
        );
        assert_eq!(
            locate_by_diff_hunk(diff_hunk, DiffSide::Right, text, 9),
            Some(7)
        );
        let left_text = "one\ntwo\nthree\n";
        assert_eq!(
            locate_by_diff_hunk(diff_hunk, DiffSide::Left, left_text, 0),
            Some(2)
        );
        assert_eq!(
            locate_by_diff_hunk(diff_hunk, DiffSide::Right, "unrelated\n", 0),
            None
        );
    }
}
