//! Multi-line input composer for the TUI.
//!
//! The REPL's rustyline is line-based; the TUI needs a real multi-line editor
//! so that a pasted multi-line task submits as a *single* turn (rather than
//! being split into one turn per line). Enter sends the whole buffer, Shift+Enter
//! inserts a newline, and pasted text with embedded newlines lands in one buffer.

use unicode_width::UnicodeWidthStr;

/// A multi-line text buffer with a cursor.
///
/// The cursor is a `(line, byte_offset)` pair; every mutation keeps `byte_offset`
/// on a UTF-8 char boundary so the render layer can slice lines safely.
#[derive(Debug, Clone, Default)]
pub struct Composer {
    lines: Vec<String>,
    cursor: (usize, usize),
}

impl Composer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }

    /// The full buffer as a single string (lines joined with `\n`).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when there is nothing to submit.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Visual row count after accounting for soft-wrap at `inner_width` columns.
    ///
    /// Each logical line is prefixed with `"> "` (first) or `"  "` (continuation),
    /// consuming 2 columns, so the effective content width is `inner_width - 2`.
    /// A logical line that overflows this width wraps into multiple visual rows.
    pub fn visual_height(&self, inner_width: u16) -> usize {
        let content_w = inner_width.saturating_sub(2) as usize;
        if content_w == 0 {
            return self.lines.len();
        }
        self.lines
            .iter()
            .map(|line| {
                let w = UnicodeWidthStr::width(line.as_str());
                w.div_ceil(content_w).max(1)
            })
            .sum()
    }

    /// The raw lines (used by the render layer).
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The `(line, byte_offset)` cursor position.
    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// Reset to an empty single-line buffer.
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Insert a string (may contain newlines) at the cursor.
    pub fn insert_str(&mut self, s: &str) {
        for (i, part) in s.split('\n').enumerate() {
            if i > 0 {
                self.split_line();
            }
            let (line, col) = self.cursor;
            self.lines[line].insert_str(col, part);
            self.cursor.1 += part.len();
        }
    }

    /// Insert a single char (wrapped as a one-char string).
    pub fn insert_char(&mut self, c: char) {
        self.insert_str(&c.to_string());
    }

    /// Split the current line at the cursor, moving the cursor to the new line.
    fn split_line(&mut self) {
        let (line, col) = self.cursor;
        let tail = self.lines[line].split_off(col);
        self.lines.insert(line + 1, tail);
        self.cursor = (line + 1, 0);
    }

    /// Delete the char before the cursor, or join with the previous line when
    /// the cursor is at the start of a line.
    pub fn backspace(&mut self) {
        let (line, col) = self.cursor;
        if col > 0 {
            let prev = prev_boundary(&self.lines[line], col);
            self.lines[line].replace_range(prev..col, "");
            self.cursor.1 = prev;
        } else if line > 0 {
            let cur = self.lines.remove(line);
            let prev_line = line - 1;
            let prev_len = self.lines[prev_line].len();
            self.lines[prev_line].push_str(&cur);
            self.cursor = (prev_line, prev_len);
        }
    }

    /// Delete the char at the cursor, or join with the next line when the cursor
    /// is at the end of a line.
    pub fn delete_forward(&mut self) {
        let (line, col) = self.cursor;
        let line_len = self.lines[line].len();
        if col < line_len {
            let next = next_boundary(&self.lines[line], col);
            self.lines[line].replace_range(col..next, "");
        } else if line + 1 < self.lines.len() {
            let next = self.lines.remove(line + 1);
            self.lines[line].push_str(&next);
        }
    }

    pub fn move_left(&mut self) {
        let (line, col) = self.cursor;
        if col > 0 {
            self.cursor.1 = prev_boundary(&self.lines[line], col);
        } else if line > 0 {
            self.cursor = (line - 1, self.lines[line - 1].len());
        }
    }

    pub fn move_right(&mut self) {
        let (line, col) = self.cursor;
        let line_len = self.lines[line].len();
        if col < line_len {
            self.cursor.1 = next_boundary(&self.lines[line], col);
        } else if line + 1 < self.lines.len() {
            self.cursor = (line + 1, 0);
        }
    }

    pub fn move_home(&mut self) {
        self.cursor.1 = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor.1 = self.lines[self.cursor.0].len();
    }

    pub fn move_up(&mut self) {
        let (line, col) = self.cursor;
        if line == 0 {
            return;
        }
        let target = line - 1;
        self.cursor = (target, clamp_boundary(&self.lines[target], col));
    }

    pub fn move_down(&mut self) {
        let (line, col) = self.cursor;
        if line + 1 >= self.lines.len() {
            return;
        }
        let target = line + 1;
        self.cursor = (target, clamp_boundary(&self.lines[target], col));
    }
}

