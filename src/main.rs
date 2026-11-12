//! phimint — an AI coding agent built on phi-agent.
//!
//! The UI is a ratatui TUI with a fixed input bar at the bottom (the cursor
//! stays in the bar while output scrolls above it).

// Modules live in the lib crate (src/lib.rs) so tests/ can exercise them.
use phimint::{
    agent, approval, banner,
    cli::{Cli, Commands, ConfigAction},
    external,
    model::{auth_watch, bootstrap, config, config_cmd, router, store, title},
    session::{birthplace, history},
    shell::{commands, file_face, skills, wire},
    tools,
};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use phi_agent::{SessionContext, load_session_messages, resolve_session};

/// Where the sidecar transport puts its provider diagnostics.
///
/// The transport (phi-wire-client) reads this variable: set, it appends the
/// diagnostics to the named file; unset, it forwards them to **stderr** —
/// which in a TUI is the alternate screen. The text then lands on the
/// user's display and the renderer never repaints those cells, so the
/// input line grows garbage the user cannot clear with a redraw.
const PROVIDER_LOG_ENV: &str = "MAI_UI_PROVIDER_LOG";

fn main() -> Result<()> {
    // The TUI owns the terminal while it is up: nothing may write to
    // stderr. Point the sidecar transport's diagnostics at a log file
    // before the runtime starts any thread — `set_var` is not thread-safe,
    // so this is the one place it can be set soundly.
    install_provider_log(&sessions_base_dir());

    // A previous run may have died without restoring the terminal (raw mode,
    // alternate screen, mouse reporting): the shell the user is looking at
    // cannot echo and cannot scroll. Repair it, and make sure this run puts
    // the terminal back even if it is killed.
    phimint::ops::terminal_restore::heal_terminal();
    phimint::ops::terminal_restore::install_signal_restore();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

/// Name the file the sidecar transport puts its diagnostics in, and return
/// the path it named.
///
/// The transport reads the variable when it spawns a sidecar, so one call
/// here covers every sidecar this process starts.
fn install_provider_log(base_dir: &Path) -> PathBuf {
    let path = provider_log_path(base_dir);
    // A state dir created now is owner-only (0700): it holds the config's
    // API key and the input history. An existing dir keeps its own mode —
    // that is the user's choice, not ours to override.
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir.mode(0o700);
    }
    if let Err(e) = dir.create(base_dir) {
        eprintln!("warning: cannot create {}: {e}", base_dir.display());
    }
    // SAFETY: called before the runtime is built — no thread exists yet, so
    // nothing can read the environment concurrently.
    unsafe { std::env::set_var(PROVIDER_LOG_ENV, &path) };
    path
}

/// The provider diagnostics file: one append-only stream per install,
/// next to the other base-level state (`approvals.json`).
fn provider_log_path(base_dir: &Path) -> PathBuf {
    base_dir.join("provider.log")
}

