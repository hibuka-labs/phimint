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

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use agent_base::{PlanStepStatus, UserEvent};
use phi_agent::{ApprovalDecision, ApprovalRequest, RuntimeEvent};

use crate::approval::ApprovalItem;
use crate::banner::{BannerRow, ColorScheme, SpanSpec};
use crate::ui::input::Composer;
use crate::ui::completer::{MentionCompleter, SlashCompleter, CompleterAction};
use crate::ui::selection::{MenuClick, SelectionState};
use crate::ui::stream::StreamState;
use crate::ui::transcript::{Transcript, DEFAULT_WRAP_WIDTH};
use crate::ui::viewport::Viewport;
use crate::ui::wrap::{one_line, wrap};

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
    /// The original (unwrapped) text for this logical block. When the terminal
    /// width changes, lines with a non-empty `original` are re-wrapped. Lines
    /// without `original` (banners, tool calls, plan blocks) are kept as-is.
    /// For multi-line originals (user text, thoughts), the `original` is stored
    /// only on the *first* output line of the block; subsequent lines have
    /// `original = None` and are replaced during re-wrap.
    pub original: Option<String>,
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
    pub sub_agents: BTreeMap<String, SubAgentStatus>,
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
    pub(crate) stream: StreamState,
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
                        text: line,
                        kind: LineKind::Error,
                    });
                }
                self.status = AgentStatus::Idle;
                self.running = false;
                self.sub_agents.clear();
            }
            TuiEvent::TurnDone => {
                self.flush_pending();
                self.transcript.push(OutputLine { spans: None, original: None,
                    text: "✅ done".to_string(),
                    kind: LineKind::Done,
                });
                self.status = AgentStatus::Idle;
                self.running = false;
                self.sub_agents.clear();
            }
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
            self.sub_agents.insert(p.to_string(), SubAgentStatus::Running);
            self.transcript.push(OutputLine { spans: None, original: None,
                text: format!("⏺ [{p}] started"),
                kind: LineKind::Tool,
            });
        }
    }

    /// Commit any pending streaming text/thought into the transcript (thought
    /// first, then prose). Called on structural events and turn end.
    pub(crate) fn flush_pending(&mut self) {
        let had_text = self.stream.has_pending_text();
        let had_thought = self.stream.has_pending_thought();
        let flushed = self.stream.flush();
        self.transcript.extend(flushed);
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
        if let Some(click) = self.selection_state.menu_click(kind, x, y, area_w, area_h) {
            return match click {
                MenuClick::Copy => Some(Action::CopySelection),
                MenuClick::Dismiss => None,
            };
        }

        match kind {
            Down(MouseButton::Left) => {
                let idx = self.line_index_at(x, y);
                tracing::debug!(x, y, selected_idx = ?idx, "mouse left down");
                self.selection_state.anchor(idx);
                None
            }
            Drag(MouseButton::Left) => {
                let idx = self.line_index_at(x, y);
                self.selection_state.extend(idx);
                None
            }
            Up(MouseButton::Left) => None,
            Down(MouseButton::Right) => {
                self.selection_state.open_menu(x, y);
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
        let committed = self.transcript.len();

        // Use visual_to_output mapping when available (after render), otherwise
        // fall back to direct output indices (for tests or before first render).
        if self.visual_to_output.is_empty() {
            // Fallback: no markdown expansion, output lines map 1:1 to visual lines.
            let tail = self.streaming_tail_lines().map(|(l, _)| l.len()).unwrap_or(0);
            let window = self.viewport.window_range(committed + tail, ah as usize);
            let idx = window.start + row;
            tracing::debug!(
                x, y, row, committed, tail,
                window_start = window.start, window_end = window.end, idx,
                "line_index_at (fallback, no mapping)"
            );
            if idx < window.end && idx < committed {
                return Some(idx);
            }
            return None;
        }

        let total = self.viewport.rendered_total;
        let window = self.viewport.window_range(total, ah as usize);
        let visual_idx = window.start + row;
        let out_idx = self.visual_to_output.get(visual_idx).copied();
        tracing::debug!(
            x, y, row, total, committed,
            window_start = window.start, window_end = window.end,
            visual_idx, out_idx,
            mapping_len = self.visual_to_output.len(),
            "line_index_at"
        );
        if visual_idx < window.end && visual_idx < self.visual_to_output.len() {
            let mapped = self.visual_to_output[visual_idx];
            if mapped < committed {
                return Some(visual_idx); // return visual index, not output index
            }
        }
        None
    }

    /// True when visual line `i` falls inside the active selection.
    pub fn is_selected(&self, i: usize) -> bool {
        let total = if self.visual_to_output.is_empty() {
            self.transcript.len()
        } else {
            self.viewport.rendered_total
        };
        self.selection_state.is_selected(i, total)
    }

    /// The selected lines joined as plain text (what-you-see-is-what-you-copy).
    /// Uses `visual_lines_text` so the copy matches exactly what the user
    /// selected visually, not the full raw `output` block.
    pub fn selection_text(&self) -> String {
        self.selection_state
            .text(&self.transcript.output, &self.visual_lines_text)
    }

    /// Clear the active transcript selection.
    pub fn clear_selection(&mut self) {
        self.selection_state.clear();
    }

    /// The right-click menu, if open.
    pub fn context_menu(&self) -> Option<&ContextMenu> {
        self.selection_state.context_menu()
    }

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
