//! TUI application state: the independent `RuntimeEvent` consumer.
//!
//! The agent loop (phi-agent) is untouched; its `RuntimeEvent`s are forwarded
//! here through a channel and mapped onto visible state — a scrollable output
//! buffer (tool calls render inline), a status state machine, and the composer.
//! This is the same event stream the `print_event` REPL renderer consumed, so
//! nothing in the framework needed to change for the TUI to exist.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use agent_base::{PlanStepStatus, UserEvent};
use phi_agent::{ApprovalDecision, ApprovalRequest, RuntimeEvent};
use unicode_width::UnicodeWidthChar;

use crate::approval::ApprovalItem;
use crate::banner::{BannerRow, ColorScheme, SpanSpec};
use crate::ui::input::Composer;
use crate::ui::mention::{self, Entry};

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

/// Live state of a sub-agent shown in the sub-agent status strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Running,
    Done,
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
    User,
}

#[derive(Debug, Clone)]
pub struct OutputLine {
    pub text: String,
    pub kind: LineKind,
    /// Optional styled byte-ranges into `text` (startup banner runs). `None`
    /// means the whole line takes the `kind` style. `text` is always the plain
    /// concatenation, so copy/selection and the frame capture stay style-blind.
    pub spans: Option<Vec<SpanSpec>>,
}

/// A line-range selection into `output` (inclusive). `anchor` is the drag
/// start and `head` the current line; they may be in either order, so
/// [`Selection::bounds`] normalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    /// Normalized inclusive `(lo, hi)` bounds, `lo <= hi`.
    fn bounds(self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

/// Right-click copy menu: anchor cell (top-left) and highlighted item index
/// (0 = copy, 1 = cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMenu {
    pub x: u16,
    pub y: u16,
    pub selected: usize,
}

/// Context-menu popup geometry, shared between rendering and mouse hit-testing
/// so a click lands exactly on the drawn popup.
pub const CONTEXT_MENU_W: u16 = 12;
pub const CONTEXT_MENU_H: u16 = 4; // border + two items

/// Clamp the menu's top-left so the fixed-size popup stays fully on screen.
pub fn context_menu_pos(x: u16, y: u16, area_w: u16, area_h: u16) -> (u16, u16) {
    (
        x.min(area_w.saturating_sub(CONTEXT_MENU_W)),
        y.min(area_h.saturating_sub(CONTEXT_MENU_H)),
    )
}

/// `@` mention picker state: the typed prefix (also echoed into the composer),
/// the directory it resolves to, the listed entries (synthetic `.` first), and
/// the highlighted index.
#[derive(Debug, Clone)]
pub struct Mention {
    /// What the user typed after `@` (e.g. `src/`, `../../demo/codex/`).
    pub prefix: String,
    /// Directory the prefix resolves to (what `entries` lists).
    pub dir: PathBuf,
    /// Listed entries; `entries[0]` is always the synthetic "use what I typed".
    pub entries: Vec<Entry>,
    pub selected: usize,
}

/// `/` skill picker state: shown when the user types `/` at the start of an
/// empty composer. Filters the loaded skill names by prefix and lets the user
/// select one with arrow keys + Enter.
#[derive(Debug, Clone)]
pub struct SlashPicker {
    /// What the user typed after `/` (e.g. `rev` for `/review`).
    pub prefix: String,
    /// Filtered entries: (name, description).
    pub entries: Vec<(String, String)>,
    /// Highlighted index.
    pub selected: usize,
}