/// Byte offset of the char boundary immediately before `col`.
fn prev_boundary(line: &str, col: usize) -> usize {
    line[..col].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
}

/// Byte offset of the char boundary immediately after `col`.
fn next_boundary(line: &str, col: usize) -> usize {
    line[col..].chars().next().map(|c| col + c.len_utf8()).unwrap_or(col)
}

/// Clamp `col` down to the nearest char boundary ≤ `col` (also clamps to `len`).
fn clamp_boundary(line: &str, col: usize) -> usize {
    let mut idx = col.min(line.len());
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_empty_single_line() {
        let c = Composer::new();
        assert!(c.is_empty());
        assert_eq!(c.text(), "");
    }

    #[test]
    fn insert_and_text_join() {
        let mut c = Composer::new();
        c.insert_str("hello");
        assert_eq!(c.text(), "hello");
        c.insert_char('\n'); // Shift+Enter
        c.insert_str("world");
        assert_eq!(c.text(), "hello\nworld");
        assert!(!c.is_empty());
    }

    #[test]
    fn paste_multiline_lands_in_one_buffer() {
        let mut c = Composer::new();
        c.insert_str("line1\nline2\nline3");
        assert_eq!(c.text(), "line1\nline2\nline3");
    }

    #[test]
    fn backspace_mid_line_deletes_char() {
        let mut c = Composer::new();
        c.insert_str("abc");
        c.move_end();
        c.backspace();
        assert_eq!(c.text(), "ab");
    }

    #[test]
    fn backspace_at_line_start_merges_with_previous() {
        let mut c = Composer::new();
        c.insert_str("ab\ncd");
        // cursor is after "cd"; move to start of second line
        c.move_home();
        c.move_up(); // cursor now at end of "ab"
        c.move_end();
        c.backspace(); // no-op, cursor at end of "ab", deletes 'b' -> "a"
        assert_eq!(c.text(), "a\ncd");
    }

    #[test]
    fn backspace_at_start_of_second_line_joins() {
        let mut c = Composer::new();
        c.insert_str("ab\ncd");
        // position cursor at (1, 0): start of "cd"
        c.move_end();
        c.move_left(); // after 'd' -> (1,1)
        c.move_left(); // -> (1,0)
        c.backspace(); // join -> "abcd"
        assert_eq!(c.text(), "abcd");
    }

    #[test]
    fn move_left_right_respect_utf8() {
        let mut c = Composer::new();
        c.insert_str("aé😀b"); // 'é' 2 bytes, '😀' 4 bytes
        c.move_end();
        c.move_left();
        assert_eq!(c.cursor().1, "aé😀".len()); // before 'b'
        c.move_left();
        assert_eq!(c.cursor().1, "aé".len()); // before '😀'
        c.move_right();
        assert_eq!(c.cursor().1, "aé😀".len());
    }

    #[test]
    fn delete_forward_at_end_joins_next_line() {
        let mut c = Composer::new();
        c.insert_str("ab\ncd");
        c.move_home();
        c.move_up(); // now at start of "ab"
        c.move_end(); // end of "ab"
        c.delete_forward(); // join -> "abcd"
        assert_eq!(c.text(), "abcd");
    }

    #[test]
    fn move_up_down_clamp_column_to_shorter_line() {
        let mut c = Composer::new();
        c.insert_str("short\nlonger line");
        c.move_end(); // end of "longer line"
        c.move_up(); // to "short", clamped
        assert_eq!(c.cursor().0, 0);
        assert_eq!(c.cursor().1, "short".len());
    }

    #[test]
    fn clear_resets() {
        let mut c = Composer::new();
        c.insert_str("abc\ndef");
        c.clear();
        assert!(c.is_empty());
    }
}
