//! Text-wrapping helpers for the TUI: hard-wrap at display columns (CJK-aware)
//! and an incremental accumulator for the streaming tail.
//!
//! Extracted from `app.rs` so the streaming state machine and the renderer can
//! both use them without reaching into the `App` god-object.

use unicode_width::UnicodeWidthChar;

/// Hard-wrap text to `width` display columns: split on existing newlines, then
/// break over-long runs at char boundaries. Each char's width is its terminal
/// column count (ASCII = 1, wide CJK = 2, combining marks = 0), so a line of
/// Chinese wraps at the same visual width as a line of English.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut col = 0usize;
        for c in raw.chars() {
            let cw = c.width().unwrap_or(0);
            // Break before a char that would overflow the line, unless the line
            // is still empty (a single over-wide char still gets its own line).
            if col + cw > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                col = 0;
            }
            line.push(c);
            col += cw;
        }
        out.push(line);
    }
    out
}

/// Collapse whitespace and truncate to a single line of at most `max` chars.
pub fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max).collect::<String>() + "…"
    }
}

/// Incremental hard-wrap accumulator for the streaming tail.
///
/// The output pane renders the live (uncommitted) streaming text every frame.
/// Re-wrapping the whole tail each frame is O(total) per frame — a perf cliff on
/// long answers (30KB+ of streamed text in debug builds) that manifests as a
/// frozen scroll. This accumulates the wrapped form as deltas arrive, so
/// rendering is O(new bytes) per frame instead of O(total). Its output is
/// byte-for-byte identical to [`wrap`].
#[derive(Debug)]
pub struct WrapCache {
    width: usize,
    /// Wrapped lines; the last element is always the current (partial) line.
    lines: Vec<String>,
    /// Display columns currently used by the last line.
    col: usize,
}

impl WrapCache {
    pub fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            lines: vec![String::new()],
            col: 0,
        }
    }

    pub fn reset(&mut self) {
        self.lines.clear();
        self.lines.push(String::new());
        self.col = 0;
    }

    pub fn extend(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                self.lines.push(String::new());
                self.col = 0;
                continue;
            }
            let cw = c.width().unwrap_or(0);
            // Break before a char that would overflow the line, unless the line
            // is still empty (a single over-wide char still gets its own line) —
            // mirrors `wrap`.
            let last_empty = self.lines.last().map_or(true, |l| l.is_empty());
            if self.col + cw > self.width && !last_empty {
                self.lines.push(String::new());
                self.col = 0;
            }
            self.lines.last_mut().expect("non-empty").push(c);
            self.col += cw;
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_preserves_blank_lines_and_hard_wraps() {
        let lines = wrap("abc\ndefghij", 3);
        assert_eq!(lines, vec!["abc", "def", "ghi", "j"]);
        let empty = wrap("", 3);
        assert_eq!(empty, vec![""]);
    }

    #[test]
    fn wrap_multi_line() {
        let lines = wrap("one\ntwo", 100);
        assert_eq!(lines, vec!["one", "two"]);
    }

    #[test]
    fn wrap_counts_wide_cjk_as_two_columns() {
        // "你好世界" is 4 wide glyphs = 8 columns; at width 4 it splits in half.
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
        // A wide glyph straddling the boundary is pushed to the next line.
        assert_eq!(wrap("a你b", 3), vec!["a你", "b"]);
        // ASCII is unchanged: width-1 glyphs wrap exactly as before.
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_cache_matches_wrap_whole_and_incremental() {
        let cases = [
            "abc\ndefghij",
            "one\ntwo",
            "你好世界",
            "a你b",
            "abcdefgh",
            "a\n\nb",
            "trailing newline\n",
            "",
            "exactlywidth",
        ];
        for &s in &cases {
            for width in [1usize, 2, 3, 4, 100] {
                let expected = wrap(s, width);
                // Whole-string extend.
                let mut whole = WrapCache::new(width);
                whole.extend(s);
                assert_eq!(whole.lines(), expected.as_slice(), "whole-extend {s:?} @{width}");
                // Char-by-char extend (true incrementality).
                let mut incr = WrapCache::new(width);
                for c in s.chars() {
                    let mut buf = [0u8; 4];
                    incr.extend(c.encode_utf8(&mut buf));
                }
                assert_eq!(incr.lines(), expected.as_slice(), "char-extend {s:?} @{width}");
            }
        }
    }
}
