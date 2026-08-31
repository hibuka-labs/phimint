//! Streaming tail state machine: accumulates fragmented `TextDelta`/`ThoughtDelta`
//! into a pending buffer and flushes whole lines on structural events.
//!
//! Extracted from the `App` god-object (previously `pending_text`/`pending_thought`/
//! `pending_agent`/`tail_wrap` + the `push_*`/`flush_*` methods). The state is
//! now unit-testable standalone; `App` owns one instance and commits the returned
//! [`OutputLine`]s into its transcript.

use crate::ui::app::{LineKind, OutputLine};
use crate::ui::wrap::WrapCache;

/// Live, uncommitted streaming state for one turn.
///
/// Text is delivered as one delta per token; accumulating here and splitting
/// into lines only when a structural event (or style switch) forces a flush
/// keeps ~1000 deltas/turn from producing ~1000 output rows.
#[derive(Debug)]
pub struct StreamState {
    pending_text: String,
    pending_thought: String,
    pending_agent: Option<String>,
    tail_wrap: WrapCache,
}

impl StreamState {
    pub fn new(width: usize) -> Self {
        Self {
            pending_text: String::new(),
            pending_thought: String::new(),
            pending_agent: None,
            tail_wrap: WrapCache::new(width),
        }
    }

    /// Accumulate an AI-prose delta. Flushes any pending thought (and pending
    /// text from a different agent) first; returns the lines committed by those
    /// flushes so the caller can append them to the transcript in order.
    pub fn push_text(&mut self, text: &str, agent: Option<&str>) -> Vec<OutputLine> {
        let mut out = Vec::new();
        if !self.pending_thought.is_empty() {
            out.extend(self.flush_thought());
        }
        let agent = agent.map(str::to_string);
        if !self.pending_text.is_empty() && self.pending_agent != agent {
            out.extend(self.flush_text());
        }
        if self.pending_text.is_empty() {
            self.pending_agent = agent;
        }
        self.pending_text.push_str(text);
        self.tail_wrap.extend(text);
        out
    }

    /// Accumulate a reasoning delta (flushed into a Thought block on switch).
    pub fn push_thought(&mut self, text: &str, agent: Option<&str>) -> Vec<OutputLine> {
        let mut out = Vec::new();
        if !self.pending_text.is_empty() {
            out.extend(self.flush_text());
        }
        let agent = agent.map(str::to_string);
        if !self.pending_thought.is_empty() && self.pending_agent != agent {
            out.extend(self.flush_thought());
        }
        if self.pending_thought.is_empty() {
            self.pending_agent = agent;
        }
        self.pending_thought.push_str(text);
        self.tail_wrap.extend(text);
        out
    }

    /// Commit any pending text/thought into whole lines (thought first, then
    /// prose), returning the lines to append to the transcript.
    pub fn flush(&mut self) -> Vec<OutputLine> {
        let mut out = self.flush_thought();
        out.extend(self.flush_text());
        out
    }

    pub fn has_pending_text(&self) -> bool {
        !self.pending_text.is_empty()
    }

    pub fn has_pending_thought(&self) -> bool {
        !self.pending_thought.is_empty()
    }

    /// The agent accumulating the current pending buffer, if any. `None` for the
    /// root agent or when nothing is pending.
    pub fn pending_agent(&self) -> Option<&str> {
        self.pending_agent.as_deref()
    }

    /// The live streaming tail as incrementally-wrapped lines. Returns `None`
    /// when there is no uncommitted text/thought.
    pub fn tail_lines(&self) -> Option<(&[String], LineKind)> {
        if !self.pending_text.is_empty() {
            Some((self.tail_wrap.lines(), LineKind::Normal))
        } else if !self.pending_thought.is_empty() {
            Some((self.tail_wrap.lines(), LineKind::Thought))
        } else {
            None
        }
    }

    /// Raw pending text for markdown rendering (streaming tail).
    pub fn tail_raw(&self) -> Option<(&str, LineKind)> {
        if !self.pending_text.is_empty() {
            Some((&self.pending_text, LineKind::Normal))
        } else if !self.pending_thought.is_empty() {
            Some((&self.pending_thought, LineKind::Thought))
        } else {
            None
        }
    }

