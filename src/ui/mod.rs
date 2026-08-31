//! ratatui TUI (Phase 5): an independent `RuntimeEvent` consumer.
//!
//! Architecture (§9.3): the agent loop is untouched. A background task runs
//! turns and forwards `RuntimeEvent`s through an mpsc channel; the main loop
//! drains those events into [`App`] state, polls keyboard input, and redraws.
//! Input and events meet only through the channel — no shared mutable state.

pub mod app;
pub mod completer;
pub mod frame_log;
pub mod handlers;
pub mod input;
pub mod markdown;
pub mod mention;
pub mod picker;
pub mod render;
pub mod selection;
pub mod stream;
pub mod transcript;
pub mod viewport;
pub mod wrap;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyEventKind,
        KeyboardEnhancementFlags, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use phi_agent::{PhiAgent, RunOutcome, RuntimeEvent, SessionContext, SessionId, save_turn_log};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use crate::approval::ApprovalItem;
use crate::banner::ColorScheme;
use crate::skills::SkillResolver;
use app::{Action, App, TuiEvent};
use frame_log::{ComposerLog, FrameCapture, PerfLog, PerfRow};

/// A command from the TUI loop to the agent task.
enum Cmd {
    Run(String),
    Quit,
}

/// Run the interactive ratatui TUI until the user quits.
///
/// Takes ownership of the agent/session because the turn runner lives in a
/// spawned task for the whole lifetime of the TUI.
pub async fn run_tui(
    agent: PhiAgent,
    skill_resolver: SkillResolver,
    session: SessionId,
    session_ctx: SessionContext,
    workspace: PathBuf,
    approval_rx: Option<mpsc::UnboundedReceiver<ApprovalItem>>,
    scheme: ColorScheme,
    show_banner: bool,
    version: &str,
) -> Result<()> {
    let agent = Arc::new(agent);
    let mut approval_rx = approval_rx;
    let log_path = session_ctx.log_path().display().to_string();
    let frames_path = session_ctx.session_dir.join("frames.txt");
    let perf_path = session_ctx.session_dir.join("perf.log");
    let composer_log_path = session_ctx.session_dir.join("composer.log");

    // Two channels: events flow agent → TUI, commands flow TUI → agent. Both
    // unbounded — the TUI drains eagerly and the agent must never be throttled
    // (a full bounded queue would drop events or stall `run_turn`).
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TuiEvent>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

    // 在 skill_resolver 移入 agent_loop 前提取 (name, description) 摘要列表
    let skill_summaries = skill_resolver.skill_summaries();

    let agent_task = tokio::spawn(agent_loop(
        agent.clone(),
        skill_resolver,
        session,
        session_ctx,
        event_tx,
        cmd_rx,
    ));

    // Terminal setup. The guard restores the terminal on any exit path.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)?;
    // Kitty keyboard protocol: lets crossterm read the Command (Super) modifier
    // so Cmd+C can be bound to copy. Terminals that don't support it (e.g. the
    // macOS Terminal.app) ignore this and keep legacy key reporting.
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    app.set_scheme(scheme);
    app.set_workspace_root(workspace.clone());
    app.set_skill_summaries(skill_summaries);
    if show_banner {
        app.push_banner(crate::banner::build(
            &workspace,
            std::path::Path::new(&log_path),
            version,
        ));
    } else {
        app.push_system("Phimint · Forged with intent. Shipped with care. · Built on phi-agent");
    }

    // Clipboard for Ctrl+Y "copy last reply". Optional: on headless or
    // Linux/Wayland setups `arboard` may fail to open a clipboard; copy then
    // just reports "clipboard unavailable" instead of crashing.
    let mut clipboard = arboard::Clipboard::new().ok();

    // Session logs (frame capture, perf timing, composer state) each own their
    // file handle + dedup/throttle state; see frame_log.rs.
    let mut frames = FrameCapture::new(&frames_path)?;
    let mut perf_log = PerfLog::new(&perf_path)?;
    let mut composer_log = ComposerLog::new(&composer_log_path)?;
    // `dirty` gates the offscreen snapshot: only re-render when state changed,
    // and then at most once per the capture interval.
    let mut dirty = true; // capture the initial screen

    let mut quit = false;
    while !quit {
        let loop_start = Instant::now();

        // Drain any events queued since the last frame into state.
        while let Ok(ev) = event_rx.try_recv() {
            app.handle_event(ev);
            dirty = true;
        }

        // Drain queued approval requests into the popup queue (one popup at a
        // time — the front of the queue is what gets rendered).
        if let Some(rx) = approval_rx.as_mut() {
            while let Ok(item) = rx.try_recv() {
                app.approval_queue.push_back(item);
                dirty = true;
            }
        }

        // Poll input with a short timeout so the frame rate stays bounded even
        // while a long turn streams events. 2 ms keeps scroll responsive (~500 Hz
        // poll rate, gated by draw cost); the old 16 ms added perceptible jank
        // on mouse-wheel scrolling because each event blocked for a full frame.
        //
        // Drain ALL pending crossterm events in one go — `read()` returns one
        // event at a time, so a fast scroll-wheel burst leaves the rest queued.
        // Without this loop, each leftover event waits an extra 2 ms poll cycle,
        // creating visible stutter on track-pad / inertial scrolling.
        let mut crossterm_count: u32 = 0;
        let mut has_scroll = false;
        let mut has_key = false;
        let mut has_mouse = false;
        while crossterm::event::poll(Duration::ZERO)? {
            let event = crossterm::event::read()?;
            crossterm_count += 1;
            match event {
                Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                    dirty = true;
                    has_key = true;
                    if let Some(action) = app.handle_key(key.code, key.modifiers) {
                        match action {
                            Action::Submit(text) => {
                                let _ = cmd_tx.send(Cmd::Run(text));
                            }
                            Action::Approve(decision) => app.approve_front(decision),
                            Action::CopyLastReply => {
                                let text = app.last_reply_text();
                                copy_text(&mut app, &mut clipboard, text);
                            }
                            Action::CopySelection => {
                                let text = app.selection_text();
                                copy_text(&mut app, &mut clipboard, text);
                                app.clear_selection();
                                tracing::info!("selection cleared after copy");
                            }
                            Action::Cancel => {
                                agent.cancel();
                                // Dismiss the popup(s) and drop any in-flight
                                // request not yet drained (the cancelled handler
                                // stops sending once its token fires).
                                app.approval_queue.clear();
                                if let Some(rx) = approval_rx.as_mut() {
                                    while rx.try_recv().is_ok() {}
                                }
                            }
                            Action::Quit => {
                                agent.cancel();
                                let _ = cmd_tx.send(Cmd::Quit);
                                quit = true;
                            }
                        }
                    }
                }
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    ..
                }) => { has_scroll = true; if app.scroll_up() { dirty = true; } }
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    ..
                }) => { has_scroll = true; if app.scroll_down() { dirty = true; } }
                Event::Mouse(MouseEvent {
                    kind,
                    column,
                    row,
                    ..
                }) => {
                    dirty = true;
                    has_mouse = true;
                    // The copy-menu hit-test needs the terminal size to locate
                    // the popup exactly where it was drawn.
                    let size = terminal.size()?;
                    if let Some(action) = app.handle_mouse(kind, column, row, size.width, size.height)
                    {
                        if let Action::CopySelection = action {
                            let text = app.selection_text();
                            copy_text(&mut app, &mut clipboard, text);
                            app.clear_selection();
                        }
                    }
                }
                Event::Paste(text) => { dirty = true; app.paste(&text); }
                Event::Resize(_, _) => { dirty = true; }
                _ => {}
            }
        }
        // Build a compact event-type tag for the log (e.g. "S" / "K" / "SM" / "SK").
        let mut event_types = String::new();
        if has_scroll { event_types.push('S'); }
        if has_key { event_types.push('K'); }
        if has_mouse { event_types.push('M'); }

        // If no events arrived, sleep briefly to avoid busy-spinning.
        let slept = !dirty;
        if slept {
            std::thread::sleep(Duration::from_millis(2));
        }

        // Only redraw when state actually changed (dirty flag). With a 2 ms
        // poll timeout the idle loop would otherwise burn ~500 useless draws/s.
        let mut draw_elapsed = Duration::ZERO;
        if dirty {
            let draw_start = Instant::now();
            terminal.draw(|f| render::draw(f, &mut app))?;
            draw_elapsed = draw_start.elapsed();

            // Log composer state after each draw (only when state changes).
            let size = terminal.size()?;
            composer_log.record(&app, frames.frame_id(), (size.width, size.height));
        }

        // Record the frame if it changed since the last one (dedup keeps the
        // flipbook small — steady states and per-token deltas collapse away).
        // Gated on `dirty` + the capture interval so the offscreen render only
        // runs after a real change, at most ~10×/s.
        let mut capture_elapsed = Duration::ZERO;
        if dirty {
            let size = terminal.size()?;
            capture_elapsed = frames.capture(true, &mut app, size.width, size.height);
        }
        let loop_elapsed = loop_start.elapsed();
        // Log every frame to perf.log (CSV) for post-hoc analysis.
        // Note: dirty is logged BEFORE reset so the column reflects whether
        // a draw/capture actually happened this iteration.
        perf_log.record(&PerfRow {
            frame_id: frames.frame_id(),
            draw_ms: draw_elapsed.as_millis(),
            capture_ms: capture_elapsed.as_millis(),
            loop_ms: loop_elapsed.as_millis(),
            dirty,
            scroll_offset: app.viewport.scroll_offset,
            follow_bottom: app.viewport.follow_bottom,
            output_lines: app.transcript.len(),
            crossterm_events: crossterm_count,
            slept,
            event_types: &event_types,
        });
        // Reset dirty after draw + optional capture so idle loops skip both.
        dirty = false;
    }

    // Ensure the agent task is told to stop and has a chance to finish.
    let _ = cmd_tx.send(Cmd::Quit);
    let _ = agent_task.await;

    let _ = frames.flush();
    let _ = perf_log.flush();
    let _ = composer_log.flush();
    Ok(())
}

