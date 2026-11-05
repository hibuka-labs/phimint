//! The product side of the shell seam (T7c): every [`phi_agent_tui`] fn slot
//! phimint fills in, in one place.
//!
//! The shell owns the mechanism — the event loop, the turn pump, the picker
//! and approval machinery; what a phimint session *means* behind each seam is
//! assembled here and handed over as data ([`phi_agent_tui::ShellSpec`]) plus
//! live objects ([`phi_agent_tui::ShellHandles`]). The split rule (charter
//! §4.5-1): mechanism in the shell, wording and policy here.
//!
//! The design-canvas panel (the M1–M3 cluster) is the one seam with real
//! per-session state behind it. Its split follows the old single-loop
//! ordering exactly: everything that touches the click mailbox runs on the
//! shell's thread inside the sync [`tick`](Wire::tick) closure (so key
//! handling and the drain can never race), and only the await points — the
//! live-handle poll, the envelope push, the consent follow-ups — move to
//! short-lived tasks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use phi_agent::{ApprovalItem, ChatMessage, Tool};
use phi_agent_tui::{
    ConsentProbe, NoticeProbe, PanelTick, PromptSource, ReopenHook, RouteOutcome, SessionBoot,
    SessionBootFn, SessionEndFn, SessionIo, SessionRow, SkillBridge, SkillRebakeFn, SkillRouteFn,
    SubmitHook, Synthetic, TickFacts, TickOut, TipsFetchFn, TipsSlot, TitleGenerator, TitleSlot,
    UpdateCheckFn, UpdateSlot,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::agent;
use crate::model::store::ModelStore;
use crate::shell::canvas_host::{self, AnswerFacts, CanvasHost, ConsentLauncher, ConsentOutcome};
use crate::shell::click_mailbox::ClickMailbox;
use crate::shell::skills::{
    ActiveSkillEntry, SkillResolver, SkillScope, SkillTelemetry, append_active_skills,
    load_active_skills, save_active_skills, sync_active_set,
};
use crate::tools::canvas::{CanvasActivity, canvas_tool_adapters, new_canvas_tools};

// ── Update slot ─────────────────────────────────────────────────────────

/// The update seam: one check closure carrying the wording (the shell only
/// schedules and displays). `None` from the closure is silence — including on
/// the manual `/upgrade` path ("already up to date" was always silent).
pub fn update_slot(
    config: crate::model::config::UpdateConfig,
    version: &str,
    auto_check: bool,
) -> UpdateSlot {
    let endpoints = config.endpoints.clone();
    let ver = version.to_string();
    let check: UpdateCheckFn = Arc::new(move || {
        let endpoints = endpoints.clone();
        let ver = ver.clone();
        Box::pin(async move {
            // Single attempt, 3s timeout per endpoint, silent on failure.
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap_or_default();
            let state = crate::update::state::load();
            match crate::update::checker::check(&client, &endpoints, &state, &ver).await {
                Ok(Some(crate::update::checker::CheckResult::UpgradeAvailable {
                    version, ..
                })) => {
                    // Channel-aware: brew/npm/cargo own their upgrades,
                    // standalone self-replaces via `phimint update` (see
                    // update::install_source).
                    let cmd = crate::update::install_source::detect().upgrade_command();
                    Some(format!("⬆ phimint {version} available - run: {cmd}"))
                }
                Ok(Some(crate::update::checker::CheckResult::UpToDate)) | Ok(None) => None,
                Err(e) => {
                    tracing::debug!(error = %e, "update check failed (silent)");
                    None
                }
            }
        })
    });
    UpdateSlot {
        check,
        auto_check,
        help: "Check for phimint updates".to_string(),
    }
}

// ── Tips slot ───────────────────────────────────────────────────────────

/// The tips seam: built-in defaults (or the cache) render at startup; the
/// fetch closure swaps in cloud content only when the manifest version
/// changed. Never blocks boot: a fresh cache makes the pass a no-op.
pub fn tips_slot(config: &crate::model::config::TipsConfig) -> TipsSlot {
    let endpoints = config.endpoints.clone();
    let interval = Duration::from_secs(config.refresh_interval_secs);
    let fetch: TipsFetchFn = Arc::new(move || {
        let endpoints = endpoints.clone();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap_or_default();
            crate::tips::refresh(&client, &endpoints, interval).await
        })
    });
    TipsSlot {
        initial: crate::tips::initial_items(),
        fetch,
    }
}