/// The runtime body of [`main`] — everything that needs the async runtime.
async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Subcommands run without a workspace and without starting the TUI, so
    // they dispatch before the `cd` below and before any config is loaded.
    match cli.command {
        Some(Commands::Update { check }) => {
            return run_update_command(check, cli.config.as_deref()).await;
        }
        Some(Commands::Uninstall { keep_data, yes }) => {
            return run_uninstall_command(keep_data, yes);
        }
        Some(Commands::Init { if_missing }) => {
            return run_init_command(cli.config.as_deref(), if_missing);
        }
        Some(Commands::Config { action }) => {
            return run_config_command(cli.config.as_deref(), action);
        }
        None => {}
    }

    // Resolve the workspace to an absolute path and `cd` into it, so that both
    // the file tools (workspace-relative) and the shell tool (process cwd)
    // operate on the same tree.
    let workspace = std::fs::canonicalize(&cli.workspace)
        .with_context(|| format!("workspace not found: {}", cli.workspace.display()))?;
    std::env::set_current_dir(&workspace)?;

    // Load model configuration: CLI > JSON file. `loaded_from` remembers the
    // file behind it — a `/model` pick is persisted there (model::persist).
    // The first-run gate (decision 1) shapes the whole block: a parse failure
    // or no usable key anywhere stops here with a message and a non-zero
    // exit, before anything touches the session.
    let env_key = std::env::var("PHIMINT_API_KEY").ok();
    let (mut model_config, loaded_from) = if let Some(config_path) = &cli.config {
        // CLI --config flag specified
        let path = PathBuf::from(config_path);
        match bootstrap::gate(
            config::ModelConfig::from_file(&path).map(Some),
            cli.api_key.as_deref(),
            env_key.as_deref(),
            &path,
        ) {
            bootstrap::Boot::Run(config) => (*config, Some(path)),
            bootstrap::Boot::Stop(msg) => stop_with(msg),
        }
    } else {
        let config_target = config_cmd::command_target(None);
        match bootstrap::gate(
            config::ModelConfig::from_default_location(),
            cli.api_key.as_deref(),
            env_key.as_deref(),
            &config_target,
        ) {
            bootstrap::Boot::Run(config) => {
                tracing::info!("loaded model config from JSON file");
                (*config, config::ModelConfig::resolve_path())
            }
            bootstrap::Boot::Stop(msg) => stop_with(msg),
        }
    };

    // Apply CLI overrides. `--model` is a session-scoped selection: it writes
    // the `model` pointer slot (CLI > file pointer > configured `main`) and is
    // never persisted — the file keeps its own pointer for the next boot, and
    // the configured `main` definition stays intact (the picker still shows
    // its row). `--lite_model`/`--advanced_model` keep their own semantics:
    // session-scoped tier DEFINITION overrides, also never persisted.
    model_config.apply_cli_model(cli.model.as_deref());
    if let Some(ref lite) = cli.lite_model {
        model_config.lite = Some(config::TierConfig::Simple(lite.clone()));
    }
    if let Some(ref advanced) = cli.advanced_model {
        model_config.advanced = Some(config::TierConfig::Simple(advanced.clone()));
    }
    if let Some(ref url) = cli.base_url {
        model_config.base_url = Some(url.clone());
    }
    // The key in force: CLI `--api-key` > PHIMINT_API_KEY > the file's
    // api_key (decision 6). The env value is used in memory and never written
    // to the file — CI / batch machines keep their key off disk. The override
    // lands on the top-level member so every tier fallback sees it; the MAIN
    // provider's key is resolved once more below (bootstrap::key_in_force),
    // where a pinned tier key must not eat the override.
    let key_in_force = bootstrap::effective_api_key(
        cli.api_key.as_deref(),
        env_key.as_deref(),
        model_config.api_key.as_deref(),
    )
    .map(str::to_string);
    if let Some(ref key) = key_in_force {
        model_config.api_key = Some(key.clone());
    }
    if let Some(ref proto) = cli.protocol {
        model_config.protocol = Some(proto.clone());
    }

    // Validate config
    model_config.validate()?;

    // Create main provider. The key is the ONE resolution the gate already
    // judged (bootstrap::key_in_force) — CLI > env > the tier's pinned key >
    // the top-level member — NOT merely tier_config's tuple, whose pin would
    // silently eat a `--api-key`. Gate and provider on the same value is
    // what keeps "configured" from meaning two different things at boot.
    let (main_model, main_url, _pinned_key, main_proto) = model_config.tier_config("main").unwrap();
    let main_key =
        bootstrap::key_in_force(&model_config, cli.api_key.as_deref(), env_key.as_deref());
    let llm_config = phi_agent::llm_trait::config::LlmConfig {
        protocol: if main_proto.is_empty() {
            None
        } else {
            main_proto.parse::<phi_agent::llm_trait::Protocol>().ok()
        },
        api_key: main_key.clone(),
        model: main_model.clone(),
        base_url: main_url.clone(),
        options: {
            let mut opts = std::collections::HashMap::new();
            opts.insert("max_tokens".to_string(), serde_json::json!("24576")); // 24K output
            opts
        },
    };
    // The runtime's auth watch (decision 2): which file holds the key, which
    // key is in force (masked, and where it came from) and which endpoint is
    // live — so a request the endpoint rejects ends in a line that points at
    // the file the user has to edit.
    let notice_path = loaded_from
        .clone()
        .unwrap_or_else(|| config_cmd::command_target(cli.config.as_deref()));
    let key_source =
        bootstrap::key_source_in_force(cli.api_key.as_deref(), env_key.as_deref(), &main_key);
    let auth_context = auth_watch::boot_context(
        &notice_path,
        Some(main_key.as_str()),
        key_source,
        &model_config
            .base_url
            .clone()
            .unwrap_or_else(|| main_url.clone()),
    );
    let llm_client: Arc<dyn phi_agent::llm_trait::LlmProvider> = auth_watch::install(
        auth_context,
        phi_agent::create_provider(&llm_config).context("Failed to create LLM provider")?,
        &main_model,
        &main_url,
        &main_key,
    );

    // Extract update config before model_config is moved into the store.
    let update_config = model_config.update.clone();
    let tips_config = model_config.ui.tips.clone();
    let exit_dump_mode = model_config.ui.exit_dump;
    // The code road (M3 T10d layer ③): `code_exec.mode` decides at the
    // assembly point what code wiring the runtime gets. `resolve_road`
    // (file_face) probes for the Python 3 the code road needs first: missing
    // interpreter → the code road degrades to the no-engine shape (no dead
    // `code_cell`; the file triad is on the menu either way) and one notice
    // explains, instead of the engine dying at build looking for a worker.
    let code_exec_mode = file_face::resolve_road(model_config.code_exec.mode);

    let popup_style = model_config
        .ui
        .popup
        .to_style()
        .context("invalid ui.popup config")?;

    // Model store and router for multi-model support
    let mut model_store_inner = store::ModelStore::new(model_config, llm_client.clone());
    model_store_inner.set_config_path(loaded_from);
    let model_store = Arc::new(tokio::sync::Mutex::new(model_store_inner));
    let router = Arc::new(router::PhimintRouter::new());

    // Session + logging. Sessions live under `~/.phimint/sessions/<id>/`; the
    // human-readable tracing log is `session.log`, and each turn's structured
    // event stream (tool calls, text deltas, …) is `turn_NNN.jsonl`.
    let base_dir = sessions_base_dir();

    // Approval is the live gate (see approval.rs): a policy (the gate) + a
    // handler (the decision) sharing one mode switch. The gate is built for
    // every mode — `--approval` only sets the switch's initial value, and
    // Shift+Tab flips it at runtime (status bar shows the current mode). The
    // queue receiver goes to the UI, which renders one popup at a time.
    // The ledger (standing tiers for external tools) lives next to the
    // sessions: `~/.phimint/approvals.json`.
    let (approval, policy, approval_rx, approval_mode) =
        approval::build_live_approval_with_workspace(
            &cli.approval,
            Some(base_dir.join("approvals.json")),
            &workspace,
        );
    // tool-socket P3: ExternalState's ledger gate is this same gate (the
    // install confirmation surface reuses the same TUI approval queue, but
    // always asks a human and ignores the tier). `agent::build` takes the
    // approval away, so keep a copy of the Arc here.
    let external_ledger = Arc::clone(&approval);
    // The act road's interaction slot (M3 T10d): mounted at build time, armed
    // per session inside the shell's boot closure with that session's
    // `ask_user` picker queue (shell_wire).
    let interact_slot = Arc::new(phi_agent_tui::InteractSlot::new(
        Arc::clone(&approval) as Arc<dyn phi_agent::ApprovalHandler>
    ));

    // Cleanup expired session data (history, notes, sessions) at startup.
    cleanup_expired_data(&base_dir, cli.session_retention_days);

    // Materialize the bundled design-canvas skill before the agent scans
    // skill dirs (1C task 9). Non-fatal: a failed write just costs the model
    // the routing skill for this run.
    if let Err(e) = skills::materialize_builtin_skills(&skills::builtin_skills_dir()) {
        tracing::warn!(error = %e, "failed to materialize bundled skills");
    }

    // Sessions created before the workspace tag existed have no home folder,
    // which makes the resume picker's folder view look like the history is
    // gone. File them once, from the repos their paths touched. Declining to
    // guess is recorded too, so this is not re-read on every launch.
    let (filed, unfiled) = birthplace::recover_missing_birthplaces(&base_dir);
    if filed > 0 {
        tracing::info!(
            filed,
            unfiled,
            "filed legacy sessions into their home workspace"
        );
    }

    // Process any pending title generation marker from a previous exit.
    // Runs in the main tokio runtime — the LLM provider is healthy here.
    {
        let pending_provider = {
            let mut store = model_store.lock().await;
            store.get_or_create_provider("lite").ok()
        };
        if let Some(p) = pending_provider {
            let sessions_dir = base_dir.join("sessions");
            if let Err(e) = title::process_pending_title(p, &sessions_dir).await {
                tracing::warn!(error = %e, "failed to process pending title");
            }
        }
    }

    // Detect terminal color scheme once for the whole process: the `--resume`
    // picker below and the startup banner palette both resolve against it.
    // `--color-scheme` flag takes priority; `auto` reads `$COLORFGBG` (set by
    // iTerm2, kitty, etc.) where the background digit is `7` → light. The OSC
    // 11 stdin probe is intentionally skipped: its termios manipulation
    // interferes with crossterm's raw-mode setup on macOS. Use
    // `--color-scheme light` or `dark` to override when COLORFGBG is absent.
    let color_fgbg = std::env::var("COLORFGBG").ok();
    let scheme = banner::resolve_scheme(&cli.color_scheme, color_fgbg.as_deref(), None);
    let theme = phi_agent_tui::Theme::for_scheme(scheme);

    // The resume seam's data face (product rows + the workspace verdict),
    // built once: the startup picker and the shell's `/resume` share it.
    let session_io = wire::session_io();

    // Resolve session: --resume shows a picker; otherwise normal resolve.
    // Session is resolved BEFORE agent::build so that token_budget_opts
    // (which needs session_id for history/notes stores) is available.
    let session_ctx = if cli.resume {
        let picked_dir = phi_agent_tui::picker::show_picker(&base_dir, None, theme, &session_io)?
            .context("No session selected")?;
        let picked_id = picked_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // resolve_session acquires the lock and refreshes last_active_at.
        resolve_session(Some(&picked_id), &base_dir)?
    } else {
        resolve_session(cli.session.as_deref(), &base_dir)?
    };
    let log_sinks = init_logging(&session_ctx, &cli.log_level).await?;

    // Token-budget context management is always on (window rotation +
    // history/notes tools instead of LLM summarization). `--token-budget`
    // overrides the work-room budget; the default comes from the flag.
    let token_budget_opts = agent::TokenBudgetOptions {
        budget: cli.token_budget,
        retention_days: cli.session_retention_days,
        base_dir: base_dir.clone(),
        session_id: session_ctx.session_id.clone(),
    };

    // The verify gate's write probe (design §5.4): in `deny` every write tool is
    // refused, so nothing can ever dirty the workspace and the gate would only
    // ever nag a read-only run. A **probe** rather than a snapshot because the
    // mode is a live switch — Shift+Tab flips it mid-session.
    let gate_writes = {
        let switch = Arc::clone(&approval_mode);
        Arc::new(move || switch.get() != phi_agent_tui::RuntimeApprovalMode::Deny)
            as agent::gate::WritesProbe
    };

    let (agent, skill_resolver, skill_telemetry, bg_registry, canvas_activity) =
        agent::build_with_code_exec(
            code_exec_mode,
            Some(interact_slot.clone()),
            llm_client,
            approval,
            policy,
            cli.shell_timeout_ms,
            workspace.clone(),
            cli.thinking_budget,
            &cli.reasoning_effort,
            llm_config.model.clone(),
            skills::default_skill_dirs(),
            Some(token_budget_opts),
            Some(model_store.clone()),
            Some(gate_writes),
        )
        .await?;
    // Shared ownership from here on: the shell takes `Arc<PhiAgent>` (T5d —
    // the product boot work below registers tools against the same instance).
    let agent = Arc::new(agent);

    // The store's serving-name resolver: the engine's provider slot names the
    // model that actually serves a turn — the tier table moves the moment a
    // pick is applied, while a staged engine swap lands at the turn boundary,
    // so only the live slot tells the identity stamp and `current_model` the
    // truth mid-swap. Carried as a closure: the engine's provider-handle type
    // is a phi-agent type this product never names.
    {
        let handle = agent.runtime().llm_engine().provider_handle();
        let mut store = model_store.lock().await;
        store.bind_serving_name(Arc::new(move || Some(handle.current().info().model)));
    }

    // Create or resume the runtime session.
    let (session, resume_messages) = if cli.resume {
        // Heal first: a session that rotated and then died may have its whole
        // conversation in the history archive and no `messages.jsonl` to
        // resume from. Without this the picker lists it (after the listing
        // fix) but the model comes back with no memory at all.
        history::restore_messages_jsonl(
            &base_dir,
            &session_ctx.session_id,
            &session_ctx.session_dir,
        );
        let messages = load_session_messages(&session_ctx.session_dir)
            .context("Failed to load session messages")?;
        let replay = messages.clone(); // kept for TUI transcript replay
        let session = agent
            .resume_session(messages)
            .await
            .context("Failed to resume session")?;
        (session, Some(replay))
    } else {
        (agent.create_session().await, None)
    };

    let show_banner = cli.banner != "off";
    let version = env!("CARGO_PKG_VERSION");

    // Brand wordmark (T5): the generator stays product code — it captures the
    // boot-time workspace and version, and answers with rows for the session
    // log path and terminal width the shell hands it at render time.
    let wordmark = show_banner.then(|| {
        let workspace = workspace.clone();
        let version = version.to_string();
        phi_agent_tui::Wordmark {
            build: Box::new(move |log_path, width| {
                // The brand layer compiles its roles into literal colors here
                // (design §4.3, Issue 2.2) — the seam never sees the roles.
                crate::banner::brand_rows(
                    crate::banner::build(&workspace, log_path, &version, width),
                    scheme,
                )
            }),
        }
    });

    // tool-socket P3: the socket board (five config surfaces merged). Loading
    // the files is sync; the startup merge (register/connect) is the boot
    // work right below — the live agent is the ToolHost.
    let external = Arc::new(tokio::sync::Mutex::new(
        external::state::ExternalState::load(&base_dir, &workspace, external_ledger),
    ));

    // T5d: the socket board's startup merge (script tools → plugins →
    // enabled connections) is product boot work (T1 row 19: moved out of the
    // shell). Once per process; a session switch never redoes it — the tool
    // table is global. Graceful load errors ride `startup_notices` into the
    // transcript, never silently dropped.
    let mut startup_notices = {
        let host = external::PhiToolHost {
            agent: agent.clone(),
        };
        let mut state = external.lock().await;
        state.start(&host).await
    };
    // The selection pointer is invisible otherwise: a `model` key in
    // config.json quietly overrides `main`, so name the pointer in force at
    // boot (finding: the new key was undocumented and silent).
    {
        let store = model_store.lock().await;
        if store.config().model.is_some() {
            startup_notices.push(format!(
                "model pointer in force: {} (config.json `model` overrides `main`)",
                store.tier_model("main")
            ));
        }
    }
    // `add_tool`: the human-review entry point for tools the agent writes on
    // the user's behalf. **post-build registration = root-only** — a
    // multi-agent's business_tools is a build-time snapshot, so a child
    // agent's tool face never sees it (a child must not grow tools for
    // itself). The registration hook holds a Weak to avoid a cycle.
    {
        let registry = agent.runtime().tools_mut();
        let add_tool =
            tools::add_tool::AddTool::new(Arc::clone(&external), Arc::downgrade(&registry));
        registry.write().await.register_arc(Arc::new(add_tool));
    }

    // The consent bridge's cancel token is the *run's* live one — the runtime
    // resets its token before every turn, so the slot takes a source, not a
    // token. With it, a desktop action waiting on the approval popup is
    // released when the turn is cancelled (Ctrl+C) instead of hanging.
    {
        let runtime = agent.runtime().clone();
        interact_slot.arm_cancel(Arc::new(move || runtime.cancel_token()));
    }

    // T5d: the product's slash-command table — the model-tier face and the
    // tool-socket face as CommandSpec rows the shell dispatches on the turn
    // pump before skill routing (the rows also feed the `/` picker). The pick
    // queue is the bare `/model` modal's road: the command pushes candidate
    // rows and parks on the human's answer.
    let (pick_tx, pick_rx) = tokio::sync::mpsc::unbounded_channel();
    let commands = commands::command_table(
        agent.clone(),
        model_store.clone(),
        router.clone(),
        Arc::clone(&external),
        Some(pick_tx),
    );

    // T7c: the product side of the shell seam — one wire handing the shell
    // each fn slot (update check, tips fetch, runtime notices, session I/O,
    // skill routing, title generation, and the design-canvas panel cluster).
    // The wire owns the skill catalog and the per-session canvas kit; the
    // shell only executes the outcomes.
    let wire = wire::Wire::new(
        skill_resolver,
        skill_telemetry,
        canvas_activity,
        interact_slot,
    );

    // T2 seam cut: two-stage wire-up — `ShellSpec` (data) + `ShellHandles`
    // (runtime live objects). `--no-update-check` folds into `auto_check`
    // right here: the flag gates the auto check only, while `/upgrade` keeps
    // running the same closure unconditionally.
    // Composer input history (#7): one global JSONL under the data dir,
    // recall filtered per workspace. `~/.phimint/history/` stays the
    // context-rotation store — this file is a sibling, not a tenant.
    // Pre-created owner-only (0600) before the shell opens it: the file
    // records every typed line, and its first bytes on disk must never be
    // group/other readable. `create_new` loses no race — if the file exists
    // already, its (already shipped) mode stands. Windows has no mode bits.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(base_dir.join("input-history.jsonl"))
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            eprintln!("warning: cannot pre-create input history: {e}");
        }
    }
    let spec = phi_agent_tui::ShellSpec {
        // Composer input history (#7): one global JSONL under the data dir,
        // recall filtered per workspace. `~/.phimint/history/` stays the
        // context-rotation store — this file is a sibling, not a tenant.
        // (Written before `data_dir` because the field moves `base_dir`.)
        input_history: Some(base_dir.join("input-history.jsonl")),
        data_dir: base_dir,
        // Frame-log instrument trio on, phimint's file names (T11 seam;
        // `None` would turn the whole trio off).
        paths: Some(phi_agent_tui::FrameLogPaths::default()),
        workspace,
        name: "Phimint",
        version: version.to_string(),
        wordmark,
        theme,
        popup_style,
        // Product-semantic keys; the default is phimint's keymap (override
        // per key to customize — design §5.2).
        keymap: phi_agent_tui::Keymap::default(),
        // Product wording for the shell chrome; the default is phimint's
        // voice (CopyTable slot, design §5).
        copy: phi_agent_tui::CopyTable::default(),
        // The product's slash commands (dispatch + `/` picker rows).
        commands,
        // Session titles: shell trigger (the 10th user turn, the pending
        // marker on exit), product generation + persistence layout.
        title_gen: Some(wire::title_slot(model_store.clone())),
        // Boot notices (clamp / socket-board errors / binary shadows /
        // code-road degrade), printed once on the first frame.
        startup_notices: wire::startup_notices(startup_notices),
        update: Some(wire::update_slot(
            update_config,
            version,
            !cli.no_update_check,
        )),
        tips: tips_config.enabled.then(|| wire::tips_slot(&tips_config)),
        // Runtime announcements (window rotations, futility brake): wording
        // and de-dup are product policy behind the probe.
        notices: Some(wire::notice_probe()),
        // Which approval prompts are canvas-launch consents (two-key legend)
        // rather than tool approvals (three-key tool face).
        is_consent: Some(wire::consent_probe()),
        // Whether the current model sees pictures (paste pipeline gate):
        // the model registry's fact for the live endpoint.
        vision: Some(wire::vision_probe(model_store.clone())),
        // phimint is session-based (/resume): the session slot is on.
        session_io: Some(session_io),
        system_prompt: wire.prompt_source(),
        skills: Some(wire.skill_bridge()),
        // The design-canvas panel: subscriptions, click drains, root-turn
        // edges, consent — product state behind one tick closure.
        tick: Some(wire.tick()),
        on_submit: Some(wire.on_submit()),
        on_reopen: Some(wire.on_reopen()),
        exit_dump: exit_dump_mode,
    };
    let handles = phi_agent_tui::ShellHandles {
        session_state: phi_agent_tui::CurrentSession {
            session_id: session,
            ctx: session_ctx,
            resume_messages,
        },
        bg_registry,
        approval_rx,
        approval_mode,
        pick_rx,
        log_sinks,
        session_boot: wire.session_boot(),
        session_end: wire.session_end(),
    };
    phi_agent_tui::run(spec, agent, handles).await
}

