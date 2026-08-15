//! Inline terminal UI (opt-in via `--inline`): a Claude Code / codex-style chat on the main screen.
//!
//! This was the fix for the three TUI complaints from Phase 5:
//!
//! 1. **Scroll** — it never enters the alternate screen, so stdout scrolls into
//!    the terminal's native scrollback. Trackpad/wheel and terminal scrollback
//!    "just work"; nothing is pinned to the last screen.
//! 2. **"Stuck" during streaming** — `TextDelta` chunks are written straight to
//!    stdout (flushed each time). There is no `pending_text` buffer to wait on a
//!    structural event, so a long answer appears token-by-token.
//! 3. **Noise** — reasoning is hidden behind an animated `thinking…` status line
//!    (still persisted to the turn JSONL), and tool results collapse to one line.
//!
//! It reuses the TUI's proven event architecture — a spawned agent task forwards
//! `RuntimeEvent`s over an mpsc channel and the main loop drains them while
//! polling keyboard input — but renders to stdout instead of a ratatui frame.
//! The ratatui TUI is the default; this is the `--inline` alternative.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use phi_agent::{ApprovalDecision, PhiAgent, RiskLevel, RuntimeEvent, SessionContext, SessionId, save_turn_log};
use tokio::sync::mpsc;

use crate::approval::ApprovalItem;
use crate::ui::app::{Phase, TuiEvent};
use crate::ui::input::Composer;

/// A command from the main loop to the agent task.
enum Cmd {
    Run(String),
    Quit,
}

/// Spinner frames for the status line, ticked on each poll timeout.
const SPINNERS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Duplicates writes to a second sink (the capture file) so an inline session
/// can be replayed offline — the inline equivalent of the TUI's `frames.txt`.
struct Tee<A: Write, B: Write> {
    primary: A,
    capture: B,
}

impl<A: Write, B: Write> Tee<A, B> {
    fn new(primary: A, capture: B) -> Self {
        Self { primary, capture }
    }
}

impl<A: Write, B: Write> Write for Tee<A, B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Mirror to the capture first (best-effort), then the real sink.
        let _ = self.capture.write_all(buf);
        self.primary.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.primary.flush()
    }
}