// ── Notice probe ────────────────────────────────────────────────────────

/// Runtime announcements whose wording and de-dup policy are product-owned:
/// window rotations (with the count since the last announcement) and the
/// futility brake (once per process). The shell pushes each returned line;
/// the probe keeps its own "already announced" state.
pub fn notice_probe() -> NoticeProbe {
    struct Probe {
        last_reset: usize,
        brake_announced: bool,
    }
    let state = Mutex::new(Probe {
        last_reset: 0,
        brake_announced: false,
    });
    Arc::new(move || {
        let mut out = Vec::new();
        let mut probe = state.lock().unwrap();
        let current = crate::shell::context_rotation::window_reset_count();
        if current > probe.last_reset {
            let n = current - probe.last_reset;
            out.push(format!(
                "~ Context window rotated (x{n}) - previous history archived"
            ));
            probe.last_reset = current;
        }
        if !probe.brake_announced && crate::shell::context_rotation::futility_braked() {
            probe.brake_announced = true;
            out.push(
                "! Futility brake: work budget too small - window rotation paused. \
                 Restart with a larger --token-budget."
                    .to_string(),
            );
        }
        out
    })
}

// ── Session I/O ─────────────────────────────────────────────────────────

/// The resume seam: which sessions exist (with the picker's default-view
/// verdict precomputed into each row), what a rotated session's archived
/// history holds, and how a switch heals `messages.jsonl`.
pub fn session_io() -> SessionIo {
    SessionIo {
        enumerate: Arc::new(|base_dir: &Path, current: Option<&str>| {
            let workspace = crate::session::scope::current_workspace();
            phi_agent::list_sessions(base_dir, current)
                .into_iter()
                .map(|entry| {
                    let in_workspace =
                        crate::session::scope::in_default_scope(&entry, workspace.as_deref());
                    SessionRow {
                        session_dir: entry.session_dir,
                        title: entry.title,
                        last_active_at: entry.last_active_at,
                        in_workspace,
                    }
                })
                .collect()
        }),
        archived: Arc::new(|base_dir: &Path, session_id: &str| {
            crate::session::history::load_archived_messages(base_dir, session_id)
        }),
        restore: Arc::new(|base_dir: &Path, session_id: &str, session_dir: &Path| {
            crate::session::history::restore_messages_jsonl(base_dir, session_id, session_dir)
        }),
        resume_help: "Switch to another session".to_string(),
    }
}

// ── Session-title generation ────────────────────────────────────────────

/// The title seam (T5d): the shell owns the trigger (the 10th user turn, and
/// the pending marker on exit); provider choice and wording are product
/// strategy behind the closures.
pub fn title_slot(model_store: Arc<tokio::sync::Mutex<ModelStore>>) -> TitleSlot {
    let store = Arc::clone(&model_store);
    let generate: TitleGenerator = Arc::new(move |msgs: Vec<ChatMessage>, dir: PathBuf| {
        let model_store = Arc::clone(&store);
        Box::pin(async move {
            let provider = {
                let mut store = model_store.lock().await;
                store.get_or_create_provider("lite").ok()
            };
            if let Some(p) = provider
                && let Err(e) = crate::model::title::generate_session_title(p, &msgs, &dir).await
            {
                tracing::warn!(error = %e, "failed to generate session title");
            }
        })
    });
    let write_marker = Arc::new(|sessions_dir: &Path, session_dir: &Path| {
        crate::model::title::write_pending_title_marker(sessions_dir, session_dir);
    });
    TitleSlot {
        generate,
        write_marker,
    }
}

// ── Consent classification ──────────────────────────────────────────────

/// Which approval prompts are canvas-launch consents (the two-key legend)
/// rather than tool approvals (the three-key tool face). Classification is
/// product policy ([`canvas_host::is_canvas_consent`]).
pub fn consent_probe() -> ConsentProbe {
    Arc::new(canvas_host::is_canvas_consent)
}

// ── Boot notices ────────────────────────────────────────────────────────

/// One-shot startup notices, aggregated product-side before `run()` (the
/// shell prints each as `! {notice}` on the first frame): the token-budget
/// clamp, the socket-board load errors, PATH binary shadows, and the code
/// road degrade. A session switch never replays them.
pub fn startup_notices(socket_notices: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(notice) = crate::shell::context_rotation::take_clamp_notice() {
        out.push(notice);
    }
    out.extend(socket_notices);
    if let Some(notice) = crate::ops::bin_shadow::notice() {
        out.push(notice);
    }
    if let Some(notice) = crate::shell::file_face::take_startup_notice() {
        out.push(notice);
    }
    out
}

