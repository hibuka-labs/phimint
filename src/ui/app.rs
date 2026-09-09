//! TUI application state: the independent `RuntimeEvent` consumer.
//!
//! The agent loop (phi-agent) is untouched; its `RuntimeEvent`s are forwarded
//! here through a channel and mapped onto visible state — a scrollable output
//! buffer (tool calls render inline), a status state machine, and the composer.
//! This is the same event stream the `print_event` REPL renderer consumed, so
//! nothing in the framework needed to change for the TUI to exist.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use phi_agent::{ApprovalDecision, ApprovalRequest, RegistrySnapshot, RuntimeEvent};

use crate::approval::ApprovalItem;
use crate::banner::{BannerRow, BannerStyle, ColorScheme};
use phi_tui::input::Composer;
use phi_tui::lines::{LineKind, OutputLine};
use phi_tui::completer::{MentionCompleter, SlashCompleter, CompleterAction};
use phi_tui::selection::SelectionState;
use phi_tui::stream::StreamState;
use phi_tui::transcript::{Transcript, DEFAULT_WRAP_WIDTH};
use phi_tui::viewport::Viewport;
use phi_tui::wrap::wrap;

// Selection / ContextMenu / context_menu_pos / CONTEXT_MENU_* moved to
// selection.rs; re-exported here for the renderer and tests.
#[allow(unused_imports)] // re-exported for external consumers (tests use it via super::*)
pub use phi_tui::selection::{
    ContextMenu, Selection, CONTEXT_MENU_H, CONTEXT_MENU_W, context_menu_pos,
};

/// Lines a PageUp/PageDown scrolls the output by.
const SCROLL_STEP: usize = 1;

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
    /// The root turn ended but sub-agents are still running — the fan-in wake
    /// (batch injection) will start the next turn automatically.
    Waiting { running: usize },
    Running { phase: Phase },
}

/// Live state of a sub-agent shown in the task panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAgentStatus {
    Running,
    Done,
}

/// A tool call event from a sub-agent (for detail display).
#[derive(Debug, Clone)]
pub struct ToolEvent {
    pub tool_name: String,
    pub summary: String,
    pub is_finished: bool,
}

/// Detailed state of a sub-agent for the task panel.
#[derive(Debug, Clone)]
pub struct SubAgentState {
    /// Task name (from spawn_agent name argument)
    pub name: String,
    /// Current status
    pub status: SubAgentStatus,
    /// Key files (from spawn_agent task argument)
    pub files: Vec<String>,
    /// When the sub-agent started
    pub started_at: Instant,
    /// When the sub-agent completed (for auto-removal timing)
    pub completed_at: Option<Instant>,
    /// Last time a tool event (started or finished) arrived. Drives the
    /// panel's `writing...` hint — a Running agent whose tool feed has been
    /// quiet this long is generating prose/thought between tool calls.
    pub last_tool_at: Instant,
    /// Tool call events (for detail display)
    pub events: Vec<ToolEvent>,
}

/// Running-agent quiet period before the activity column switches from the
/// (stale) last tool event to `writing...`. Chosen above typical inter-tool
/// gaps so it never flashes during an active tool loop.
pub(crate) const CHILD_WRITING_HINT_AFTER: Duration = Duration::from_secs(20);

/// Whether the panel should show `writing...` for this sub-agent: it is
/// Running, its last tool call has finished (or none yet), and the tool feed
/// has been quiet for [`CHILD_WRITING_HINT_AFTER`]. An in-flight tool keeps
/// its own honest `→ tool` display.
pub(crate) fn is_writing_hint(state: &SubAgentState, now: Instant) -> bool {
    if !matches!(state.status, SubAgentStatus::Running) {
        return false;
    }
    if state.events.last().is_some_and(|e| !e.is_finished) {
        return false;
    }
    now.duration_since(state.last_tool_at) >= CHILD_WRITING_HINT_AFTER
}

/// Where the keyboard focus is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusTarget {
    /// Focus is on the input box
    Input,
    /// Focus is on the task list, with the given index selected
    TaskList(usize),
}

/// Task panel UI state.
#[derive(Debug, Clone)]
pub struct TaskPanel {
    /// Current focus position
    pub focus: FocusTarget,
}

