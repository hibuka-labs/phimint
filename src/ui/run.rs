//! The TUI main loop: terminal setup, the event/command channels, and the
//! agent-side turn runner.
//!
//! Architecture (§9.3): the agent loop is untouched. A background task runs
//! turns and forwards `RuntimeEvent`s through an mpsc channel; the main loop
//! drains those events into [`App`] state, polls keyboard input, and redraws.
//! Input and events meet only through the channel — no shared mutable state.

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
use phi_agent::{
    ChatMessage, ChildResultEvent, PhiAgent, RunOutcome, RuntimeEvent, SessionContext, SessionId,
    persist_window_messages, read_session_title, save_turn_log,
};
use phi_kernel_tools::background_shell::BackgroundTaskRegistry;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use super::app::{Action, App, TuiEvent};
use super::child_results::{ChildResultRoute, ChildResultRouter};
use super::frame_log::{ComposerLog, FrameCapture, PerfLog, PerfRow};
use super::render;
use crate::approval::ApprovalItem;
use crate::banner::ColorScheme;
use crate::model_store::ModelStore;
use crate::router::PhimintRouter;
use crate::skills::{
    ActiveSkillEntry, SkillResolver, SkillScope, SkillTelemetry, append_active_skills,
    load_active_skills, save_active_skills,
};
use crate::title_gen::{generate_session_title, write_pending_title_marker};

/// A command from the TUI loop to the agent task.
enum Cmd {
    Run(String),
    Quit,
}

/// Outcome of one TUI lifecycle (one session's worth of UI).
enum TuiOutcome {
    /// User pressed Ctrl+C / Ctrl+D twice.
    Quit,
    /// User selected a different session via `/resume`.
    SwitchSession(PathBuf),
}

/// Liveness tick: while a turn or a child wait is in flight, re-render at
/// least this often even with zero events, so the spinner/elapsed readout in
/// the status bar advance through silent stretches (a long LLM call emits no
/// events — the old purely event-driven loop froze the whole screen for its
/// full duration, session 20260904_e6612477, "frozen for a while"). Tick-only redraws are
/// NOT captured to frames.txt — an animated status bar would otherwise defeat
/// the flipbook's content dedup (~4 frames/s of nothing but spinner motion).
const UI_TICK: Duration = Duration::from_millis(250);

