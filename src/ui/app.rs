//! TUI application state: the independent `RuntimeEvent` consumer.
//!
//! The agent loop (phi-agent) is untouched; its `RuntimeEvent`s are forwarded
//! here through a channel and mapped onto visible state — a scrollable output
//! buffer (tool calls render inline), a status state machine, and the composer.
//! This is the same event stream the `print_event` REPL renderer consumed, so
//! nothing in the framework needed to change for the TUI to exist.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};
use phi_agent::{ApprovalDecision, ApprovalRequest, RuntimeEvent};

use crate::approval::ApprovalItem;
use crate::banner::{BannerRow, ColorScheme, SpanSpec};
use crate::ui::input::Composer;
use crate::ui::completer::{MentionCompleter, SlashCompleter, CompleterAction};
use crate::ui::selection::SelectionState;
use crate::ui::stream::StreamState;
use crate::ui::transcript::{Transcript, DEFAULT_WRAP_WIDTH};
use crate::ui::viewport::Viewport;
use crate::ui::wrap::wrap;

// Selection / ContextMenu / context_menu_pos / CONTEXT_MENU_* moved to
// selection.rs; re-exported here for the renderer and tests.
#[allow(unused_imports)] // re-exported for external consumers (tests use it via super::*)
pub use crate::ui::selection::{
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
    /// Task description (from spawn_agent task argument)
    pub task: String,
    /// Context info (from spawn_agent task argument)
    pub context: String,
    /// When the sub-agent started
    pub started_at: Instant,
    /// When the sub-agent completed (for auto-removal timing)
    pub completed_at: Option<Instant>,
    /// Tool call events (for detail display)
    pub events: Vec<ToolEvent>,
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

/// Structured detail for a tool call, rendered as a multi-line visual block.
/// Attached to `OutputLine.detail`; `None` for non-file tools (zero impact on
/// existing code paths).
#[derive(Debug, Clone)]
pub enum ToolDetail {
    /// Inline diff for `edit_file` / `write_file`.
    Diff { path: String, hunks: Vec<DiffHunk> },
}

/// A diff hunk: a group of related changes with a unified-diff header.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// `"@@ -a,b +c,d @@"` header line.
    pub header: String,
    pub lines: Vec<DiffLine>,
    /// Which edit (0-based) this hunk came from. Used to apply real file line offsets.
    pub edit_index: usize,
}

/// A single line within a diff hunk.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    /// Line number in the old file (1-based). `None` for Add lines.
    pub old_line: Option<u32>,
    /// Line number in the new file (1-based). `None` for Del lines.
    pub new_line: Option<u32>,
}

/// Whether a diff line is an addition, deletion, or unchanged context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Add,
    Del,
    Context,
}

