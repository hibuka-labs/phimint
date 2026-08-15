//! Build the phiforge coding agent: system prompt + tool registration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use agent_base::StreamClient;
use phi_agent::{ApprovalHandler, ChildPermissionMode, MultiAgentConfig, OpenAiClient, PhiAgent, PhiAgentConfig, ToolPolicy, base_agent_builder_with_excludes};
use phi_kernel_tools::local_shell::LocalShellTool;

use crate::tools::decompose::DecomposeTool;
use crate::tools::merge::MergeTool;
use crate::tools::workspace::WorkspaceTracker;
use crate::tools::{repomap::RepoMapTool, ripgrep::RipgrepTool, verify::VerifyTool};

/// Coding-oriented system prompt.
const SYSTEM_PROMPT: &str = r#"You are phiforge, an AI coding agent. You write, edit, and debug code inside a workspace.

Tools available:
- repo_map — get a structural map of the codebase (files + top-level symbols). Use this FIRST to orient.
- search_content — search file contents with ripgrep (regex) to locate symbols or strings.
- read_file / write_file / edit_file / list_files — inspect and modify files (paths are workspace-relative).
- execute_command — run shell commands (e.g. cargo build, cargo check, cargo test).
- verify — run a build/test command (default `cargo check`) and get a terse error summary. Prefer this for compiling.
- decompose / merge — for large multi-part tasks, split into parallel sub-agent slices and reconcile them (see below).

Note: tool output is capped (~16k chars); oversized output is rejected, not truncated.
read_file takes `offset` and `limit` (lines) — read files longer than ~300 lines in chunks.

How to work:
1. Understand the request: call repo_map for the layout and search_content to locate symbols, then read the relevant files.
2. Edit with edit_file (or write_file for new files). For edit_file, `old_text` must match the file exactly and appear exactly once.
3. Verify your work: call `verify` (or `execute_command` `cargo check` / `cargo build` / `cargo test`). `verify` returns compact `file:line:col  code  message` errors.
4. When a command fails, read the error, fix the code, and re-run until it passes.

Multi-agent (for tasks with clearly independent parts):
1. Call `decompose` with the full task. It returns either `serial` (do it inline) or `parallel` with independent slices.
2. If `parallel`: spawn one sub-agent per slice — `spawn_agent` with `task_name` = slice name, `message` = "Context: <slice.context>\nTask: <slice.task>". Then `wait_agent` for each (generous timeout_ms, e.g. 300000).
3. After all sub-agents finish, call `merge` — it diffs the workspace against the pre-decompose snapshot, flags conflicts (overlapping or out-of-scope edits), and runs `cargo check`.
4. If `merge` reports conflicts or a failing verify, fix them and re-run `merge` until green.

Sub-agent write permissions follow the session's approval mode: in `ask` mode their file writes prompt for approval just like your own; in `deny` mode they are read-only. Never assume a spawned sub-agent can write — if it reports a denied tool, do that slice's edit yourself.

Be precise and minimal. Don't rewrite code that already works. When done, briefly report what you changed."#;

/// Build a phiforge agent bound to `workspace_root`.
///
/// `base_agent_builder` already registers the file tools (read/write/edit/list).
/// We add the shell, verify, search, and repo-map tools ourselves — none of them
/// are part of `base_agent_builder` (the phi CLI registers shell manually too).
pub fn build(
    llm_client: Arc<OpenAiClient>,
    approval: Arc<dyn ApprovalHandler>,
    policy: Option<Arc<dyn ToolPolicy>>,
    shell_timeout_ms: u64,
    workspace_root: PathBuf,
) -> Result<PhiAgent> {
    // Coerce the concrete client to `Arc<dyn StreamClient>` once; the builder and
    // the `decompose` tool (which makes its own nested LLM call) each need a clone.
    let llm: Arc<dyn StreamClient> = llm_client.clone();

    let mut builder = base_agent_builder_with_excludes(
        llm_client,
        // Coding-specific noise the framework (domain-agnostic) must not know
        // about. Excludes flow into list_files so a bare directory's build
        // output doesn't flood the listing.
        vec!["target".to_string(), "node_modules".to_string()],
    )
        .system_prompt(SYSTEM_PROMPT)
        .approval_handler(approval)
        // base_agent_builder caps tool output at 4000 chars and REJECTS (rather
        // than truncates) anything larger. That is too small to read a normal
        // source file — read_file's own default limit is 2000 *lines*, so a
        // ~100-line file already overflows the char cap. Raise it to fit a few
        // hundred lines; the system prompt tells the agent to paginate beyond.
        .max_tool_output_chars(16_000)
        .register_tool(LocalShellTool::new(shell_timeout_ms))
        .register_tool(VerifyTool::new(shell_timeout_ms))
        .register_tool(RipgrepTool::new(workspace_root.clone()))
        .register_tool(RepoMapTool::new(workspace_root.clone()));

    // Phase 4 multi-agent orchestration: `decompose` and `merge` share a
    // `WorkspaceTracker` so the latter can diff against the former's snapshot.
    let tracker = Arc::new(WorkspaceTracker::new());
    builder = builder
        .register_tool(DecomposeTool::new(llm, tracker.clone(), workspace_root.clone()))
        .register_tool(MergeTool::new(tracker, workspace_root.clone(), shell_timeout_ms));

    // Child permission follows the approval mode (codex-style delegation lives in
    // agent-works). `auto` (no policy) → children full-permission; `ask`/`deny`
    // (a policy is present) → children restricted, routing approval decisions up
    // to the parent's handler instead of hard-denying locally.
    let child_permission_mode = if policy.is_some() {
        ChildPermissionMode::None
    } else {
        ChildPermissionMode::Full
    };
    builder = builder.with_multi_agent(MultiAgentConfig {
        child_permission_mode,
        ..MultiAgentConfig::default()
    });

    // A policy is what makes approval meaningful (see approval.rs). Only `ask`
    // and `deny` modes carry one; `auto` leaves it unset, so every call is
    // approved without consulting the handler.
    if let Some(p) = policy {
        builder = builder.tool_policy(p);
    }

    let agent = PhiAgent::build(builder, PhiAgentConfig::default())?;
    Ok(agent)
}
