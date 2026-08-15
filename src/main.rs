//! phiforge — an AI coding agent built on phi-agent.
//!
//! Phase 1 vertical slice: single agent + file/shell tools + a terminal REPL.
//! The TUI (ratatui) is a later phase; for now we render `RuntimeEvent`s to
//! stdout, which also exercises the exact event stream the TUI will consume.

mod agent;
mod approval;
mod tools;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use phi_agent::{
    OpenAiClient, PhiAgent, RuntimeEvent, SessionContext, SessionId, resolve_llm_config,
    resolve_session, save_turn_log,
};

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

    /// Log level for the session.log file (debug/info/warn/error)
    #[arg(long, default_value = "info")]
    log_level: String,
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

    // Approval is two layers (see approval.rs): a policy (the gate) + a handler
    // (the decision). `build_approval` wires both for the chosen CLI mode.
    let (approval, policy) = approval::build_approval(&cli.approval);

    // Session + logging. Sessions live under `~/.phiforge/sessions/<id>/`; the
    // human-readable tracing log is `session.log`, and each turn's structured
    // event stream (tool calls, text deltas, …) is `turn_NNN.jsonl`.
    let base_dir = sessions_base_dir();
    let session_ctx = resolve_session(cli.session.as_deref(), &base_dir)?;
    init_logging(&session_ctx, &cli.log_level).await?;

    let agent = agent::build(llm_client, approval, policy, cli.shell_timeout_ms, workspace.clone())?;
    let session = agent.create_session().await;

    run_repl(&agent, session, &session_ctx).await
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

async fn run_repl(agent: &PhiAgent, session: SessionId, session_ctx: &SessionContext) -> Result<()> {
    println!("phiforge — coding agent on phi-agent.");
    println!("Workspace: {}", std::env::current_dir()?.display());
    println!("Logs:     {}", session_ctx.session_dir.display());
    println!("Type a task, or `exit` / `quit` to leave.\n");

    let mut rl = rustyline::Editor::<(), rustyline::history::FileHistory>::new()?;
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let history_path = std::path::PathBuf::from(home).join(".phiforge").join("history");
    let _ = rl.load_history(&history_path);

    // Resume turn numbering from any turns already logged for this session, so
    // reusing `--session` across runs continues the sequence (turn_001..N from
    // run 1, turn_N+1.. from run 2) instead of appending into turn_001.jsonl again.
    let mut turn_number: u32 = session_ctx.last_turn_number();

    loop {
        match rl.readline("\x1b[1mphiforge>\x1b[0m ") {
            Ok(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if line == "exit" || line == "quit" {
                    break;
                }

                turn_number += 1;

                // Collect the full event stream for this turn so we can persist
                // it as structured JSONL — this is the "process log" that lets
                // us post-mortem how many tool calls / error iterations a turn
                // took, beyond what's printed to the terminal.
                let mut turn_events: Vec<RuntimeEvent> = Vec::new();
                let result = agent
                    .run_turn(session.clone(), &line, |ev| {
                        turn_events.push(ev.clone());
                        print_event(&ev);
                        Ok(())
                    })
                    .await;

                // Persist regardless of whether the turn succeeded or errored.
                if let Err(e) = save_turn_log(session_ctx, turn_number, &turn_events, &line) {
                    eprintln!("\nwarning: failed to save turn log: {e}");
                }

                if let Err(e) = result {
                    eprintln!("\n\x1b[31m❌ {}\x1b[0m", e);
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

/// Render a `RuntimeEvent` to stdout.
///
/// This is the seed of the Phase-N TUI's event consumer: it maps each event
/// to a visible effect. `run_turn` feeds events one at a time through the
/// `on_event` closure; the TUI will instead forward them into its own channel.
fn print_event(ev: &RuntimeEvent) {
    match ev {
        RuntimeEvent::TextDelta { text, .. } => {
            print!("{}", text);
            let _ = std::io::stdout().flush();
        }
        RuntimeEvent::ThoughtDelta { text, .. } => {
            print!("\x1b[2m{}\x1b[0m", text);
            let _ = std::io::stdout().flush();
        }
        RuntimeEvent::ToolCallStarted {
            tool_name, args_json, ..
        } => {
            println!("\n\x1b[1m🔧 {}\x1b[0m {}", tool_name, args_json);
        }
        RuntimeEvent::ToolCallFinished {
            tool_name,
            denied,
            summary,
            ..
        } => {
            if *denied {
                println!("\x1b[31m⛔ {} denied\x1b[0m", tool_name);
                return;
            }
            let preview: String = summary.chars().take(240).collect();
            if !preview.is_empty() {
                println!("   ↳ {}", preview.replace('\n', "\n   "));
            }
        }
        RuntimeEvent::PlanUpdated {
            objective, plan, ..
        } => {
            println!("\n📋 {}", objective);
            for item in plan {
                println!("   - {:?}", item);
            }
        }
        RuntimeEvent::AwaitingApproval { .. } => {
            // The approval handler (`CliApprovalHandler`) prints the prompt to
            // stderr itself; nothing to render here. In a later TUI phase this
            // event will drive the inline approval dialog instead.
        }
        RuntimeEvent::RunFinished { .. } => {
            println!("\n\x1b[32m✅ done\x1b[0m");
        }
        RuntimeEvent::RunCancelled { .. } => {
            println!("\n⏹️  cancelled");
        }
        _ => {}
    }
}
