//! TUI application state: the independent `RuntimeEvent` consumer.
//!
//! The agent loop (phi-agent) is untouched; its `RuntimeEvent`s are forwarded
//! here through a channel and mapped onto visible state — a scrollable output
//! buffer (tool calls render inline), a status state machine, and the composer.
//! This is the same event stream the `print_event` REPL renderer consumed, so
//! nothing in the framework needed to change for the TUI to exist.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyModifiers};
use phi_agent::{ApprovalDecision, ApprovalRequest, RuntimeEvent};
use unicode_width::UnicodeWidthChar;

use crate::approval::ApprovalItem;
use crate::ui::input::Composer;

/// Columns the output buffer wraps at. Fixed (rather than terminal-width) so a
/// resize doesn't reflow history; greedy char-boundary wrap, 1 column per char.
pub const WRAP_WIDTH: usize = 100;

/// Lines a PageUp/PageDown scrolls the output by.
const SCROLL_STEP: usize = 10;

/// The phase of a running turn, driven by the event stream (§9.6 state machine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Thinking,
    Streaming,
    ToolCall { tool: String },
    AwaitingApproval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running { phase: Phase },
}

/// Visual kind of an output line (mapped to a ratatui style in render.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Normal,
    Thought,
    Plan,
    Tool,
    Done,
    ToolResult,
    Error,
    System,
    Cancelled,
    Approval,
}

#[derive(Debug, Clone)]
pub struct OutputLine {
    pub text: String,
    pub kind: LineKind,
}

/// A user action surfaced from key handling, consumed by the TUI loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Submit(String),
    Quit,
    Cancel,
    Approve(ApprovalDecision),
}

/// An event delivered from the agent task to the TUI.
#[derive(Debug)]
pub enum TuiEvent {
    Runtime(RuntimeEvent),
    /// A turn ended with an error (surfaced as a red output line).
    TurnError(String),
    /// A turn completed (safety net in case `RunFinished` was never seen).
    TurnDone,
}