/// A user action surfaced from key handling, consumed by the TUI loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Submit(String),
    Quit,
    Cancel,
    Approve(ApprovalDecision),
    /// Copy the assistant's last reply to the system clipboard (Ctrl+Y).
    CopyLastReply,
    /// Copy the active transcript selection to the system clipboard (Ctrl+C
    /// with a selection, or right-click → copy).
    CopySelection,
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
    /// Start of the current plan block in `output`. `update_plan`'s contract is
    /// "full plan replaces previous", so a later update splices this range out
    /// and re-inserts at the same position instead of appending a duplicate.
    /// Cleared at each turn start.
    plan_range: Option<Range<usize>>,
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
    /// Live sub-agent states, keyed by `agent_id` (`root/<task_name>`).
    pub sub_agents: BTreeMap<String, SubAgentStatus>,
    /// Active transcript selection (line indices into `output`), built by mouse
    /// drag. Stored as `anchor`/`head` so drag-up is supported.
    selection: Option<Selection>,
    /// Right-click copy menu, shown while a selection is active.
    context_menu: Option<ContextMenu>,
    /// `@` mention picker, open while the user is choosing a path.
    mention: Option<Mention>,
    /// `/` skill picker, open while the user is choosing a skill.
    slash: Option<SlashPicker>,
    /// Loaded skill summaries (name, description) from `SkillResolver`, powering the `/` picker.
    skill_summaries: Vec<(String, String)>,
    /// The workspace root — in-workspace paths render relative to it.
    pub workspace_root: PathBuf,
    /// The terminal color scheme (startup probe result, see main.rs). Banner
    /// styled runs resolve their colors against this at render time.
    scheme: ColorScheme,
    /// Output pane rect `(x, y, w, h)` in cells, refreshed each draw so mouse
    /// events can be hit-tested against the transcript.
    pub(crate) output_area: Option<(u16, u16, u16, u16)>,
    pending_text: String,
    pending_thought: String,
    /// The agent whose text/thought is currently accumulating in the pending
    /// buffer, so a sub-agent's stream can be `[path]`-prefixed on flush.
    pending_agent: Option<String>,
    /// Latest live tool-progress line (e.g. streaming `execute_command` output),
    /// shown in the status bar and cleared when the tool call finishes.
    live_progress: Option<String>,
    /// Transient status-bar notice (e.g. "📋 copied …"), cleared on the next key.
    notice: Option<String>,
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
            plan_range: None,
            composer: Composer::new(),
            status: AgentStatus::Idle,
            running: false,
            scroll_offset: 0,
            follow_bottom: true,
            approval_queue: VecDeque::new(),
            sub_agents: BTreeMap::new(),
            selection: None,
            context_menu: None,
            mention: None,
            slash: None,
            skill_summaries: Vec::new(),
            workspace_root: PathBuf::new(),
            scheme: ColorScheme::Dark,
            output_area: None,
            pending_text: String::new(),
            pending_thought: String::new(),
            pending_agent: None,
            live_progress: None,
            notice: None,
            tail_wrap: WrapCache::new(WRAP_WIDTH),
        }
    }

    /// Append a system/banner line (welcome, workspace, log path).
    pub fn push_system(&mut self, text: &str) {
        for line in wrap(text, WRAP_WIDTH) {
            self.output.push(OutputLine { spans: None,
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
            });
        }
    }

    /// The terminal color scheme banner spans resolve against at render time.
    pub fn scheme(&self) -> ColorScheme {
        self.scheme
    }

    /// Set the terminal color scheme (called once at TUI start).
    pub fn set_scheme(&mut self, scheme: ColorScheme) {
        self.scheme = scheme;
    }

    /// Echo the user's submitted message into the transcript, so the frame log
    /// and output buffer retain the human side of the conversation (the JSONL
    /// turn log already stores `user_input`, but the rendered transcript didn't
    /// show it). `❯` on the first line, indented continuations after.
    pub fn push_user(&mut self, text: &str) {
        let mut first = true;
        for line in wrap(text, WRAP_WIDTH) {
            let text = if first {
                first = false;
                format!("❯ {line}")
            } else {
                format!("  {line}")
            };
            self.output.push(OutputLine { spans: None,
                text,
                kind: LineKind::User,
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
                    self.output.push(OutputLine { spans: None,
                        text: line,
                        kind: LineKind::Error,
                    });
                }
                self.status = AgentStatus::Idle;
                self.running = false;
                self.sub_agents.clear();
            }
            TuiEvent::TurnDone => {
                // `RunFinished` (root) already handles the normal path; this is
                // the backstop for turns that end without one.
                self.flush_pending();
                self.status = AgentStatus::Idle;
                self.running = false;
                self.sub_agents.clear();
            }
        }
    }

    fn handle_runtime(&mut self, ev: RuntimeEvent) {
        match ev {
            RuntimeEvent::TextDelta { text, agent_id, .. } => {
                self.track_agent(agent_id.as_deref());
                self.push_text(&text, agent_id.as_deref());
                self.status = AgentStatus::Running {
                    phase: Phase::Streaming,
                };
            }
            RuntimeEvent::ThoughtDelta { text, agent_id, .. } => {
                self.track_agent(agent_id.as_deref());
                self.push_thought(&text, agent_id.as_deref());
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
                // Fresh tool call → drop any progress from the previous one.
                self.live_progress = None;
                // Tools render inline in the transcript (Claude Code style): an
                // invocation line now, a result line on `ToolCallFinished`.
                self.track_agent(agent_id.as_deref());
                let prefix = agent_prefix(agent_id.as_deref());
                // `update_plan` renders as a plan block (see `PlanUpdated`), so both
                // its invocation line (raw JSON args) and result line are noise —
                // suppress them. Its status transition is still applied.
                if tool_name != "update_plan" {
                    let args = one_line(&args_json, 80);
                    let text = if args.is_empty() {
                        format!("⏺ {prefix}{tool_name}")
                    } else {
                        format!("⏺ {prefix}{tool_name} {args}")
                    };
                    self.output.push(OutputLine { spans: None,
                        text,
                        kind: LineKind::Tool,
                    });
                }
                self.status = AgentStatus::Running {
                    phase: Phase::ToolCall { tool: tool_name },
                };
            }
            RuntimeEvent::ToolCallFinished {
                tool_name,
                summary,
                denied,
                agent_id,
                ..
            } => {
                self.track_agent(agent_id.as_deref());
                let prefix = agent_prefix(agent_id.as_deref());
                // `update_plan` renders as a plan block (see `PlanUpdated`), so its
                // tool-result line is redundant — suppress it. Everything else
                // still finalizes the tool-progress state.
                if tool_name != "update_plan" {
                    let (text, kind) = if denied {
                        (format!("  {prefix}⛔ {tool_name} denied"), LineKind::Error)
                    } else {
                        let s = one_line(&summary, 80);
                        let text = if s.is_empty() {
                            format!("  {prefix}✓ {tool_name}")
                        } else {
                            format!("  {prefix}✓ {tool_name} {s}")
                        };
                        (text, LineKind::ToolResult)
                    };
                    self.output.push(OutputLine { spans: None, text, kind });
                }
                self.live_progress = None;
                self.status = AgentStatus::Running {
                    phase: Phase::Thinking,
                };
            }
            RuntimeEvent::PlanUpdated {
                objective,
                explanation,
                plan,
                ..
            } => {
                self.flush_pending();

                // Build the new plan block: objective + status-marked steps + an
                // optional explanation. Steps are normalized to ≤60 chars by the
                // tool (fits one line); objective/explanation wrap with a hanging
                // indent.
                let mut block: Vec<OutputLine> = Vec::new();
                for (i, line) in wrap(&objective, WRAP_WIDTH).into_iter().enumerate() {
                    let text = if i == 0 {
                        format!("📋 {line}")
                    } else {
                        format!("   {line}")
                    };
                    block.push(OutputLine { spans: None, text, kind: LineKind::Plan });
                }
                for item in &plan {
                    let marker = match item.status {
                        PlanStepStatus::Completed => "✅",
                        PlanStepStatus::InProgress => "🔄",
                        PlanStepStatus::Pending => "○",
                    };
                    block.push(OutputLine { spans: None,
                        text: format!("   {marker} {}", item.step),
                        kind: LineKind::Plan,
                    });
                }
                if let Some(exp) = &explanation {
                    let exp = exp.trim();
                    if !exp.is_empty() {
                        for (i, line) in wrap(exp, WRAP_WIDTH).into_iter().enumerate() {
                            let text = if i == 0 {
                                format!("   ↳ {line}")
                            } else {
                                format!("     {line}")
                            };
                            block.push(OutputLine { spans: None, text, kind: LineKind::Plan });
                        }
                    }
                }

                // Replace the previous block in place so it stays anchored where
                // it first appeared (the tool always sends the full plan).
                let new_len = block.len();
                let start = self.plan_range.as_ref().map_or(self.output.len(), |r| r.start);
                if let Some(range) = self.plan_range.take() {
                    self.output.splice(range, block);
                } else {
                    self.output.extend(block);
                }
                self.plan_range = Some(start..start + new_len);
            }
            RuntimeEvent::AwaitingApproval { request, .. } => {
                // Log line for history; the interactive popup (5b) is driven by
                // the approval queue, which the QueuedApprovalHandler feeds.
                self.flush_pending();
                self.output.push(OutputLine { spans: None,
                    text: format!("⚠️  approval: {}", request.title),
                    kind: LineKind::Approval,
                });
                self.output.push(OutputLine { spans: None,
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
                // their own `RunFinished` with a non-empty `agent_id` (marking
                // that sub-agent done in the transcript + status strip).
                match agent_id.as_deref() {
                    Some(p) if !p.is_empty() => {
                        self.sub_agents.insert(p.to_string(), SubAgentStatus::Done);
                        self.output.push(OutputLine { spans: None,
                            text: format!("✓ [{p}] done"),
                            kind: LineKind::Done,
                        });
                    }
                    _ => {
                        self.output.push(OutputLine { spans: None,
                            text: "✅ done".to_string(),
                            kind: LineKind::Done,
                        });
                        self.status = AgentStatus::Idle;
                        self.running = false;
                        self.sub_agents.clear();
                    }
                }
            }
            RuntimeEvent::RunCancelled { agent_id, .. } => {
                self.flush_pending();
                match agent_id.as_deref() {
                    Some(p) if !p.is_empty() => {
                        self.sub_agents.insert(p.to_string(), SubAgentStatus::Done);
                        self.output.push(OutputLine { spans: None,
                            text: format!("✓ [{p}] done"),
                            kind: LineKind::Done,
                        });
                    }
                    _ => {
                        self.output.push(OutputLine { spans: None,
                            text: "⏹ cancelled".to_string(),
                            kind: LineKind::Cancelled,
                        });
                        self.status = AgentStatus::Idle;
                        self.running = false;
                        self.sub_agents.clear();
                    }
                }
            }
            RuntimeEvent::UserEvent { event, .. } => match event {
                // Live tool progress (e.g. streaming `execute_command` output):
                // surface the latest line in the status bar. Full output is
                // still delivered as the tool's final summary, so this is
                // feedback only — no transcript pollution.
                UserEvent::Progress { text } => {
                    self.live_progress = Some(text);
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Record a sub-agent's first appearance — emitting a `⏺ [p] started` marker
    /// and registering it in `sub_agents` — so the transcript and status strip
    /// show its lifecycle. No-op for the root agent (`agent_id == None`) and for
    /// sub-agents already seen this turn.
    fn track_agent(&mut self, agent_id: Option<&str>) {
        let Some(p) = agent_id.filter(|p| !p.is_empty()) else {
            return;
        };
        if !self.sub_agents.contains_key(p) {
            // Any pending root text precedes the first sub-agent event, so flush
            // it before the `started` marker to keep transcript order correct.
            self.flush_pending();
            self.sub_agents.insert(p.to_string(), SubAgentStatus::Running);
            self.output.push(OutputLine { spans: None,
                text: format!("⏺ [{p}] started"),
                kind: LineKind::Tool,
            });
        }
    }

    // Streaming text is fragmented (one delta per token); accumulate into a
    // pending buffer and split into lines only when a structural event or a
    // style switch forces a flush. This keeps ~1000 deltas/turn from producing
    // ~1000 output rows.

    fn push_text(&mut self, text: &str, agent: Option<&str>) {
        if !self.pending_thought.is_empty() {
            self.flush_thought();
        }
        let agent = agent.map(str::to_string);
        if !self.pending_text.is_empty() && self.pending_agent != agent {
            self.flush_text();
        }
        if self.pending_text.is_empty() {
            self.pending_agent = agent;
        }
        self.pending_text.push_str(text);
        self.tail_wrap.extend(text);
    }

    fn push_thought(&mut self, text: &str, agent: Option<&str>) {
        if !self.pending_text.is_empty() {
            self.flush_text();
        }
        let agent = agent.map(str::to_string);
        if !self.pending_thought.is_empty() && self.pending_agent != agent {
            self.flush_thought();
        }
        if self.pending_thought.is_empty() {
            self.pending_agent = agent;
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
        let prefix = self
            .pending_agent
            .take()
            .map(|p| format!("[{p}] "))
            .unwrap_or_default();
        let prefix = prefix.as_str();
        for (i, line) in self.tail_wrap.lines().iter().enumerate() {
            let text = if i == 0 && !prefix.is_empty() {
                format!("{prefix}{line}")
            } else {
                line.clone()
            };
            self.output.push(OutputLine { spans: None,
                text,
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
        let prefix = self
            .pending_agent
            .take()
            .map(|p| format!("[{p}] "))
            .unwrap_or_default();
        let prefix = prefix.as_str();
        for (i, line) in self.tail_wrap.lines().iter().enumerate() {
            let text = if i == 0 && !prefix.is_empty() {
                format!("{prefix}{line}")
            } else {
                line.clone()
            };
            self.output.push(OutputLine { spans: None,
                text,
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

    /// The assistant's last reply as plain text: every committed `Normal` line
    /// since the last user turn, plus any reply still streaming. Tool calls,
    /// tool results, plan lines and the `✅ done` marker are excluded — this is
    /// the prose the user most often wants to grab (e.g. a URL the AI printed).
    pub fn last_reply_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for line in self.output.iter().rev() {
            match line.kind {
                LineKind::User => break,
                LineKind::Normal => lines.push(line.text.clone()),
                _ => {}
            }
        }
        lines.reverse();

        // Include the uncommitted streaming tail, prefixed like `flush_text` would.
        if let Some((tail, LineKind::Normal)) = self.streaming_tail_lines() {
            let prefix = self
                .pending_agent
                .as_ref()
                .map(|p| format!("[{p}] "))
                .unwrap_or_default();
            for (i, line) in tail.iter().enumerate() {
                let text = if i == 0 && !prefix.is_empty() {
                    format!("{prefix}{line}")
                } else {
                    line.clone()
                };
                lines.push(text);
            }
        }

        lines.join("\n")
    }

    /// Show a transient status-bar notice (e.g. copy feedback), cleared on the
    /// next keypress.
    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
    }

    // ── Mouse selection ───────────────────────────────────────────────────

    /// Handle a mouse event at `(x, y)` (terminal cells) against a terminal of
    /// `(area_w, area_h)` cells: left-press anchors a selection, left-drag
    /// extends it, right-press opens the copy menu. While the copy menu is
    /// open, a click inside its popup activates that item — "拷贝" copies the
    /// selection, "取消" closes — and returns `Action::CopySelection` for the
    /// caller to run (mirrors the keyboard Enter path).
    pub(crate) fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        x: u16,
        y: u16,
        area_w: u16,
        area_h: u16,
    ) -> Option<Action> {
        use MouseEventKind::*;

        // Menu is open: a click (either button) inside the drawn popup activates
        // the item under the cursor instead of falling through to selection.
        if let Some(menu) = self.context_menu {
            let (mx, my) = context_menu_pos(menu.x, menu.y, area_w, area_h);
            if x >= mx && x < mx + CONTEXT_MENU_W && y >= my && y < my + CONTEXT_MENU_H
                && matches!(kind, Down(MouseButton::Left) | Down(MouseButton::Right))
            {
                let clicked_copy = y.saturating_sub(my + 1) == 0;
                self.context_menu = None;
                return if clicked_copy {
                    Some(Action::CopySelection)
                } else {
                    None
                };
            }
        }

        match kind {
            Down(MouseButton::Left) => {
                self.context_menu = None;
                self.selection = self
                    .line_index_at(x, y)
                    .map(|idx| Selection { anchor: idx, head: idx });
                None
            }
            Drag(MouseButton::Left) => {
                if let Some(idx) = self.line_index_at(x, y) {
                    if let Some(sel) = self.selection.as_mut() {
                        sel.head = idx;
                    }
                }
                None
            }
            Up(MouseButton::Left) => None,
            Down(MouseButton::Right) => {
                if self.selection.is_some() {
                    self.context_menu = Some(ContextMenu { x, y, selected: 0 });
                }
                None
            }
            _ => None,
        }
    }

    /// Map a screen cell `(x, y)` to an `output` line index, or `None` when
    /// outside the output pane or over a still-streaming (uncommitted) line.
    fn line_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let (ax, ay, aw, ah) = self.output_area?;
        if x < ax || x >= ax.saturating_add(aw) || y < ay || y >= ay.saturating_add(ah) {
            return None;
        }
        let row = (y - ay) as usize;
        let committed = self.output.len();
        let tail = self.streaming_tail_lines().map(|(l, _)| l.len()).unwrap_or(0);
        let window = window_range(committed + tail, self, ah as usize);
        let idx = window.start + row;
        if idx < window.end && idx < committed {
            Some(idx)
        } else {
            None
        }
    }

    /// The active selection as a normalized, `output`-len-clamped inclusive
    /// range, or `None` when empty/stale.
    fn selection_range(&self) -> Option<(usize, usize)> {
        let sel = self.selection?;
        let (lo, hi) = sel.bounds();
        if self.output.is_empty() || lo >= self.output.len() {
            return None;
        }
        Some((lo, hi.min(self.output.len() - 1)))
    }

    /// True when output line `i` falls inside the active selection.
    pub fn is_selected(&self, i: usize) -> bool {
        self.selection_range()
            .map_or(false, |(lo, hi)| lo <= i && i <= hi)
    }

    /// The selected lines joined as plain text (what-you-see-is-what-you-copy).
    pub fn selection_text(&self) -> String {
        let Some((lo, hi)) = self.selection_range() else {
            return String::new();
        };
        self.output[lo..=hi]
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The right-click menu, if open.
    pub fn context_menu(&self) -> Option<&ContextMenu> {
        self.context_menu.as_ref()
    }

    // ── @ mention picker ─────────────────────────────────────────────────

    /// Set the workspace root (once, at TUI start); relative paths are rendered
    /// relative to this, outside paths as absolute.
    pub fn set_workspace_root(&mut self, root: PathBuf) {
        self.workspace_root = root;
    }

    /// The active mention picker, if open.
    pub fn mention(&self) -> Option<&Mention> {
        self.mention.as_ref()
    }

    /// Open the picker with an empty prefix. The caller has already inserted
    /// `@` into the composer.
    fn start_mention(&mut self) {
        self.mention = Some(Mention {
            prefix: String::new(),
            dir: self.workspace_root.clone(),
            entries: Vec::new(),
            selected: 0,
        });
        self.refresh_mention();
    }

    /// Re-list entries from the current prefix, prepending the synthetic
    /// "use what I typed" row (always first; `selected` clamped into range).
    fn refresh_mention(&mut self) {
        let Some(m) = self.mention.as_mut() else {
            return;
        };
        let (dir, name) = mention::split_prefix(&self.workspace_root, &m.prefix);
        let mut entries = mention::list_entries(&dir, &name);

        // Synthetic row: the resolved form of whatever was typed, so Enter with
        // nothing highlighted inserts the typed path (`../../demo/codex/`).
        let full = if name.is_empty() { dir.clone() } else { dir.join(&name) };
        entries.insert(
            0,
            Entry {
                name: mention::rel_or_abs(&self.workspace_root, &full),
                path: full,
                is_dir: false,
                synthetic: true,
            },
        );

        m.dir = dir;
        m.entries = entries;
        m.selected = m.selected.min(m.entries.len().saturating_sub(1));
    }

    /// Append a char to the typed prefix, echoing it into the composer so the
    /// picker reads as an inline filter. Newlines are dropped (paths only).
    fn mention_push_char(&mut self, c: char) {
        if c == '\n' {
            return;
        }
        self.composer.insert_char(c);
        if let Some(m) = self.mention.as_mut() {
            m.prefix.push(c);
            m.selected = 0;
        }
        self.refresh_mention();
    }

    /// Remove the last prefix char (and the matching composer char), or cancel
    /// the picker entirely when the prefix is already empty.
    fn mention_backspace(&mut self) {
        let empty = self.mention.as_ref().map_or(true, |m| m.prefix.is_empty());
        if empty {
            self.mention = None;
            self.composer.backspace(); // remove the `@`
        } else {
            self.composer.backspace();
            if let Some(m) = self.mention.as_mut() {
                m.prefix.pop();
                m.selected = 0;
            }
            self.refresh_mention();
        }
    }

    fn move_mention(&mut self, delta: i32) {
        let Some(m) = self.mention.as_mut() else {
            return;
        };
        let n = m.entries.len() as i32;
        if n == 0 {
            return;
        }
        m.selected = (m.selected as i32 + delta).clamp(0, n - 1) as usize;
    }

    /// Replace `@<prefix>` in the composer with the selected path (or the typed
    /// prefix, when the synthetic row is highlighted).
    fn finish_mention(&mut self) {
        let Some(m) = self.mention.take() else {
            return;
        };
        let to_delete = 1 + m.prefix.chars().count();
        for _ in 0..to_delete {
            self.composer.backspace();
        }
        let text = m
            .entries
            .get(m.selected)
            .map(|e| mention::rel_or_abs(&self.workspace_root, &e.path))
            .unwrap_or_else(|| mention::rel_or_abs(&self.workspace_root, &m.dir));
        self.composer.insert_str(&text);
    }

    /// Key handling while the mention picker is open (swallows everything).
    fn handle_mention_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        use KeyCode::*;
        let _ = modifiers;
        match code {
            Esc => {
                let to_delete = 1 + self.mention.as_ref().map_or(0, |m| m.prefix.chars().count());
                self.mention = None;
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                None
            }
            Up => {
                self.move_mention(-1);
                None
            }
            Down => {
                self.move_mention(1);
                None
            }
            Backspace => {
                self.mention_backspace();
                None
            }
            Enter => {
                self.finish_mention();
                None
            }
            Char(c) => {
                self.mention_push_char(c);
                None
            }
            _ => None,
        }
    }

    // ── / skill picker ──────────────────────────────────────────────────────

    /// Inject the loaded skill summaries (called once at startup from `run_tui`).
    pub fn set_skill_summaries(&mut self, summaries: Vec<(String, String)>) {
        self.skill_summaries = summaries;
    }

    /// The active skill picker, if open.
    pub fn slash(&self) -> Option<&SlashPicker> {
        self.slash.as_ref()
    }

    /// Open the skill picker (triggered when `/` is typed at line start).
    fn start_slash(&mut self) {
        self.slash = Some(SlashPicker {
            prefix: String::new(),
            entries: self.skill_summaries.clone(),
            selected: 0,
        });
    }

    /// Refresh the filtered entries based on the current prefix.
    fn refresh_slash(&mut self) {
        let Some(s) = self.slash.as_mut() else { return };
        if s.prefix.is_empty() {
            s.entries = self.skill_summaries.clone();
        } else {
            let q = s.prefix.to_lowercase();
            s.entries = self
                .skill_summaries
                .iter()
                .filter(|(name, desc)| {
                    name.to_lowercase().contains(&q) || desc.to_lowercase().contains(&q)
                })
                .cloned()
                .collect();
        }
        s.selected = s.selected.min(s.entries.len().saturating_sub(1));
    }

    /// Push a character into the slash prefix.
    fn slash_push_char(&mut self, c: char) {
        if let Some(s) = self.slash.as_mut() {
            s.prefix.push(c);
        }
        self.composer.insert_char(c);
        self.refresh_slash();
    }

    /// Backspace in the slash picker: remove last char from prefix; close if
    /// prefix becomes empty and the user backspaces again (removes the `/`).
    fn slash_backspace(&mut self) {
        let empty = self.slash.as_ref().map_or(true, |s| s.prefix.is_empty());
        if empty {
            // Prefix is already empty → user is deleting the `/` itself → close picker
            self.slash = None;
            self.composer.backspace();
            return;
        }
        if let Some(s) = self.slash.as_mut() {
            s.prefix.pop();
        }
        self.composer.backspace();
        self.refresh_slash();
    }

    /// Move the slash picker highlight by `delta` (±1).
    fn move_slash(&mut self, delta: i32) {
        let Some(s) = self.slash.as_mut() else { return };
        let n = s.entries.len();
        if n == 0 {
            return;
        }
        s.selected = (s.selected as i32 + delta).clamp(0, (n - 1) as i32) as usize;
    }

    /// Confirm selection: replace `/prefix` in the composer with `/selected-name `.
    fn finish_slash(&mut self) {
        let Some(s) = self.slash.take() else { return };
        let prefix_len = s.prefix.chars().count();
        let name = s
            .entries
            .get(s.selected)
            .map(|(n, _)| n.clone())
            .unwrap_or(s.prefix);
        // 删掉已输入的 `/prefix`
        let to_delete = 1 + prefix_len;
        for _ in 0..to_delete {
            self.composer.backspace();
        }
        // 插入 `/name `（带尾部空格，方便用户继续输入参数）
        self.composer.insert_str(&format!("/{name} "));
    }

    /// Key handling while the skill picker is open.
    fn handle_slash_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        use KeyCode::*;
        let _ = modifiers;
        match code {
            Esc => {
                // 删掉 `/prefix` 并关闭
                let to_delete = 1 + self.slash.as_ref().map_or(0, |s| s.prefix.chars().count());
                self.slash = None;
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                None
            }
            Up => {
                self.move_slash(-1);
                None
            }
            Down => {
                self.move_slash(1);
                None
            }
            Backspace => {
                self.slash_backspace();
                None
            }
            Enter => {
                self.finish_slash();
                None
            }
            Char(c) => {
                self.slash_push_char(c);
                None
            }
            _ => None,
        }
    }

    /// Paste text into the composer — or into the mention/slash prefix when a
    /// picker is open (newlines are dropped from a path prefix).
    pub fn paste(&mut self, text: &str) {
        if self.mention.is_some() {
            for c in text.chars() {
                self.mention_push_char(c);
            }
        } else if self.slash.is_some() {
            for c in text.chars() {
                self.slash_push_char(c);
            }
        } else {
            self.composer.insert_str(text);
        }
    }

    // ── Key handling ──────────────────────────────────────────────────────

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        use KeyCode::*;

        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let shift = modifiers.contains(KeyModifiers::SHIFT);
        let super_key = modifiers.contains(KeyModifiers::SUPER);

        // Any key dismisses a transient notice (copy feedback) before handling.
        self.notice = None;

        // Cmd+C (Super) is the macOS copy shortcut: copy the selection, and do
        // nothing when there's no selection — it never cancels/quits (that's
        // Ctrl+C's job).
        if super_key && code == Char('c') {
            if self.selection.is_some() {
                self.context_menu = None;
                return Some(Action::CopySelection);
            }
            return None;
        }

        // Ctrl+C always wins: with an active selection it copies the selection;
        // otherwise it cancels a running turn (or pending approval) / quits.
        if ctrl && code == Char('c') {
            if self.selection.is_some() {
                self.context_menu = None;
                return Some(Action::CopySelection);
            }
            return Some(if self.running || !self.approval_queue.is_empty() {
                Action::Cancel
            } else {
                Action::Quit
            });
        }

        // Right-click copy menu: Up/Down move the highlight, Enter copies (or
        // cancels), Esc closes; everything else is swallowed.
        if self.context_menu.is_some() {
            let selected = self.context_menu.as_ref().map_or(0, |m| m.selected);
            return match code {
                Up => {
                    if let Some(m) = self.context_menu.as_mut() {
                        m.selected = m.selected.saturating_sub(1);
                    }
                    None
                }
                Down => {
                    if let Some(m) = self.context_menu.as_mut() {
                        m.selected = (m.selected + 1).min(1);
                    }
                    None
                }
                Enter => {
                    self.context_menu = None;
                    if selected == 0 {
                        Some(Action::CopySelection)
                    } else {
                        None
                    }
                }
                Esc => {
                    self.context_menu = None;
                    None
                }
                _ => None,
            };
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

        // While the @ mention picker is open, every key drives the picker — the
        // composer only receives the final path on Enter/Esc.
        if self.mention.is_some() {
            return self.handle_mention_key(code, modifiers);
        }

        // While the / skill picker is open, every key drives the picker.
        if self.slash.is_some() {
            return self.handle_slash_key(code, modifiers);
        }

        match code {
            Char('d') if ctrl => Some(Action::Quit),
            // Ctrl+Y copies the last reply (vim-yank convention); plain 'y'
            // still falls through to `Char(c)` and types a literal 'y'.
            Char('y') if ctrl => Some(Action::CopyLastReply),
            Esc => {
                // No menu is open here (handled above); clear a selection if
                // present, else clear the composer.
                if self.selection.is_some() {
                    self.selection = None;
                } else {
                    self.composer.clear();
                }
                None
            }
            Enter => {
                if shift {
                    self.composer.insert_char('\n');
                    None
                } else if !self.running && !self.composer.is_empty() {
                    let text = self.composer.text();
                    self.composer.clear();
                    // Echo the user's message into the transcript before the
                    // agent's reply, so the record keeps the human turn too.
                    self.push_user(&text);
                    self.plan_range = None;
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
            Char('/') if self.composer.is_empty() && !self.skill_summaries.is_empty() => {
                self.composer.insert_char('/');
                self.start_slash();
                None
            }
            Char('@') => {
                self.composer.insert_char('@');
                self.start_mention();
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
                if let Some(n) = &self.notice {
                    n.clone()
                } else {
                    "⏸ Idle — Enter send · Shift+Enter newline · Ctrl+Y copy · PgUp/PgDn scroll · Ctrl+C quit".to_string()
                }
            }
            AgentStatus::Running { phase } => match phase {
                Phase::Thinking => "🤔 thinking… (Ctrl+C cancel)".to_string(),
                Phase::Streaming => "💬 streaming… (Ctrl+C cancel)".to_string(),
                Phase::ToolCall { tool } => match &self.live_progress {
                    Some(p) => format!("🔧 {tool}: {p}"),
                    None => format!("🔧 {tool} (Ctrl+C cancel)"),
                },
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

/// The `[start, end)` range of `total` lines to show in a window of `height`
/// rows, honoring `follow_bottom` and `scroll_offset`.
pub(crate) fn window_range(total: usize, app: &App, height: usize) -> Range<usize> {
    if total <= height {
        return 0..total;
    }
    if app.follow_bottom {
        return total - height..total;
    }
    let start = total.saturating_sub(height).saturating_sub(app.scroll_offset);
    start..(start + height).min(total)
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

/// `"[path] "` for a sub-agent, `""` for the root agent — labels a sub-agent's
/// lines so they read `[root/searcher] …` in the transcript.
pub fn agent_prefix(agent_id: Option<&str>) -> String {
    match agent_id {
        Some(p) if !p.is_empty() => format!("[{p}] "),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_base::{PlanItem, PlanStepStatus};
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

    fn child_text(agent: &str, s: &str) -> RuntimeEvent {
        RuntimeEvent::TextDelta {
            session_id: SessionId::new(1),
            text: s.to_string(),
            agent_id: Some(agent.to_string()),
            trace_id: None,
        }
    }

    fn child_tool_started(agent: &str, name: &str) -> RuntimeEvent {
        RuntimeEvent::ToolCallStarted {
            session_id: SessionId::new(1),
            tool_name: name.to_string(),
            args_json: "{}".to_string(),
            agent_id: Some(agent.to_string()),
            trace_id: None,
        }
    }

    fn child_tool_finished(agent: &str, name: &str) -> RuntimeEvent {
        RuntimeEvent::ToolCallFinished {
            session_id: SessionId::new(1),
            tool_name: name.to_string(),
            summary: "done".to_string(),
            agent_id: Some(agent.to_string()),
            trace_id: None,
            denied: false,
        }
    }

    fn plan(objective: &str, steps: Vec<(&str, PlanStepStatus)>) -> RuntimeEvent {
        plan_ex(objective, None, steps)
    }

    fn plan_ex(
        objective: &str,
        explanation: Option<&str>,
        steps: Vec<(&str, PlanStepStatus)>,
    ) -> RuntimeEvent {
        RuntimeEvent::PlanUpdated {
            session_id: SessionId::new(1),
            objective: objective.to_string(),
            explanation: explanation.map(String::from),
            plan: steps
                .into_iter()
                .map(|(step, status)| PlanItem {
                    step: step.to_string(),
                    status,
                })
                .collect(),
            agent_id: None,
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
    fn submit_echoes_user_message() {
        let mut app = App::new();
        let action = submit(&mut app, "hello world");
        assert_eq!(action, Action::Submit("hello world".to_string()));
        let users: Vec<&str> = app
            .output
            .iter()
            .filter(|l| l.kind == LineKind::User)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(users, vec!["❯ hello world"]);
    }

    #[test]
    fn progress_updates_and_clears_live_progress() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(tool_started("execute_command")));
        assert_eq!(app.status_line(), "🔧 execute_command (Ctrl+C cancel)");

        let prog = RuntimeEvent::UserEvent {
            session_id: SessionId::new(1),
            event: UserEvent::Progress {
                text: "Compiling phiforge v0.1.0".to_string(),
            },
            agent_id: None,
            trace_id: None,
        };
        app.handle_event(TuiEvent::Runtime(prog));
        assert_eq!(
            app.status_line(),
            "🔧 execute_command: Compiling phiforge v0.1.0"
        );

        // A new tool call drops the previous tool's live progress.
        app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
        assert_eq!(app.status_line(), "🔧 read_file (Ctrl+C cancel)");
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
            app.output.push(OutputLine { spans: None, text: format!("line {i}"), kind: LineKind::Normal });
        }
        app.scroll_up();
        assert!(!app.follow_bottom);
        assert_eq!(app.scroll_offset, SCROLL_STEP);
    }

    #[test]
    fn scroll_down_reenters_follow_bottom() {
        let mut app = App::new();
        for i in 0..100 {
            app.output.push(OutputLine { spans: None, text: format!("line {i}"), kind: LineKind::Normal });
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

    #[test]
    fn sub_agent_text_is_labeled_and_lifecycle_tracked() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(child_text("root/a", "found a thing")));
        assert_eq!(app.sub_agents.get("root/a"), Some(&SubAgentStatus::Running));
        app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
        assert_eq!(app.sub_agents.get("root/a"), Some(&SubAgentStatus::Done));
        let texts: Vec<&str> = app.output.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "⏺ [root/a] started",
                "[root/a] found a thing",
                "✓ [root/a] done",
            ]
        );
    }

    #[test]
    fn sub_agent_tool_calls_are_labeled() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(child_tool_started("root/a", "read_file")));
        app.handle_event(TuiEvent::Runtime(child_tool_finished("root/a", "read_file")));
        let texts: Vec<&str> = app.output.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "⏺ [root/a] started",
                "⏺ [root/a] read_file {}",
                "  [root/a] ✓ read_file done",
            ]
        );
    }

    #[test]
    fn sub_agents_cleared_on_root_finish() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(child_text("root/a", "hi")));
        app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
        assert!(!app.sub_agents.is_empty());
        app.handle_event(TuiEvent::Runtime(run_finished(None)));
        assert!(app.sub_agents.is_empty());
    }

    #[test]
    fn sub_agent_run_finished_does_not_end_turn() {
        let mut app = App::new();
        app.running = true;
        app.handle_event(TuiEvent::Runtime(child_text("root/a", "hi")));
        app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
        assert!(app.running);
        assert!(matches!(app.status, AgentStatus::Running { .. }));
        assert_eq!(app.sub_agents.get("root/a"), Some(&SubAgentStatus::Done));
    }

    #[test]
    fn agent_prefix_labels_sub_agents_only() {
        assert_eq!(agent_prefix(None), "");
        assert_eq!(agent_prefix(Some("")), "");
        assert_eq!(agent_prefix(Some("root/a")), "[root/a] ");
    }

    #[test]
    fn last_reply_text_captures_reply_after_last_user() {
        let mut app = App::new();
        app.push_user("give me a url");
        app.handle_event(TuiEvent::Runtime(text("here: https://example.com/x")));
        app.handle_event(TuiEvent::Runtime(run_finished(None)));
        assert_eq!(app.last_reply_text(), "here: https://example.com/x");
    }

    #[test]
    fn last_reply_text_skips_tools_and_stops_at_previous_user() {
        let mut app = App::new();
        app.push_user("turn one");
        app.handle_event(TuiEvent::Runtime(text("old answer")));
        app.handle_event(TuiEvent::Runtime(run_finished(None)));

        app.push_user("turn two");
        app.handle_event(TuiEvent::Runtime(text("part one")));
        app.handle_event(TuiEvent::Runtime(tool_started("verify")));
        app.handle_event(TuiEvent::Runtime(tool_finished("verify", false)));
        app.handle_event(TuiEvent::Runtime(text("part two")));
        app.handle_event(TuiEvent::Runtime(run_finished(None)));

        // Only turn two's prose; tool lines, results and the `✅ done` marker
        // are excluded, and the scan stops at the previous user turn.
        assert_eq!(app.last_reply_text(), "part one\npart two");
    }

    #[test]
    fn last_reply_text_empty_without_reply() {
        let app = App::new();
        assert_eq!(app.last_reply_text(), "");
    }

    #[test]
    fn ctrl_y_emits_copy_last_reply_and_plain_y_types() {
        let mut app = App::new();
        assert_eq!(
            app.handle_key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            Some(Action::CopyLastReply)
        );
        // Plain 'y' (no Ctrl) still inserts a literal 'y'.
        assert_eq!(app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE), None);
        assert_eq!(app.composer.text(), "y");
    }

    #[test]
    fn notice_shows_in_status_line_and_clears_on_key() {
        let mut app = App::new();
        assert!(app.status_line().starts_with("⏸ Idle"));
        app.set_notice("📋 copied 5 chars");
        assert_eq!(app.status_line(), "📋 copied 5 chars");
        // Any keypress clears the notice before being handled.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.status_line().starts_with("⏸ Idle"));
    }

    #[test]
    fn plan_renders_status_markers() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(plan(
            "目标",
            vec![
                ("已完成", PlanStepStatus::Completed),
                ("进行中", PlanStepStatus::InProgress),
                ("待办", PlanStepStatus::Pending),
            ],
        )));
        let lines: Vec<&str> = app.output.iter().map(|l| l.text.as_str()).collect();
        assert!(lines.iter().any(|l| l.contains("📋 目标")), "got: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("✅ 已完成")), "got: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("🔄 进行中")), "got: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("○ 待办")), "got: {lines:?}");
    }

    #[test]
    fn plan_replaces_in_place_instead_of_appending() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(plan(
            "目标",
            vec![("步骤1", PlanStepStatus::Completed)],
        )));
        app.handle_event(TuiEvent::Runtime(plan(
            "目标",
            vec![
                ("步骤1", PlanStepStatus::Completed),
                ("步骤2", PlanStepStatus::Completed),
            ],
        )));
        let texts: Vec<&str> = app.output.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts.iter().filter(|t| t.contains("📋 目标")).count(),
            1,
            "plan replaced, not duplicated: {texts:?}"
        );
        assert_eq!(texts.iter().filter(|t| t.contains("✅ 步骤1")).count(), 1);
        assert_eq!(texts.iter().filter(|t| t.contains("✅ 步骤2")).count(), 1);
    }

    #[test]
    fn plan_renders_explanation() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(plan_ex(
            "目标",
            Some("检测到已有配置，直接复用"),
            vec![("步骤", PlanStepStatus::Pending)],
        )));
        assert!(app.output.iter().any(|l| l.text.contains("↳ 检测到已有配置")));
    }

    #[test]
    fn update_plan_tool_result_line_is_suppressed() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(tool_started("update_plan")));
        app.handle_event(TuiEvent::Runtime(tool_finished("update_plan", false)));
        assert!(!app.output.iter().any(|l| l.text.contains("update_plan")));
    }

    #[test]
    fn plan_resets_across_turns() {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(plan(
            "目标A",
            vec![("步骤", PlanStepStatus::Pending)],
        )));
        // Starting a new turn (submit) clears the tracked plan range, so the next
        // plan appends instead of replacing the previous turn's plan.
        assert_eq!(submit(&mut app, "next"), Action::Submit("next".to_string()));
        app.handle_event(TuiEvent::Runtime(plan(
            "目标B",
            vec![("步骤B", PlanStepStatus::Pending)],
        )));
        let texts: Vec<&str> = app.output.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("📋 目标A")), "target A kept: {texts:?}");
        assert!(texts.iter().any(|t| t.contains("📋 目标B")), "target B appended: {texts:?}");
        assert_eq!(texts.iter().filter(|t| t.contains("📋")).count(), 2);
    }

    #[test]
    fn mouse_drag_selects_line_range() {
        let mut app = App::new();
        for i in 0..10 {
            app.output.push(OutputLine { spans: None, text: format!("line {i}"), kind: LineKind::Normal });
        }
        app.output_area = Some((0, 0, 100, 10));
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2, 100, 10);
        app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 0, 5, 100, 10);
        assert_eq!(app.selection, Some(Selection { anchor: 2, head: 5 }));
        assert_eq!(app.selection_text(), "line 2\nline 3\nline 4\nline 5");
    }

    #[test]
    fn mouse_drag_up_normalizes_selection() {
        let mut app = App::new();
        for i in 0..10 {
            app.output.push(OutputLine { spans: None, text: format!("line {i}"), kind: LineKind::Normal });
        }
        app.output_area = Some((0, 0, 100, 10));
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 5, 100, 10);
        app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 0, 2, 100, 10);
        assert!(app.is_selected(3));
        assert_eq!(app.selection_text(), "line 2\nline 3\nline 4\nline 5");
    }

    #[test]
    fn click_outside_output_clears_selection() {
        let mut app = App::new();
        app.output.push(OutputLine { spans: None, text: "x".into(), kind: LineKind::Normal });
        app.output_area = Some((0, 0, 10, 5));
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 10, 5);
        assert!(app.selection.is_some());
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 50, 10, 5); // below pane
        assert!(app.selection.is_none());
    }

    #[test]
    fn selection_text_clamps_stale_indices() {
        let mut app = App::new();
        app.output.push(OutputLine { spans: None, text: "a".into(), kind: LineKind::Normal });
        app.selection = Some(Selection { anchor: 0, head: 5 });
        assert_eq!(app.selection_text(), "a");
    }

    #[test]
    fn ctrl_c_copies_selection_else_cancel_quit() {
        let mut app = App::new();
        assert_eq!(
            app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::Quit)
        );
        app.selection = Some(Selection { anchor: 0, head: 0 });
        assert_eq!(
            app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(Action::CopySelection)
        );
    }

    #[test]
    fn right_click_opens_menu_and_enter_copies() {
        let mut app = App::new();
        app.output.push(OutputLine { spans: None, text: "x".into(), kind: LineKind::Normal });
        app.output_area = Some((0, 0, 10, 10));
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 20, 20);
        app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 3, 4, 20, 20);
        assert_eq!(app.context_menu, Some(ContextMenu { x: 3, y: 4, selected: 0 }));
        assert_eq!(
            app.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            Some(Action::CopySelection)
        );
        assert_eq!(app.context_menu, None);
    }

    #[test]
    fn context_menu_arrows_move_highlight_and_esc_closes() {
        let mut app = App::new();
        app.output.push(OutputLine { spans: None, text: "x".into(), kind: LineKind::Normal });
        app.output_area = Some((0, 0, 10, 10));
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 20, 20);
        app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
        assert_eq!(app.handle_key(KeyCode::Down, KeyModifiers::NONE), None);
        assert_eq!(app.context_menu.as_ref().unwrap().selected, 1);
        // Enter on the "cancel" item closes without copying.
        assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
        assert!(app.context_menu.is_none());
    }

    #[test]
    fn clicking_menu_copy_item_copies_and_closes() {
        let mut app = App::new();
        app.output.push(OutputLine { spans: None, text: "x".into(), kind: LineKind::Normal });
        app.output_area = Some((0, 0, 10, 10));
        app.selection = Some(Selection { anchor: 0, head: 0 });
        // Open the menu at (0,0): 12×4 box, items at rows y+1 ("拷贝") and y+2
        // ("取消").
        assert_eq!(
            app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20),
            None
        );
        assert!(app.context_menu.is_some());
        // Left-click the "拷贝" row → copy, menu closes, selection preserved.
        assert_eq!(
            app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 1, 1, 20, 20),
            Some(Action::CopySelection)
        );
        assert!(app.context_menu.is_none());
        assert!(app.selection.is_some());
    }

    #[test]
    fn clicking_menu_cancel_item_closes_without_copy() {
        let mut app = App::new();
        app.output.push(OutputLine { spans: None, text: "x".into(), kind: LineKind::Normal });
        app.output_area = Some((0, 0, 10, 10));
        app.selection = Some(Selection { anchor: 0, head: 0 });
        app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
        // "取消" is the second item, row y+2. It closes the menu but keeps the
        // selection.
        assert_eq!(
            app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 1, 2, 20, 20),
            None
        );
        assert!(app.context_menu.is_none());
        assert!(app.selection.is_some());
    }

    #[test]
    fn clicking_outside_menu_closes_it_and_restarts_selection() {
        let mut app = App::new();
        app.output.push(OutputLine { spans: None, text: "x".into(), kind: LineKind::Normal });
        app.output_area = Some((0, 0, 10, 10));
        app.selection = Some(Selection { anchor: 0, head: 0 });
        app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
        assert!(app.context_menu.is_some());
        // A click far outside the popup closes the menu (and starts a new
        // selection) exactly as before.
        assert_eq!(
            app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 15, 15, 20, 20),
            None
        );
        assert!(app.context_menu.is_none());
    }

    #[test]
    fn esc_clears_selection_before_composer() {
        let mut app = App::new();
        app.selection = Some(Selection { anchor: 0, head: 2 });
        app.composer.insert_str("keep");
        assert_eq!(app.handle_key(KeyCode::Esc, KeyModifiers::NONE), None);
        assert!(app.selection.is_none());
        assert_eq!(app.composer.text(), "keep");
    }

    #[test]
    fn cmd_c_copies_selection_but_never_quits() {
        let mut app = App::new();
        // No selection → Cmd+C does nothing (it must never quit).
        assert_eq!(app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER), None);
        // With a selection → copy.
        app.selection = Some(Selection { anchor: 0, head: 0 });
        assert_eq!(
            app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER),
            Some(Action::CopySelection)
        );
    }

    fn mention_scratch(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "phiforge-app-mention-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn at_opens_mention_picker_listing_root() {
        let mut app = App::new();
        let root = mention_scratch("open");
        std::fs::write(root.join("a.txt"), "x").unwrap();
        app.set_workspace_root(root.clone());

        assert_eq!(app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE), None);
        let m = app.mention().expect("mention open");
        assert_eq!(m.prefix, "");
        assert!(m.entries[0].synthetic, "synthetic row is first");
        assert!(m.entries.iter().any(|e| e.name == "a.txt"));
        assert_eq!(app.composer.text(), "@");
    }

    #[test]
    fn enter_on_empty_prefix_inserts_dot_and_closes() {
        let mut app = App::new();
        let root = mention_scratch("enter-empty");
        app.set_workspace_root(root.clone());

        app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.mention().is_none());
        assert_eq!(app.composer.text(), ".");
    }

    #[test]
    fn dotdot_prefix_inserts_absolute_parent_path() {
        let mut app = App::new();
        let root = mention_scratch("dotdot-app");
        app.set_workspace_root(root.clone());

        app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
        for c in "../".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(app.mention().is_some(), "picker stays open while typing");
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let text = app.composer.text();
        assert!(!text.contains('@'), "composer holds the path, got {text:?}");
        assert!(text.starts_with('/'), "outside path is absolute, got {text:?}");
    }

    #[test]
    fn typing_name_then_arrow_selects_file() {
        let mut app = App::new();
        let root = mention_scratch("select");
        std::fs::write(root.join("main.rs"), "x").unwrap();
        std::fs::write(root.join("other.rs"), "x").unwrap();
        app.set_workspace_root(root.clone());

        app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
        for c in "main".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        // Arrow down off the synthetic row onto the single real match, accept it.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.composer.text(), "main.rs");
    }

    #[test]
    fn esc_cancels_mention_removing_at_and_prefix() {
        let mut app = App::new();
        let root = mention_scratch("esc");
        app.set_workspace_root(root.clone());

        app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
        for c in "src".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.mention().is_none());
        assert_eq!(app.composer.text(), "");
    }

    #[test]
    fn backspace_with_empty_prefix_cancels_mention() {
        let mut app = App::new();
        let root = mention_scratch("bsp");
        app.set_workspace_root(root.clone());

        app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(app.mention().is_none());
        assert_eq!(app.composer.text(), "");
    }

    #[test]
    fn mention_inserts_after_existing_text() {
        let mut app = App::new();
        let root = mention_scratch("mid");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        app.set_workspace_root(root.clone());

        app.composer.insert_str("read ");
        app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
        for c in "sub".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.composer.text(), "read sub");
    }

    #[test]
    fn paste_routes_to_mention_prefix_when_open() {
        let mut app = App::new();
        let root = mention_scratch("paste");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        app.set_workspace_root(root.clone());

        app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
        app.paste("sub");
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.composer.text(), "sub");
    }

    // ── / skill picker tests ──

    fn app_with_skills() -> App {
        let mut app = App::new();
        // 排序后: commit(0), requesting-code-review(1), review(2)
        app.set_skill_summaries(vec![
            ("commit".into(), "Generate a commit message".into()),
            ("requesting-code-review".into(), "Request a code review".into()),
            ("review".into(), "Pre-landing PR review".into()),
        ]);
        app
    }

    #[test]
    fn slash_opens_picker_on_empty_composer() {
        let mut app = app_with_skills();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(app.slash().is_some(), "slash picker should open");
        assert_eq!(app.composer.text(), "/");
    }

    #[test]
    fn slash_does_not_open_when_composer_not_empty() {
        let mut app = app_with_skills();
        app.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(app.slash().is_none(), "slash picker should not open mid-input");
    }

    #[test]
    fn slash_filters_by_prefix() {
        let mut app = app_with_skills();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);

        let s = app.slash().unwrap();
        let names: Vec<&str> = s.entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"commit"));
        assert!(names.contains(&"requesting-code-review"));
        // "review" does not contain 'c' → filtered out
        assert!(!names.contains(&"review"));
    }

    #[test]
    fn slash_enter_confirms_selection() {
        let mut app = app_with_skills();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        // 默认选中第一个（排序后是 "commit"）
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.slash().is_none(), "picker should close after Enter");
        assert_eq!(app.composer.text(), "/commit ", "should insert /name + space");
    }

    #[test]
    fn slash_esc_cancels() {
        let mut app = app_with_skills();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.slash().is_none(), "picker should close on Esc");
        assert!(app.composer.is_empty(), "composer should be empty after cancel");
    }

    #[test]
    fn slash_arrow_keys_navigate() {
        let mut app = app_with_skills();
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);

        assert_eq!(app.slash().unwrap().selected, 0);

        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.slash().unwrap().selected, 1);

        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.slash().unwrap().selected, 2);

        // 已在末尾，Down 不动
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.slash().unwrap().selected, 2);

        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.slash().unwrap().selected, 1);
    }
}