/// Base directory for all phimint session data (~/.phimint).
fn sessions_base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let dir = PathBuf::from(home).join(".phimint");
    // Absolutize at the exit point (survey P6): downstream joins assume an
    // absolute base, and the `"."` fallback (HOME unset) used to leave a
    // relative base whose double-join produced `.phimint/sessions/sessions/`.
    if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&dir))
            .unwrap_or(dir)
    }
}

/// Cleanup expired session data at startup.
///
/// Removes history/ and notes/ subdirectories for sessions older than
/// `retention_days`, and delegates session directory cleanup to phi-agent.
fn cleanup_expired_data(base_dir: &Path, retention_days: i64) {
    // Cleanup history and notes directories
    for subdir in &["history", "notes"] {
        let dir = base_dir.join(subdir);
        if !dir.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // Check directory age via metadata modification time
            if let Ok(meta) = std::fs::metadata(&path)
                && let Ok(modified) = meta.modified()
            {
                let age = std::time::SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or_default();
                if age.as_secs() > (retention_days as u64) * 86400 {
                    if let Err(e) = std::fs::remove_dir_all(&path) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to remove expired directory"
                        );
                    } else {
                        tracing::info!(
                            path = %path.display(),
                            "removed expired data directory"
                        );
                    }
                }
            }
        }
    }

    // Delegate session directory cleanup to phi-agent
    match phi_agent::cleanup_expired_sessions(base_dir, retention_days) {
        Ok(count) if count > 0 => {
            tracing::info!(count, "cleaned up expired session directories");
        }
        Err(e) => {
            tracing::warn!(error = %e, "session cleanup failed");
        }
        _ => {}
    }
}