// ── Skill routing (skill-lifetime v3) ───────────────────────────────────

/// Per-session skill bookkeeping shared by the two skill closures: where the
/// active list lives (`active_skills.json`), the pristine build-time prompt
/// to bake from, and the list itself.
#[derive(Default)]
struct SkillState {
    session_dir: Option<PathBuf>,
    bake_base: Option<String>,
    active: Vec<ActiveSkillEntry>,
}

// ── The design-canvas panel ─────────────────────────────────────────────

/// How often the tick asks the tools layer for the live canvas handle (a
/// rebind replaces the handle; the poll is de-duped and never blocks).
const HANDLE_POLL: Duration = Duration::from_millis(100);

/// Per-session panel state. Everything here is touched either on the shell's
/// thread (the mailbox and the flags below) or from short-lived tasks (the
/// handle cache); the two never nest their locks.
struct PanelShared {
    // Live-canvas cache, maintained by the handle-poll task.
    live: Option<Arc<phi_canvas::Canvas>>,
    events: Option<mpsc::UnboundedReceiver<phi_canvas::CanvasEvent>>,
    subscribed: Option<Arc<phi_canvas::Canvas>>,
    check_in_flight: bool,
    last_check: Instant,
    // A rebind replaced the canvas: the mailbox's per-screen bookkeeping
    // belongs to the old one — the tick rebuilds it before draining.
    needs_mailbox_reset: bool,
    // The click mailbox (shell-thread only: tick + submit hook).
    mailbox: ClickMailbox,
    // Root-turn falling edge, latched by the tick for its own falling-edge
    // block (one tick's worth of skew is the old loop's own ordering).
    turn_fell: bool,
    // Notes from the spawned async bits (a failed reopen), drained per tick.
    notes: Vec<String>,
}

impl PanelShared {
    fn new() -> Self {
        Self {
            live: None,
            events: None,
            subscribed: None,
            check_in_flight: false,
            // Start throttled-out so the first tick polls immediately.
            last_check: Instant::now()
                .checked_sub(HANDLE_POLL)
                .unwrap_or_else(Instant::now),
            needs_mailbox_reset: false,
            mailbox: ClickMailbox::new(),
            turn_fell: false,
            notes: Vec::new(),
        }
    }
}

/// One session's canvas kit, re-armed by the boot closure and taken by the
/// end closure. All handles are `Arc`, so clones cross into tasks cheaply.
#[derive(Clone)]
struct SessionKit {
    canvas_tools: Arc<phi_canvas::CanvasTools>,
    canvas_host: Arc<CanvasHost>,
    shared: Arc<Mutex<PanelShared>>,
}

/// The product's per-process wire: assembled once in `main` from the live
/// handles, handing the shell each spec slot and runtime closure.
pub struct Wire {
    resolver: Arc<SkillResolver>,
    telemetry: Arc<SkillTelemetry>,
    skill: Mutex<SkillState>,
    activity: Arc<CanvasActivity>,
    interact_slot: Arc<phi_agent_tui::InteractSlot>,
    kit: Mutex<Option<SessionKit>>,
    reopen_gate: AsyncMutex<()>,
    dirty: Arc<AtomicBool>,
}

