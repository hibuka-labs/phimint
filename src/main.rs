//! phiforge — an AI coding agent built on phi-agent.
//!
//! The default UI is a ratatui TUI with a fixed input bar at the bottom (the
//! cursor stays in the bar while output scrolls above it). The inline chat is
//! opt-in via `--inline` for native-terminal-scrollback use.

mod agent;
mod approval;
mod banner;
mod gate;
mod inline;
mod lang;
mod lsp;
mod markdown;
mod tools;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use phi_agent::{OpenAiClient, SessionContext, resolve_llm_config, resolve_session};

#[derive(Parser)]
#[command(name = "phiforge", version, about = "AI coding agent built on phi-agent")]
struct Cli {
    /// Workspace directory to operate in (default: current directory)
    #[arg(short, long, default_value = ".")]
    workspace: PathBuf,

    /// Model name (overrides LLM_MODEL / OPENAI_MODEL env)
    #[arg(long)]
    model: Option<String>,

    /// API base URL (overrides env)
    #[arg(long)]
    base_url: Option<String>,

    /// Approval mode: `auto` (default), `ask` (prompt on writes/risky shell), or `deny` (reject all writes)
    #[arg(long, default_value = "auto")]
    approval: String,

    /// Shell command timeout in milliseconds
    #[arg(long, default_value_t = 120_000)]
    shell_timeout_ms: u64,

    /// Session ID (defaults to PHI_SESSION_ID env, else auto-generated).
    /// Reuse the same ID across runs to append turns to the same log directory.
    #[arg(long)]
    session: Option<String>,

    /// Run the inline chat instead of the ratatui TUI (TUI is the default).
    #[arg(long)]
    inline: bool,

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
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Resolve the workspace to an absolute path and `cd` into it, so that both
    // the file tools (workspace-relative) and the shell tool (process cwd)
    // operate on the same tree.
    let workspace = std::fs::canonicalize(&cli.workspace)
        .with_context(|| format!("workspace not found: {}", cli.workspace.display()))?;
    std::env::set_current_dir(&workspace)?;

    let llm = resolve_llm_config(cli.model.as_deref(), cli.base_url.as_deref())?;
    let llm_client = Arc::new(OpenAiClient::new(llm.api_key, llm.model, Some(llm.base_url)));

    // The ratatui TUI is the default; `--inline` opts into the inline chat.
    let use_tui = !cli.inline;

    // Approval is two layers (see approval.rs): a policy (the gate) + a handler
    // (the decision). In `ask` mode the handler enqueues requests for the inline
    // (or TUI) approval prompt instead of reading stdin; the queue receiver is
    // handed to whichever UI runs.
    let (approval, policy, approval_rx) = if cli.approval == "ask" {
        let (handler, policy, rx) = approval::build_queued_approval();
        (handler, policy, Some(rx))
    } else {
        let (handler, policy) = approval::build_approval(&cli.approval);
        (handler, policy, None)
    };

    // Session + logging. Sessions live under `~/.phiforge/sessions/<id>/`; the
    // human-readable tracing log is `session.log`, and each turn's structured
    // event stream (tool calls, text deltas, …) is `turn_NNN.jsonl`.
    let base_dir = sessions_base_dir();
    let session_ctx = resolve_session(cli.session.as_deref(), &base_dir)?;
    init_logging(&session_ctx, &cli.log_level).await?;

    // `deny` 模式只读（写工具全被拒），强制 verify 闸门无意义，故关闭。
    let agent = agent::build(
        llm_client,
        approval,
        policy,
        cli.shell_timeout_ms,
        workspace.clone(),
        cli.approval != "deny",
    )?;
    let session = agent.create_session().await;

    // Probe the terminal background *before* entering raw mode / alt-screen so
    // the startup banner renders in the right palette. Best-effort: a 100ms
    // timeout keeps startup fast on terminals that don't answer OSC 11.
    let osc11 = banner::probe_osc11(std::time::Duration::from_millis(100));
    let color_fgbg = std::env::var("COLORFGBG").ok();
    let scheme = banner::resolve_scheme(&cli.color_scheme, color_fgbg.as_deref(), osc11);
    let show_banner = cli.banner != "off";
    let version = env!("CARGO_PKG_VERSION");

    if use_tui {
        ui::run_tui(agent, session, session_ctx, workspace, approval_rx, scheme, show_banner, version).await
    } else {
        inline::run_inline(agent, session, session_ctx, workspace, approval_rx, scheme, show_banner, version).await
    }
}

/// Base directory for all phiforge session data (~/.phiforge).
fn sessions_base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".phiforge")
}

/// Initialize tracing to write to the session's `session.log` file (no console).
///
/// Mirrors `phi-agent`'s CLI `init_logging`: a `log-core` `LogCoreLayer` sink
/// fed by `tracing` records from the framework internals (LLM calls, tool
/// execution, turn lifecycle). The structured event stream is persisted
/// separately by `save_turn_log` after each turn.
async fn init_logging(session_ctx: &SessionContext, log_level: &str) -> Result<()> {
    use log_core::{LogCoreLayer, LogLevel};
    use tracing_subscriber::prelude::*;

    let session_log_path = session_ctx.log_path();

    let level = match log_level {
        "debug" => LogLevel::Debug,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    };

    let layer = LogCoreLayer::file(session_log_path.to_str().unwrap_or("phiforge.log"), level).await?;

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .with(layer)
        .init();

    tracing::info!(path = %session_log_path.display(), "logging initialized");

    Ok(())
}
