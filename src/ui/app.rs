//! TUI application state: the independent `RuntimeEvent` consumer.
//!
//! The agent loop (phi-agent) is untouched; its `RuntimeEvent`s are forwarded
//! here through a channel and mapped onto visible state — a scrollable output
//! buffer (tool calls render inline), a status state machine, and the composer.
//! This is the same event stream the `print_event` REPL renderer consumed, so
//! nothing in the framework needed to change for the TUI to exist.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use phi_agent::{ApprovalDecision, ApprovalRequest, RegistrySnapshot, RuntimeEvent};
use phi_kernel_tools::background_shell::{BackgroundTaskRegistry, BackgroundTaskStatus};

use crate::approval::ApprovalItem;
use crate::banner::{BannerRow, BannerStyle, ColorScheme};
use phi_tui::input::Composer;
use phi_tui::lines::{LineKind, OutputLine};
use phi_tui::completer::{MentionCompleter, SlashCompleter, CompleterAction};
use phi_tui::selection::SelectionState;
use phi_tui::stream::StreamState;
use phi_tui::transcript::{Transcript, DEFAULT_WRAP_WIDTH};
use phi_tui::visual::VisualMap;
use phi_tui::viewport::Viewport;
use phi_tui::wrap::wrap;

// Selection / ContextMenu / context_menu_pos / CONTEXT_MENU_* moved to
// selection.rs; re-exported here for the renderer and tests.
#[allow(unused_imports)] // re-exported for external consumers (tests use it via super::*)
pub use phi_tui::selection::{
    ContextMenu, Selection, CONTEXT_MENU_H, CONTEXT_MENU_W, context_menu_pos,
};

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
    /// The root turn ended but work is still in flight — sub-agents
    /// (`running`, fan-in wake) and/or background shell tasks (`bg`, bg
    /// wake). The two counts are independent: either alone, or both, keep
    /// the agent in `Waiting` instead of dropping to `Idle`.
    Waiting { running: usize, bg: usize },
    Running { phase: Phase },
}

/// Live state of a sub-agent shown in the task panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAgentStatus {
    Running,
    Done,
}

/// A background shell task tracked in the task panel.
#[derive(Debug, Clone)]
pub struct BackgroundTaskEntry {
    pub id: String,
    pub command: String,
    /// Per-call timeout fuse in ms — mirrors the registry's snapshot. The
    /// sole classifier for [`Self::is_indefinite`]: `0` means the task was
    /// launched daemon-style (no fuse), not that a fuse is pending.
    pub timeout_ms: u64,
    pub status: BackgroundTaskStatus,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    /// Whether this task's completion has already been included in a
    /// background-wake injection (`take_bg_wake`). One task is reported to
    /// the agent at most once — a later wake (new tasks spawned after the
    /// report turn) lists only the not-yet-reported ones.
    pub reported: bool,
}