/// Initialize tracing to write to the session's `session.log` file (no console).
///
/// Mirrors `phi-agent`'s CLI `init_logging`: a `log-core` `LogCoreLayer` sink
/// fed by `tracing` records from the framework internals (LLM calls, tool
/// execution, turn lifecycle). The structured event stream is persisted
/// separately by `save_turn_log` after each turn.
///
/// Returns a `SinkHandle` so an in-TUI `/resume` can re-point the file sink at
/// the resumed session's directory (the global subscriber itself can only be
/// initialized once per process).
async fn init_logging(
    session_ctx: &SessionContext,
    log_level: &str,
) -> Result<log_core::SinkHandle> {
    use log_core::{LogCoreLayer, LogLevel};
    use tracing_subscriber::prelude::*;

    let session_log_path = session_ctx.log_path();

    let level = match log_level {
        "debug" => LogLevel::Debug,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    };

    let layer =
        LogCoreLayer::file(session_log_path.to_str().unwrap_or("phimint.log"), level).await?;
    let sink_handle = layer.sink_handle();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .with(layer)
        .init();

    tracing::info!(path = %session_log_path.display(), "logging initialized");

    Ok(sink_handle)
}

/// `phimint update` — check for a new version; self-replace when this is a
/// standalone install, otherwise print the channel's own upgrade command.
///
/// Never writes over a package-manager-owned binary (brew checksums break).
async fn run_update_command(check_only: bool, config_path: Option<&str>) -> Result<()> {
    use phimint::update::checker::{self, CheckResult};
    use phimint::update::install_source;

    // Endpoints: user config when present, else the built-in GitHub→Gitee pair.
    let endpoints = match config_path {
        Some(p) => {
            config::ModelConfig::from_file(&PathBuf::from(p))?
                .update
                .endpoints
        }
        None => match config::ModelConfig::from_default_location() {
            Ok(Some(c)) => c.update.endpoints,
            _ => config::UpdateConfig::default().endpoints,
        },
    };

    let current = env!("CARGO_PKG_VERSION");
    let source = install_source::detect();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()?;

    let result = checker::check(
        &client,
        &endpoints,
        &phimint::update::state::load(),
        current,
    )
    .await?;

    let Some(CheckResult::UpgradeAvailable {
        version,
        download_url,
        sha256,
        ..
    }) = result
    else {
        println!("phimint {current} is up to date.");
        return Ok(());
    };

    println!("phimint {version} available (current: {current}).");
    if check_only || !source.supports_self_update() {
        // Managed installs (brew/npm/cargo) are upgraded by their manager.
        println!("Upgrade with: {}", source.upgrade_command());
        return Ok(());
    }
    if cfg!(windows) {
        // Replacing a running Windows executable needs the installer dance;
        // point at the installer instead of half-working.
        println!("Self-update on Windows is not supported yet. Run:");
        println!(
            "  irm https://github.com/hibuka-labs/phimint/releases/latest/download/install.ps1 | iex"
        );
        return Ok(());
    }

    println!("Downloading {download_url} ...");
    let path =
        phimint::update::apply::download_and_replace(&client, &download_url, sha256.as_deref())
            .await?;
    println!("Updated to {version}: {}", path.display());
    println!("Restart phimint to use the new version.");
    Ok(())
}

