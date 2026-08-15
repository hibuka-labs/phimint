//! TUI application state: the independent `RuntimeEvent` consumer.
//!
//! The agent loop (phi-agent) is untouched; its `RuntimeEvent`s are forwarded
//! here through a channel and mapped onto visible state — a scrollable output
//! buffer, a tool-call log, a status state machine, and the composer. This is
//! the same event stream the `print_event` REPL renderer consumed, so nothing
//! in the framework needed to change for the TUI to exist.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyModifiers};
use phi_agent::{ApprovalDecision, ApprovalRequest, RuntimeEvent};

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
    Done,
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

/// A single tool invocation, filled in across `ToolCallStarted`/`Finished`.
#[derive(Debug, Clone)]
pub struct ToolCallEntry {
    pub agent_id: Option<String>,
    pub tool_name: String,
    pub args: String,
    pub summary: Option<String>,
    pub denied: bool,
    pub done: bool,
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
    pub tool_log: Vec<ToolCallEntry>,
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
            tool_log: Vec::new(),
            composer: Composer::new(),
            status: AgentStatus::Idle,
            running: false,
            scroll_offset: 0,
            follow_bottom: true,
            approval_queue: VecDeque::new(),
            pending_text: String::new(),
            pending_thought: String::new(),
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
                self.tool_log.push(ToolCallEntry {
                    agent_id,
                    tool_name: tool_name.clone(),
                    args: one_line(&args_json, 80),
                    summary: None,
                    denied: false,
                    done: false,
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
                // Pair with the most recent in-progress entry of that tool name.
                if let Some(entry) = self
                    .tool_log
                    .iter_mut()
                    .rev()
                    .find(|e| !e.done && e.tool_name == tool_name)
                {
                    entry.summary = Some(one_line(&summary, 80));
                    entry.denied = denied;
                    entry.done = true;
                }
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
    }

    fn push_thought(&mut self, text: &str) {
        if !self.pending_text.is_empty() {
            self.flush_text();
        }
        self.pending_thought.push_str(text);
    }

    fn flush_pending(&mut self) {
        self.flush_thought();
        self.flush_text();
    }

    fn flush_thought(&mut self) {
        if self.pending_thought.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_thought);
        for line in wrap(&text, WRAP_WIDTH) {
            self.output.push(OutputLine {
                text: line,
                kind: LineKind::Thought,
            });
        }
    }

    fn flush_text(&mut self) {
        if self.pending_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_text);
        for line in wrap(&text, WRAP_WIDTH) {
            self.output.push(OutputLine {
                text: line,
                kind: LineKind::Normal,
            });
        }
    }

    /// The live (uncommitted) streaming text, for progressive rendering before
    /// the next structural event flushes it into `output`. Text and thought are
    /// mutually exclusive (`push_text`/`push_thought` flush the other first).
    pub fn streaming_tail(&self) -> Option<(&str, LineKind)> {
        if !self.pending_text.is_empty() {
            Some((self.pending_text.as_str(), LineKind::Normal))
        } else if !self.pending_thought.is_empty() {
            Some((self.pending_thought.as_str(), LineKind::Thought))
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

/// Hard-wrap text to `width` columns: split on existing newlines, then break
/// over-long runs at char boundaries (greedy, 1 column per char).
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
            if col == width {
                out.push(std::mem::take(&mut line));
                col = 0;
            }
            line.push(c);
            col += 1;
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
    fn tool_log_accumulates_and_pairs_finish() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
        app.handle_event(TuiEvent::Runtime(tool_started("verify")));
        assert_eq!(app.tool_log.len(), 2);
        assert!(!app.tool_log[0].done);

        app.handle_event(TuiEvent::Runtime(tool_finished("read_file", false)));
        let read = app.tool_log.iter().find(|e| e.tool_name == "read_file").unwrap();
        assert!(read.done);
        assert_eq!(read.summary.as_deref(), Some("done"));
        // verify is still in flight
        let verify = app.tool_log.iter().find(|e| e.tool_name == "verify").unwrap();
        assert!(!verify.done);
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
    fn streaming_tail_exposes_uncommitted_text() {
        let mut app = App::new();
        assert_eq!(app.streaming_tail(), None);
        app.handle_event(TuiEvent::Runtime(text("hel")));
        app.handle_event(TuiEvent::Runtime(text("lo")));
        assert_eq!(app.streaming_tail(), Some(("hello", LineKind::Normal)));
        // A structural event flushes the tail into committed output.
        app.handle_event(TuiEvent::Runtime(tool_started("verify")));
        assert_eq!(app.streaming_tail(), None);
        assert_eq!(app.output.len(), 1);
    }

    #[test]
    fn streaming_tail_flips_to_text_after_thought() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(thought("hmm")));
        assert_eq!(app.streaming_tail().map(|(t, _)| t), Some("hmm"));
        app.handle_event(TuiEvent::Runtime(text("answer")));
        assert_eq!(app.streaming_tail(), Some(("answer", LineKind::Normal)));
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