/// Run the interactive ratatui TUI until the user quits.
///
/// Takes ownership of the agent/session because the turn runner lives in a
/// spawned task for the whole lifetime of the TUI.
pub async fn run_tui(
    agent: PhiAgent,
    skill_resolver: Arc<SkillResolver>,
    skill_telemetry: Arc<SkillTelemetry>,
    bg_registry: Arc<BackgroundTaskRegistry>,
    session: SessionId,
    session_ctx: SessionContext,
    base_dir: PathBuf,
    workspace: PathBuf,
    approval_rx: Option<mpsc::UnboundedReceiver<ApprovalItem>>,
    scheme: ColorScheme,
    popup_style: phi_tui::popup_list::PopupStyle,
    show_banner: bool,
    version: &str,
    model_store: Arc<tokio::sync::Mutex<ModelStore>>,
    router: Arc<PhimintRouter>,
    resume_messages: Option<Vec<ChatMessage>>,
    update_config: crate::config::UpdateConfig,
    no_update_check: bool,
    log_sinks: log_core::SinkHandle,
) -> Result<()> {
    let agent = Arc::new(agent);

    // Session state (updated on each /resume switch).
    let mut session = session;
    let mut session_ctx = session_ctx;
    let mut resume_messages = resume_messages;

    // Terminal setup. The guard restores the terminal on any exit path.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
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

    // Approval channel lives for the whole TUI lifetime (session-independent).
    let mut approval_rx = approval_rx;

    // Whether ANY wheel/trackpad scroll event arrived during this whole run.
    // Session switches keep it: zero scrolls across a full session in Apple
    // Terminal is the fingerprint of "Allow Mouse Reporting" being off in
    // that window, reported at exit (see wheel_dead_notice).
    let mut saw_scroll = false;

    // ── Outer loop: each iteration = one session's TUI lifecycle ──
    loop {
        // Force a clean redraw after picker or session switch.
        terminal.clear()?;

        let session_dir = session_ctx.session_dir.clone();
        let log_path = session_ctx.log_path().display().to_string();
        let frames_path = session_ctx.session_dir.join("frames.txt");
        let perf_path = session_ctx.session_dir.join("perf.log");
        let composer_log_path = session_ctx.session_dir.join("composer.log");

        // Two channels: events flow agent → TUI, commands flow TUI → agent. Both
        // unbounded — the TUI drains eagerly and the agent must never be throttled
        // (a full bounded queue would drop events or stall `run_turn`).
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TuiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

        // Extract the (name, description) summary list before skill_resolver moves into agent_loop
        let mut skill_summaries = skill_resolver.skill_summaries();
        // Built-in command: /resume (not a skill, but triggered through the same / popup)
        skill_summaries.push(("resume".into(), "Switch to another session".into()));
        // Built-in command: /upgrade (a local UI command, never reaches the LLM)
        skill_summaries.push(("upgrade".into(), "Check for phimint updates".into()));

        // Spawn background update check (if enabled).
        // Sends TuiEvent::UpdateAvailable if a newer version is found.
        // Single attempt, 3s timeout per endpoint, silent on failure.
        let current_version = version.to_string();
        if !no_update_check && update_config.auto_check {
            let update_tx = event_tx.clone();
            let endpoints = update_config.endpoints.clone();
            let ver = current_version.clone();
            tokio::spawn(async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .unwrap_or_default();
                let state = crate::update::state::load();
                match crate::update::checker::check(&client, &endpoints, &state, &ver).await {
                    Ok(Some(crate::update::checker::CheckResult::UpgradeAvailable {
                        version,
                        download_url,
                        ..
                    })) => {
                        let _ = update_tx.send(TuiEvent::UpdateAvailable {
                            version,
                            download_url,
                        });
                    }
                    Ok(Some(crate::update::checker::CheckResult::UpToDate)) | Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "update check failed (silent)");
                    }
                }
            });
        }

        // Start the child-result watcher (Phase 2): monitors the mailbox and
        // delivers each finished child's result as a ChildResultEvent. The loop
        // below decides per event: inject immediately (agent idle) or stash and
        // inject after the current turn ends (agent running).
        let (_watcher_handle, mut child_result_rx) =
            if let Some(ma_rt) = agent.multi_agent_runtime() {
                let (h, cr) = ma_rt.start_watcher();

                // Phase 5: forward registry lifecycle snapshots into the UI event
                // stream. The task panel reconciles against these facts (entries
                // appear at spawn, before the child's first tool call) instead of
                // inferring state purely from child runtime events.
                let mut life_rx = ma_rt.subscribe_lifecycle();
                let life_tx = event_tx.clone();
                tokio::spawn(async move {
                    while life_rx.changed().await.is_ok() {
                        let snap = life_rx.borrow_and_update().clone();
                        if life_tx.send(TuiEvent::Lifecycle(snap)).is_err() {
                            break; // UI loop gone
                        }
                    }
                });

                (Some(h), Some(cr))
            } else {
                (None, None)
            };

        let agent_task = tokio::spawn(agent_loop(
            agent.clone(),
            skill_resolver.clone(),
            skill_telemetry.clone(),
            session.clone(),
            session_ctx,
            event_tx.clone(),
            cmd_rx,
            model_store.clone(),
            router.clone(),
        ));

        // Persistent event bridge: subscribe to the runtime's event bus each
        // session. The spawned task dies when event_tx is dropped (session ends).
        {
            let mut bus_rx = agent.runtime().subscribe_runtime_events();
            let bus_tx = event_tx.clone();
            tokio::spawn(async move {
                loop {
                    match bus_rx.recv().await {
                        Ok(ev) => {
                            if bus_tx.send(TuiEvent::Runtime(ev)).is_err() {
                                break; // UI loop gone
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "UI event bus consumer lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let mut app = App::new();
        app.set_scheme(scheme);
        app.set_popup_style(popup_style);
        app.set_workspace_root(workspace.clone());
        app.set_skill_summaries(skill_summaries);
        app.set_background_registry(bg_registry.clone());
        if show_banner {
            // Divider width follows the startup window (see banner::build docs
            // for the accepted resize staleness).
            let term_width = terminal
                .size()
                .map(|s| s.width as usize)
                .unwrap_or(80)
                .max(1);
            app.push_banner(crate::banner::build(
                &workspace,
                std::path::Path::new(&log_path),
                version,
                term_width,
            ));
        } else {
            // No wordmark when the banner is off — the brand name has to
            // appear here or nowhere. ASCII `-`, not `—`: the CJK chrome
            // guard (`chrome_sources_stay_cjk_width_safe`) bans the em dash.
            app.push_system(&format!(
                "Phimint v{version} - Forged with intent. Shipped with care."
            ));
        }

        // Clipboard for Ctrl+Y "copy last reply". Optional: on headless or
        // Linux/Wayland setups `arboard` may fail to open a clipboard; copy then
        // just reports "clipboard unavailable" instead of crashing.
        let mut clipboard = arboard::Clipboard::new().ok();

        // v4 A-side: surface a clamped token budget (requested < viability
        // floor) right in the transcript — the user must see the true minimum.
        if let Some(notice) = crate::context_rotation::take_clamp_notice() {
            app.push_system(&format!("! {notice}"));
        }

        // Replay historical messages into the transcript so the user sees the
        // previous conversation (not just a blank screen) after --resume.
        if let Some(messages) = &resume_messages {
            replay_messages_to_transcript(&mut app, messages);
            if !messages.is_empty() {
                // Digest goes AFTER the replay block: the viewport follows the
                // bottom, so this lands on the visible last screen — right
                // where the user is looking, telling them the history above is
                // browsable with PgUp (half a page per press).
                let title = read_session_title(&session_dir);
                app.push_system(&resume_digest_line(messages, title));
            }
        }

        // Terminal.app forwards wheel/trackpad events to the app only while
        // View → Allow Mouse Reporting is checked for the window; when off, the
        // swipe is swallowed by the terminal and scrolling silently dies. Give
        // Apple Terminal users the menu path plus the keyboard fallback
        // (fn+Up/Down = PgUp/PgDn) up front.
        if let Some(notice) = terminal_scroll_notice(std::env::var("TERM_PROGRAM").ok().as_deref())
        {
            app.push_system(&notice);
        }

        // Track window rotations for TUI notification.
        let mut last_reset_count: usize = 0;
        // Announce the futility brake at most once.
        let mut brake_announced = false;
        // Track user messages for title generation at the 10-message threshold.
        let mut user_msg_count: usize = 0;

        // Session logs (frame capture, perf timing, composer state) each own their
        // file handle + dedup/throttle state; see frame_log.rs.
        let mut frames = FrameCapture::new(&frames_path)?;
        let mut perf_log = PerfLog::new(&perf_path)?;
        let mut composer_log = ComposerLog::new(&composer_log_path)?;
        // `dirty` gates the offscreen snapshot: only re-render when state changed,
        // and then at most once per the capture interval.
        let mut dirty = true; // capture the initial screen
        // Last real render time — drives the UI_TICK liveness redraw.
        let mut last_draw = Instant::now();

        // Child-result delivery (fan-in redesign). The watcher coordinates: a
        // Progress event is display-only, a Batch event wakes the parent. The
        // router decides per event — inject immediately when the agent is idle,
        // hold and flush as one batch right after the turn ends when it is
        // running — and the loop below only executes the side effects. Decision
        // logic lives in `child_results` (unit-tested); this loop must stay free
        // of timing policy.
        let mut child_results = ChildResultRouter::new();
        let mut outcome: Option<TuiOutcome> = None;
        while outcome.is_none() {
            let loop_start = Instant::now();

            // Drain any events queued since the last frame into state.
            while let Ok(ev) = event_rx.try_recv() {
                // Thought deltas only feed the thinking panel, which animates on
                // the 250 ms UI_TICK — redrawing per delta bought nothing but a
                // wall of churn; ≤4 fps reads calmer. Prose deltas still redraw
                // at event rate.
                let thought_only = matches!(
                    &ev,
                    super::app::TuiEvent::Runtime(RuntimeEvent::ThoughtDelta { .. })
                );
                app.handle_event(ev);
                if !thought_only {
                    dirty = true;
                }
            }

            // Check for window rotation after turn ends.
            {
                let current = crate::context_rotation::window_reset_count();
                if current > last_reset_count {
                    let n = current - last_reset_count;
                    app.push_system(&format!(
                        "~ Context window rotated (x{n}) - previous history archived"
                    ));
                    last_reset_count = current;
                    dirty = true;
                }
                if !brake_announced && crate::context_rotation::futility_braked() {
                    brake_announced = true;
                    app.push_system(
                        "! Futility brake: work budget too small - window rotation paused. \
                     Restart with a larger --token-budget.",
                    );
                    dirty = true;
                }
            }

            // Drain child-result events from the watcher task.
            if let Some(rx) = child_result_rx.as_mut() {
                while let Ok(cr) = rx.try_recv() {
                    // Task-panel bookkeeping: a finished child flips its panel
                    // entry to done and refreshes the Waiting count. Must happen
                    // before routing so the status strip is current this frame.
                    if let ChildResultEvent::Progress {
                        agent_path, status, ..
                    } = &cr
                    {
                        if status != "running" {
                            app.mark_sub_agent_finished(agent_path);
                        }
                    }
                    let route = child_results.on_event(app.running, cr);
                    tracing::info!(
                        running = app.running,
                        route = ?std::mem::discriminant(&route),
                        "tui: child_result event routed"
                    );
                    match route {
                        ChildResultRoute::Notice { notice } => app.push_system(&notice),
                        ChildResultRoute::Hold { notice } => app.set_notice(notice),
                        ChildResultRoute::Inject { notice, input } => {
                            // A batch means every child returned — settle the panel
                            // before the synthetic run starts.
                            app.mark_all_sub_agents_finished();
                            app.push_system(&notice);
                            if cmd_tx.send(Cmd::Run(input)).is_ok() {
                                app.running = true;
                            }
                        }
                    }
                    dirty = true;
                }
            }

            // The turn just ended and results arrived while it was running —
            // inject them now as one synthetic run.
            if !app.running {
                if let Some(ChildResultRoute::Inject { notice, input }) =
                    child_results.flush_when_idle()
                {
                    tracing::info!("tui: flush_when_idle -> injecting batch");
                    app.mark_all_sub_agents_finished();
                    app.push_system(&notice);
                    if cmd_tx.send(Cmd::Run(input)).is_ok() {
                        app.running = true;
                    }
                    dirty = true;
                }
            }

            // Background-task auto-wake (the bg sibling of the fan-in batch):
            // every background shell task has ended, nothing else is in flight,
            // and at least one done/timed-out/errored outcome is unreported —
            // start a synthetic run telling the agent to fetch each output via
            // `task_output` and report it. The delivery gates (hold while the
            // agent is mid-turn / children run / other tasks run, the 15s
            // aggregation quiet window, Cancelled suppressed, report-once)
            // live in `bg_wake::take_bg_wake`; this loop only executes the
            // side effects.
            if let Some((notice, input)) = app.take_bg_wake(Instant::now()) {
                tracing::info!("tui: bg_wake -> injecting background task report");
                app.push_system(&notice);
                if cmd_tx.send(Cmd::Run(input)).is_ok() {
                    app.running = true;
                }
                dirty = true;
            }

            // Cleanup completed sub-agents (auto-remove after 3 seconds)
            if app.cleanup_completed_agents() {
                dirty = true;
            }

            // Reconcile background shell tasks from the registry (tick polling).
            if app.reconcile_background_tasks() {
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
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        dirty = true;
                        has_key = true;
                        if let Some(action) = app.handle_key(key.code, key.modifiers) {
                            match action {
                                Action::Submit(text) => {
                                    // Intercept built-in commands before sending to agent.
                                    if text.trim() == "/resume" {
                                        let base = session_dir.parent().unwrap().parent().unwrap();
                                        let picked = super::picker::show_picker_from_tui(
                                            base,
                                            Some(
                                                &session_dir.file_name().unwrap().to_string_lossy(),
                                            ),
                                        )?;
                                        if let Some(picked_dir) = picked {
                                            outcome = Some(TuiOutcome::SwitchSession(picked_dir));
                                        }
                                        // Don't send to agent, don't count as user message.
                                        break;
                                    }
                                    // /upgrade: manual update check + display result.
                                    if text.trim() == "/upgrade" {
                                        let current_ver = current_version.clone();
                                        let endpoints = update_config.endpoints.clone();
                                        let update_tx = event_tx.clone();
                                        tokio::spawn(async move {
                                            let client = reqwest::Client::builder()
                                                .timeout(std::time::Duration::from_secs(3))
                                                .build()
                                                .unwrap_or_default();
                                            let state = crate::update::state::load();
                                            match crate::update::checker::check(&client, &endpoints, &state, &current_ver).await {
                                            Ok(Some(crate::update::checker::CheckResult::UpgradeAvailable {
                                                version, download_url, ..
                                            })) => {
                                                let _ = update_tx.send(TuiEvent::UpdateAvailable {
                                                    version: version.clone(),
                                                    download_url: download_url.clone(),
                                                });
                                            }
                                            Ok(Some(crate::update::checker::CheckResult::UpToDate)) | Ok(None) => {
                                                // Already up to date — nothing to show
                                            }
                                            Err(e) => {
                                                tracing::warn!(error = %e, "manual update check failed");
                                            }
                                        }
                                        });
                                        break;
                                    }
                                    if cmd_tx.send(Cmd::Run(text)).is_ok() {
                                        // Root turn queued. Synthetic turns set the
                                        // same flag at their send sites — `running`
                                        // means "a turn is in flight or queued", and
                                        // it is cleared only by settle_after_turn.
                                        app.running = true;
                                    }
                                    user_msg_count += 1;
                                    if user_msg_count == 10 {
                                        // Fire-and-forget: generate and persist title via lite model.
                                        let provider = {
                                            let mut store = model_store.lock().await;
                                            store.get_or_create_provider("lite").ok()
                                        };
                                        if let Some(p) = provider {
                                            let msgs = agent
                                                .runtime()
                                                .get_messages(&session)
                                                .await
                                                .unwrap_or_default();
                                            let dir = session_dir.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) =
                                                    generate_session_title(p, &msgs, &dir).await
                                                {
                                                    tracing::warn!(error = %e, "failed to generate session title");
                                                }
                                            });
                                        }
                                    }
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
                                    outcome = Some(TuiOutcome::Quit);
                                }
                            }
                        }
                    }
                    Event::Mouse(MouseEvent {
                        kind: MouseEventKind::ScrollUp,
                        ..
                    }) => {
                        has_scroll = true;
                        saw_scroll = true;
                        if app.scroll_wheel_up() {
                            dirty = true;
                        }
                    }
                    Event::Mouse(MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        ..
                    }) => {
                        has_scroll = true;
                        saw_scroll = true;
                        if app.scroll_wheel_down() {
                            dirty = true;
                        }
                    }
                    Event::Mouse(MouseEvent {
                        kind, column, row, ..
                    }) => {
                        dirty = true;
                        has_mouse = true;
                        // The copy-menu hit-test needs the terminal size to locate
                        // the popup exactly where it was drawn.
                        let size = terminal.size()?;
                        if let Some(action) =
                            app.handle_mouse(kind, column, row, size.width, size.height)
                        {
                            if let Action::CopySelection = action {
                                let text = app.selection_text();
                                copy_text(&mut app, &mut clipboard, text);
                                app.clear_selection();
                            }
                        }
                    }
                    Event::Paste(text) => {
                        dirty = true;
                        app.paste(&text);
                    }
                    Event::Resize(_, _) => {
                        dirty = true;
                    }
                    _ => {}
                }
            }
            // Build a compact event-type tag for the log (e.g. "S" / "K" / "SM" / "SK").
            let mut event_types = String::new();
            if has_scroll {
                event_types.push('S');
            }
            if has_key {
                event_types.push('K');
            }
            if has_mouse {
                event_types.push('M');
            }

            // If no events arrived, sleep briefly to avoid busy-spinning.
            let slept = !dirty;
            if slept {
                std::thread::sleep(Duration::from_millis(2));
            }

            // Liveness tick: active (Running/Waiting) with no events this
            // iteration → redraw so spinner/elapsed advance. Tick-only frames are
            // drawn but not captured (see UI_TICK).
            let mut tick_fired = false;
            if !dirty && app.is_active() && last_draw.elapsed() >= UI_TICK {
                dirty = true;
                tick_fired = true;
            }

            // Only redraw when state actually changed (dirty flag). With a 2 ms
            // poll timeout the idle loop would otherwise burn ~500 useless draws/s.
            let mut draw_elapsed = Duration::ZERO;
            if dirty {
                last_draw = Instant::now();
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
                capture_elapsed = frames.capture(!tick_fired, &mut app, size.width, size.height);
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

        // ── End of inner event loop ──

        // Ensure the agent task is told to stop and has a chance to finish.
        let _ = cmd_tx.send(Cmd::Quit);
        let _ = agent_task.await;

        tracing::info!("tui: agent task joined, flushing logs");
        let _ = frames.flush();
        let _ = perf_log.flush();
        let _ = composer_log.flush();

        tracing::info!("tui: logs flushed, entering outcome match");
        match outcome.unwrap_or(TuiOutcome::Quit) {
            TuiOutcome::Quit => {
                // Write a pending-title marker on exit.  The title will be generated
                // asynchronously on next startup — zero blocking here.
                if user_msg_count < 10 && read_session_title(&session_dir).is_none() {
                    let sessions_dir = session_dir.parent().unwrap();
                    write_pending_title_marker(sessions_dir, &session_dir);
                }

                // Shutdown background task registry: cancel all running tasks and
                // SIGKILL all process groups. Must be explicit — executor tasks hold
                // Arc<Registry> so Drop won't fire.
                tracing::info!("tui: quit arm, shutting down bg registry");
                bg_registry.shutdown();
                tracing::info!("tui: bg registry shut down, dropping terminal guard");

                // Restore the terminal BEFORE printing so the diagnostic lands in
                // the normal screen, visible above the next shell prompt.
                drop(_guard);
                tracing::info!("tui: guard dropped, evaluating wheel diagnostic");
                if let Some(notice) = wheel_dead_notice(
                    std::env::var("TERM_PROGRAM").ok().as_deref(),
                    saw_scroll,
                    app.viewport.rendered_total > app.viewport.viewport_height,
                ) {
                    tracing::info!("wheel diagnostic: printing to restored terminal");
                    println!("{}", notice);
                } else {
                    tracing::info!(saw_scroll, "wheel diagnostic: not fired");
                }
                tracing::info!("tui: quit complete");

                return Ok(());
            }
            TuiOutcome::SwitchSession(picked_dir) => {
                match agent.switch_to_session(&picked_dir, &base_dir).await {
                    Ok((new_id, messages, new_ctx)) => {
                        // Re-point the tracing file sink so logs follow the data
                        // directory. Without this, logs keep streaming into the
                        // launch session's dir while messages/turn logs/metrics go
                        // to the resumed session's dir (2026-09-22 bfef1017 vs
                        // 317d95e1 split — forensics had to correlate two dirs).
                        if new_ctx.log_path().display().to_string() != log_path {
                            match log_core::FileSink::new(new_ctx.log_path()).await {
                                Ok(sink) => {
                                    log_sinks.replace_sinks(vec![Box::new(sink)]).await;
                                    tracing::info!(
                                        path = %new_ctx.log_path().display(),
                                        "logging re-pointed to resumed session directory"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "failed to re-point logging after session switch; logs stay in the launch directory");
                                }
                            }
                        }
                        session = new_id;
                        session_ctx = new_ctx;
                        resume_messages = Some(messages);
                        continue; // Re-enter outer loop: fresh App + channels + agent_loop
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "session switch failed, shutting down");
                        bg_registry.shutdown();
                        return Ok(());
                    }
                }
            }
        }
    } // end outer loop
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
    skill_resolver: Arc<SkillResolver>,
    skill_telemetry: Arc<SkillTelemetry>,
    session: SessionId,
    session_ctx: SessionContext,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    model_store: Arc<tokio::sync::Mutex<ModelStore>>,
    router: Arc<PhimintRouter>,
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

    // Session-scope skills (skill-lifetime v3): reload the active list and
    // re-bake it into the system prompt. Covers both process resume (fresh
    // agent, restored history) and session switches (fresh agent_loop).
    //
    // The bake base is the PRISTINE build-time prompt captured once from the
    // runtime config — that string already carries everything the builder
    // injected (catalog, token-budget suffix, CLAUDE.md, memory index).
    // Recomposing from the resolver here would strip those sections.
    let bake_base: String = agent
        .system_prompt()
        .await
        .unwrap_or_else(|| crate::agent::compose_system_prompt(&skill_resolver));
    let mut active_skills = load_active_skills(&session_ctx.session_dir);
    if !active_skills.is_empty() {
        let prompt = append_active_skills(&bake_base, &active_skills);
        if let Err(e) = agent.set_system_prompt(&session, prompt).await {
            tracing::warn!(error = %e, "failed to re-bake active skills on resume");
        }
    }

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
                // Check for model-related commands first
                if let Some(response) = handle_model_command(&input, &model_store, &router) {
                    // Send response as a system message (reuse TurnError for display)
                    let _ = event_tx.send(TuiEvent::TurnError(response));
                    continue;
                }

                turn_number += 1;
                let turn_events: std::sync::Arc<std::sync::Mutex<Vec<RuntimeEvent>>> =
                    std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

                // Skill routing (skill-lifetime v3): resolve the slash command,
                // then dispatch by the skill's declared scope. Session-scope
                // (the default) bakes the body into the system prompt for the
                // rest of the session; turn-scope rides the v2 ephemeral path;
                // plain input passes through untouched.
                let original_input = input.clone();
                let resolved = skill_resolver.resolve_with_meta(&input);
                if let Some(r) = &resolved {
                    // Canonical name straight from SKILL.md — no string parse.
                    skill_telemetry.record_slash(&r.name);
                }
                // Session-scope turns submit the RAW command: `$ARGUMENTS`
                // reach the model through it, and the body is already baked
                // into the system prompt below.
                let turn_input = match &resolved {
                    None => original_input.clone(),
                    Some(r) if r.scope == SkillScope::Turn => r.body.clone(),
                    Some(_) => original_input.clone(),
                };

                // Pre-turn telemetry: snapshot slash events BEFORE run_turn so
                // the on_turn_end hook (which fires inside run_turn) sees them.
                // The hook reads `pending_turn_custom` at turn-end; setting it
                // now ensures slash-triggered skills appear in turn.custom.
                // Model-triggered events (SkillTool::call) happen DURING the
                // turn, after the hook fires — they only appear in session-level
                // custom (via set_session_custom post-turn).
                {
                    let turn_skill_snap = skill_telemetry.snapshot_and_reset();
                    if !turn_skill_snap.as_object().map_or(true, |m| m.is_empty()) {
                        telemetry.set_turn_custom(turn_skill_snap);
                    }
                }

                let turn_events_clone = turn_events.clone();
                let scope = resolved.as_ref().map(|r| r.scope);
                // Turn-scope skills inject the resolved body as an *ephemeral*
                // user message: the LLM sees it for this turn only, then the
                // engine strips it from memory and persistence at turn end.
                // The turn leaves NO trace in history — neither body nor
                // command (v2-pinned behavior; resume replay shows this
                // turn's answer without a user line). The raw command lives
                // only in the turn log below.
                let result = match scope {
                    Some(SkillScope::Turn) => {
                        agent
                            .run_turn_ephemeral_input(session.clone(), &turn_input, move |ev| {
                                // Persistence only — the UI is fed by the persistent
                                // bus subscription (see the bridge in run_tui). Sending
                                // here too would double-deliver every event.
                                turn_events_clone.lock().unwrap().push(ev);
                                Ok(())
                            })
                            .await
                    }
                    scope => {
                        if let Some(SkillScope::Session) = scope {
                            // Activate (or refresh) in the session's active list,
                            // re-bake the prompt, and persist for resume.
                            let r = resolved.as_ref().expect("Session scope implies resolved");
                            let entry = ActiveSkillEntry {
                                name: r.name.clone(),
                                body: r.body.clone(),
                            };
                            if let Some(existing) =
                                active_skills.iter_mut().find(|e| e.name == entry.name)
                            {
                                // Re-trigger: refresh the $ARGUMENTS substitution.
                                existing.body = entry.body;
                            } else {
                                active_skills.push(entry);
                            }
                            // Re-bake from the pristine build-time prompt
                            // (`bake_base`): append-only, so re-triggers stay
                            // idempotent and builder-injected sections
                            // (CLAUDE.md / memory / token-budget) survive.
                            let prompt = append_active_skills(&bake_base, &active_skills);
                            let prompt_len = prompt.len();
                            if let Err(e) = agent.set_system_prompt(&session, prompt).await {
                                tracing::warn!(
                                    error = %e,
                                    "failed to bake active skills into system prompt"
                                );
                                // Mid-turn warning: the run continues, so this
                                // must not settle the turn (TurnError does).
                                let _ = event_tx.send(TuiEvent::Warning(format!(
                                    "! skill `{}` could not be activated (prompt bake failed): {e}",
                                    r.name
                                )));
                            }
                            if let Err(e) =
                                save_active_skills(&session_ctx.session_dir, &active_skills)
                            {
                                tracing::warn!(error = %e, "failed to persist active skills");
                                let _ = event_tx.send(TuiEvent::Warning(format!(
                                    "! skill `{}` active now but will NOT survive resume (persist failed): {e}",
                                    r.name
                                )));
                            }
                            // Budget note (spec, known limitation): the token-budget
                            // core estimated its base overhead from the prompt at
                            // build time; baking a body shifts actual usage by this
                            // much. The 10% buffer absorbs typical skill bodies.
                            tracing::info!(
                                skill = %r.name,
                                active = active_skills.len(),
                                baked_chars = prompt_len,
                                "session-scope skill baked into system prompt"
                            );
                        }
                        agent
                            .run_turn(session.clone(), &turn_input, move |ev| {
                                turn_events_clone.lock().unwrap().push(ev);
                                Ok(())
                            })
                            .await
                    }
                };

                // Persist regardless of success (matches the REPL behavior).
                // The log records the original command, never the skill body.
                let turn_events_vec = turn_events.lock().unwrap().clone();
                if let Err(e) =
                    save_turn_log(&session_ctx, turn_number, &turn_events_vec, &original_input)
                {
                    tracing::warn!(error = %e, "failed to save turn log");
                }

                // Snapshot current window messages for resume support.
                if let Ok(msgs) = agent.runtime().get_messages(&session).await {
                    if let Err(e) = persist_window_messages(&session_ctx.session_dir, &msgs) {
                        tracing::warn!(error = %e, "failed to persist window messages");
                    }
                }

                // Post-turn telemetry: model-triggered skill events (recorded by
                // SkillTool::call during the turn) are captured here. These can't
                // go into turn.custom (the on_turn_end hook already fired), so
                // they only appear in session-level custom via set_session_custom.
                {
                    skill_telemetry.snapshot_and_reset(); // clear per-turn state
                    telemetry.set_session_custom(skill_telemetry.session_snapshot());
                    // Yield so the observer processes SetSessionCustom before we
                    // read the session for the per-turn save. Without this, the
                    // save races with the observer and reads stale custom data.
                    tokio::task::yield_now().await;

                    let metrics = telemetry.session.read().await;
                    let _ = phi_telemetry::save_metrics(&metrics, &session_ctx.session_dir);
                }

                match result {
                    Ok(RunOutcome::Failed { error }) => {
                        let _ = event_tx.send(TuiEvent::TurnError(error));
                    }
                    Ok(RunOutcome::MaxTurnsExceeded { turns }) => {
                        let _ = event_tx
                            .send(TuiEvent::TurnError(format!("max turns ({turns}) exceeded")));
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

/// Handle model-related commands.
///
/// Returns Some(response) if the input was a model command, None otherwise.
fn handle_model_command(
    input: &str,
    model_store: &Arc<tokio::sync::Mutex<ModelStore>>,
    router: &Arc<PhimintRouter>,
) -> Option<String> {
    let input = input.trim();

    // /model [name] - show or set main model
    if input == "/model" || input.starts_with("/model ") {
        let args = input.strip_prefix("/model").unwrap_or("").trim();
        if args.is_empty() {
            // Show current main model
            let store = model_store.blocking_lock();
            let model = store.tier_model("main");
            Some(format!("Current main model: {}", model))
        } else {
            // Set main model
            let mut store = model_store.blocking_lock();
            store.set_tier_model("main", args.to_string());
            Some(format!("Main model set to: {}", args))
        }
    }
    // /lite [name] - show or set lite model
    else if input == "/lite" || input.starts_with("/lite ") {
        let args = input.strip_prefix("/lite").unwrap_or("").trim();
        if args.is_empty() {
            let store = model_store.blocking_lock();
            let model = store.tier_model("lite");
            Some(format!("Current lite model: {}", model))
        } else {
            let mut store = model_store.blocking_lock();
            store.set_tier_model("lite", args.to_string());
            Some(format!("Lite model set to: {}", args))
        }
    }
    // /advanced [name] - show or set advanced model
    else if input == "/advanced" || input.starts_with("/advanced ") {
        let args = input.strip_prefix("/advanced").unwrap_or("").trim();
        if args.is_empty() {
            let store = model_store.blocking_lock();
            let model = store.tier_model("advanced");
            Some(format!("Current advanced model: {}", model))
        } else {
            let mut store = model_store.blocking_lock();
            store.set_tier_model("advanced", args.to_string());
            Some(format!("Advanced model set to: {}", args))
        }
    }
    // /models - list all configured models
    else if input == "/models" {
        let store = model_store.blocking_lock();
        let tiers = store.list_tiers();
        let mut response = String::from("Configured models:\n");
        for (tier, model) in tiers {
            response.push_str(&format!("  {}: {}\n", tier, model));
        }
        response.push_str(&format!(
            "Focus mode: {}",
            if router.is_focus_mode() { "on" } else { "off" }
        ));
        Some(response)
    }
    // /focus - toggle focus mode
    else if input == "/focus" {
        let new_state = !router.is_focus_mode();
        router.set_focus_mode(new_state);
        Some(format!(
            "Focus mode: {}",
            if new_state { "on" } else { "off" }
        ))
    } else {
        None
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

/// One-line startup notice for Apple Terminal users. Terminal.app forwards
/// wheel/trackpad events to the app only while View → Allow Mouse Reporting
/// is checked for that window (the macOS default is ON — an unchecked window
/// is almost always a prior ⌘R/“refresh” toggle). When it is off, the swipe
/// is swallowed by the terminal itself and scrolling silently dies.
/// `term_program` is the value of `$TERM_PROGRAM`; only `Apple_Terminal`
/// gets the notice.
fn terminal_scroll_notice(term_program: Option<&str>) -> Option<String> {
    if term_program != Some("Apple_Terminal") {
        return None;
    }
    Some(
        "Note: wheel/trackpad scroll needs View > Allow Mouse Reporting - fn+Up/fn+Down page history".to_string(),
    )
}

/// Exit-time diagnostic. Printed to the restored terminal when an entire run
/// in Apple Terminal never received a single wheel event even though the
/// transcript was taller than one screen — the exact fingerprint of Allow
/// Mouse Reporting being off in that window. Turns the previously silent
/// failure into an actionable message right after the session ends.
fn wheel_dead_notice(
    term_program: Option<&str>,
    saw_scroll: bool,
    scrollable: bool,
) -> Option<String> {
    if term_program != Some("Apple_Terminal") || saw_scroll || !scrollable {
        return None;
    }
    Some(
        "Note: no wheel events were received this run. If trackpad scrolling does nothing, enable View > Allow Mouse Reporting in your terminal and retry; fn+Up/fn+Down always work".to_string(),
    )
}

/// One-line digest pushed after the replayed history. The resume viewport
/// lands on the tail of the conversation, so without this the user has no
/// signal that the full history is loaded (or that PgUp reaches it).
fn resume_digest_line(messages: &[ChatMessage], title: Option<String>) -> String {
    let turns = messages
        .iter()
        .filter(|m| {
            matches!(
                m,
                ChatMessage::User {
                    ephemeral: false,
                    ..
                }
            )
        })
        .count();
    // Leading space so a titled digest reads `resumed session "foo"` and an
    // untitled one still reads `resumed session` with no dangling gap.
    let label = title.map(|t| format!(" \"{t}\"")).unwrap_or_default();
    let turn_word = if turns == 1 { "turn" } else { "turns" };
    let msg_word = if messages.len() == 1 {
        "message"
    } else {
        "messages"
    };
    format!(
        "↩ resumed session{label}: {turns} {turn_word}, {} {msg_word}. PgUp scrolls history, send a message to continue",
        messages.len()
    )
}

/// Replay historical `ChatMessage`s into the TUI transcript so the user sees
/// the previous conversation after `--resume`.
fn replay_messages_to_transcript(app: &mut super::app::App, messages: &[ChatMessage]) {
    use phi_tui::lines::{LineKind, OutputLine};

    app.push_system("── resumed conversation ──");

    for msg in messages {
        match msg {
            ChatMessage::User { content, .. } => {
                app.transcript.push_user(content);
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                // Push text content (if any).
                // ONE raw OutputLine for the whole message: Normal prose is
                // wrapped at display time (markdown + `wrap_line`), and
                // `rewrap_output` keeps Normal lines whole for exactly that
                // reason. Pre-wrapping here froze the resume-time width into
                // the transcript, so a resized terminal kept stale breaks.
                if let Some(text) = content {
                    if !text.is_empty() {
                        let text = text.clone();
                        app.transcript.push(OutputLine {
                            original: Some(text.clone()),
                            text,
                            kind: LineKind::Normal,
                            spans: None,
                            detail: None,
                        });
                    }
                }
                // Push tool call markers.
                if let Some(tcs) = tool_calls {
                    for tc in tcs {
                        app.transcript.push(OutputLine {
                            text: format!("⚙ {}", tc.name),
                            kind: LineKind::Tool,
                            spans: None,
                            original: None,
                            detail: None,
                        });
                    }
                }
            }
            ChatMessage::Tool { name, content, .. } => {
                let label = name.as_deref().unwrap_or("tool");
                let preview = content.lines().next().unwrap_or(content);
                // `elide`, not a byte slice: `&preview[..120]` panics mid-UTF-8
                // on non-ASCII tool output (any CJK repo), and the budget is
                // display columns so the row cannot overflow the pane.
                let display = format!(
                    "↻ {}: {}",
                    label,
                    phi_tui::wrap::elide(preview, 120, phi_tui::wrap::Elide::Head)
                );
                app.transcript.push(OutputLine {
                    text: display,
                    kind: LineKind::ToolResult,
                    spans: None,
                    original: None,
                    detail: None,
                });
            }
            _ => {} // System / Custom — already filtered, skip.
        }
    }

    app.push_system("── end of resumed conversation ──");
}

#[cfg(test)]
mod resume_digest_tests {
    use super::*;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage::Assistant {
            content: Some(text.into()),
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: None,
        }
    }

    #[test]
    fn digest_counts_turns_and_messages_with_title() {
        let msgs = vec![
            user("你好"),
            assistant("你好！"),
            ChatMessage::Tool {
                tool_call_id: "tc1".into(),
                name: Some("read".into()),
                content: "x".into(),
            },
            user("继续"),
            assistant("好的"),
        ];
        let line = resume_digest_line(&msgs, Some("探究工程".into()));
        assert!(line.contains("resumed session \"探究工程\""), "{line}");
        assert!(line.contains("2 turns"), "{line}");
        assert!(line.contains("5 messages"), "{line}");
        assert!(line.contains("PgUp"), "{line}");
    }

    #[test]
    fn digest_omits_title_when_absent() {
        let msgs = vec![user("hi"), assistant("hello")];
        let line = resume_digest_line(&msgs, None);
        assert!(!line.contains('"'), "{line}");
        assert!(line.contains("1 turn, 2 messages"), "{line}");
    }

    /// Ephemeral user messages (skill bodies) never reach the persisted
    /// history, but if one ever did leak into a restored list it must not
    /// inflate the turn count the user is told about.
    #[test]
    fn digest_does_not_count_ephemeral_as_turn() {
        let msgs = vec![ChatMessage::user_ephemeral("skill body"), assistant("ok")];
        let line = resume_digest_line(&msgs, None);
        assert!(line.contains("0 turns, 2 messages"), "{line}");
    }

    /// Terminal.app needs View → Allow Mouse Reporting before wheel events
    /// reach the app; users get an actionable notice instead of a dead
    /// trackpad. Other terminals (iTerm2, Ghostty, ...) are untouched.
    #[test]
    fn scroll_notice_targets_apple_terminal_only() {
        let notice = terminal_scroll_notice(Some("Apple_Terminal")).expect("notice expected");
        assert!(notice.contains("Allow Mouse Reporting"), "{notice}");
        assert!(notice.contains("fn+Up"), "{notice}");
        assert_eq!(terminal_scroll_notice(Some("iTerm.app")), None);
        assert_eq!(terminal_scroll_notice(Some("ghostty")), None);
        assert_eq!(terminal_scroll_notice(None), None);
    }

    /// The exit diagnostic fires only for the full failure fingerprint in
    /// Apple Terminal: zero wheel events all run, and there was more history
    /// than one screen. Any wheel event, another terminal, or a short
    /// transcript keeps the exit output clean.
    #[test]
    fn wheel_dead_notice_needs_apple_terminal_zero_scroll_and_history() {
        let notice =
            wheel_dead_notice(Some("Apple_Terminal"), false, true).expect("notice expected");
        assert!(notice.contains("no wheel events were received"), "{notice}");
        assert!(notice.contains("Allow Mouse Reporting"), "{notice}");
        // Saw a wheel event: reporting works, stay quiet.
        assert_eq!(wheel_dead_notice(Some("Apple_Terminal"), true, true), None);
        // Nothing to scroll: the message would be noise.
        assert_eq!(
            wheel_dead_notice(Some("Apple_Terminal"), false, false),
            None
        );
        // Other terminals handle wheel natively or via their own settings.
        assert_eq!(wheel_dead_notice(Some("iTerm.app"), false, true), None);
        assert_eq!(wheel_dead_notice(None, false, true), None);
    }
}

#[cfg(test)]
mod model_command_tests {
    use super::*;
    use crate::model_store::ModelStore;
    use crate::router::PhimintRouter;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn setup() -> (Arc<Mutex<ModelStore>>, Arc<PhimintRouter>) {
        let config = crate::config::ModelConfig {
            base_url: Some("https://api.openai.com/v1".to_string()),
            api_key: Some("test-key".to_string()),
            protocol: Some("openai".to_string()),
            main: crate::config::TierConfig::Simple("gpt-4".to_string()),
            lite: Some(crate::config::TierConfig::Simple(
                "gpt-3.5-turbo".to_string(),
            )),
            advanced: Some(crate::config::TierConfig::Simple("gpt-4-turbo".to_string())),
            scene_tiers: None,
            update: Default::default(),
            ui: Default::default(),
        };

        // Create a mock provider for testing
        struct MockProvider;

        #[async_trait::async_trait]
        impl phi_agent::llm_trait::LlmProvider for MockProvider {
            async fn stream(
                &self,
                _request: phi_agent::llm_trait::ChatRequest,
            ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError>
            {
                unimplemented!()
            }

            async fn chat(
                &self,
                _request: phi_agent::llm_trait::ChatRequest,
            ) -> Result<phi_agent::llm_trait::ChatResponse, phi_agent::llm_trait::LlmError>
            {
                unimplemented!()
            }

            fn capabilities(&self) -> phi_agent::llm_trait::Capabilities {
                phi_agent::llm_trait::Capabilities::default()
            }

            fn info(&self) -> phi_agent::llm_trait::ProviderInfo {
                phi_agent::llm_trait::ProviderInfo {
                    name: "mock".to_string(),
                    model: "mock-model".to_string(),
                    version: None,
                }
            }
        }

        let provider = Arc::new(MockProvider);
        let model_store = Arc::new(Mutex::new(ModelStore::new(config, provider)));
        let router = Arc::new(PhimintRouter::new());
        (model_store, router)
    }

    #[test]
    fn test_model_command_show_main() {
        let (model_store, router) = setup();
        let response = handle_model_command("/model", &model_store, &router);
        assert_eq!(response, Some("Current main model: gpt-4".to_string()));
    }

    #[test]
    fn test_model_command_set_main() {
        let (model_store, router) = setup();
        let response = handle_model_command("/model gpt-4-turbo", &model_store, &router);
        assert_eq!(response, Some("Main model set to: gpt-4-turbo".to_string()));

        // Verify the model was actually set
        let store = model_store.blocking_lock();
        assert_eq!(store.tier_model("main"), "gpt-4-turbo");
    }

    #[test]
    fn test_lite_command_show() {
        let (model_store, router) = setup();
        let response = handle_model_command("/lite", &model_store, &router);
        assert_eq!(
            response,
            Some("Current lite model: gpt-3.5-turbo".to_string())
        );
    }

    #[test]
    fn test_lite_command_set() {
        let (model_store, router) = setup();
        let response = handle_model_command("/lite gpt-4o-mini", &model_store, &router);
        assert_eq!(response, Some("Lite model set to: gpt-4o-mini".to_string()));
    }

    #[test]
    fn test_advanced_command_show() {
        let (model_store, router) = setup();
        let response = handle_model_command("/advanced", &model_store, &router);
        assert_eq!(
            response,
            Some("Current advanced model: gpt-4-turbo".to_string())
        );
    }

    #[test]
    fn test_advanced_command_set() {
        let (model_store, router) = setup();
        let response = handle_model_command("/advanced o1-preview", &model_store, &router);
        assert_eq!(
            response,
            Some("Advanced model set to: o1-preview".to_string())
        );
    }

    #[test]
    fn test_models_command() {
        let (model_store, router) = setup();
        let response = handle_model_command("/models", &model_store, &router);
        assert!(response.is_some());
        let response = response.unwrap();
        assert!(response.contains("main: gpt-4"));
        assert!(response.contains("lite: gpt-3.5-turbo"));
        assert!(response.contains("advanced: gpt-4-turbo"));
        assert!(response.contains("Focus mode: off"));
    }

    #[test]
    fn test_focus_command_toggle() {
        let (model_store, router) = setup();

        // Toggle focus on
        let response = handle_model_command("/focus", &model_store, &router);
        assert_eq!(response, Some("Focus mode: on".to_string()));
        assert!(router.is_focus_mode());

        // Toggle focus off
        let response = handle_model_command("/focus", &model_store, &router);
        assert_eq!(response, Some("Focus mode: off".to_string()));
        assert!(!router.is_focus_mode());
    }

    #[test]
    fn test_non_model_command() {
        let (model_store, router) = setup();
        let response = handle_model_command("hello", &model_store, &router);
        assert_eq!(response, None);
    }

    #[test]
    fn test_model_command_with_extra_spaces() {
        let (model_store, router) = setup();
        let response = handle_model_command("  /model   gpt-4-turbo  ", &model_store, &router);
        assert_eq!(response, Some("Main model set to: gpt-4-turbo".to_string()));
    }
}

#[cfg(test)]
mod skill_turn_log_tests {
    use super::*;
    use crate::skills::{refresh_catalog, render_catalog};
    use phi_agent::resolve_session;

    /// Fixture with one session-scope skill (frontmatter default) and one
    /// turn-scope skill (`scope: turn`).
    fn fixture_skill_dir(tmp: &std::path::Path) -> std::path::PathBuf {
        let session_dir = tmp.join("skills").join("catalog-skill");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("SKILL.md"),
            "---\nname: catalog-skill\ndescription: guard fixture skill\n\
             user-invocable: true\n---\n\nEPHEMERAL-FIXTURE-BODY skill instructions for: $ARGUMENTS",
        )
        .unwrap();
        let turn_dir = tmp.join("skills").join("oneshot-skill");
        std::fs::create_dir_all(&turn_dir).unwrap();
        std::fs::write(
            turn_dir.join("SKILL.md"),
            "---\nname: oneshot-skill\ndescription: one-shot fixture skill\n\
             user-invocable: true\nscope: turn\n---\n\nONESHOT-FIXTURE-BODY instructions",
        )
        .unwrap();
        tmp.join("skills")
    }

    /// Spec §Verification (skill-lifetime v3): a session-scope skill turn
    /// (frontmatter default) submits the RAW command — `$ARGUMENTS` reach the
    /// model through it — while the active list gains the entry and the
    /// composed prompt gains the Active Skills section. `turn_NNN.jsonl`'s
    /// `user_input` stays the raw command, never the body.
    #[test]
    fn session_scope_turn_keeps_raw_command_and_activates_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = fixture_skill_dir(tmp.path());
        let resolver = SkillResolver::from_dirs(&[skill_dir]);
        let session_ctx = resolve_session(Some("skill-session-activate"), tmp.path()).unwrap();

        // agent_loop wiring: resolve → dispatch by scope.
        let input = "/catalog-skill fix the parser".to_string();
        let original_input = input.clone();
        let resolved = resolver.resolve_with_meta(&input);
        let turn_input = match &resolved {
            None => original_input.clone(),
            Some(r) if r.scope == SkillScope::Turn => r.body.clone(),
            Some(_) => original_input.clone(),
        };

        let r = resolved.expect("slash command must resolve (fixture mismatch)");
        assert_eq!(
            r.scope,
            SkillScope::Session,
            "frontmatter default is session"
        );
        assert_eq!(
            turn_input, original_input,
            "session-scope turn must submit the raw command, not the body"
        );
        assert!(
            !turn_input.contains("EPHEMERAL-FIXTURE-BODY"),
            "raw command must not leak the body"
        );

        // Activation side effects: entry in the list, section in the prompt,
        // persisted for resume.
        let mut active: Vec<ActiveSkillEntry> = Vec::new();
        let entry = ActiveSkillEntry {
            name: r.name.clone(),
            body: r.body.clone(),
        };
        active.push(entry);
        let base = crate::agent::compose_system_prompt(&resolver);
        let prompt = append_active_skills(&base, &active);
        assert!(
            prompt.starts_with(&base),
            "base prompt must be preserved verbatim"
        );
        assert!(
            prompt.contains("## Active Skills") && prompt.contains("EPHEMERAL-FIXTURE-BODY"),
            "composed prompt must contain the baked section"
        );
        save_active_skills(&session_ctx.session_dir, &active).unwrap();
        assert_eq!(
            load_active_skills(&session_ctx.session_dir),
            active,
            "persisted list must round-trip"
        );

        save_turn_log(&session_ctx, 1, &[], &original_input).unwrap();
        let log = std::fs::read_to_string(session_ctx.turn_path(1)).unwrap();
        let meta: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(
            meta["user_input"], "/catalog-skill fix the parser",
            "turn JSONL user_input must be the raw command"
        );
        assert!(
            !log.contains("EPHEMERAL-FIXTURE-BODY"),
            "turn JSONL must never contain the skill body: {log}"
        );
    }

    /// Turn-scope skills (`scope: turn`) keep the v2 ephemeral contract: the
    /// resolved body is what gets submitted (as ephemeral input), and the
    /// active list is untouched.
    #[test]
    fn turn_scope_turn_submits_body_and_skips_activation() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = fixture_skill_dir(tmp.path());
        let resolver = SkillResolver::from_dirs(&[skill_dir]);

        let input = "/oneshot-skill".to_string();
        let original_input = input.clone();
        let resolved = resolver.resolve_with_meta(&input);
        let turn_input = match &resolved {
            None => original_input.clone(),
            Some(r) if r.scope == SkillScope::Turn => r.body.clone(),
            Some(_) => original_input.clone(),
        };

        let r = resolved.expect("slash command must resolve (fixture mismatch)");
        assert_eq!(r.scope, SkillScope::Turn, "scope: turn must parse");
        assert_eq!(
            turn_input, r.body,
            "turn-scope turn must submit the resolved body (ephemeral path)"
        );
        // No activation: the dispatcher's Session branch never runs for Turn.
        assert!(
            !turn_input.is_empty() && turn_input != original_input,
            "body must differ from the raw command (fixture mismatch)"
        );
    }

    /// Non-skill turns behave exactly as before: no resolve transform, no
    /// ephemeral dispatch, no activation, log records the verbatim input.
    #[test]
    fn turn_log_records_plain_input_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let session_ctx = resolve_session(Some("skill-ephemeral-plain"), tmp.path()).unwrap();

        let input = "fix the flaky test in app.rs".to_string();
        let original_input = input.clone();
        let resolver = SkillResolver::from_dirs(&[tmp.path().join("nonexistent")]);
        let resolved = resolver.resolve_with_meta(&input);
        let turn_input = match &resolved {
            None => original_input.clone(),
            Some(r) if r.scope == SkillScope::Turn => r.body.clone(),
            Some(_) => original_input.clone(),
        };

        assert!(
            resolved.is_none(),
            "plain input must not resolve to a skill"
        );
        assert_eq!(
            turn_input, original_input,
            "plain input must pass through untouched"
        );

        save_turn_log(&session_ctx, 1, &[], &original_input).unwrap();
        let log = std::fs::read_to_string(session_ctx.turn_path(1)).unwrap();
        let meta: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(meta["user_input"], "fix the flaky test in app.rs");
    }

    /// Verification #3: re-triggering a session-scope skill (with different
    /// `$ARGUMENTS`, hence a different resolved body) must replace the entry
    /// in place — exactly one entry per name, exactly one section copy.
    #[test]
    fn repeated_session_activation_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = fixture_skill_dir(tmp.path());
        let resolver = SkillResolver::from_dirs(&[skill_dir]);

        // Two triggers, different args → different bodies ($ARGUMENTS baked in).
        let first = resolver
            .resolve_with_meta("/catalog-skill task one")
            .unwrap();
        let second = resolver
            .resolve_with_meta("/catalog-skill task two")
            .unwrap();
        assert_ne!(
            first.body, second.body,
            "args must change the resolved body"
        );

        // The dispatcher's update rule: find by name → replace body.
        let mut active: Vec<ActiveSkillEntry> = Vec::new();
        for r in [&first, &second] {
            let entry = ActiveSkillEntry {
                name: r.name.clone(),
                body: r.body.clone(),
            };
            if let Some(existing) = active.iter_mut().find(|e| e.name == entry.name) {
                existing.body = entry.body;
            } else {
                active.push(entry);
            }
        }

        assert_eq!(active.len(), 1, "re-trigger must not duplicate the entry");
        assert_eq!(active[0].body, second.body, "latest trigger wins");

        let prompt = append_active_skills(
            crate::agent::compose_system_prompt(&resolver).as_str(),
            &active,
        );
        assert_eq!(
            prompt.matches("### skill: catalog-skill").count(),
            1,
            "section must carry exactly one copy of the skill"
        );
    }

    /// Cross-layer coexistence: the Active Skills section is appended AFTER
    /// the catalog region, and bodies are H2-demoted, so the catalog-refresh
    /// middleware (which re-splices the `## Skills` region every turn) can
    /// never eat the baked section — even when a skill body forges the
    /// catalog anchor.
    #[test]
    fn active_skills_section_survives_catalog_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills").join("forgey");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: forgey\ndescription: anchor forger\nuser-invocable: true\n---\n\n\
             step one\n\n## Skills\n\nforge the anchor\n",
        )
        .unwrap();
        let resolver = SkillResolver::from_dirs(&[tmp.path().join("skills")]);

        let active = vec![ActiveSkillEntry {
            name: "forgey".to_string(),
            body: resolver.resolve("/forgey").unwrap(),
        }];
        let composed = append_active_skills(
            crate::agent::compose_system_prompt(&resolver).as_str(),
            &active,
        );

        // The baked body must arrive demoted — no forged H2 anchor.
        assert!(
            !composed.contains("\n\n## Skills\n\nforge"),
            "demotion must neutralize the forged catalog anchor"
        );
        // Simulate the per-turn middleware re-splice with a fresh catalog.
        let fresh = render_catalog(&resolver).expect("fixture skill must be visible");
        let refreshed = refresh_catalog(&composed, &fresh);
        assert!(
            refreshed.contains("## Active Skills") && refreshed.contains("forge the anchor"),
            "Active Skills section must survive catalog refresh"
        );
    }
}