/// `phimint uninstall` — remove the binary and the data it left behind.
///
/// The binary is always handed to whoever owns it: brew/npm/cargo run their own
/// uninstall, a standalone install deletes its own file. The two things no
/// package manager can see — the `~/.phimint/` tree and the Windows user-PATH
/// entry `install.ps1` added — are cleaned up here.
fn run_uninstall_command(keep_data: bool, yes: bool) -> Result<()> {
    use phimint::ops::uninstall;

    let plan = uninstall::plan(!keep_data)?;
    print!("{}", plan.describe());
    uninstall::run(&plan, yes)
}

/// `phimint init` — write the config skeleton where the loader would read it
/// (the installer calls this with `--if-missing`). The command semantics live
/// in `model::config_cmd`; this is the print/exit glue.
fn run_init_command(config_flag: Option<&str>, if_missing: bool) -> Result<()> {
    let target = config_cmd::command_target(config_flag);
    emit(config_cmd::init(&target, if_missing))
}

/// `phimint config show|set` — inspect or edit the config without the TUI.
/// `show` resolves the key source exactly like a session would (the env
/// override is named in the report) so the report never lies about what runs.
fn run_config_command(config_flag: Option<&str>, action: Option<ConfigAction>) -> Result<()> {
    let target = config_cmd::command_target(config_flag);
    let env_key = std::env::var("PHIMINT_API_KEY").ok();
    let out = match action.unwrap_or(ConfigAction::Show) {
        ConfigAction::Show => config_cmd::show(&target, env_key.as_deref()),
        ConfigAction::Set { key, value } => {
            // An api_key on argv lands in shell history and process listings;
            // `-` switches the secret to stdin. The bool reports which happened
            // so `set` can name the leak only when there is one.
            match secret_value(&key, value) {
                Ok((value, on_argv)) => {
                    config_cmd::set(&target, &key, &value, env_key.as_deref(), on_argv)
                }
                Err(msg) => Err(msg),
            }
        }
    };
    emit(out)
}