impl Wire {
    /// Build the wire. `resolver`/`telemetry` are the skill catalog's shared
    /// instances (the `skill` tool and the `/` path consult the same set);
    /// `activity` is the browser tool-face liveness probe; `interact_slot` is
    /// the act road's interaction slot, armed per session with that session's
    /// `ask_user` picker queue.
    pub fn new(
        resolver: Arc<SkillResolver>,
        telemetry: Arc<SkillTelemetry>,
        activity: Arc<CanvasActivity>,
        interact_slot: Arc<phi_agent_tui::InteractSlot>,
    ) -> Arc<Self> {
        Arc::new(Self {
            resolver,
            telemetry,
            skill: Mutex::new(SkillState::default()),
            activity,
            interact_slot,
            kit: Mutex::new(None),
            reopen_gate: AsyncMutex::new(()),
            dirty: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The prompt fallback: the pristine build-time prompt (everything the
    /// builder injected — catalog, token-budget suffix, CLAUDE.md, memory
    /// index) for the case the runtime holds none.
    pub fn prompt_source(self: &Arc<Self>) -> PromptSource {
        let resolver = Arc::clone(&self.resolver);
        PromptSource {
            fallback: Arc::new(move || agent::compose_system_prompt(&resolver)),
        }
    }

    /// The skill surface: route (slash resolution + scope dispatch), rebake
    /// (resume), and the telemetry/summary feeds.
    pub fn skill_bridge(self: &Arc<Self>) -> SkillBridge {
        let wire = Arc::clone(self);
        let route: SkillRouteFn = Arc::new(move |input: &str| {
            let resolved = wire.resolver.resolve_with_meta(input);
            let Some(r) = resolved else {
                return RouteOutcome::Turn {
                    input: input.to_string(),
                };
            };
            // Canonical name straight from SKILL.md — no string parse.
            wire.telemetry.record_slash(&r.name);
            // Session-scope turns submit the RAW command: `$ARGUMENTS` reach
            // the model through it, and the body is already baked into the
            // prompt below. Turn-scope skills inject the resolved body as an
            // ephemeral user message instead (the shell strips it from memory
            // and persistence at turn end).
            if r.scope == SkillScope::Turn {
                return RouteOutcome::Ephemeral {
                    input: r.body.clone(),
                };
            }
            let mut state = wire.skill.lock().unwrap();
            let entry = ActiveSkillEntry {
                name: r.name.clone(),
                body: r.body.clone(),
            };
            if let Some(existing) = state.active.iter_mut().find(|e| e.name == entry.name) {
                // Re-trigger: refresh the `$ARGUMENTS` substitution.
                existing.body = entry.body;
            } else {
                state.active.push(entry);
            }
            // Single-injection guard (survey P4): the `skill` tool and catalog
            // must see the name the moment its body goes into the prompt.
            sync_active_set(wire.resolver.active(), &state.active);
            // Re-bake from the pristine build-time prompt: append-only, so
            // re-triggers stay idempotent and builder-injected sections
            // (CLAUDE.md / memory / token-budget) survive.
            let base = state
                .bake_base
                .clone()
                .unwrap_or_else(|| agent::compose_system_prompt(&wire.resolver));
            let prompt = append_active_skills(&base, &state.active);
            let prompt_len = prompt.len();
            let mut warnings = Vec::new();
            if let Some(dir) = &state.session_dir
                && let Err(e) = save_active_skills(dir, &state.active)
            {
                warnings.push(format!(
                    "! skill `{}` active now but will NOT survive resume (persist failed): {e}",
                    r.name
                ));
            }
            // Budget note (spec, known limitation): the token-budget core
            // estimated its base overhead from the prompt at build time;
            // baking a body shifts actual usage by this much. The 10% buffer
            // absorbs typical skill bodies.
            tracing::info!(
                skill = %r.name,
                active = state.active.len(),
                baked_chars = prompt_len,
                "session-scope skill baked into system prompt"
            );
            RouteOutcome::SessionBaked {
                input: input.to_string(),
                prompt,
                skill: r.name.clone(),
                warnings,
            }
        });

        let wire = Arc::clone(self);
        let rebake: SkillRebakeFn = Arc::new(move |bake_base: &str| {
            // Covers both process resume (fresh agent, restored history) and
            // session switches (fresh agent loop): reload the session's active
            // list and re-bake it into the system prompt.
            let mut state = wire.skill.lock().unwrap();
            state.bake_base = Some(bake_base.to_string());
            let dir = state.session_dir.clone()?;
            state.active = load_active_skills(&dir);
            sync_active_set(wire.resolver.active(), &state.active);
            if state.active.is_empty() {
                None
            } else {
                Some(append_active_skills(bake_base, &state.active))
            }
        });

        let resolver = Arc::clone(&self.resolver);
        let summaries = Arc::new(move || resolver.skill_summaries());
        let telemetry = Arc::clone(&self.telemetry);
        let turn_custom = Arc::new(move || {
            // Slash/skill usage snapshot for the turn record; `None` when the
            // turn used no skill (the shell then sets no custom).
            let snap = telemetry.snapshot_and_reset();
            (!snap.as_object().is_none_or(|m| m.is_empty())).then_some(snap)
        });
        let telemetry = Arc::clone(&self.telemetry);
        let session_custom = Arc::new(move || {
            // Model-triggered events (recorded during the turn) can't go into
            // turn.custom — clear per-turn state and report the session view.
            telemetry.snapshot_and_reset();
            telemetry.session_snapshot()
        });

        SkillBridge {
            rebake,
            route,
            summaries,
            turn_custom,
            session_custom,
        }
    }

    /// Per-session boot: the canvas tools + `ask_user` (registered root-only
    /// by the shell — post-build registration never reaches a child's tool
    /// face) and the two queues the run loop services. Re-armed every session
    /// iteration: a `/resume` switch must not carry one session's queues into
    /// the next.
    pub fn session_boot(self: &Arc<Self>) -> SessionBootFn {
        let wire = Arc::clone(self);
        Arc::new(move |session_id: &str, base_dir: &Path| {
            let wire = Arc::clone(&wire);
            let session_id = session_id.to_string();
            let base_dir = base_dir.to_path_buf();
            Box::pin(async move {
                // Every session-keyed store (rotation archive, notes, code
                // journal, captures) must serve THIS session: a `/resume`
                // switch re-points the shared scope here, once per session
                // iteration, and the stores resolve their paths at use time
                // (2026-11-04: frozen copies kept writing the abandoned
                // bootstrap session's trees after a switch).
                crate::shell::context_rotation::repoint_current_session(&session_id);
                // Design canvas (M3): per-session canvas server + the three
                // canvas tools. Per-session because the store is
                // `~/.phimint/canvas/<session>/` — a switch must not write one
                // session's screens into another's directory. The launcher is
                // the host's `y` consent gate (1C): `canvas_open` may only
                // ask, and `canvas_push`'s implicit open never launches a
                // browser.
                let (canvas_prompts_tx, canvas_prompts_rx) =
                    mpsc::unbounded_channel::<ApprovalItem>();
                let canvas_host = CanvasHost::new(canvas_prompts_tx);
                let canvas_tools = new_canvas_tools(
                    &base_dir,
                    &session_id,
                    Arc::new(ConsentLauncher(canvas_host.clone())),
                );
                // Browser tool-face loading: the tool-face middleware asks
                // this probe before every LLM call, so bind this session's
                // canvas here — open / close / crash need no separate
                // notification (it reads the live canvas each turn).
                wire.activity.bind(canvas_tools.clone()).await;
                let mut tools: Vec<Arc<dyn Tool>> = canvas_tool_adapters(canvas_tools.clone())
                    .into_iter()
                    .map(|tool| Arc::new(tool) as Arc<dyn Tool>)
                    .collect();
                // `ask_user` — the text half of the design routing: a concept
                // choice answered in a terminal picker instead of on the
                // canvas. Root-only like `canvas_*`. The run loop services the
                // receiver one modal picker at a time.
                let (ask_user_tool, ask_rx) = crate::tools::ask_user::new_ask_user_tool();
                wire.interact_slot.arm(ask_user_tool.ask_sender());
                tools.push(ask_user_tool);
                // Row 18 / resume: a session that ever had a canvas comes
                // back closed and reopenable (Ctrl+B) — never auto-relaunched.
                // Sessions with no canvas history show no status at all.
                if base_dir
                    .join("canvas")
                    .join(&session_id)
                    .join("server-info")
                    .exists()
                {
                    canvas_host.note_closed();
                }
                // Session-scope skill bookkeeping: where this session's
                // active list lives (loaded at agent-loop entry).
                {
                    let mut skill = wire.skill.lock().unwrap();
                    skill.session_dir = Some(base_dir.join("sessions").join(&session_id));
                    skill.bake_base = None;
                }
                let shared = Arc::new(Mutex::new(PanelShared::new()));
                *wire.kit.lock().unwrap() = Some(SessionKit {
                    canvas_tools,
                    canvas_host,
                    shared,
                });
                SessionBoot {
                    tools,
                    ask_rx: Some(ask_rx),
                    consent_rx: Some(canvas_prompts_rx),
                }
            })
        })
    }

    /// Per-session teardown (Quit and `/resume` alike): the canvas closes
    /// gracefully first (open tabs land on the farewell page) and only then
    /// stops serving — a bare `shutdown` is exactly the crash shape
    /// `is_failed` reports. The tool-face probe must not keep pointing at a
    /// dead session's canvas while the next one boots.
    pub fn session_end(self: &Arc<Self>) -> SessionEndFn {
        let wire = Arc::clone(self);
        Arc::new(move || {
            let wire = Arc::clone(&wire);
            Box::pin(async move {
                let kit = wire.kit.lock().unwrap().take();
                if let Some(kit) = kit
                    && let Some(canvas) = kit.canvas_tools.canvas().await
                {
                    tracing::info!("tui: session over -> canvas close(SessionEnded) + shutdown");
                    let _ = canvas.close(phi_canvas::CloseReason::SessionEnded).await;
                    canvas.shutdown().await;
                }
                wire.activity.unbind().await;
            })
        })
    }

    /// The panel tick (design §3): canvas subscriptions, lifecycle polls,
    /// click drains, root-turn edges, consent — all product state behind this
    /// one closure; the shell only executes the outcome. Called once per loop
    /// iteration with that tick's facts. T16: the shell honors a synthetic
    /// turn only when no turn is active.
    pub fn tick(self: &Arc<Self>) -> PanelTick {
        let wire = Arc::clone(self);
        Arc::new(move |facts: &TickFacts| {
            let empty = TickOut {
                notes: Vec::new(),
                synthetic: None,
                status: None,
                dirty: false,
            };
            let kit = wire.kit.lock().unwrap().clone();
            let Some(kit) = kit else {
                return empty;
            };
            let host = kit.canvas_host.clone();

            {
                let mut shared = kit.shared.lock().unwrap();
                if facts.turn_fell {
                    shared.turn_fell = true;
                }
                // Lazy live-handle poll: a rebind replaces the handle, and the
                // rest of the cluster reads it next tick. De-duped and
                // throttled; the task swaps the cache and arms the mailbox
                // reset — never the mailbox itself (shell-thread only).
                let now = Instant::now();
                if !shared.check_in_flight && now.duration_since(shared.last_check) >= HANDLE_POLL {
                    shared.check_in_flight = true;
                    shared.last_check = now;
                    let tools = kit.canvas_tools.clone();
                    let shared = kit.shared.clone();
                    tokio::spawn(async move { poll_live_handle(tools, shared).await });
                }
            }

            // 1C lifecycle poll: the phase the status bar reports. The bind is
            // noted before anything acts on its screens, so the host's
            // bookkeeping is reset to the (possibly restored) history the
            // replay below refills — never a mix of two canvases.
            let live = kit.shared.lock().unwrap().live.clone();
            if let Some(canvas) = live.as_ref() {
                if canvas.is_failed() {
                    host.note_failed();
                } else if canvas.close_reason().is_some() {
                    host.note_closed();
                } else {
                    host.note_canvas(&canvas.url(), canvas.port());
                    // An implicit `canvas_push` binds the server without
                    // consulting the launcher (M3) — somebody still has to ask
                    // the user about the browser. Idempotent per URL: an
                    // explicit `canvas_open` already asked.
                    let _ = host.request_model_launch(&canvas.url());
                }
                // Push replay. The broadcast subscription would race the
                // tool's first push (it holds its state lock across `push`),
                // so the host polls the full history — recording is idempotent
                // by seq.
                host.replay_pushes(canvas);
            }

            // Design canvas clicks (M2): queue every event into the mailbox —
            // never injected into an active turn — then drain at the boundary.
            // Idle is ≤ one coalesce window away (M2a). A running turn holds
            // the debt until it settles.
            let mut drain_started = false;
            let mut synthetic = None;
            let mut submitted = None;
            let facts_out;
            {
                let mut shared = kit.shared.lock().unwrap();
                if shared.needs_mailbox_reset {
                    // A rebind rebuilt canvas state from the session store
                    // (seq continues, the last screen comes back), but the
                    // mailbox's per-screen bookkeeping belongs to the old
                    // Canvas and its pending events died with the old
                    // subscription — start it fresh.
                    shared.mailbox = ClickMailbox::new();
                    shared.needs_mailbox_reset = false;
                }
                let mut drained = Vec::new();
                if let Some(rx) = shared.events.as_mut() {
                    while let Ok(event) = rx.try_recv() {
                        drained.push(event);
                    }
                }
                for event in drained {
                    let screen = shared
                        .subscribed
                        .as_ref()
                        .and_then(|canvas| canvas.screen_envelope(event.screen));
                    shared
                        .mailbox
                        .push_event(Instant::now(), &event, screen.as_ref());
                }
                if let Some(text) =
                    shared
                        .mailbox
                        .take_submit(Instant::now(), facts.running, facts.composer_holds)
                {
                    tracing::info!("tui: click mailbox -> user answer");
                    // M2c: a click IS a real user answer — the shell commits
                    // user-turn state at its send site, exactly like the Enter
                    // path. The pick lines are the waiting summary's decided
                    // entries (M2d).
                    drain_started = true;
                    synthetic = Some(Synthetic {
                        text: text.clone(),
                        user: true,
                    });
                    submitted = Some(text);
                }
                facts_out = AnswerFacts {
                    answered: shared.mailbox.answered_screens().into_iter().collect(),
                    pending: shared.mailbox.pending_screens().into_iter().collect(),
                    queued: shared.mailbox.queued_screens().into_iter().collect(),
                    queued_labels: shared.mailbox.queued_labels(),
                };
            }
            // Merged or drained, any pick lines are now sent — they belong in
            // the waiting summary's decided set (host state, outside the lock).
            if let Some(text) = &submitted {
                host.on_answered(&canvas_host::canvas_answer_lines(text));
            }

            // Root-turn falling edge → the waiting screen (1C row 15). The
            // policy lives in canvas_host; this block only executes the push.
            let turn_fell = std::mem::take(&mut kit.shared.lock().unwrap().turn_fell);
            if turn_fell && let Some(envelope) = host.on_root_turn_end(&facts_out, drain_started) {
                // The push is async (it mints a receipt); the bookkeeping
                // is the host's own, and the face follows on the next tick.
                let tools = kit.canvas_tools.clone();
                let host = host.clone();
                let dirty = Arc::clone(&wire.dirty);
                tokio::spawn(async move {
                    if let Some(canvas) = tools.canvas().await
                        && let Ok(receipt) = canvas.push(envelope).await
                    {
                        host.on_push(&receipt);
                        host.on_waiting_push(receipt.screen);
                        dirty.store(true, Ordering::SeqCst);
                    }
                });
            }

            // Consent gate resolution (M3): the user's answer reaches the host
            // here, and the tool's cached launch state follows it so a later
            // `canvas_open` cannot report a stale unanswered wait. A yes that
            // lands after the canvas died recovers like Ctrl+B.
            match host.poll_consent() {
                Some(ConsentOutcome::Decision(decision)) => {
                    let tools = kit.canvas_tools.clone();
                    let dirty = Arc::clone(&wire.dirty);
                    tokio::spawn(async move {
                        tools.set_launch_decision(decision).await;
                        dirty.store(true, Ordering::SeqCst);
                    });
                }
                Some(ConsentOutcome::ReopenAndLaunch) => {
                    let wire = Arc::clone(&wire);
                    let kit = kit.clone();
                    tokio::spawn(async move { wire.reopen_panel(&kit).await });
                }
                None => {}
            }

            // Host notes plus the spawned bits' notes (a failed reopen), all
            // one-shots into the transcript.
            let mut notes = Vec::new();
            while let Some(note) = host.take_note() {
                notes.push(note);
            }
            notes.append(&mut kit.shared.lock().unwrap().notes);

            // Status-bar face (1C row 13). Cheap enough to recompute per tick;
            // only a real change redraws (the shell compares).
            let status = host.status_line(&facts_out);
            let dirty = wire.dirty.swap(false, Ordering::SeqCst);
            TickOut {
                notes,
                synthetic,
                status,
                dirty,
            }
        })
    }

    /// The Enter-boundary rewrite (M2e): pending panel picks merge behind the
    /// typed text as one user message (typed first, blank line, then the pick
    /// lines); the waiting summary is settled here. A `/`-leading submit skips
    /// the merge and keeps the picks queued (M2c slash guard — inside
    /// `take_for_enter`).
    pub fn on_submit(self: &Arc<Self>) -> SubmitHook {
        let wire = Arc::clone(self);
        Arc::new(move |typed: &str| {
            let kit = wire.kit.lock().unwrap().clone()?;
            let merged = {
                let mut shared = kit.shared.lock().unwrap();
                if shared.needs_mailbox_reset {
                    shared.mailbox = ClickMailbox::new();
                    shared.needs_mailbox_reset = false;
                }
                shared.mailbox.take_for_enter(typed)
            };
            let text = merged.unwrap_or_else(|| typed.to_string());
            // Merged or not, any pick lines are now sent — they belong in the
            // waiting summary's decided set.
            kit.canvas_host
                .on_answered(&canvas_host::canvas_answer_lines(&text));
            Some(text)
        })
    }

    /// The panel-reopen key (Ctrl+B, its binding is shell keymap data): the
    /// product rebinds its server — async because a rebind may mint a new URL
    /// — and settles its own consent and notes (a failure note rides the next
    /// tick's `TickOut::notes`). Awaited inline so the redraw sees the result.
    pub fn on_reopen(self: &Arc<Self>) -> ReopenHook {
        let wire = Arc::clone(self);
        Arc::new(move || {
            let wire = Arc::clone(&wire);
            Box::pin(async move {
                let kit = wire.kit.lock().unwrap().clone();
                if let Some(kit) = kit {
                    wire.reopen_panel(&kit).await;
                }
            })
        })
    }

    /// Recover a closed/crashed canvas (1C row 16): rebind a fresh server
    /// (new port ⇒ new URL), note it, and open the browser directly. The
    /// caller's path is already the user's consent — Ctrl+B's own key, or a
    /// `y` that landed after the canvas died. Serialized through the reopen
    /// gate: the key and the consent path can both land here.
    async fn reopen_panel(&self, kit: &SessionKit) {
        let _gate = self.reopen_gate.lock().await;
        match kit.canvas_tools.rebind().await {
            Ok(url) => {
                let canvas = kit.canvas_tools.canvas().await;
                let port = canvas.as_ref().map(|canvas| canvas.port()).unwrap_or(0);
                // Note the URL before granting: a later note for a *different*
                // URL resets consent, and the grant must not be clobbered by
                // the tick.
                kit.canvas_host.note_canvas(&url, port);
                // The bind cleared the host's history — refill it from the
                // store before launching, or the launch target is computed
                // over the cleared list and lands on the root instead of the
                // design.
                if let Some(canvas) = &canvas {
                    kit.canvas_host.replay_pushes(canvas);
                }
                kit.canvas_host.request_user_launch(&url);
                kit.canvas_tools
                    .set_launch_decision(phi_canvas::LaunchDecision::Launched)
                    .await;
                // A rebind is a new Canvas: force the next tick's handle poll
                // so the event subscription follows it immediately.
                let mut shared = kit.shared.lock().unwrap();
                shared.last_check = Instant::now()
                    .checked_sub(HANDLE_POLL)
                    .unwrap_or_else(Instant::now);
            }
            Err(err) => {
                tracing::warn!(error = %err, "canvas reopen failed");
                kit.shared
                    .lock()
                    .unwrap()
                    .notes
                    .push(format!("! Canvas reopen failed: {err}"));
            }
        }
    }
}

/// The async half of the handle poll: ask the tools layer for the live canvas
/// and (re)subscribe whenever the handle is not the one whose events we
/// forward (port 0 ⇒ a rebind is a new Canvas).
async fn poll_live_handle(tools: Arc<phi_canvas::CanvasTools>, shared: Arc<Mutex<PanelShared>>) {
    let live = tools.canvas().await;
    let mut state = shared.lock().unwrap();
    state.check_in_flight = false;
    let needs_sub = match (&live, &state.subscribed) {
        (Some(current), Some(subscribed)) => !Arc::ptr_eq(current, subscribed),
        (Some(_), None) => true,
        (None, _) => false,
    };
    if needs_sub {
        let canvas = live.clone().expect("Some whenever needs_sub");
        let (tx, rx) = mpsc::unbounded_channel();
        let mut sub = canvas.subscribe();
        tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            break; // UI loop gone
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "canvas click events consumer lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        state.events = Some(rx);
        state.subscribed = live.clone();
        // The mailbox's per-screen bookkeeping belongs to the old Canvas —
        // the tick rebuilds it before draining (shell-thread only).
        state.needs_mailbox_reset = true;
        // PTY harness seam (row 16): kill the first server task without a
        // close to exercise the crash face. One-shot per process — recovery
        // (Ctrl+B) re-subscribes, and a seam re-armed there would kill the
        // recovered canvas again every interval.
        if let Ok(ms) = std::env::var("PHIMINT_CANVAS_CRASH_AFTER_MS")
            && let Ok(ms) = ms.parse::<u64>()
        {
            static CRASH_SEAM_USED: AtomicBool = AtomicBool::new(false);
            if !CRASH_SEAM_USED.swap(true, Ordering::SeqCst) {
                let canvas = live.clone().expect("Some whenever needs_sub");
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    canvas.shutdown().await;
                });
            }
        }
    }
    state.live = live;
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