#[derive(Debug)]
pub struct App {
    pub output: Vec<OutputLine>,
    pub composer: Composer,
    pub status: AgentStatus,
    /// True while a turn is running (gates submitting another task).
    pub running: bool,
    /// Lines scrolled up from the bottom (`0` while following the bottom).
    pub scroll_offset: usize,
    /// Whether the output view is pinned to the newest lines.
    pub follow_bottom: bool,
    /// Pending approval requests (front = the popup currently shown).
    pub approval_queue: VecDeque<ApprovalItem>,
    pending_text: String,
    pending_thought: String,
    /// Incrementally-wrapped form of the live streaming tail, kept in sync with
    /// `pending_text`/`pending_thought` so rendering a long stream is O(new
    /// delta) per frame instead of re-wrapping the whole buffer.
    tail_wrap: WrapCache,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            composer: Composer::new(),
            status: AgentStatus::Idle,
            running: false,
            scroll_offset: 0,
            follow_bottom: true,
            approval_queue: VecDeque::new(),
            pending_text: String::new(),
            pending_thought: String::new(),
            tail_wrap: WrapCache::new(WRAP_WIDTH),
        }
    }

    /// Append a system/banner line (welcome, workspace, log path).
    pub fn push_system(&mut self, text: &str) {
        for line in wrap(text, WRAP_WIDTH) {
            self.output.push(OutputLine {
                text: line,
                kind: LineKind::System,
            });
        }
    }

    // ── Event handling ────────────────────────────────────────────────────

    pub fn handle_event(&mut self, ev: TuiEvent) {
        match ev {
            TuiEvent::Runtime(ev) => self.handle_runtime(ev),
            TuiEvent::TurnError(msg) => {
                self.flush_pending();
                for line in wrap(&format!("❌ {msg}"), WRAP_WIDTH) {
                    self.output.push(OutputLine {
                        text: line,
                        kind: LineKind::Error,
                    });
                }
                self.status = AgentStatus::Idle;
                self.running = false;
            }
            TuiEvent::TurnDone => {
                // `RunFinished` (root) already handles the normal path; this is
                // the backstop for turns that end without one.
                self.flush_pending();
                self.status = AgentStatus::Idle;
                self.running = false;
            }
        }
    }

    fn handle_runtime(&mut self, ev: RuntimeEvent) {
        match ev {
            RuntimeEvent::TextDelta { text, .. } => {
                self.push_text(&text);
                self.status = AgentStatus::Running {
                    phase: Phase::Streaming,
                };
            }
            RuntimeEvent::ThoughtDelta { text, .. } => {
                self.push_thought(&text);
                self.status = AgentStatus::Running {
                    phase: Phase::Thinking,
                };
            }
            RuntimeEvent::ToolCallStarted {
                tool_name,
                args_json,
                agent_id,
                ..
            } => {
                self.flush_pending();
                // Tools render inline in the transcript (Claude Code style): an
                // invocation line now, a result line on `ToolCallFinished`.
                let agent = agent_id
                    .as_deref()
                    .map(|a| format!("[{a}] "))
                    .unwrap_or_default();
                let args = one_line(&args_json, 80);
                let text = if args.is_empty() {
                    format!("⏺ {agent}{tool_name}")
                } else {
                    format!("⏺ {agent}{tool_name} {args}")
                };
                self.output.push(OutputLine {
                    text,
                    kind: LineKind::Tool,
                });
                self.status = AgentStatus::Running {
                    phase: Phase::ToolCall { tool: tool_name },
                };
            }
            RuntimeEvent::ToolCallFinished {
                tool_name,
                summary,
                denied,
                ..
            } => {
                let (text, kind) = if denied {
                    (format!("  ⛔ {tool_name} denied"), LineKind::Error)
                } else {
                    let s = one_line(&summary, 80);
                    let text = if s.is_empty() {
                        format!("  ✓ {tool_name}")
                    } else {
                        format!("  ✓ {tool_name} {s}")
                    };
                    (text, LineKind::ToolResult)
                };
                self.output.push(OutputLine { text, kind });
                self.status = AgentStatus::Running {
                    phase: Phase::Thinking,
                };
            }
            RuntimeEvent::PlanUpdated {
                objective, plan, ..
            } => {
                self.flush_pending();
                self.output.push(OutputLine {
                    text: format!("📋 {objective}"),
                    kind: LineKind::Plan,
                });
                for item in &plan {
                    self.output.push(OutputLine {
                        text: format!("   - {}", item.step),
                        kind: LineKind::Plan,
                    });
                }
            }
            RuntimeEvent::AwaitingApproval { request, .. } => {
                // Log line for history; the interactive popup (5b) is driven by
                // the approval queue, which the QueuedApprovalHandler feeds.
                self.flush_pending();
                self.output.push(OutputLine {
                    text: format!("⚠️  approval: {}", request.title),
                    kind: LineKind::Approval,
                });
                self.output.push(OutputLine {
                    text: format!("     {}", request.message),
                    kind: LineKind::Approval,
                });
                self.status = AgentStatus::Running {
                    phase: Phase::AwaitingApproval,
                };
            }
            RuntimeEvent::RunFinished { agent_id, .. } => {
                self.flush_pending();
                // Only the root agent's finish ends the turn; sub-agents report
                // their own `RunFinished` with a non-empty `agent_id`.
                if agent_id.is_none() {
                    self.output.push(OutputLine {
                        text: "✅ done".to_string(),
                        kind: LineKind::Done,
                    });
                    self.status = AgentStatus::Idle;
                    self.running = false;
                }
            }
            RuntimeEvent::RunCancelled { agent_id, .. } => {
                self.flush_pending();
                if agent_id.is_none() {
                    self.output.push(OutputLine {
                        text: "⏹ cancelled".to_string(),
                        kind: LineKind::Cancelled,
                    });
                    self.status = AgentStatus::Idle;
                    self.running = false;
                }
            }
            _ => {}
        }
    }

    // Streaming text is fragmented (one delta per token); accumulate into a
    // pending buffer and split into lines only when a structural event or a
    // style switch forces a flush. This keeps ~1000 deltas/turn from producing
    // ~1000 output rows.

    fn push_text(&mut self, text: &str) {
        if !self.pending_thought.is_empty() {
            self.flush_thought();
        }
        self.pending_text.push_str(text);
        self.tail_wrap.extend(text);
    }

    fn push_thought(&mut self, text: &str) {
        if !self.pending_text.is_empty() {
            self.flush_text();
        }
        self.pending_thought.push_str(text);
        self.tail_wrap.extend(text);
    }

    fn flush_pending(&mut self) {
        self.flush_thought();
        self.flush_text();
    }

    fn flush_thought(&mut self) {
        if self.pending_thought.is_empty() {
            return;
        }
        self.pending_thought.clear();
        for line in self.tail_wrap.lines() {
            self.output.push(OutputLine {
                text: line.clone(),
                kind: LineKind::Thought,
            });
        }
        self.tail_wrap.reset();
    }

    fn flush_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        self.pending_text.clear();
        for line in self.tail_wrap.lines() {
            self.output.push(OutputLine {
                text: line.clone(),
                kind: LineKind::Normal,
            });
        }
        self.tail_wrap.reset();
    }

    /// The live streaming tail as incrementally-wrapped lines (see [`WrapCache`]).
    /// Returns `None` when there is no uncommitted text/thought.
    pub fn streaming_tail_lines(&self) -> Option<(&[String], LineKind)> {
        if !self.pending_text.is_empty() {
            Some((self.tail_wrap.lines(), LineKind::Normal))
        } else if !self.pending_thought.is_empty() {
            Some((self.tail_wrap.lines(), LineKind::Thought))
        } else {
            None
        }
    }

    // ── Key handling ──────────────────────────────────────────────────────

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        use KeyCode::*;

        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let shift = modifiers.contains(KeyModifiers::SHIFT);

        // Ctrl+C always wins: cancel a running turn (or a pending approval),
        // otherwise quit.
        if ctrl && code == Char('c') {
            return Some(if self.running || !self.approval_queue.is_empty() {
                Action::Cancel
            } else {
                Action::Quit
            });
        }

        // While an approval popup is showing, route y/a/n and swallow the rest.
        if !self.approval_queue.is_empty() {
            return match code {
                Char('y') => Some(Action::Approve(ApprovalDecision::AllowOnce)),
                Char('a') => Some(Action::Approve(ApprovalDecision::AllowAlways)),
                Char('n') => Some(Action::Approve(ApprovalDecision::Deny)),
                _ => None,
            };
        }

        match code {
            Char('d') if ctrl => Some(Action::Quit),
            Esc => {
                self.composer.clear();
                None
            }
            Enter => {
                if shift {
                    self.composer.insert_char('\n');
                    None
                } else if !self.running && !self.composer.is_empty() {
                    let text = self.composer.text();
                    self.composer.clear();
                    self.running = true;
                    self.follow_bottom = true;
                    self.scroll_offset = 0;
                    self.status = AgentStatus::Running {
                        phase: Phase::Thinking,
                    };
                    Some(Action::Submit(text))
                } else {
                    None
                }
            }
            Backspace => {
                self.composer.backspace();
                None
            }
            Delete => {
                self.composer.delete_forward();
                None
            }
            Left => {
                self.composer.move_left();
                None
            }
            Right => {
                self.composer.move_right();
                None
            }
            Up => {
                self.composer.move_up();
                None
            }
            Down => {
                self.composer.move_down();
                None
            }
            Home => {
                self.composer.move_home();
                None
            }
            End => {
                self.composer.move_end();
                None
            }
            PageUp => {
                self.scroll_up();
                None
            }
            PageDown => {
                self.scroll_down();
                None
            }
            Char(c) => {
                self.composer.insert_char(c);
                None
            }
            _ => None,
        }
    }

    pub(crate) fn scroll_up(&mut self) {
        self.follow_bottom = false;
        self.scroll_offset += SCROLL_STEP;
    }

    pub(crate) fn scroll_down(&mut self) {
        if self.follow_bottom {
            return;
        }
        if self.scroll_offset <= SCROLL_STEP {
            self.scroll_offset = 0;
            self.follow_bottom = true;
        } else {
            self.scroll_offset -= SCROLL_STEP;
        }
    }

    /// The status-bar text for the current state (§9.6).
    pub fn status_line(&self) -> String {
        match &self.status {
            AgentStatus::Idle => {
                "⏸ Idle — Enter send · Shift+Enter newline · wheel/PgUp/PgDn scroll · Ctrl+C quit".to_string()
            }
            AgentStatus::Running { phase } => match phase {
                Phase::Thinking => "🤔 thinking… (Ctrl+C cancel)".to_string(),
                Phase::Streaming => "💬 streaming… (Ctrl+C cancel)".to_string(),
                Phase::ToolCall { tool } => format!("🔧 {tool} (Ctrl+C cancel)"),
                Phase::AwaitingApproval => "⚠️ waiting approval… (y/a/n, Ctrl+C cancel)".to_string(),
            },
        }
    }

    /// The request currently shown by the approval popup (front of the queue).
    pub fn current_approval(&self) -> Option<&ApprovalRequest> {
        self.approval_queue.front().map(|item| &item.request)
    }

    /// Whether an approval popup is pending.
    pub fn has_pending_approval(&self) -> bool {
        !self.approval_queue.is_empty()
    }

    /// Resolve the front approval with `decision` (popping it off the queue).
    pub fn approve_front(&mut self, decision: ApprovalDecision) {
        if let Some(item) = self.approval_queue.pop_front() {
            let _ = item.decision_tx.send(decision);
        }
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
struct WrapCache {
    width: usize,
    /// Wrapped lines; the last element is always the current (partial) line.
    lines: Vec<String>,
    /// Display columns currently used by the last line.
    col: usize,
}

impl WrapCache {
    fn new(width: usize) -> Self {
        Self {
            width: width.max(1),
            lines: vec![String::new()],
            col: 0,
        }
    }

    fn reset(&mut self) {
        self.lines.clear();
        self.lines.push(String::new());
        self.col = 0;
    }

    fn extend(&mut self, text: &str) {
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

    fn lines(&self) -> &[String] {
        &self.lines
    }
}

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
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::SessionId;

    fn text(s: &str) -> RuntimeEvent {
        RuntimeEvent::TextDelta {
            session_id: SessionId::new(1),
            text: s.to_string(),
            agent_id: None,
            trace_id: None,
        }
    }

    fn thought(s: &str) -> RuntimeEvent {
        RuntimeEvent::ThoughtDelta {
            session_id: SessionId::new(1),
            text: s.to_string(),
            agent_id: None,
            trace_id: None,
        }
    }

    fn tool_started(name: &str) -> RuntimeEvent {
        RuntimeEvent::ToolCallStarted {
            session_id: SessionId::new(1),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
            agent_id: None,
            trace_id: None,
        }
    }

    fn tool_finished(name: &str, denied: bool) -> RuntimeEvent {
        RuntimeEvent::ToolCallFinished {
            session_id: SessionId::new(1),
            tool_name: name.to_string(),
            summary: "done".to_string(),
            agent_id: None,
            trace_id: None,
            denied,
        }
    }

    fn run_finished(agent_id: Option<&str>) -> RuntimeEvent {
        RuntimeEvent::RunFinished {
            session_id: SessionId::new(1),
            agent_id: agent_id.map(String::from),
            trace_id: None,
        }
    }

    fn submit(app: &mut App, input: &str) -> Action {
        app.composer.clear();
        app.composer.insert_str(input);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap()
    }

    #[test]
    fn status_tracks_state_machine() {
        let mut app = App::new();
        assert_eq!(app.status, AgentStatus::Idle);

        app.handle_event(TuiEvent::Runtime(thought("hmm")));
        assert_eq!(app.status, AgentStatus::Running { phase: Phase::Thinking });

        app.handle_event(TuiEvent::Runtime(text("answer")));
        assert_eq!(app.status, AgentStatus::Running { phase: Phase::Streaming });

        app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
        assert_eq!(
            app.status,
            AgentStatus::Running {
                phase: Phase::ToolCall {
                    tool: "read_file".into()
                }
            }
        );

        app.handle_event(TuiEvent::Runtime(tool_finished("read_file", false)));
        assert_eq!(app.status, AgentStatus::Running { phase: Phase::Thinking });

        app.handle_event(TuiEvent::Runtime(run_finished(None)));
        assert_eq!(app.status, AgentStatus::Idle);
        assert!(!app.running);
    }

    #[test]
    fn tool_calls_render_inline_in_output() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
        app.handle_event(TuiEvent::Runtime(tool_finished("read_file", false)));
        app.handle_event(TuiEvent::Runtime(tool_finished("execute_command", true)));

        // Invocation + result lines land inline in `output`, in event order.
        let texts: Vec<&str> = app.output.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "⏺ read_file {}",
                "  ✓ read_file done",
                "  ⛔ execute_command denied",
            ]
        );
        assert_eq!(app.output[0].kind, LineKind::Tool);
        assert_eq!(app.output[1].kind, LineKind::ToolResult);
        assert_eq!(app.output[2].kind, LineKind::Error);
    }

    #[test]
    fn run_finished_from_sub_agent_does_not_end_turn() {
        let mut app = App::new();
        app.running = true;
        app.handle_event(TuiEvent::Runtime(run_finished(Some("sub/1"))));
        // Sub-agent finish must not flip the top-level status to Idle.
        assert!(app.running);
        app.handle_event(TuiEvent::Runtime(run_finished(None)));
        assert!(!app.running);
        assert_eq!(app.status, AgentStatus::Idle);
    }

    #[test]
    fn text_deltas_coalesce_into_fewer_lines() {
        let mut app = App::new();
        // Three fragments with no newline → one output line, not three.
        app.handle_event(TuiEvent::Runtime(text("hel")));
        app.handle_event(TuiEvent::Runtime(text("lo ")));
        app.handle_event(TuiEvent::Runtime(text("world")));
        app.handle_event(TuiEvent::Runtime(tool_started("verify"))); // flush
        let normals: Vec<_> = app
            .output
            .iter()
            .filter(|l| l.kind == LineKind::Normal)
            .collect();
        assert_eq!(normals.len(), 1);
        assert_eq!(normals[0].text, "hello world");
    }

    #[test]
    fn text_with_newline_splits_lines() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(text("a\nb")));
        app.handle_event(TuiEvent::Runtime(tool_started("verify")));
        let normals: Vec<_> = app
            .output
            .iter()
            .filter(|l| l.kind == LineKind::Normal)
            .collect();
        assert_eq!(normals.len(), 2);
        assert_eq!(normals[0].text, "a");
        assert_eq!(normals[1].text, "b");
    }

    #[test]
    fn thought_renders_before_text() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(thought("thinking")));
        app.handle_event(TuiEvent::Runtime(text("answer")));
        app.handle_event(TuiEvent::Runtime(run_finished(None)));
        assert_eq!(app.output[0].kind, LineKind::Thought);
        assert_eq!(app.output[0].text, "thinking");
        assert_eq!(app.output[1].kind, LineKind::Normal);
        assert_eq!(app.output[1].text, "answer");
    }

    #[test]
    fn submit_requires_nonempty_and_not_running() {
        let mut app = App::new();
        // Empty composer → no submit.
        app.composer.clear();
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);

        // Non-empty → Submit + running.
        let action = submit(&mut app, "do a thing");
        assert_eq!(action, Action::Submit("do a thing".to_string()));
        assert!(app.running);

        // Already running → Enter ignored.
        app.composer.insert_str("second");
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    }

    #[test]
    fn shift_enter_inserts_newline_not_submit() {
        let mut app = App::new();
        app.composer.insert_str("a");
        let r = app.handle_key(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(r, None);
        assert_eq!(app.composer.text(), "a\n");
    }

    #[test]
    fn ctrl_c_cancels_when_running() {
        let mut app = App::new();
        app.running = false;
        assert_eq!(
            app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::Quit)
        );
        app.running = true;
        assert_eq!(
            app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::Cancel)
        );
    }

    #[test]
    fn turn_error_appends_red_line_and_resets() {
        let mut app = App::new();
        app.running = true;
        app.handle_event(TuiEvent::TurnError("boom".to_string()));
        assert!(!app.running);
        assert_eq!(app.status, AgentStatus::Idle);
        assert_eq!(app.output.last().unwrap().kind, LineKind::Error);
        assert!(app.output.last().unwrap().text.contains("boom"));
    }

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

    #[test]
    fn streaming_tail_lines_wraps_long_text() {
        let mut app = App::new();
        // 250 chars at WRAP_WIDTH=100 → three wrapped lines.
        let long = "a".repeat(250);
        app.handle_event(TuiEvent::Runtime(text(&long)));
        let (lines, kind) = app.streaming_tail_lines().expect("tail present");
        assert_eq!(
            lines,
            ["a".repeat(100), "a".repeat(100), "a".repeat(50)].as_slice()
        );
        assert_eq!(kind, LineKind::Normal);
    }

    #[test]
    fn streaming_tail_exposes_uncommitted_text() {
        let mut app = App::new();
        assert_eq!(app.streaming_tail_lines(), None);
        app.handle_event(TuiEvent::Runtime(text("hel")));
        app.handle_event(TuiEvent::Runtime(text("lo")));
        let (lines, kind) = app.streaming_tail_lines().expect("tail present");
        assert_eq!(lines, ["hello".to_string()].as_slice());
        assert_eq!(kind, LineKind::Normal);
        // A structural event flushes the tail into committed output (and, for a
        // tool call, also appends an inline invocation line).
        app.handle_event(TuiEvent::Runtime(tool_started("verify")));
        assert_eq!(app.streaming_tail_lines(), None);
        assert_eq!(app.output[0].text, "hello");
        assert_eq!(app.output[1].kind, LineKind::Tool);
    }

    #[test]
    fn streaming_tail_flips_to_text_after_thought() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(thought("hmm")));
        let (lines, kind) = app.streaming_tail_lines().expect("thought tail present");
        assert_eq!(lines, ["hmm".to_string()].as_slice());
        assert_eq!(kind, LineKind::Thought);
        app.handle_event(TuiEvent::Runtime(text("answer")));
        let (lines, kind) = app.streaming_tail_lines().expect("text tail present");
        assert_eq!(lines, ["answer".to_string()].as_slice());
        assert_eq!(kind, LineKind::Normal);
    }

    #[test]
    fn scroll_up_steps_from_bottom_not_noop() {
        let mut app = App::new();
        for i in 0..100 {
            app.output.push(OutputLine { text: format!("line {i}"), kind: LineKind::Normal });
        }
        app.scroll_up();
        assert!(!app.follow_bottom);
        assert_eq!(app.scroll_offset, SCROLL_STEP);
    }

    #[test]
    fn scroll_down_reenters_follow_bottom() {
        let mut app = App::new();
        for i in 0..100 {
            app.output.push(OutputLine { text: format!("line {i}"), kind: LineKind::Normal });
        }
        app.scroll_up();
        app.scroll_up();
        assert_eq!(app.scroll_offset, 2 * SCROLL_STEP);
        app.scroll_down();
        assert_eq!(app.scroll_offset, SCROLL_STEP);
        app.scroll_offset = SCROLL_STEP;
        app.scroll_down();
        assert!(app.follow_bottom);
        assert_eq!(app.scroll_offset, 0);
    }

    fn pending_approval(app: &mut App) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.approval_queue.push_back(ApprovalItem {
            request: ApprovalRequest {
                title: "write_file".to_string(),
                message: "Write file: src/lib.rs".to_string(),
                action_key: None,
                risk_level: phi_agent::RiskLevel::Sensitive,
                raw: None,
            },
            decision_tx: tx,
        });
    }

    #[test]
    fn approval_popup_routes_yan_and_swallows_others() {
        let mut app = App::new();
        pending_approval(&mut app);
        assert_eq!(app.current_approval().map(|r| r.title.as_str()), Some("write_file"));
        assert!(app.has_pending_approval());

        // y/a/n route to the matching decision.
        assert_eq!(
            app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE),
            Some(Action::Approve(ApprovalDecision::AllowOnce))
        );
        assert_eq!(
            app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(Action::Approve(ApprovalDecision::AllowAlways))
        );
        assert_eq!(
            app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE),
            Some(Action::Approve(ApprovalDecision::Deny))
        );

        // Other keys (including Enter/submit) are swallowed while a popup is up.
        assert_eq!(app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE), None);
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);

        // Ctrl+C cancels even when a popup is up (and no turn is running).
        assert_eq!(
            app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::Cancel)
        );
    }

    #[test]
    fn approve_front_pops_and_sends() {
        let mut app = App::new();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.approval_queue.push_back(ApprovalItem {
            request: ApprovalRequest {
                title: "t".to_string(),
                message: "m".to_string(),
                action_key: None,
                risk_level: phi_agent::RiskLevel::Safe,
                raw: None,
            },
            decision_tx: tx,
        });

        app.approve_front(ApprovalDecision::Deny);
        assert!(!app.has_pending_approval());
        assert_eq!(rx.try_recv().unwrap(), ApprovalDecision::Deny);
    }

    #[test]
    fn awaiting_approval_sets_status() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(RuntimeEvent::AwaitingApproval {
            session_id: SessionId::new(1),
            request: ApprovalRequest {
                title: "write_file".to_string(),
                message: "Write file: src/lib.rs".to_string(),
                action_key: None,
                risk_level: phi_agent::RiskLevel::Sensitive,
                raw: None,
            },
            agent_id: None,
            trace_id: None,
        }));
        assert_eq!(
            app.status,
            AgentStatus::Running { phase: Phase::AwaitingApproval }
        );
    }
}