/// The value for `config set`, with `-` on a secret meaning "read stdin".
/// Returns the value and whether it really rode in on the command line.
fn secret_value(key: &str, value: String) -> std::result::Result<(String, bool), String> {
    if key == "api_key" && value == "-" {
        use std::io::Read as _;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("cannot read the key from stdin: {e}"))?;
        return Ok((buf.trim().to_string(), false));
    }
    Ok((value, true))
}

/// Print a command message (an empty one stays silent — `init --if-missing`
/// on a configured machine has nothing to say) or fail with the message.
fn emit(out: std::result::Result<String, String>) -> Result<()> {
    match out {
        Ok(msg) if msg.is_empty() => Ok(()),
        Ok(msg) => {
            println!("{msg}");
            Ok(())
        }
        Err(msg) => Err(anyhow::anyhow!("{msg}")),
    }
}

/// The first-run / load-failure exit: print the message and leave non-zero,
/// so a script (CI, the installer) can tell "not configured" from "done".
fn stop_with(msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diagnostics file is base-level (next to `approvals.json`) and
    /// named after the variable the transport reads — the two are a pair,
    /// so they travel together.
    #[test]
    fn provider_log_path_pairs_with_the_env_var_name() {
        assert_eq!(
            provider_log_path(Path::new("/tmp/phimint-base")),
            PathBuf::from("/tmp/phimint-base/provider.log")
        );
        assert_eq!(PROVIDER_LOG_ENV, "MAI_UI_PROVIDER_LOG");
    }

    /// The wiring the fix rests on: the transport's diagnostics need a file
    /// named, and the name has to be the one the transport reads.
    #[test]
    fn installing_the_provider_log_names_the_file_for_the_transport() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_provider_log(dir.path());
        assert_eq!(path, dir.path().join("provider.log"));
        assert_eq!(
            std::env::var(PROVIDER_LOG_ENV).unwrap(),
            path.to_str().unwrap()
        );
    }

    /// protects: only an api_key's `-` flips to the stdin route - a literal
    /// `-` for another member is just a value, and any other api_key value
    /// stays the argv value it is (the caller reports the leak).
    /// fails_when: every `-` swallows stdin, or a plain key value is routed
    /// to stdin. no_claim: the stdin read itself.
    #[test]
    fn only_an_api_key_dash_routes_to_stdin() {
        assert!(secret_value("api_key", "sk-x".to_string()).unwrap().1);
        let (value, on_argv) = secret_value("base_url", "-".to_string()).unwrap();
        assert_eq!(value, "-", "a dash is just a value here");
        assert!(on_argv);
    }
}
