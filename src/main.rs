//! phimint — an AI coding agent built on phi-agent.
//!
//! The UI is a ratatui TUI with a fixed input bar at the bottom (the cursor
//! stays in the bar while output scrolls above it).

// Modules live in the lib crate (src/lib.rs) so tests/ can exercise them.
use phimint::{agent, approval, banner, config, model_store, router, skills, title_gen, ui};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use phi_agent::{SessionContext, load_session_messages, resolve_session};

#[derive(Parser)]
#[command(
    name = "phimint",
    version,
    about = "AI coding agent built on phi-agent"
)]
struct Cli {
    /// Workspace directory to operate in (default: current directory)
    #[arg(short, long, default_value = ".")]
    workspace: PathBuf,

    /// Model name (overrides main tier model)
    #[arg(long)]
    model: Option<String>,

    /// Lite model name (overrides lite tier)
    #[arg(long)]
    lite_model: Option<String>,

    /// Advanced model name (overrides advanced tier)
    #[arg(long)]
    advanced_model: Option<String>,

    /// API base URL (overrides default base_url)
    #[arg(long)]
    base_url: Option<String>,

    /// API key (overrides default api_key)
    #[arg(long)]
    api_key: Option<String>,

    /// API protocol (overrides default protocol)
    #[arg(long)]
    protocol: Option<String>,

    /// Model config file path (overrides default locations)
    #[arg(long)]
    config: Option<String>,

    /// Approval mode: `auto` (default), `ask` (prompt on writes/risky shell), or `deny` (reject all writes)
    #[arg(long, default_value = "auto")]
    approval: String,

    /// Shell command timeout in milliseconds
    #[arg(long, default_value_t = 120_000)]
    shell_timeout_ms: u64,

    /// Session ID (defaults to PHI_SESSION_ID env, else auto-generated).
    /// Reuse the same ID across runs to append turns to the same log directory.
    #[arg(long, conflicts_with = "resume")]
    session: Option<String>,

    /// Resume a previous session. Shows an interactive picker to choose from
    /// recent sessions. Conflicts with --session.
    #[arg(long)]
    resume: bool,

    /// Log level for the session.log file (debug/info/warn/error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Color scheme for the startup banner: `auto` (default, detects terminal
    /// background), `dark` or `light` (override).
    #[arg(long, default_value = "auto")]
    color_scheme: String,

    /// Show the startup banner (default: `on`; set to `off` to suppress it).
    #[arg(long, default_value = "on")]
    banner: String,

    /// Thinking/reasoning budget in tokens (default: 8192).
    /// Controls how many tokens the model can use for internal reasoning.
    #[arg(long, default_value_t = 8192)]
    thinking_budget: u64,

    /// Reasoning effort level: none/low/medium/high/xhigh (default: medium).
    /// Controls the depth of model's reasoning. Higher = more thorough but slower.
    #[arg(long, default_value = "medium")]
    reasoning_effort: String,