/// Copy `text` to the clipboard (if available), surfacing a status-bar notice.
fn copy_text(app: &mut App, clipboard: &mut Option<arboard::Clipboard>, text: String) {
    if text.is_empty() {
        app.set_notice("nothing to copy");
    } else {
        match clipboard.as_mut() {
            Some(cb) => match cb.set_text(text.clone()) {
                Ok(_) => app.set_notice(format!("📋 copied {} chars", text.chars().count())),
                Err(e) => app.set_notice(format!("copy failed: {e}")),
            },
            None => app.set_notice("clipboard unavailable"),
        }
    }
}

/// The agent-side task: run turns on command, forward events to the TUI, and
/// persist per-turn JSONL logs exactly like the REPL path does.
async fn agent_loop(
    agent: Arc<PhiAgent>,
    skill_resolver: SkillResolver,
    session: SessionId,
    session_ctx: SessionContext,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) {
    // Resume turn numbering from any turns already logged for this session.
    let mut turn_number = session_ctx.last_turn_number();

    // Initialize phi-telemetry for per-turn token usage tracking.
    let mut telemetry = phi_telemetry::init_telemetry(
        agent.runtime(),
        session_ctx.session_id.clone(),
        "phimint".to_string(),
        agent.config.model.clone(),
    );

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Cmd::Quit => {
                // Finalize telemetry and persist session metrics.
                telemetry.shutdown().await;
                let metrics = telemetry.session.read().await;
                let mut metrics = metrics.clone();
                metrics.finalize(phi_telemetry::SessionOutcome::Completed);
                let _ = phi_telemetry::save_metrics(&metrics, &session_ctx.session_dir);
                break;
            }
            Cmd::Run(input) => {
                turn_number += 1;
                let turn_events: std::sync::Arc<std::sync::Mutex<Vec<RuntimeEvent>>> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

                // 7b: `/skill-name args` → 解析为 skill body 再提交给 agent
                let resolved_input = skill_resolver.resolve(&input).unwrap_or(input);
                let turn_input = resolved_input;

                let turn_events_clone = turn_events.clone();
                let event_tx_clone = event_tx.clone();
                let result = agent
                    .run_turn(session.clone(), &turn_input, move |ev| {
                        let _ = event_tx_clone.send(TuiEvent::Runtime(ev.clone()));
                        turn_events_clone.lock().unwrap().push(ev);
                        Ok(())
                    })
                    .await;

                // Persist regardless of success (matches the REPL behavior).
                let turn_events_vec = turn_events.lock().unwrap().clone();
                if let Err(e) = save_turn_log(&session_ctx, turn_number, &turn_events_vec, &turn_input) {
                    tracing::warn!(error = %e, "failed to save turn log");
                }

                // Save token usage metrics incrementally.
                {
                    let metrics = telemetry.session.read().await;
                    let _ = phi_telemetry::save_metrics(&metrics, &session_ctx.session_dir);
                }

                match result {
                    Ok(RunOutcome::Failed { error }) => {
                        let _ = event_tx.send(TuiEvent::TurnError(error));
                    }
                    Ok(RunOutcome::MaxTurnsExceeded { turns }) => {
                        let _ = event_tx.send(TuiEvent::TurnError(format!(
                            "max turns ({turns}) exceeded"
                        )));
                    }
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

/// Restores the terminal (raw mode + alternate screen) when dropped.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen
        );
    }
}