impl Default for TaskPanel {
    fn default() -> Self {
        Self {
            focus: FocusTarget::Input,
        }
    }
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
    /// Registry lifecycle snapshot (Phase 5): the authoritative fact view of
    /// every registered sub-agent, forwarded from `subscribe_lifecycle`. The
    /// task panel reconciles against this instead of inferring state from
    /// child runtime events.
    Lifecycle(std::sync::Arc<RegistrySnapshot>),
    /// A turn ended with an error (surfaced as a red output line).
    TurnError(String),
    /// A turn completed (safety net in case `RunFinished` was never seen).
    TurnDone,
}

#[derive(Debug)]
pub struct App {
    /// Committed transcript: output lines, the plan block, and wrap width.
    pub transcript: Transcript<BannerStyle>,
    pub composer: Composer,
    pub status: AgentStatus,
    /// True while a turn is running (gates submitting another task).
    pub running: bool,
    /// Scrollable output viewport (offset + follow-bottom + rendered size).
    pub viewport: Viewport,
    /// Pending approval requests (front = the popup currently shown).
    pub approval_queue: VecDeque<ApprovalItem>,
    /// Live sub-agent states, keyed by `agent_id` (`root/<task_name>`).
    pub sub_agents: BTreeMap<String, SubAgentState>,
    /// Task panel UI state (focus, selection).
    pub task_panel: TaskPanel,
    /// Sub-agent transcripts (separate from main transcript).
    /// Key: agent_id, value: output lines for that sub-agent.
    pub sub_agent_transcripts: BTreeMap<String, Vec<OutputLine<BannerStyle>>>,
    /// Mouse selection + right-click copy menu.
    pub selection_state: SelectionState,
    /// Timestamp of the last "press Ctrl+C again to exit" hint. If set and
    /// within `QUIT_HINT_TIMEOUT`, a second Ctrl+C actually quits.
    pub(crate) quit_hint_at: Option<Instant>,
    /// `@` mention picker, open while the user is choosing a path.
    pub(crate) mention: Option<MentionCompleter>,
    /// `/` skill picker, open while the user is choosing a skill.
    pub(crate) slash: Option<SlashCompleter>,
    /// Loaded skill summaries (name, description) from `SkillResolver`, powering the `/` picker.
    pub(crate) skill_summaries: Vec<(String, String)>,
    /// The workspace root — in-workspace paths render relative to it.
    pub workspace_root: PathBuf,
    /// The terminal color scheme (startup probe result, see main.rs). Banner
    /// styled runs resolve their colors against this at render time.
    scheme: ColorScheme,
    /// Output pane rect `(x, y, w, h)` in cells, refreshed each draw so mouse
    /// events can be hit-tested against the transcript.
    pub(crate) output_area: Option<(u16, u16, u16, u16)>,
    /// Mapping from visual line index (after markdown expansion) to the
    /// originating `output` line index.  Built by `render_output` each frame;
    /// used by `line_index_at` so mouse selection works correctly on
    /// markdown-expanded content.
    pub(crate) visual_to_output: Vec<usize>,
    /// Plain text for each visual line (parallel to `visual_to_output`).
    /// Built by `render_output`; used by `selection_text` so copy returns
    /// the visually-selected lines rather than the full raw `output` block.
    pub(crate) visual_lines_text: Vec<String>,
    /// Live streaming tail state (pending text/thought + incremental wrap cache).
    /// Root-agent deltas only — see `child_streams` for why children are separate.
    pub(crate) stream: StreamState<BannerStyle>,
    /// Per-child streaming accumulators. Child deltas never touch the shared
    /// `stream`: that buffer's pending tail renders as the main view's live
    /// tail, so a child streaming for minutes flooded it at ~10 events/sec
    /// (session 20260904_3eeb5610 "一直在刷"). Per-child buffers also stop two
    /// interleaved children from chopping each other's pending text into
    /// fragments on every agent switch. Flushed lines land in
    /// `sub_agent_transcripts` (the child's focus view).
    pub(crate) child_streams: BTreeMap<String, StreamState<BannerStyle>>,
    /// Latest live tool-progress line (e.g. streaming `execute_command` output),
    /// shown in the status bar and cleared when the tool call finishes.
    pub(crate) live_progress: Option<String>,
    /// Transient status-bar notice (e.g. "📋 copied …"), cleared on the next key.
    pub(crate) notice: Option<String>,
    /// When the current Running/Waiting stretch began — drives the spinner and
    /// the elapsed readout in the status bar. `None` while Idle. The event loop
    /// has no other clock source: with zero events it never redraws, so a
    /// silent LLM call used to freeze the whole screen (session
    /// 20260904_e6612477 "卡一会"). The ~250 ms tick re-renders while this is
    /// set, keeping spinner/elapsed alive through event-less stretches.
    pub(crate) activity_since: Option<Instant>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            transcript: Transcript::new(),
            composer: Composer::new(),
            status: AgentStatus::Idle,
            running: false,
            viewport: Viewport::new(),
            approval_queue: VecDeque::new(),
            sub_agents: BTreeMap::new(),
            task_panel: TaskPanel::default(),
            sub_agent_transcripts: BTreeMap::new(),
            selection_state: SelectionState::new(),
            quit_hint_at: None,
            mention: None,
            slash: None,
            skill_summaries: Vec::new(),
            workspace_root: PathBuf::new(),
            scheme: ColorScheme::Dark,
            output_area: None,
            visual_to_output: Vec::new(),
            visual_lines_text: Vec::new(),
            stream: StreamState::new(DEFAULT_WRAP_WIDTH),
            child_streams: BTreeMap::new(),
            live_progress: None,
            notice: None,
            activity_since: None,
        }
    }

    /// Append a system/banner line (welcome, workspace, log path).
    pub fn push_system(&mut self, text: &str) {
        self.transcript.push_system(text);
    }

    /// Append startup-banner rows verbatim: one output line per row, **no**
    /// soft-wrap (the wordmark must stay whole; ratatui clips on narrow
    /// terminals). Styled runs become `spans`, resolved against `scheme` at
    /// render time; the plain `text` keeps copy/selection style-blind.
    /// (Brand-side: `BannerRow` runs are flattened to the generic
    /// styled-line entry here, at the product layer.)
    pub fn push_banner(&mut self, rows: Vec<BannerRow>) {
        for row in rows {
            let (text, spans) = row.to_runs();
            self.transcript.push_styled_line(text, spans);
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

    /// Update the output wrap width to match the terminal's content area.
    /// Called each frame from `draw()` so output and composer widths stay
    /// aligned. When the width changes, committed output lines are re-wrapped.
    pub fn set_wrap_width(&mut self, width: usize) {
        let width = width.max(1);
        if width == self.transcript.wrap_width() {
            return;
        }
        self.transcript.set_wrap_width(width);
        // Always rebuild the tail cache at the new width so future streaming
        // text wraps correctly, even when there is no pending text right now.
        self.stream.rewrap(width);
        for child in self.child_streams.values_mut() {
            child.rewrap(width);
        }
    }

    /// Current output wrap width (columns).
    #[allow(dead_code)]
    pub fn current_wrap_width(&self) -> usize {
        self.transcript.wrap_width()
    }

    /// Echo the user's submitted message into the transcript, so the frame log
    /// and output buffer retain the human side of the conversation (the JSONL
    /// turn log already stores `user_input`, but the rendered transcript didn't
    /// show it). `❯` on the first line, indented continuations after.
    pub fn push_user(&mut self, text: &str) {
        self.transcript.push_user(text);
    }

    // ── Event handling ────────────────────────────────────────────────────

    pub fn handle_event(&mut self, ev: TuiEvent) {
        match ev {
            TuiEvent::Runtime(ev) => self.handle_runtime(ev),
            TuiEvent::TurnError(msg) => {
                self.flush_pending();
                let err_text = format!("❌ {msg}");
                for (i, line) in wrap(&err_text, self.transcript.wrap_width()).into_iter().enumerate() {
                    self.transcript.push(OutputLine { spans: None,
                        original: if i == 0 { Some(err_text.clone()) } else { None },
                        detail: None,
                        text: line,
                        kind: LineKind::Error,
                    });
                }
                self.settle_after_turn(false);
            }
            TuiEvent::Lifecycle(snap) => self.apply_lifecycle_snapshot(&snap),
            TuiEvent::TurnDone => {
                self.flush_pending();
                self.settle_after_turn(true);
            }
        }
    }

    /// Common end-of-turn settlement. If sub-agents are still running the task
    /// is NOT done — enter the fan-in `Waiting` state and keep the task panel
    /// alive (the batch injection starts the next turn). Otherwise show the
    /// done marker (unless an error line was already pushed) and clear the
    /// panel.
    fn settle_after_turn(&mut self, show_done_marker: bool) {
        let running = self.running_sub_agents();
        if running > 0 {
            let text = format!("... 等待子 agent 返回（{running} 个运行中），结果将自动注入");
            self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                text,
                kind: LineKind::System,
            });
            self.status = AgentStatus::Waiting { running };
        } else {
            if show_done_marker {
                self.transcript.push(OutputLine { spans: None, original: None,
                    detail: None,
                    text: "✅ done".to_string(),
                    kind: LineKind::Done,
                });
            }
            self.status = AgentStatus::Idle;
            self.activity_since = None;
            self.sub_agents.clear();
            self.sub_agent_transcripts.clear();
            self.child_streams.clear();
        }
        self.running = false;
    }

    /// The live streaming tail as incrementally-wrapped lines.
    /// Returns `None` when there is no uncommitted text/thought.
    pub fn streaming_tail_lines(&self) -> Option<(&[String], LineKind)> {
        self.stream.tail_lines()
    }

    /// Raw pending text for markdown rendering (streaming tail).
    /// Returns `(text, kind)` or `None` when there is nothing pending.
    pub fn streaming_tail_raw(&self) -> Option<(&str, LineKind)> {
        self.stream.tail_raw()
    }

    /// The assistant's last reply as plain text: every committed `Normal` line
    /// since the last user turn, plus any reply still streaming. Tool calls,
    /// tool results, plan lines and the `✅ done` marker are excluded — this is
    /// the prose the user most often wants to grab (e.g. a URL the AI printed).
    pub fn last_reply_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for line in self.transcript.output.iter().rev() {
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
                .stream
                .pending_agent()
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

    // ── @ mention picker ─────────────────────────────────────────────────

    /// Set the workspace root (once, at TUI start); relative paths are rendered
    /// relative to this, outside paths as absolute.
    pub fn set_workspace_root(&mut self, root: PathBuf) {
        self.workspace_root = root;
    }

    /// The active mention picker, if open.
    pub fn mention(&self) -> Option<&MentionCompleter> {
        self.mention.as_ref()
    }

    /// Open the picker with an empty prefix. The caller has already inserted
    /// `@` into the composer.
    pub(crate) fn start_mention(&mut self) {
        self.mention = Some(MentionCompleter::new(self.workspace_root.clone()));
    }

    /// Append a char to the typed prefix, echoing it into the composer so the
    /// picker reads as an inline filter. Newlines are dropped (paths only).
    fn mention_push_char(&mut self, c: char) {
        if c == '\n' {
            return;
        }
        self.composer.insert_char(c);
        if let Some(m) = self.mention.as_mut() {
            let _ = m.handle_key(KeyCode::Char(c));
        }
    }

    /// Key handling while the mention picker is open (swallows everything).
    pub(crate) fn handle_mention_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        let _ = modifiers;
        let Some(m) = self.mention.as_mut() else {
            return None;
        };

        match m.handle_key(code) {
            CompleterAction::Cancelled => {
                let to_delete = 1 + m.prefix().chars().count();
                self.mention = None;
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                None
            }
            CompleterAction::Selected(_) => {
                // 先获取需要删除的字符数和插入的文本
                let to_delete = 1 + m.prefix().chars().count();
                let text = m.finish_text();
                // 然后 take mention
                self.mention = None;
                // 删除 `@prefix`
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                // 插入选中的路径
                self.composer.insert_str(&text);
                None
            }
            CompleterAction::Continue => {
                // 如果是字符输入，需要同步到 composer
                if let KeyCode::Char(c) = code {
                    self.composer.insert_char(c);
                }
                None
            }
            CompleterAction::Unhandled(_) => None,
        }
    }

    // ── / skill picker ──────────────────────────────────────────────────────

    /// Inject the loaded skill summaries (called once at startup from `run_tui`).
    pub fn set_skill_summaries(&mut self, summaries: Vec<(String, String)>) {
        self.skill_summaries = summaries;
    }

    /// The active skill picker, if open.
    pub fn slash(&self) -> Option<&SlashCompleter> {
        self.slash.as_ref()
    }

    /// Open the skill picker (triggered when `/` is typed at line start).
    pub(crate) fn start_slash(&mut self) {
        self.slash = Some(SlashCompleter::new(self.skill_summaries.clone()));
    }

    /// Push a character into the slash prefix.
    fn slash_push_char(&mut self, c: char) {
        if let Some(s) = self.slash.as_mut() {
            let _ = s.handle_key(KeyCode::Char(c));
        }
        self.composer.insert_char(c);
    }

    // ── / skill picker ──────────────────────────────────────────────────────

    /// Key handling while the skill picker is open.
    pub(crate) fn handle_slash_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        let _ = modifiers;
        let Some(s) = self.slash.as_mut() else {
            return None;
        };

        match s.handle_key(code) {
            CompleterAction::Cancelled => {
                let to_delete = 1 + s.prefix().chars().count();
                self.slash = None;
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                None
            }
            CompleterAction::Selected((name, _)) => {
                let to_delete = 1 + s.prefix().chars().count();
                self.slash = None;
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                self.composer.insert_str(&format!("/{name} "));
                None
            }
            CompleterAction::Continue => None,
            CompleterAction::Unhandled(_) => None,
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

    /// Scroll up by `SCROLL_STEP` lines. Returns `true` if the viewport moved.
    pub(crate) fn scroll_up(&mut self) -> bool {
        self.viewport.scroll_up(SCROLL_STEP)
    }

    /// Scroll down by `SCROLL_STEP` lines. Returns `true` if the viewport moved.
    pub(crate) fn scroll_down(&mut self) -> bool {
        self.viewport.scroll_down(SCROLL_STEP)
    }

    /// True while a turn or a child wait is in flight — gates the event loop's
    /// liveness tick (spinner/elapsed need periodic redraws even with zero
    /// events; see `activity_since`).
    pub fn is_active(&self) -> bool {
        !matches!(self.status, AgentStatus::Idle)
    }

    /// Braille spinner frame for the current activity stretch (advances every
    /// 120 ms). Static when idle.
    pub(crate) fn spinner_char(&self) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        let ms = self.activity_since.map_or(0, |t| t.elapsed().as_millis());
        FRAMES[(ms / 120) as usize % FRAMES.len()]
    }

    /// ` · 42s`-style elapsed readout for the active stretch (empty when idle).
    /// Rounded up so a just-started stretch doesn't read as "0s".
    fn elapsed_suffix(&self) -> String {
        match self.activity_since {
            Some(t) => {
                let s = t.elapsed().as_secs() + 1;
                if s < 60 {
                    format!(" - {s}s")
                } else {
                    format!(" - {}m{:02}s", s / 60, s % 60)
                }
            }
            None => String::new(),
        }
    }

    /// The status-bar text for the current state (§9.6).
    pub fn status_line(&self) -> String {
        match &self.status {
            AgentStatus::Idle => {
                if let Some(n) = &self.notice {
                    n.clone()
                } else {
                    "Idle - Enter send | Shift+Enter newline | Ctrl+Y copy | PgUp/PgDn scroll | Ctrl+C quit".to_string()
                }
            }
            AgentStatus::Waiting { running } => {
                format!(
                    "{} 等待子 agent 返回（{running} 个运行中）...{} 结果到达后自动继续",
                    self.spinner_char(),
                    self.elapsed_suffix()
                )
            }
            AgentStatus::Running { phase } => match phase {
                Phase::Thinking => {
                    format!("{} thinking...{} (Ctrl+C cancel)", self.spinner_char(), self.elapsed_suffix())
                }
                Phase::Streaming => {
                    format!("{} streaming...{} (Ctrl+C cancel)", self.spinner_char(), self.elapsed_suffix())
                }
                Phase::ToolCall { tool } => match &self.live_progress {
                    Some(p) => {
                        format!("{} 🔧 {tool}: {p}{}", self.spinner_char(), self.elapsed_suffix())
                    }
                    None => {
                        format!("{} 🔧 {tool}{} (Ctrl+C cancel)", self.spinner_char(), self.elapsed_suffix())
                    }
                },
                Phase::AwaitingApproval => "!! waiting approval... (y/a/n, Ctrl+C cancel)".to_string(),
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

#[cfg(test)]
#[path = "app_tests.rs"]
mod app_tests;
