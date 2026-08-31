//! Line-level diff algorithm for inline file-change display.
//!
//! Hand-written LCS (Longest Common Subsequence) — no external dependencies.
//! Designed for AI edit sizes (tens of lines), not large-file diffs.

use super::app::{DiffHunk, DiffLine, DiffLineKind};

/// Compute a line-level diff between `old` and `new` text.
///
/// Returns `DiffLine`s tagged as `Add`, `Del`, or `Context`.
pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let m = old_lines.len();
    let n = new_lines.len();

    // LCS table
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if old_lines[i - 1] == new_lines[j - 1] {
                dp[i - 1][j - 1] + 1
            } else {
                dp[i - 1][j].max(dp[i][j - 1])
            };
        }
    }

    // Backtrack to produce the diff
    let mut result = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_lines[i - 1] == new_lines[j - 1] {
            result.push(DiffLine {
                kind: DiffLineKind::Context,
                text: old_lines[i - 1].to_string(),
                old_line: Some(i as u32),
                new_line: Some(j as u32),
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            result.push(DiffLine {
                kind: DiffLineKind::Add,
                text: new_lines[j - 1].to_string(),
                old_line: None,
                new_line: Some(j as u32),
            });
            j -= 1;
        } else {
            result.push(DiffLine {
                kind: DiffLineKind::Del,
                text: old_lines[i - 1].to_string(),
                old_line: Some(i as u32),
                new_line: None,
            });
            i -= 1;
        }
    }
    result.reverse();
    result
}

/// Group flat diff lines into hunks with `@@ -a,b +c,d @@` headers.
///
/// `context` is the number of unchanged lines to show around each change
/// (default 3, matching git).
pub fn diff_to_hunks(old: &str, new: &str, context: usize) -> Vec<DiffHunk> {
    diff_to_hunks_indexed(old, new, context, 0)
}

/// Like `diff_to_hunks` but tags each hunk with the originating edit index.
pub fn diff_to_hunks_indexed(old: &str, new: &str, context: usize, edit_index: usize) -> Vec<DiffHunk> {
    let lines = diff_lines(old, new);
    if lines.is_empty() {
        return Vec::new();
    }

    // Find indices of changed lines
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != DiffLineKind::Context)
        .map(|(i, _)| i)
        .collect();

    if changed.is_empty() {
        // No differences — return empty (or a single context-only hunk if small)
        return Vec::new();
    }

    // Build hunk ranges: merge overlapping context windows
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &idx in &changed {
        let start = idx.saturating_sub(context);
        let end = (idx + context + 1).min(lines.len());
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 {
                last.1 = end;
                continue;
            }
        }
        ranges.push((start, end));
    }

    // Build hunks
    let mut hunks = Vec::new();
    for (start, end) in ranges {
        let hunk_lines: Vec<DiffLine> = lines[start..end].to_vec();

        // Derive header line numbers from the first line in the hunk
        let old_line = hunk_lines
            .iter()
            .find_map(|l| l.old_line)
            .unwrap_or(1);
        let new_line = hunk_lines
            .iter()
            .find_map(|l| l.new_line)
            .unwrap_or(1);
        let old_count = hunk_lines
            .iter()
            .filter(|l| l.old_line.is_some())
            .count();
        let new_count = hunk_lines
            .iter()
            .filter(|l| l.new_line.is_some())
            .count();

        let header = format!("@@ -{},{} +{},{} @@", old_line, old_count, new_line, new_count);
        hunks.push(DiffHunk {
            header,
            lines: hunk_lines,
            edit_index,
        });
    }

    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_old_is_all_add() {
        let lines = diff_lines("", "hello\nworld");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Add));
        assert_eq!(lines[0].new_line, Some(1));
        assert_eq!(lines[1].new_line, Some(2));
        assert!(lines.iter().all(|l| l.old_line.is_none()));
    }

    #[test]
    fn identical_is_all_context() {
        let lines = diff_lines("a\nb\nc", "a\nb\nc");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Context));
        assert_eq!(lines[0].old_line, Some(1));
        assert_eq!(lines[0].new_line, Some(1));
    }

    #[test]
    fn single_line_change() {
        let lines = diff_lines("aaa\nbbb\nccc", "aaa\nxxx\nccc");
        // aaa (Context), bbb (Del), xxx (Add), ccc (Context) = 4 lines
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].kind, DiffLineKind::Context);
        assert_eq!(lines[0].text, "aaa");
        assert_eq!(lines[0].old_line, Some(1));
        assert_eq!(lines[0].new_line, Some(1));
        assert_eq!(lines[1].kind, DiffLineKind::Del);
        assert_eq!(lines[1].text, "bbb");
        assert_eq!(lines[1].old_line, Some(2));
        assert_eq!(lines[1].new_line, None);
        assert_eq!(lines[2].kind, DiffLineKind::Add);
        assert_eq!(lines[2].text, "xxx");
        assert_eq!(lines[2].old_line, None);
        assert_eq!(lines[2].new_line, Some(2));
        assert_eq!(lines[3].kind, DiffLineKind::Context);
        assert_eq!(lines[3].text, "ccc");
        assert_eq!(lines[3].old_line, Some(3));
        assert_eq!(lines[3].new_line, Some(3));
    }

    #[test]
    fn all_deleted() {
        let lines = diff_lines("x\ny", "");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Del));
    }

    #[test]
    fn hunks_have_headers() {
        let hunks = diff_to_hunks("aaa\nbbb\nccc", "aaa\nxxx\nccc", 1);
        assert!(!hunks.is_empty());
        assert!(hunks[0].header.starts_with("@@"));
        // Should contain the changed line
        assert!(hunks[0].lines.iter().any(|l| l.kind == DiffLineKind::Del));
        assert!(hunks[0].lines.iter().any(|l| l.kind == DiffLineKind::Add));
    }

    #[test]
    fn empty_both_returns_empty() {
        let hunks = diff_to_hunks("", "", 3);
        assert!(hunks.is_empty());
    }

    #[test]
    fn no_diff_returns_empty() {
        let hunks = diff_to_hunks("same\ntext", "same\ntext", 3);
        assert!(hunks.is_empty());
    }
}