#[derive(Debug, Clone)]
pub struct OutputLine {
    pub text: String,
    pub kind: LineKind,
    /// Optional styled byte-ranges into `text` (startup banner runs). `None`
    /// means the whole line takes the `kind` style. `text` is always the plain
    /// concatenation, so copy/selection and the frame capture stay style-blind.
    pub spans: Option<Vec<SpanSpec>>,
    /// The original (unwrapped) text for this logical block. When the terminal
    /// width changes, lines with a non-empty `original` are re-wrapped. Lines
    /// without `original` (banners, tool calls, plan blocks) are kept as-is.
    /// For multi-line originals (user text, thoughts), the `original` is stored
    /// only on the *first* output line of the block; subsequent lines have
    /// `original = None` and are replaced during re-wrap.
    pub original: Option<String>,
    /// Structured detail for multi-line tool output (e.g. inline diff).
    /// When `Some`, the renderer expands this into a visual block instead of
    /// rendering `text` alone. `None` for all non-file tools.
    pub detail: Option<ToolDetail>,
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
    /// Committed transcript: output lines, the plan block, and wrap width.
    pub transcript: Transcript,
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
    pub sub_agent_transcripts: BTreeMap<String, Vec<OutputLine>>,
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
    pub(crate) stream: StreamState,
    /// Per-child streaming accumulators. Child deltas never touch the shared
    /// `stream`: that buffer's pending tail renders as the main view's live
    /// tail, so a child streaming for minutes flooded it at ~10 events/sec
    /// (session 20260904_3eeb5610 "一直在刷"). Per-child buffers also stop two
    /// interleaved children from chopping each other's pending text into
    /// fragments on every agent switch. Flushed lines land in
    /// `sub_agent_transcripts` (the child's focus view).
    pub(crate) child_streams: BTreeMap<String, StreamState>,
    /// Latest live tool-progress line (e.g. streaming `execute_command` output),
    /// shown in the status bar and cleared when the tool call finishes.
    pub(crate) live_progress: Option<String>,
    /// Transient status-bar notice (e.g. "📋 copied …"), cleared on the next key.
    pub(crate) notice: Option<String>,
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
    pub fn push_banner(&mut self, rows: Vec<BannerRow>) {
        self.transcript.push_banner(rows);
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
            let text = format!("⏳ 等待子 agent 返回（{running} 个运行中），结果将自动注入");
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
            self.sub_agents.clear();
            self.sub_agent_transcripts.clear();
            self.child_streams.clear();
        }
        self.running = false;
    }

    /// Number of tracked sub-agents still `Running`.
    fn running_sub_agents(&self) -> usize {
        self.sub_agents.values().filter(|s| s.status == SubAgentStatus::Running).count()
    }

    /// A sub-agent finished (watcher Progress event): mark it done in the task
    /// panel and, while in the `Waiting` state, refresh the remaining count.
    pub fn mark_sub_agent_finished(&mut self, agent_path: &str) {
        if let Some(state) = self.sub_agents.get_mut(agent_path) {
            if state.status == SubAgentStatus::Running {
                state.status = SubAgentStatus::Done;
                state.completed_at = Some(std::time::Instant::now());
            }
        }
        self.refresh_waiting_count();
    }

    /// All pending sub-agents finished (batch injection): mark every still-
    /// running entry done so the 3s auto-cleanup can sweep the panel.
    pub fn mark_all_sub_agents_finished(&mut self) {
        for state in self.sub_agents.values_mut() {
            if state.status == SubAgentStatus::Running {
                state.status = SubAgentStatus::Done;
                state.completed_at = Some(std::time::Instant::now());
            }
        }
        self.refresh_waiting_count();
    }

    /// While `Waiting`, recompute the running count from the panel; drop to
    /// `Idle` when the last sub-agent finished (the batch injection follows).
    fn refresh_waiting_count(&mut self) {
        if let AgentStatus::Waiting { .. } = self.status {
            let n = self.running_sub_agents();
            self.status = if n > 0 {
                AgentStatus::Waiting { running: n }
            } else {
                AgentStatus::Idle
            };
        }
    }

    /// Record a sub-agent's first appearance — emitting a `⏺ [p] started` marker
    /// and registering it in `sub_agents` — so the transcript and status strip
    /// show its lifecycle. No-op for the root agent (`agent_id == None`) and for
    /// sub-agents already seen this turn.
    pub(crate) fn track_agent(&mut self, agent_id: Option<&str>) {
        let Some(p) = agent_id.filter(|p| !p.is_empty()) else {
            return;
        };
        if !self.sub_agents.contains_key(p) {
            // Any pending root text precedes the first sub-agent event, so flush
            // it before the `started` marker to keep transcript order correct.
            self.flush_pending();
            // Extract task name from agent_id (format: "root/<task_name>")
            let name = p.split('/').last().unwrap_or(p).to_string();
            self.sub_agents.insert(p.to_string(), SubAgentState {
                name,
                status: SubAgentStatus::Running,
                files: Vec::new(),
                task: String::new(),
                context: String::new(),
                started_at: Instant::now(),
                completed_at: None,
                events: Vec::new(),
            });
            self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                text: format!("⏺ [{p}] started"),
                kind: LineKind::Tool,
            });
        }
    }

    /// Whether the task panel should be shown (has active sub-agents).
    pub fn should_show_task_panel(&self) -> bool {
        !self.sub_agents.is_empty()
    }

    /// Remove completed sub-agents that have been done for more than 3 seconds.
    /// Returns true if any agents were removed.
    pub fn cleanup_completed_agents(&mut self) -> bool {
        // Only reap when the root is fully Idle. While the root is Waiting
        // (children still running) the finished children's transcripts are the
        // only place the user can review what they did — deleting them 3s after
        // completion made sub-agent execution invisible (session 20260903 fan-in
        // made "turn ends while children run" the norm, so this fired constantly).
        if !matches!(self.status, AgentStatus::Idle) {
            return false;
        }
        // Likewise while the user is inspecting the task list: reaping under
        // their eyes is exactly the "can't see what the sub-agent did" bug.
        // Cleanup resumes once focus returns to the input box.
        if matches!(self.task_panel.focus, FocusTarget::TaskList(_)) {
            return false;
        }
        let now = Instant::now();
        let mut removed = false;
        let ids_to_remove: Vec<String> = self.sub_agents.iter()
            .filter(|(_, state)| {
                state.status == SubAgentStatus::Done &&
                state.completed_at.map_or(false, |at| now.duration_since(at).as_secs() >= 3)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in ids_to_remove {
            self.sub_agents.remove(&id);
            self.sub_agent_transcripts.remove(&id);
            self.child_streams.remove(&id);
            removed = true;
        }

        // Reset focus if it's now out of bounds
        if let FocusTarget::TaskList(index) = &self.task_panel.focus {
            if *index >= self.sub_agents.len() {
                self.task_panel.focus = if self.sub_agents.is_empty() {
                    FocusTarget::Input
                } else {
                    FocusTarget::TaskList(self.sub_agents.len() - 1)
                };
            }
        }

        removed
    }

    /// Commit any pending streaming text/thought into the transcript (thought
    /// first, then prose). Called on structural events and turn end.
    pub(crate) fn flush_pending(&mut self) {
        let had_text = self.stream.has_pending_text();
        let had_thought = self.stream.has_pending_thought();
        // Get the pending agent BEFORE flushing (flush takes it)
        let pending_agent = self.stream.pending_agent().map(|s| s.to_string());
        let flushed = self.stream.flush();
        // Route flushed lines to the correct transcript based on pending_agent
        for line in flushed {
            if let Some(ref agent_id) = pending_agent {
                if !agent_id.is_empty() {
                    self.sub_agent_transcripts
                        .entry(agent_id.clone())
                        .or_default()
                        .push(line);
                } else {
                    self.transcript.push(line);
                }
            } else {
                self.transcript.push(line);
            }
        }
        if had_text || had_thought {
            tracing::info!(
                follow_bottom = self.viewport.follow_bottom,
                scroll_offset = self.viewport.scroll_offset,
                output_len = self.transcript.len(),
                had_text,
                had_thought,
                "flush_pending: view state"
            );
        }
    }

    /// The live streaming tail as incrementally-wrapped lines.
    /// Returns `None` when there is no uncommitted text/thought.
    pub fn streaming_tail_lines(&self) -> Option<(&[String], LineKind)> {
        self.stream.tail_lines()
    }

    // ── Per-child streaming ───────────────────────────────────────────────

    /// Accumulate a child's prose delta into its own stream buffer, returning
    /// any lines the flush committed (caller routes them into the child's
    /// transcript). Lines are prefixed `[<id>] ` like the pre-split behavior.
    pub(crate) fn push_child_text(&mut self, id: &str, text: &str) -> Vec<OutputLine> {
        self.child_streams
            .entry(id.to_string())
            .or_insert_with(|| StreamState::new(DEFAULT_WRAP_WIDTH))
            .push_text(text, Some(id))
    }

    /// Accumulate a child's reasoning delta into its own stream buffer.
    pub(crate) fn push_child_thought(&mut self, id: &str, text: &str) -> Vec<OutputLine> {
        self.child_streams
            .entry(id.to_string())
            .or_insert_with(|| StreamState::new(DEFAULT_WRAP_WIDTH))
            .push_thought(text, Some(id))
    }

    /// Commit a child's pending text/thought into whole lines (e.g. before its
    /// tool-invocation line or its `done` marker, so ordering inside the
    /// child's transcript stays chronological). Empty when the child has no
    /// pending stream content.
    pub(crate) fn flush_child_stream(&mut self, id: &str) -> Vec<OutputLine> {
        match self.child_streams.get_mut(id) {
            Some(st) => st.flush(),
            None => Vec::new(),
        }
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

    /// Remove the last prefix char (and the matching composer char), or cancel
    /// the picker entirely when the prefix is already empty.
    fn mention_backspace(&mut self) {
        let empty = self.mention.as_ref().map_or(true, |m| m.is_empty());
        if empty {
            self.mention = None;
            self.composer.backspace(); // remove the `@`
        } else {
            self.composer.backspace();
            if let Some(m) = self.mention.as_mut() {
                let _ = m.handle_key(KeyCode::Backspace);
            }
        }
    }

    fn move_mention(&mut self, delta: i32) {
        if let Some(m) = self.mention.as_mut() {
            let _ = m.handle_key(if delta > 0 { KeyCode::Down } else { KeyCode::Up });
        }
    }

    /// Replace `@<prefix>` in the composer with the selected path (or the typed
    /// prefix, when the synthetic row is highlighted).
    fn finish_mention(&mut self) {
        let Some(m) = self.mention.take() else {
            return;
        };
        let to_delete = 1 + m.prefix().chars().count();
        for _ in 0..to_delete {
            self.composer.backspace();
        }
        let text = m.finish_text();
        self.composer.insert_str(&text);
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

    /// Backspace in the slash picker: remove last char from prefix; close if
    /// prefix becomes empty and the user backspaces again (removes the `/`).
    fn slash_backspace(&mut self) {
        let empty = self.slash.as_ref().map_or(true, |s| s.is_empty());
        if empty {
            // Prefix is already empty → user is deleting the `/` itself → close picker
            self.slash = None;
            self.composer.backspace();
            return;
        }
        if let Some(s) = self.slash.as_mut() {
            let _ = s.handle_key(KeyCode::Backspace);
        }
        self.composer.backspace();
    }

    /// Move the slash picker highlight by `delta` (±1).
    fn move_slash(&mut self, delta: i32) {
        if let Some(s) = self.slash.as_mut() {
            let _ = s.handle_key(if delta > 0 { KeyCode::Down } else { KeyCode::Up });
        }
    }

    /// Confirm selection: replace `/prefix` in the composer with `/selected-name `.
    fn finish_slash(&mut self) {
        let Some(s) = self.slash.take() else { return };
        let prefix_len = s.prefix().chars().count();
        let name = s.selected_item()
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| s.prefix().to_string());
        // 删掉已输入的 `/prefix`
        let to_delete = 1 + prefix_len;
        for _ in 0..to_delete {
            self.composer.backspace();
        }
        // 插入 `/name `（带尾部空格，方便用户继续输入参数）
        self.composer.insert_str(&format!("/{name} "));
    }

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
            AgentStatus::Waiting { running } => {
                format!("⏳ 等待子 agent 返回（{running} 个运行中）… 结果到达后自动继续")
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


/// `"[path] "` for a sub-agent, `""` for the root agent — labels a sub-agent's
/// lines so they read `[root/searcher] …` in the transcript.
pub fn agent_prefix(agent_id: Option<&str>) -> String {
    match agent_id {
        Some(p) if !p.is_empty() => format!("[{p}] "),
        _ => String::new(),
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod app_tests;

#[cfg(test)]
#[path = "task_panel_tests.rs"]
mod task_panel_tests;
