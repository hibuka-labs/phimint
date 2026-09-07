//! phimint — an AI coding agent built on phi-agent.
//!
//! The UI is a ratatui TUI with a fixed input bar at the bottom (the cursor
//! stays in the bar while output scrolls above it).

mod agent;
mod approval;
mod banner;
mod gate;
mod skills;
mod tools;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use phi_agent::{SessionContext, resolve_session};

#[derive(Parser)]
#[command(name = "phimint", version, about = "AI coding agent built on phi-agent")]
struct Cli {
    /// Workspace directory to operate in (default: current directory)
    #[arg(short, long, default_value = ".")]
    workspace: PathBuf,

    /// Model name (overrides LLM_MODEL env; genai auto-detects provider)
    #[arg(long)]
    model: Option<String>,

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

    // LLM client — genai auto-detects provider from model name
    // (e.g. "gpt-5.4-mini" → OpenAI, "deepseek-chat" → DeepSeek, "aliyun::qwen-plus" → Aliyun)
    // Set provider-specific API key in env: OPENAI_API_KEY, DEEPSEEK_API_KEY, ALIYUN_API_KEY, etc.
    let model = cli.model
        .or_else(|| std::env::var("LLM_MODEL").ok())
        .unwrap_or_else(|| "gpt-5.4-mini".to_string());
    // Build LLM provider from env vars + CLI model override.
    // LlmAdapter is gone; use phi_agent::create_provider() with LlmConfig.
    // Resolve API key from LLM_API_KEY or OPENAI_API_KEY.
    let api_key = std::env::var("LLM_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .context("Set LLM_API_KEY (or OPENAI_API_KEY) in .env or environment")?;
    let base_url = std::env::var("LLM_BASE_URL")
        .context("Set LLM_BASE_URL in .env or environment")?;
    let llm_config = phi_agent::llm_trait::config::LlmConfig {
        protocol: std::env::var("LLM_PROTOCOL")
            .ok()
            .and_then(|s| s.parse::<phi_agent::llm_trait::Protocol>().ok()),
        api_key,
        model,
        base_url,
        options: {
            let mut opts = std::collections::HashMap::new();
            opts.insert("max_tokens".to_string(), serde_json::json!("24576"));  // 24K output
            opts
        },
    };
    let llm_client: Arc<dyn phi_agent::llm_trait::LlmProvider> =
        phi_agent::create_provider(&llm_config)
            .context("Failed to create LLM provider")?;

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
    let session_ctx = resolve_session(cli.session.as_deref(), &base_dir)?;
    init_logging(&session_ctx, &cli.log_level).await?;

    // `deny` 模式只读（写工具全被拒），强制 verify 闸门无意义，故关闭。
    let (agent, skill_resolver) = agent::build(
        llm_client,
        approval,
        policy,
        cli.shell_timeout_ms,
        workspace.clone(),
        cli.approval != "deny",
        cli.thinking_budget,
        &cli.reasoning_effort,
        llm_config.model.clone(),
    )?;
    let session = agent.create_session().await;

    // Detect terminal color scheme for the startup banner palette.
    // `--color-scheme` flag takes priority; `auto` reads `$COLORFGBG` (set by
    // iTerm2, kitty, etc.) where the background digit is `7` → light. The OSC
    // 11 stdin probe is intentionally skipped: its termios manipulation
    // interferes with crossterm's raw-mode setup on macOS. Use
    // `--color-scheme light` or `dark` to override when COLORFGBG is absent.
    let color_fgbg = std::env::var("COLORFGBG").ok();
    let scheme = banner::resolve_scheme(&cli.color_scheme, color_fgbg.as_deref(), None);
    let show_banner = cli.banner != "off";
    let version = env!("CARGO_PKG_VERSION");

    ui::run_tui(agent, skill_resolver, session, session_ctx, workspace, approval_rx, scheme, show_banner, version).await
}

/// Base directory for all phimint session data (~/.phimint).
fn sessions_base_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".phimint")
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

    let layer = LogCoreLayer::file(session_log_path.to_str().unwrap_or("phimint.log"), level).await?;

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