/// Replay must follow the Normal-prose contract: one raw `OutputLine`, wrapped
/// at display time. Pre-wrapping froze the resume-time width into the
/// transcript — a later resize kept the stale breaks, because
/// `rewrap_output` deliberately leaves Normal lines whole.
#[cfg(test)]
mod replay_tests {
    use super::*;
    use crate::ui::app::App;
    use phi_tui::lines::LineKind;

    fn assistant(text: &str) -> ChatMessage {
        ChatMessage::Assistant {
            content: Some(text.into()),
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: None,
        }
    }

    #[test]
    fn replay_keeps_prose_as_one_raw_line() {
        let mut app = App::new();
        let long = "The policy lives in the wake module and batches completions behind a quiet window so a burst of finished tasks costs one notification turn, not ten of them.";
        let msgs = vec![assistant(long)];
        replay_messages_to_transcript(&mut app, &msgs);
        let normals: Vec<_> = app
            .transcript
            .output
            .iter()
            .filter(|l| l.kind == LineKind::Normal)
            .collect();
        assert_eq!(normals.len(), 1, "prose must stay one logical line");
        assert_eq!(normals[0].text, long);
        assert_eq!(normals[0].original.as_deref(), Some(long));
    }

    /// The tool preview used to be a raw byte slice at 120, which panics
    /// mid-UTF-8 on non-ASCII output (any CJK repo) during resume.
    #[test]
    fn replay_tool_preview_never_panics_on_multibyte() {
        let mut app = App::new();
        let msgs = vec![ChatMessage::Tool {
            tool_call_id: "tc1".into(),
            name: Some("bash".into()),
            content: "中".repeat(200), // 600 bytes — past the old cut
        }];
        replay_messages_to_transcript(&mut app, &msgs);
        let preview = app
            .transcript
            .output
            .iter()
            .find(|l| l.kind == LineKind::ToolResult)
            .expect("tool result line");
        assert!(preview.text.contains("bash"), "{}", preview.text);
        assert!(preview.text.len() < 300, "preview elided: {}", preview.text);
    }
}
