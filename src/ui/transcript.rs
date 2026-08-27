//! Committed transcript buffer: output lines, the plan block, and re-wrapping.
//!
//! Extracted from the `App` god-object (previously `output`/`plan_range`/
//! `wrap_width` + `push_system`/`push_banner`/`push_user`/`rewrap_output`).
//! `App` owns one instance: runtime events commit lines into it, and the
//! renderer reads them back for display / selection / copy.

use std::ops::Range;

use crate::banner::BannerRow;
use crate::ui::app::{LineKind, OutputLine};
use crate::ui::wrap::wrap;

/// Default columns the output buffer wraps at. Overridden at runtime to match
/// the terminal width (see [`Transcript::set_wrap_width`]).
pub const DEFAULT_WRAP_WIDTH: usize = 100;

/// The committed side of the transcript: every `OutputLine` the agent's
/// `RuntimeEvent`s (and user turns) have produced, plus the tracked plan block
/// and the current wrap width.
#[derive(Debug)]
pub struct Transcript {
    pub output: Vec<OutputLine>,
    /// Start of the current plan block in `output`. `update_plan`'s contract is
    /// "full plan replaces previous", so a later update splices this range out
    /// and re-inserts at the same position instead of appending a duplicate.
    /// Cleared at each turn start.
    plan_range: Option<Range<usize>>,
    /// Dynamic wrap width for output lines, updated each frame to match the
    /// terminal width. Keeps output and composer widths aligned.
    wrap_width: usize,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            plan_range: None,
            wrap_width: DEFAULT_WRAP_WIDTH,
        }
    }

    pub fn len(&self) -> usize {
        self.output.len()
    }

    /// Append one committed line.
    pub fn push(&mut self, line: OutputLine) {
        self.output.push(line);
    }

    /// Append committed lines (e.g. flushed streaming text).
    pub fn extend(&mut self, lines: Vec<OutputLine>) {
        self.output.extend(lines);
    }

    /// Current output wrap width (columns).
    pub fn wrap_width(&self) -> usize {
        self.wrap_width
    }

    /// Update the output wrap width. When the width changes, committed output
    /// lines with an `original` source are re-wrapped at the new width.
    pub fn set_wrap_width(&mut self, width: usize) {
        let width = width.max(1);
        if width == self.wrap_width {
            return;
        }
        self.wrap_width = width;
        self.rewrap_output();
    }

    /// Append a system/banner line (welcome, workspace, log path).
    pub fn push_system(&mut self, text: &str) {
        for (i, line) in wrap(text, self.wrap_width).into_iter().enumerate() {
            self.output.push(OutputLine { spans: None,
                original: if i == 0 { Some(text.to_string()) } else { None },
                text: line,
                kind: LineKind::System,
            });
        }
    }

    /// Append startup-banner rows verbatim: one output line per row, **no**
    /// soft-wrap (the wordmark must stay whole; ratatui clips on narrow
    /// terminals). Styled runs become `spans`, resolved against `scheme` at
    /// render time; the plain `text` keeps copy/selection style-blind.
    pub fn push_banner(&mut self, rows: Vec<BannerRow>) {
        for row in rows {
            let (text, spans) = row.to_runs();
            self.output.push(OutputLine {
                text,
                kind: LineKind::System,
                spans: Some(spans),
                original: None,
            });
        }
    }

    /// Echo the user's submitted message into the transcript, so the frame log
    /// and output buffer retain the human side of the conversation (the JSONL
    /// turn log already stores `user_input`, but the rendered transcript didn't
    /// show it). `❯` on the first line, indented continuations after.
    pub fn push_user(&mut self, text: &str) {
        let mut first = true;
        for (i, line) in wrap(text, self.wrap_width).into_iter().enumerate() {
            let display = if first {
                first = false;
                format!("❯ {line}")
            } else {
                format!("  {line}")
            };
            self.output.push(OutputLine { spans: None,
                original: if i == 0 { Some(text.to_string()) } else { None },
                text: display,
                kind: LineKind::User,
            });
        }
    }

    /// Forget the tracked plan block. Called at each turn start so the next
    /// plan appends instead of replacing the previous turn's block.
    pub fn clear_plan(&mut self) {
        self.plan_range = None;
    }

    /// Replace the previous plan block in place so it stays anchored where it
    /// first appeared (the tool always sends the full plan). When no block is
    /// tracked yet, the new block is appended.
    pub fn replace_plan(&mut self, block: Vec<OutputLine>) {
        let new_len = block.len();
        let start = self
            .plan_range
            .as_ref()
            .map_or(self.output.len(), |r| r.start);
        if let Some(range) = self.plan_range.take() {
            self.output.splice(range, block);
        } else {
            self.output.extend(block);
        }
        self.plan_range = Some(start..start + new_len);
    }

    /// Re-wrap all committed output lines that have an `original` source.
    /// Called when the terminal width changes so output lines follow the new
    /// width instead of staying at the old wrap width.
    fn rewrap_output(&mut self) {
        let mut new: Vec<OutputLine> = Vec::with_capacity(self.output.len());
        for line in self.output.drain(..) {
            if let Some(ref original) = line.original {
                if original.is_empty() || line.spans.is_some() {
                    // Banner lines or empty originals: keep as-is.
                    new.push(line);
                    continue;
                }
                // Normal AI prose: keep as a single line; the renderer handles
                // markdown parsing and wrapping at display time.
                if line.kind == LineKind::Normal {
                    new.push(line);
                    continue;
                }
                let kind = line.kind;
                for (j, wrapped) in wrap(original, self.wrap_width).into_iter().enumerate() {
                    let display = if kind == LineKind::User {
                        if j == 0 {
                            format!("❯ {wrapped}")
                        } else {
                            format!("  {wrapped}")
                        }
                    } else {
                        wrapped
                    };
                    new.push(OutputLine {
                        text: display,
                        kind,
                        spans: None,
                        original: if j == 0 { Some(original.clone()) } else { None },
                    });
                }
            } else {
                new.push(line);
            }
        }
        self.output = new;
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::LineKind;

    fn texts(t: &Transcript) -> Vec<&str> {
        t.output.iter().map(|l| l.text.as_str()).collect()
    }

    fn plan_line(text: &str) -> OutputLine {
        OutputLine {
            spans: None,
            original: None,
            text: text.to_string(),
            kind: LineKind::Plan,
        }
    }

    #[test]
    fn push_system_wraps_and_marks_first_line_original() {
        let mut t = Transcript::new();
        t.set_wrap_width(10);
        t.push_system("hello world");
        assert_eq!(texts(&t), vec!["hello worl", "d"]);
        assert_eq!(t.output[0].original.as_deref(), Some("hello world"));
        assert!(t.output[1].original.is_none());
        assert_eq!(t.output[0].kind, LineKind::System);
    }

    #[test]
    fn push_user_prefixes_continuation_lines() {
        let mut t = Transcript::new();
        t.set_wrap_width(10);
        t.push_user("abcdefghijk");
        assert_eq!(texts(&t), vec!["❯ abcdefghij", "  k"]);
        assert_eq!(t.output[0].original.as_deref(), Some("abcdefghijk"));
    }

    #[test]
    fn set_wrap_width_rewraps_committed_lines() {
        let mut t = Transcript::new();
        t.set_wrap_width(20);
        t.push_user("abcdefghijk");
        assert_eq!(texts(&t), vec!["❯ abcdefghijk"]);
        // Narrower terminal: the user line re-wraps at the new width.
        t.set_wrap_width(10);
        assert_eq!(texts(&t), vec!["❯ abcdefghij", "  k"]);
        // Same width again: no-op, lines untouched.
        t.set_wrap_width(10);
        assert_eq!(texts(&t), vec!["❯ abcdefghij", "  k"]);
    }

    #[test]
    fn replace_plan_appends_then_replaces_in_place() {
        let mut t = Transcript::new();
        t.push_user("hello");
        t.replace_plan(vec![plan_line("📋 目标A")]);
        t.replace_plan(vec![plan_line("📋 目标B")]);
        // The second full-plan update replaced the first in place, so only one
        // plan block remains, anchored after the user line.
        assert_eq!(texts(&t), vec!["❯ hello", "📋 目标B"]);
    }

    #[test]
    fn clear_plan_makes_next_plan_append() {
        let mut t = Transcript::new();
        t.replace_plan(vec![plan_line("📋 A")]);
        // A new turn resets the tracked range, so the next plan appends
        // instead of replacing the previous turn's block.
        t.clear_plan();
        t.replace_plan(vec![plan_line("📋 B")]);
        assert_eq!(texts(&t), vec!["📋 A", "📋 B"]);
    }

    #[test]
    fn normal_lines_are_not_rewrapped() {
        // AI prose keeps one raw line (markdown rendering happens at display
        // time), so a width change must not split it.
        let mut t = Transcript::new();
        t.set_wrap_width(5);
        t.push(OutputLine {
            spans: None,
            original: Some("prose with markdown **bold**".to_string()),
            text: "prose with markdown **bold**".to_string(),
            kind: LineKind::Normal,
        });
        t.set_wrap_width(20);
        assert_eq!(texts(&t), vec!["prose with markdown **bold**"]);
    }
}