/// Run the interactive inline UI until the user quits.
///
/// Takes ownership of the agent/session because the turn runner lives in a
/// spawned task for the whole lifetime of the UI (same contract as `run_tui`).
pub async fn run_inline(
    agent: PhiAgent,
    session: SessionId,
    session_ctx: SessionContext,
    workspace: PathBuf,
    approval_rx: Option<mpsc::UnboundedReceiver<ApprovalItem>>,
) -> Result<()> {
    let agent = Arc::new(agent);
    let mut approval_rx = approval_rx;
    let session_dir = session_ctx.session_dir.display().to_string();
    let log_path = session_ctx.log_path().display().to_string();
    let inline_raw = session_ctx.session_dir.join("inline.raw");

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

    let agent_task = tokio::spawn(agent_loop(
        agent.clone(),
        session,
        session_ctx,
        event_tx,
        cmd_rx,
    ));

    // Raw mode + bracketed paste only — deliberately NO `EnterAlternateScreen`.
    // The guard restores the terminal on any exit path.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnableBracketedPaste)?;
    let _guard = TerminalGuard;

    // Tee everything we render to a capture file (`<session>/inline.raw`) so the
    // session can be replayed offline with `cargo run --bin replay`.
    let mut capture = std::io::BufWriter::new(std::fs::File::create(&inline_raw)?);
    let mut renderer = Renderer::new(Tee::new(io::stdout(), &mut capture));
    renderer.line("phiforge — coding agent on phi-agent.");
    renderer.line(&format!("Workspace: {}", workspace.display()));
    renderer.line(&format!("Logs: {log_path}"));
    renderer.line(&format!("Session: {session_dir}"));
    // Key hints now live inside the input box (see `render_input`), not here.

    let mut composer = Composer::new();
    let mut cursor_line = 0usize;
    renderer.render_input(&composer, &mut cursor_line);

    let mut state = Inline::new();

    loop {
        // Drain any events streamed since the last iteration.
        while let Ok(ev) = event_rx.try_recv() {
            state.on_event(ev, &mut renderer);
        }

        // Drain queued approval requests into the pending queue (one prompt at
        // a time — the front of the queue is what gets rendered).
        if let Some(rx) = approval_rx.as_mut() {
            while let Ok(item) = rx.try_recv() {
                state.pending.push_back(item);
                state.approval_dirty = true;
            }
        }

        // Approval prompt: render once, then read a single decision key.
        if !state.pending.is_empty() {
            if state.approval_dirty {
                render_approval(&mut renderer, state.pending.front().unwrap());
                state.approval_dirty = false;
            }
            if crossterm::event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = crossterm::event::read()? {
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                            agent.cancel();
                            state.pending.clear();
                            if let Some(rx) = approval_rx.as_mut() {
                                while rx.try_recv().is_ok() {}
                            }
                            renderer.line("⏹ cancelled");
                            state.running = false;
                            state.prompt_drawn = false;
                        } else if let Some(decision) = approval_key(key.code) {
                            if let Some(item) = state.pending.pop_front() {
                                let label = decision_label(&decision);
                                let _ = item.decision_tx.send(decision);
                                renderer.line(&format!("✓ {label}"));
                                state.approval_dirty = true;
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Idle input / running streaming. A short timeout keeps the loop
        // responsive while a long turn streams events.
        let timeout = if state.running {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(200)
        };
        if crossterm::event::poll(timeout)? {
            match crossterm::event::read()? {
                Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                    if ctrl && key.code == KeyCode::Char('c') {
                        if state.running {
                            agent.cancel();
                            renderer.line("⏹ cancelled");
                            state.running = false;
                            state.prompt_drawn = false;
                        } else {
                            state.quit = true;
                        }
                    } else if state.running {
                        // Only Ctrl+C is handled while streaming; the composer is
                        // not editable until the turn finishes.
                    } else {
                        match key.code {
                            KeyCode::Enter => {
                                if shift {
                                    composer.insert_char('\n');
                                    renderer.render_input(&composer, &mut cursor_line);
                                } else if !composer.is_empty() {
                                    let text = composer.text();
                                    composer.clear();
                                    renderer.clear_input(cursor_line);
                                    renderer.status(&status_text(0, &Phase::Thinking));
                                    state.start_turn();
                                    let _ = cmd_tx.send(Cmd::Run(text));
                                }
                            }
                            KeyCode::Backspace => {
                                composer.backspace();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Delete => {
                                composer.delete_forward();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Left => {
                                composer.move_left();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Right => {
                                composer.move_right();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Up => {
                                composer.move_up();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Down => {
                                composer.move_down();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Home => {
                                composer.move_home();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::End => {
                                composer.move_end();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Esc => {
                                composer.clear();
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            KeyCode::Char(c) => {
                                composer.insert_char(c);
                                renderer.render_input(&composer, &mut cursor_line);
                            }
                            _ => {}
                        }
                    }
                }
                Event::Paste(text) => {
                    if !state.running {
                        composer.insert_str(&text);
                        renderer.render_input(&composer, &mut cursor_line);
                    }
                }
                _ => {}
            }
        } else if state.running {
            state.tick(&mut renderer);
        }

        // Redraw the prompt once a turn has finished.
        if !state.running && !state.prompt_drawn {
            renderer.render_input(&composer, &mut cursor_line);
            state.prompt_drawn = true;
        }

        if state.quit {
            break;
        }
    }

    let _ = cmd_tx.send(Cmd::Quit);
    let _ = agent_task.await;
    drop(renderer);
    let _ = capture.flush();
    Ok(())
}

/// The agent-side task: run turns on command, forward events to the UI, and
/// persist per-turn JSONL logs exactly like the TUI and REPL paths do.
async fn agent_loop(
    agent: Arc<PhiAgent>,
    session: SessionId,
    session_ctx: SessionContext,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut turn_number = session_ctx.last_turn_number();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::Quit => break,
            Cmd::Run(input) => {
                turn_number += 1;
                let mut turn_events: Vec<RuntimeEvent> = Vec::new();

                let result = agent
                    .run_turn(session.clone(), &input, |ev| {
                        let _ = event_tx.send(TuiEvent::Runtime(ev.clone()));
                        turn_events.push(ev);
                        Ok(())
                    })
                    .await;

                if let Err(e) = save_turn_log(&session_ctx, turn_number, &turn_events, &input) {
                    tracing::warn!(error = %e, "failed to save turn log");
                }

                match result {
                    Ok(_) => {
                        let _ = event_tx.send(TuiEvent::TurnDone);
                    }
                    Err(e) => {
                        let _ = event_tx.send(TuiEvent::TurnError(format!("{e}")));
                    }
                }
            }
        }
    }
}

/// Minimal UI state: `running` + phase + the approval queue. Kept deliberately
/// separate from the ratatui [`crate::ui::app::App`] (which buffers and wraps
/// text — the exact things we're avoiding here).
struct Inline {
    running: bool,
    phase: Phase,
    pending: VecDeque<ApprovalItem>,
    approval_dirty: bool,
    prompt_drawn: bool,
    spinner_idx: usize,
    quit: bool,
}

impl Inline {
    fn new() -> Self {
        Self {
            running: false,
            phase: Phase::Thinking,
            pending: VecDeque::new(),
            approval_dirty: false,
            prompt_drawn: false,
            spinner_idx: 0,
            quit: false,
        }
    }

    fn start_turn(&mut self) {
        self.running = true;
        self.phase = Phase::Thinking;
        self.prompt_drawn = false;
    }

    fn on_event<W: Write>(&mut self, ev: TuiEvent, r: &mut Renderer<W>) {
        match ev {
            TuiEvent::Runtime(ev) => self.on_runtime(ev, r),
            TuiEvent::TurnError(msg) => {
                r.line(&format!("❌ {msg}"));
                self.running = false;
                self.prompt_drawn = false;
            }
            TuiEvent::TurnDone => {
                self.running = false;
                self.prompt_drawn = false;
            }
        }
    }

    fn on_runtime<W: Write>(&mut self, ev: RuntimeEvent, r: &mut Renderer<W>) {
        match ev {
            RuntimeEvent::TextDelta { text, .. } => {
                self.phase = Phase::Streaming;
                r.stream(&text, false);
            }
            // Reasoning is hidden behind the spinner (still in the turn JSONL).
            RuntimeEvent::ThoughtDelta { .. } => {
                self.phase = Phase::Thinking;
                r.status(&status_text(self.spinner_idx, &Phase::Thinking));
            }
            RuntimeEvent::ToolCallStarted { tool_name, .. } => {
                self.phase = Phase::ToolCall { tool: tool_name.clone() };
                r.status(&status_text(self.spinner_idx, &self.phase));
            }
            RuntimeEvent::ToolCallFinished {
                tool_name,
                denied,
                summary,
                ..
            } => {
                self.phase = Phase::Thinking;
                if denied {
                    r.line(&format!("⛔ {tool_name} denied"));
                } else {
                    r.line(&format!("✓ {tool_name} {}", one_line(&summary, 120)));
                }
            }
            RuntimeEvent::PlanUpdated { .. } => {
                // Plans are large and the streaming summary covers them; skip.
            }
            RuntimeEvent::AwaitingApproval { .. } => {
                self.phase = Phase::AwaitingApproval;
                // The interactive prompt is rendered from the approval queue.
            }
            RuntimeEvent::RunFinished { agent_id, .. } => {
                // Sub-agents (decompose/merge) emit their own `RunFinished` with
                // a non-empty `agent_id`; only the root run ends the turn.
                if agent_id.is_none() {
                    r.line("✅ done");
                    self.running = false;
                    self.prompt_drawn = false;
                }
            }
            RuntimeEvent::RunCancelled { agent_id, .. } => {
                if agent_id.is_none() {
                    r.line("⏹ cancelled");
                    self.running = false;
                    self.prompt_drawn = false;
                }
            }
            _ => {}
        }
    }

    fn tick<W: Write>(&mut self, r: &mut Renderer<W>) {
        // No spinner while text is streaming — the text itself is the activity.
        if matches!(self.phase, Phase::Streaming) {
            return;
        }
        self.spinner_idx = (self.spinner_idx + 1) % SPINNERS.len();
        r.status(&status_text(self.spinner_idx, &self.phase));
    }
}

/// Which state the current output line is in, so in-place status rewrites never
/// clobber streamed text (and vice-versa).
enum LineState {
    /// Cursor at column 0 of an empty line.
    Fresh,
    /// Streamed text is on the current line (no trailing newline yet).
    Content,
    /// A status line (spinner) is on the current line, awaiting in-place overwrite.
    LiveStatus,
}

/// Writes the inline UI to a [`Write`] sink, tracking the current line so status
/// lines overwrite in place but streamed text scrolls into history.
struct Renderer<W: Write> {
    out: W,
    line: LineState,
}

impl<W: Write> Renderer<W> {
    fn new(out: W) -> Self {
        Self {
            out,
            line: LineState::Fresh,
        }
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }

    /// Stream a chunk of text. Commits any live status line first so the text
    /// starts on its own line, then writes the chunk immediately (no buffering).
    fn stream(&mut self, text: &str, dim: bool) {
        // Erase a live status (spinner) in place — it is transient, and committing
        // it would leave a dead "🔧 …" line in scrollback ahead of the answer.
        if matches!(self.line, LineState::LiveStatus) {
            let _ = write!(self.out, "\r\x1b[K");
        }
        // Raw mode clears OPOST, so a bare \n line-feeds without returning the
        // carriage. Translate any newlines in streamed model text to \r\n so a
        // multi-line answer doesn't drift rightward.
        let text = text.replace('\n', "\r\n");
        if dim {
            let _ = write!(self.out, "\x1b[2m{text}\x1b[0m");
        } else {
            let _ = write!(self.out, "{text}");
        }
        self.line = if text.ends_with('\n') {
            LineState::Fresh
        } else {
            LineState::Content
        };
        self.flush();
    }

    /// Overwrite the current line with a status string. If the line is already a
    /// live status, rewrite in place (`\r\x1b[K`); otherwise start a new line.
    fn status(&mut self, text: &str) {
        match self.line {
            LineState::LiveStatus => {}
            LineState::Content => {
                let _ = write!(self.out, "\r\n");
            }
            LineState::Fresh => {}
        }
        let _ = write!(self.out, "\r\x1b[K{text}");
        self.line = LineState::LiveStatus;
        self.flush();
    }

    /// Print a full line, finalizing any in-progress content or status first.
    fn line(&mut self, text: &str) {
        match self.line {
            // A live status (spinner) is transient — erase it in place rather than
            // committing a dead "🔧 …" line to scrollback.
            LineState::LiveStatus => {
                let _ = write!(self.out, "\r\x1b[K");
            }
            // Streamed text ends the current line; the full line starts fresh.
            LineState::Content => {
                let _ = write!(self.out, "\r\n");
            }
            LineState::Fresh => {}
        }
        let _ = write!(self.out, "{text}\r\n");
        self.line = LineState::Fresh;
        self.flush();
    }

    /// Redraw the composer at the bottom of the screen (idle input). The cursor
    /// is left at the composer's logical `(line, col)`, and `cursor_line` is
    /// updated so the next redraw can walk back to the top of the region.
    fn render_input(&mut self, composer: &Composer, cursor_line: &mut usize) {
        if *cursor_line > 0 {
            let _ = write!(self.out, "\x1b[{}A", *cursor_line);
        }
        let _ = write!(self.out, "\r\x1b[J");

        let lines = composer.lines();
        let (cl, cb) = composer.cursor();

        // Draw the input as a rounded box (Claude Code style): the top border
        // carries the `>` prompt and the first line, middle lines indent to the
        // same text column, and the bottom border carries the key hints.
        let _ = write!(self.out, "╭─ > {}", lines[0]);
        for line in &lines[1..] {
            let _ = write!(self.out, "\r\n│    {line}");
        }
        let _ = write!(self.out, "\r\n╰─ ⏎ send · ⇧⏎ newline · ^C quit");

        // Leave the cursor on composer line `cl`, at the text column (5 chars of
        // left gutter) plus the char-count offset into that line. CJK wide chars
        // are under-counted — known follow-up, same as the TUI wrap width.
        let up = lines.len().saturating_sub(cl);
        if up > 0 {
            let _ = write!(self.out, "\x1b[{up}A");
        }
        let _ = write!(self.out, "\r");
        let col = 5 + char_count(&lines[cl][..cb]);
        if col > 0 {
            let _ = write!(self.out, "\x1b[{col}C");
        }

        *cursor_line = cl;
        self.line = LineState::Content;
        self.flush();
    }

    /// Remove the composer region, leaving the cursor at a fresh line.
    fn clear_input(&mut self, cursor_line: usize) {
        if cursor_line > 0 {
            let _ = write!(self.out, "\x1b[{}A", cursor_line);
        }
        let _ = write!(self.out, "\r\x1b[J");
        self.line = LineState::Fresh;
        self.flush();
    }

    #[cfg(test)]
    fn into_inner(self) -> W {
        self.out
    }
}

/// The status line text for a phase (the spinner char is prepended separately).
fn status_label(phase: &Phase) -> String {
    match phase {
        Phase::Thinking => "thinking…".to_string(),
        Phase::Streaming => "streaming…".to_string(),
        Phase::ToolCall { tool } => format!("🔧 {tool}"),
        Phase::AwaitingApproval => "waiting approval…".to_string(),
    }
}

fn status_text(spinner_idx: usize, phase: &Phase) -> String {
    format!("{} {}", SPINNERS[spinner_idx % SPINNERS.len()], status_label(phase))
}

fn risk_badge(level: &RiskLevel) -> &'static str {
    match level {
        RiskLevel::Safe => "🟢 Safe",
        RiskLevel::Sensitive => "🟡 Sensitive",
        RiskLevel::Destructive => "🔴 Destructive",
    }
}

fn approval_key(code: KeyCode) -> Option<ApprovalDecision> {
    match code {
        KeyCode::Char('y') => Some(ApprovalDecision::AllowOnce),
        KeyCode::Char('a') => Some(ApprovalDecision::AllowAlways),
        KeyCode::Char('n') => Some(ApprovalDecision::Deny),
        _ => None,
    }
}

fn decision_label(decision: &ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "allow once",
        ApprovalDecision::AllowAlways => "allow always",
        ApprovalDecision::Deny => "deny",
    }
}

/// Collapse whitespace/newlines and truncate to `max` chars with an ellipsis.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Column width of a string as a char count (not byte count; wide CJK chars are
/// a known follow-up and still under-counted).
fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn render_approval<W: Write>(r: &mut Renderer<W>, item: &ApprovalItem) {
    let req = &item.request;
    r.line(&format!("⚠️  {}   {}", req.title, risk_badge(&req.risk_level)));
    if !req.message.is_empty() {
        r.line(&format!("   {}", req.message));
    }
    r.line("   [y] allow once  [a] allow always  [n] deny");
}

/// Restores the terminal (raw mode + bracketed paste) when dropped.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_string<F: FnOnce(&mut Renderer<Vec<u8>>)>(f: F) -> String {
        let mut r = Renderer::new(Vec::new());
        f(&mut r);
        String::from_utf8(r.into_inner()).unwrap()
    }

    #[test]
    fn stream_erases_live_status() {
        let out = render_to_string(|r| {
            r.status("thinking");
            r.stream("hello", false);
        });
        // The spinner is erased in place, not committed as a dead line; the
        // answer starts on the same line.
        assert!(out.contains("\r\x1b[Khello"), "got: {out:?}");
        assert!(!out.contains("thinking\n"), "got: {out:?}");
    }

    #[test]
    fn stream_is_written_immediately() {
        let out = render_to_string(|r| {
            r.stream("one", false);
            r.stream("two", false);
        });
        assert!(out.contains("onetwo"), "got: {out:?}");
    }

    #[test]
    fn status_rewrites_in_place() {
        let out = render_to_string(|r| {
            r.status("a");
            r.status("b");
        });
        // No newline between the two statuses — the second clears the first.
        assert!(!out.contains('\n'), "got: {out:?}");
        assert!(out.ends_with('b'), "got: {out:?}");
    }

    #[test]
    fn status_after_stream_starts_new_line() {
        let out = render_to_string(|r| {
            r.stream("text", false);
            r.status("s");
        });
        assert!(out.contains("text\r\n"), "got: {out:?}");
        assert!(out.ends_with('s'), "got: {out:?}");
    }

    #[test]
    fn line_erases_live_status() {
        let out = render_to_string(|r| {
            r.status("thinking");
            r.line("done");
        });
        assert!(out.contains("\r\x1b[Kdone"), "got: {out:?}");
        assert!(!out.contains("thinking\n"), "got: {out:?}");
    }

    #[test]
    fn no_bare_newlines_so_lines_do_not_drift() {
        // In raw mode crossterm clears OPOST, so a lone \n line-feeds without a
        // carriage return and every subsequent line drifts right. Every newline
        // the renderer emits must therefore be \r\n (never a bare \n).
        let out = render_to_string(|r| {
            r.line("a");
            r.status("s");
            r.stream("text\nmulti", false);
            r.line("b");
            let mut c = Composer::new();
            c.insert_char('\n'); // two composer lines
            let mut cl = 0;
            r.render_input(&c, &mut cl);
            r.clear_input(cl);
        });
        let bytes = out.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                assert!(i > 0 && bytes[i - 1] == b'\r', "bare \\n at byte {i}: {out:?}");
            }
        }
    }

    #[test]
    fn status_labels() {
        assert_eq!(status_label(&Phase::Thinking), "thinking…");
        assert_eq!(status_label(&Phase::Streaming), "streaming…");
        assert_eq!(
            status_label(&Phase::ToolCall { tool: "read_file".into() }),
            "🔧 read_file"
        );
        assert_eq!(status_label(&Phase::AwaitingApproval), "waiting approval…");
    }

    #[test]
    fn approval_keys_map_to_decisions() {
        assert_eq!(approval_key(KeyCode::Char('y')), Some(ApprovalDecision::AllowOnce));
        assert_eq!(approval_key(KeyCode::Char('a')), Some(ApprovalDecision::AllowAlways));
        assert_eq!(approval_key(KeyCode::Char('n')), Some(ApprovalDecision::Deny));
        assert_eq!(approval_key(KeyCode::Char('x')), None);
        assert_eq!(approval_key(KeyCode::Enter), None);
    }

    #[test]
    fn risk_badges() {
        assert_eq!(risk_badge(&RiskLevel::Safe), "🟢 Safe");
        assert_eq!(risk_badge(&RiskLevel::Sensitive), "🟡 Sensitive");
        assert_eq!(risk_badge(&RiskLevel::Destructive), "🔴 Destructive");
    }

    #[test]
    fn one_line_collapses_whitespace_and_truncates() {
        assert_eq!(one_line("a\n  b", 10), "a b");
        let long = one_line(&"x".repeat(50), 5);
        assert_eq!(long.chars().count(), 6); // 5 + ellipsis
        assert!(long.ends_with('…'));
    }

    #[test]
    fn render_input_draws_rounded_box() {
        let out = render_to_string(|r| {
            let mut c = Composer::new();
            c.insert_str("hi");
            let mut cl = 0;
            r.render_input(&c, &mut cl);
        });
        // Top border carries the prompt + first line; bottom border the hints.
        assert!(out.contains("╭─ > hi"), "got: {out:?}");
        assert!(out.contains("╰─ "), "got: {out:?}");
        assert!(out.contains("⏎ send"), "got: {out:?}");
    }

    #[test]
    fn render_input_multiline_indents_to_text_column() {
        let out = render_to_string(|r| {
            let mut c = Composer::new();
            c.insert_str("first\nsecond");
            let mut cl = 0;
            r.render_input(&c, &mut cl);
        });
        assert!(out.contains("╭─ > first"), "got: {out:?}");
        assert!(out.contains("│    second"), "got: {out:?}");
    }

    /// Debug aid: drive the renderer through a realistic turn and dump the raw
    /// ANSI to `/tmp/inline_demo.raw`, replayable via `cargo run --bin replay`.
    #[test]
    fn demo_simulated_turn_writes_capture() {
        let mut r = Renderer::new(Vec::new());
        r.line("phiforge — coding agent on phi-agent.");
        r.line("Workspace: /tmp/ws");

        let mut composer = Composer::new();
        let mut cl = 0;
        r.render_input(&composer, &mut cl);
        composer.insert_str("总结这个工程");
        r.render_input(&composer, &mut cl);
        r.clear_input(cl);
        r.status(&status_text(0, &Phase::Thinking));

        // reasoning → spinner ticks (hidden behind the status line)
        r.status(&status_text(1, &Phase::Thinking));
        r.status(&status_text(2, &Phase::Thinking));

        // two tool calls
        r.status(&status_text(3, &Phase::ToolCall { tool: "read_file".into() }));
        r.line("✓ read_file File: src/main.rs (lines 1-90 of 90)");
        r.status(&status_text(4, &Phase::ToolCall { tool: "list_files".into() }));
        r.line("✓ list_files dirs:1 files:5");

        // final reasoning, then the answer streams
        r.status(&status_text(5, &Phase::Thinking));
        r.stream("phiforge 是一个基于 phi-agent 的 AI 编码 agent。", false);
        r.stream("它通过写代码来压测框架。", false);

        // done + prompt redraw
        r.line("✅ done");
        composer.clear();
        r.render_input(&composer, &mut cl);

        let raw = r.into_inner();
        // Regression: raw mode clears OPOST, so a bare \n drifts right — assert
        // every newline is \r\n.
        for (i, &b) in raw.iter().enumerate() {
            if b == b'\n' {
                assert!(i > 0 && raw[i - 1] == b'\r', "bare \\n at byte {i}");
            }
        }
        // Regression: the "🔧 read_file" spinner is erased in place, not left as a
        // frozen line above its result.
        let s = String::from_utf8(raw.clone()).unwrap();
        assert!(!s.contains("🔧 read_file\n"), "frozen spinner left in output");

        std::fs::write("/tmp/inline_demo.raw", raw).unwrap();
    }
}