    /// Rebuild the tail cache at a new wrap width, keeping any pending text.
    /// Called from `App::set_wrap_width` on terminal resize.
    pub fn rewrap(&mut self, width: usize) {
        let source = if !self.pending_text.is_empty() {
            self.pending_text.clone()
        } else if !self.pending_thought.is_empty() {
            self.pending_thought.clone()
        } else {
            String::new()
        };
        self.tail_wrap = WrapCache::new(width);
        if !source.is_empty() {
            self.tail_wrap.extend(&source);
        }
    }

    fn flush_thought(&mut self) -> Vec<OutputLine> {
        if self.pending_thought.is_empty() {
            return Vec::new();
        }
        let original = self.pending_thought.clone();
        self.pending_thought.clear();
        let prefix = self
            .pending_agent
            .take()
            .map(|p| format!("[{p}] "))
            .unwrap_or_default();
        let prefix = prefix.as_str();
        let mut out = Vec::with_capacity(self.tail_wrap.lines().len());
        for (i, line) in self.tail_wrap.lines().iter().enumerate() {
            let text = if i == 0 && !prefix.is_empty() {
                format!("{prefix}{line}")
            } else {
                line.clone()
            };
            out.push(OutputLine {
                spans: None,
                original: if i == 0 { Some(original.clone()) } else { None },
                detail: None,
                text,
                kind: LineKind::Thought,
            });
        }
        self.tail_wrap.reset();
        out
    }

    fn flush_text(&mut self) -> Vec<OutputLine> {
        if self.pending_text.is_empty() {
            return Vec::new();
        }
        let original = self.pending_text.clone();
        self.pending_text.clear();
        let prefix = self
            .pending_agent
            .take()
            .map(|p| format!("[{p}] "))
            .unwrap_or_default();
        // Store the full raw text in ONE OutputLine. The renderer will parse
        // it through markdown and wrap at display time, so markdown syntax
        // that spans multiple visual lines (headings, bold, tables) is preserved.
        let text = if prefix.is_empty() {
            original.clone()
        } else {
            format!("{prefix}{original}")
        };
        self.tail_wrap.reset();
        vec![OutputLine {
            spans: None,
            original: Some(original),
            detail: None,
            text,
            kind: LineKind::Normal,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_wraps_long_text_at_new_width() {
        let mut st = StreamState::new(100);
        let long = "a".repeat(250);
        st.push_text(&long, None);
        let (lines, kind) = st.tail_lines().expect("tail present");
        assert_eq!(
            lines,
            ["a".repeat(100), "a".repeat(100), "a".repeat(50)].as_slice()
        );
        assert_eq!(kind, LineKind::Normal);
    }

    #[test]
    fn push_accumulates_and_flush_commits() {
        let mut st = StreamState::new(100);
        assert!(st.tail_lines().is_none());
        let l = st.push_text("hel", None);
        assert!(l.is_empty(), "no flush on same-agent accumulation");
        let l = st.push_text("lo", None);
        assert!(l.is_empty());
        let (lines, _) = st.tail_raw().expect("tail present");
        assert_eq!(lines, "hello");
        // A structural flush commits the raw text as one Normal line.
        let flushed = st.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].text, "hello");
        assert_eq!(flushed[0].kind, LineKind::Normal);
        assert!(st.tail_lines().is_none());
    }

    #[test]
    fn thought_flips_to_text_with_flush() {
        let mut st = StreamState::new(100);
        st.push_thought("hmm", None);
        let (lines, kind) = st.tail_lines().expect("thought tail present");
        assert_eq!(lines, ["hmm".to_string()].as_slice());
        assert_eq!(kind, LineKind::Thought);
        // Switching to prose flushes the thought into committed lines.
        let flushed = st.push_text("answer", None);
        assert_eq!(flushed.len(), 1, "thought flush returns one Thought line");
        assert_eq!(flushed[0].kind, LineKind::Thought);
        let (_, kind) = st.tail_raw().expect("text tail present");
        assert_eq!(kind, LineKind::Normal);
    }

    #[test]
    fn agent_change_flushes_pending_text() {
        let mut st = StreamState::new(100);
        st.push_text("root text", None);
        let flushed = st.push_text("child text", Some("root/searcher"));
        // Root prose committed as a prefixed Normal line.
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].kind, LineKind::Normal);
        assert_eq!(flushed[0].text, "root text");
    }
}