impl BackgroundTaskEntry {
    /// Daemon-style task (`timeout_ms == 0`): a server/watcher that runs until
    /// it dies. Not "work in flight" — there is nothing to wait on and no
    /// completion to promise. Its exit is still the interesting event (a died
    /// server must be reported), so it stays wake-worthy; it just never holds
    /// the user in a fake "waiting" state.
    pub(crate) fn is_indefinite(&self) -> bool {
        self.timeout_ms == 0
    }
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

/// Lines per wheel tick. 1 is the finest step the terminal's line grid
/// allows — a trackpad swipe is a stream of ticks, so it composes into the
/// smoothest possible motion; PgUp/PgDn stay the fast path for history.
pub(crate) const WHEEL_STEP: usize = 1;

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
    /// An update check found a new version.
    UpdateAvailable { version: String, download_url: String },
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
    /// Visual-row map for the current frame (visual row -> output index +
    /// plain-text twin), rebuilt by `render_output` each frame. Feeds mouse
    /// hit-testing (`line_index_at`), copy (`selection_text`) and the
    /// block-aware scroll anchor (`phi_tui::visual`).
    pub(crate) visual_map: VisualMap,
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
    /// Persistent upgrade hint (e.g. "⬆ phimint 0.2.0 available — type /upgrade").
    /// Survives keypresses; cleared on /upgrade or when already up-to-date.
    pub(crate) upgrade_hint: Option<String>,
    /// When the current Running/Waiting stretch began — drives the spinner and
    /// the elapsed readout in the status bar. `None` while Idle. The event loop
    /// has no other clock source: with zero events it never redraws, so a
    /// silent LLM call used to freeze the whole screen (session
    /// 20260904_e6612477 "卡一会"). The ~250 ms tick re-renders while this is
    /// set, keeping spinner/elapsed alive through event-less stretches.
    pub(crate) activity_since: Option<Instant>,
    /// Fold state for committed thought blocks: `false` = collapsed summary
    /// lines, `true` = full text inline. Global toggle (Ctrl+O); folding is a
    /// render-time decision — `OutputLine` always carries the full text.
    /// Consumed by the thought-fold renderer (render.rs).
    pub(crate) show_thoughts: bool,
    /// When each stream's current pending thought segment began, keyed by
    /// agent path (`""` = the root stream, matching the `agent_id` routing
    /// convention). A segment's first delta inserts, its owning flush
    /// removes; drives the thinking panel's elapsed readout.
    pub(crate) thinking_since: HashMap<String, Instant>,
    /// Background task registry (injected from agent::build).
    pub(crate) background_registry: Option<Arc<BackgroundTaskRegistry>>,
    /// Background shell tasks displayed in the task panel.
    pub(crate) background_tasks: BTreeMap<String, BackgroundTaskEntry>,
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
            visual_map: VisualMap::new(),
            stream: StreamState::new(DEFAULT_WRAP_WIDTH),
            child_streams: BTreeMap::new(),
            live_progress: None,
            notice: None,
            upgrade_hint: None,
            activity_since: None,
            show_thoughts: false,
            thinking_since: HashMap::new(),
            background_registry: None,
            background_tasks: BTreeMap::new(),
        }
    }

    /// Inject the background task registry (from agent::build).
    pub fn set_background_registry(&mut self, registry: Arc<BackgroundTaskRegistry>) {
        self.background_registry = Some(registry);
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
            TuiEvent::UpdateAvailable { version, .. } => {
                self.upgrade_hint = Some(format!("⬆ phimint {version} available - type /upgrade"));
                tracing::info!(version = %version, "upgrade available");
            }
        }
    }

    /// Common end-of-turn settlement. If sub-agents or *bounded* background
    /// jobs are still running the task is NOT done — enter the `Waiting` state
    /// and keep the bookkeeping alive (the fan-in batch injection or the
    /// background wake starts the next turn automatically). Indefinite daemons
    /// never hold the wait: "the server is up" is steady state, not pending
    /// work, and promising a completion report for it would be a lie (the
    /// confusion this caused is session 20260922_6d262d0f).
    fn settle_after_turn(&mut self, show_done_marker: bool) {
        let (_, daemons) = self.bg_running_split();
        if self.settle_status_from_inflight() {
            if let AgentStatus::Waiting { running, .. } = &self.status {
                let (bounded, _) = self.bg_running_split();
                let mut text = match (*running, bounded) {
                    (r, 0) => {
                        format!("... 等待子 agent 返回（{r} 个运行中），结果将自动注入")
                    }
                    (0, b) => format!(
                        "... 等待后台任务完成（{b} 个运行中），完成后将自动汇报结果"
                    ),
                    (r, b) => format!(
                        "... 等待子 agent（{r} 个）+ 后台任务（{b} 个）完成，结果将自动注入"
                    ),
                };
                if daemons > 0 {
                    text.push_str(&format!("（另有 {daemons} 个后台服务长驻运行中）"));
                }
                self.transcript.push(OutputLine { spans: None, original: None,
                    detail: None,
                    text,
                    kind: LineKind::System,
                });
            }
        } else if show_done_marker {
            self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                text: "✅ done".to_string(),
                kind: LineKind::Done,
            });
        }
        // Daemon-only settlement: the agent IS done — say so plainly, then
        // note the service without any "waiting" framing.
        if daemons > 0 && matches!(self.status, AgentStatus::Idle) {
            self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                text: format!("🟢 后台服务 {daemons} 个运行中（长驻；异常退出时会自动通知）"),
                kind: LineKind::System,
            });
        }
        self.running = false;
    }

    /// Settle the status enum from in-flight work — the single source of
    /// truth for "can the agent rest?". Sub-agents and/or *bounded* background
    /// jobs still running → `Waiting { running, bg }` (returns `true`; `bg`
    /// counts every running background task, daemons included, so the counter
    /// never under-reports). Only indefinite daemons (or nothing) → `Idle`:
    /// a server left running is the product working as intended.
    ///
    /// Every turn-end path (TurnDone/TurnError settlement, root `RunFinished`,
    /// root `RunCancelled`) must go through this so a running bounded job can
    /// never be dropped on the floor as `Idle` — the user would see "done"
    /// while the task is still working.
    pub(crate) fn settle_status_from_inflight(&mut self) -> bool {
        let running = self.running_sub_agents();
        let (bounded, daemons) = self.bg_running_split();
        if running > 0 || bounded > 0 {
            self.status = AgentStatus::Waiting { running, bg: bounded + daemons };
            true
        } else {
            self.status = AgentStatus::Idle;
            self.activity_since = None;
            self.sub_agents.clear();
            self.sub_agent_transcripts.clear();
            self.child_streams.clear();
            self.thinking_since.clear();
            false
        }
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

        // Save old prefix to detect directory navigation
        let old_prefix = m.prefix().to_string();

        match m.handle_key(code) {
            CompleterAction::Cancelled => {
                self.mention = None;
                // Esc with a typed prefix: the text is already visible in the
                // composer, so closing the picker must not wipe it. Only a
                // bare `@` (nothing typed yet) or backspacing out removes the
                // trigger from the input.
                let keep_text = matches!(code, KeyCode::Esc) && !old_prefix.is_empty();
                if !keep_text {
                    let to_delete = 1 + old_prefix.chars().count();
                    for _ in 0..to_delete {
                        self.composer.backspace();
                    }
                }
                None
            }
            CompleterAction::Selected(_) => {
                // Get delete count and insertion text first
                let to_delete = 1 + old_prefix.chars().count();
                let text = m.finish_text();
                // Then take mention
                self.mention = None;
                // Delete `@prefix`
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                // Insert selected path
                self.composer.insert_str(&text);
                None
            }
            CompleterAction::Continue => {
                // Check if prefix changed (directory navigation)
                let new_prefix = m.prefix().to_string();
                if new_prefix != old_prefix {
                    // Prefix changed, sync composer
                    // Delete old prefix
                    for _ in 0..old_prefix.chars().count() {
                        self.composer.backspace();
                    }
                    // Insert new prefix
                    self.composer.insert_str(&new_prefix);
                } else if let KeyCode::Char(c) = code {
                    // Normal char input, sync to composer
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

        // Capture old prefix BEFORE handle_key modifies it (same pattern as handle_mention_key)
        let old_prefix = s.prefix().to_string();

        match s.handle_key(code) {
            CompleterAction::Cancelled => {
                // Delete "/" + old_prefix from composer
                let to_delete = 1 + old_prefix.chars().count();
                self.slash = None;
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                None
            }
            CompleterAction::Selected((name, _)) => {
                // Delete "/" + old_prefix from composer, insert selected command
                let to_delete = 1 + old_prefix.chars().count();
                self.slash = None;
                for _ in 0..to_delete {
                    self.composer.backspace();
                }
                self.composer.insert_str(&format!("/{name} "));
                None
            }
            CompleterAction::Continue => {
                // Sync composer with completer prefix (composer is source of truth for display)
                let new_prefix = s.prefix().to_string();
                if new_prefix != old_prefix {
                    // Prefix changed (char typed or backspace): replace in composer
                    for _ in 0..old_prefix.chars().count() {
                        self.composer.backspace();
                    }
                    self.composer.insert_str(&new_prefix);
                }
                None
            }
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

    /// Scroll up by half a screen (one PageUp press). Returns `true` if the
    /// viewport moved.
    pub(crate) fn scroll_up(&mut self) -> bool {
        self.viewport.scroll_up(self.viewport.page_step())
    }

    /// Scroll down by half a screen (one PageDown press). Returns `true` if
    /// the viewport moved.
    pub(crate) fn scroll_down(&mut self) -> bool {
        self.viewport.scroll_down(self.viewport.page_step())
    }

    /// Snap back to the newest lines (re-enter follow-bottom). Used on
    /// submit so the reply streams into view.
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.viewport.scroll_to_bottom();
    }

    /// Scroll up by a few lines (one wheel tick). A trackpad swipe delivers
    /// a rapid stream of wheel events, so a small step composes into smooth
    /// motion; the half-page step stays reserved for PgUp/PgDn presses.
    pub(crate) fn scroll_wheel_up(&mut self) -> bool {
        self.viewport.scroll_up(WHEEL_STEP)
    }

    /// Scroll down by a few lines (one wheel tick).
    pub(crate) fn scroll_wheel_down(&mut self) -> bool {
        self.viewport.scroll_down(WHEEL_STEP)
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

    /// `9s` / `10m37s` — shared by the status-bar suffix and the thinking
    /// panel title so a long stretch reads the same in both places.
    pub(crate) fn fmt_elapsed(secs: u64) -> String {
        if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m{:02}s", secs / 60, secs % 60)
        }
    }

    /// ` · 42s`-style elapsed readout for the active stretch (empty when idle).
    /// Rounded up so a just-started stretch doesn't read as "0s".
    fn elapsed_suffix(&self) -> String {
        match self.activity_since {
            Some(t) => format!(" - {}", Self::fmt_elapsed(t.elapsed().as_secs() + 1)),
            None => String::new(),
        }
    }

    /// The status-bar text for the current state (§9.6).
    ///
    /// Wording follows the bounded/daemon split: bounded jobs carry a
    /// completion promise (auto-report when done), daemons are merely counted
    /// as running services — the two must never share a "waiting for
    /// completion" promise.
    pub fn status_line(&self) -> String {
        match &self.status {
            AgentStatus::Idle => {
                let base = if let Some(u) = &self.upgrade_hint {
                    u.clone()
                } else if let Some(n) = &self.notice {
                    n.clone()
                } else {
                    "Idle - Enter send | Shift+Enter newline | Ctrl+Y copy | 滚轮/fn+Up/Down scroll | Ctrl+C quit".to_string()
                };
                let (_, daemons) = self.bg_running_split();
                if daemons > 0 {
                    format!("{base}（后台服务 {daemons} 个）")
                } else {
                    base
                }
            }
            AgentStatus::Waiting { running, .. } => {
                let (bounded, daemons) = self.bg_running_split();
                let mut line = match (*running, bounded) {
                    (_, 0) => format!(
                        "{} 等待子 agent 返回（{running} 个运行中）...{} 结果到达后自动继续",
                        self.spinner_char(),
                        self.elapsed_suffix()
                    ),
                    (0, _) => format!(
                        "{} 等待后台任务（{bounded} 个运行中）...{} 完成后自动汇报结果",
                        self.spinner_char(),
                        self.elapsed_suffix()
                    ),
                    (_, _) => format!(
                        "{} 等待子 agent（{running} 个）+ 后台任务（{bounded} 个）...{} 完成后自动继续",
                        self.spinner_char(),
                        self.elapsed_suffix()
                    ),
                };
                if daemons > 0 {
                    line.push_str(&format!("（另有 {daemons} 个后台服务）"));
                }
                line
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