    /// Work-room token budget for context windows (tokens of conversation
    /// space ABOVE the fixed window base). Window rotation + history/notes
    /// tools replace LLM summarization; this is now the default mode. Bare
    /// `--token-budget` or omitting the flag uses the default (210K,
    /// aligned with Codex's ~256K context window × 90% minus base overhead).
    #[arg(
        long,
        num_args(0..=1),
        default_missing_value = "210000",
        default_value = "210000"
    )]
    token_budget: usize,

    /// Days to retain session history and notes (default: 7).
    /// Expired data is cleaned up at startup.
    #[arg(long, default_value_t = 7)]
    session_retention_days: i64,

    /// Skip the automatic update check on startup.
    #[arg(long)]
    no_update_check: bool,

    /// Subcommands (`phimint update`) that run without starting the TUI.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Check for a new phimint version and update this binary.
    ///
    /// Standalone installs (curl installer) self-replace; installs owned by
    /// brew/npm/cargo print their package manager's upgrade command instead.
    Update {
        /// Only report whether an update is available; never download.
        #[arg(long)]
        check: bool,
    },

    /// Remove phimint and the data it left behind.
    ///
    /// The binary goes back to the channel that owns it (brew/npm/cargo run
    /// their own uninstall; a standalone install is deleted here). Then
    /// `~/.phimint/` is removed — that tree holds your API key and sessions,
    /// which no package manager knows about — and the Windows user-PATH entry
    /// the installer added is dropped.
    Uninstall {
        /// Keep ~/.phimint (config, sessions, history, notes).
        #[arg(long)]
        keep_data: bool,

        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
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
        None => {}
    }

    // Resolve the workspace to an absolute path and `cd` into it, so that both
    // the file tools (workspace-relative) and the shell tool (process cwd)
    // operate on the same tree.
    let workspace = std::fs::canonicalize(&cli.workspace)
        .with_context(|| format!("workspace not found: {}", cli.workspace.display()))?;
    std::env::set_current_dir(&workspace)?;

    // Load model configuration: CLI > JSON file
    let mut model_config = if let Some(config_path) = &cli.config {
        // CLI --config flag specified
        let path = PathBuf::from(config_path);
        config::ModelConfig::from_file(&path).context("Failed to load config file")?
    } else {
        match config::ModelConfig::from_default_location() {
            Ok(Some(config)) => {
                tracing::info!("loaded model config from JSON file");
                config
            }
            Ok(None) => {
                return Err(anyhow::anyhow!(
                    "No config file found. Create ~/.phimint/config.json with your model configuration.\n\
                     See config.json.example for format."
                ));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to load config: {}", e));
            }
        }
    };

    // Apply CLI overrides
    if let Some(ref model) = cli.model {
        model_config.main = config::TierConfig::Simple(model.clone());
    }
    if let Some(ref lite) = cli.lite_model {
        model_config.lite = Some(config::TierConfig::Simple(lite.clone()));
    }
    if let Some(ref advanced) = cli.advanced_model {
        model_config.advanced = Some(config::TierConfig::Simple(advanced.clone()));
    }
    if let Some(ref url) = cli.base_url {
        model_config.base_url = Some(url.clone());
    }
    if let Some(ref key) = cli.api_key {
        model_config.api_key = Some(key.clone());
    }
    if let Some(ref proto) = cli.protocol {
        model_config.protocol = Some(proto.clone());
    }

    // Validate config
    model_config.validate()?;

    // Create main provider
    let (main_model, main_url, main_key, main_proto) = model_config.tier_config("main").unwrap();
    let llm_config = phi_agent::llm_trait::config::LlmConfig {
        protocol: if main_proto.is_empty() {
            None
        } else {
            main_proto.parse::<phi_agent::llm_trait::Protocol>().ok()
        },
        api_key: main_key,
        model: main_model,
        base_url: main_url,
        options: {
            let mut opts = std::collections::HashMap::new();
            opts.insert("max_tokens".to_string(), serde_json::json!("24576")); // 24K output
            opts
        },
    };
    let llm_client: Arc<dyn phi_agent::llm_trait::LlmProvider> =
        phi_agent::create_provider(&llm_config).context("Failed to create LLM provider")?;

    // Extract update config before model_config is moved into the store.
    let update_config = model_config.update.clone();

    let popup_style = model_config
        .ui
        .popup
        .to_style()
        .context("invalid ui.popup config")?;

    // Model store and router for multi-model support
    let model_store = Arc::new(tokio::sync::Mutex::new(model_store::ModelStore::new(
        model_config,
        llm_client.clone(),
    )));
    let router = Arc::new(router::PhimintRouter::new());

    // Approval is two layers (see approval.rs): a policy (the gate) + a handler
    // (the decision). In `ask` mode the handler enqueues requests for the TUI
    // approval prompt instead of reading stdin; the queue receiver is handed to
    // the UI.
    let (approval, policy, approval_rx) = if cli.approval == "ask" {
        let (handler, policy, rx) = approval::build_queued_approval();
        (handler, policy, Some(rx))
    } else {
        let (handler, policy) = approval::build_approval(&cli.approval);
        (handler, policy, None)
    };

    // Session + logging. Sessions live under `~/.phimint/sessions/<id>/`; the
    // human-readable tracing log is `session.log`, and each turn's structured
    // event stream (tool calls, text deltas, …) is `turn_NNN.jsonl`.
    let base_dir = sessions_base_dir();

    // Cleanup expired session data (history, notes, sessions) at startup.
    cleanup_expired_data(&base_dir, cli.session_retention_days);

    // Process any pending title generation marker from a previous exit.
    // Runs in the main tokio runtime — the LLM provider is healthy here.
    {
        let pending_provider = {
            let mut store = model_store.lock().await;
            store.get_or_create_provider("lite").ok()
        };
        if let Some(p) = pending_provider {
            let sessions_dir = base_dir.join("sessions");
            if let Err(e) = title_gen::process_pending_title(p, &sessions_dir).await {
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

    // Resolve session: --resume shows a picker; otherwise normal resolve.
    // Session is resolved BEFORE agent::build so that token_budget_opts
    // (which needs session_id for history/notes stores) is available.
    let session_ctx = if cli.resume {
        let picked_dir =
            ui::picker::show_picker(&base_dir, None, scheme)?.context("No session selected")?;
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

    let (agent, skill_resolver, skill_telemetry, bg_registry) = agent::build(
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
    )?;

    // Create or resume the runtime session.
    let (session, resume_messages) = if cli.resume {
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

    ui::run_tui(
        agent,
        skill_resolver,
        skill_telemetry,
        bg_registry,
        session,
        session_ctx,
        base_dir,
        workspace,
        approval_rx,
        scheme,
        popup_style,
        show_banner,
        version,
        model_store,
        router,
        resume_messages,
        update_config,
        cli.no_update_check,
        log_sinks,
    )
    .await
}

/// Base directory for all phimint session data (~/.phimint).
fn sessions_base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".phimint")
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
    use phimint::uninstall;

    let plan = uninstall::plan(!keep_data)?;
    print!("{}", plan.describe());
    uninstall::run(&plan, yes)
}
