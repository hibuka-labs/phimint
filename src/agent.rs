//! Build the phiforge coding agent: system prompt + tool registration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use agent_base::{ReasoningEffort, StreamClient};
use phi_agent::{ApprovalHandler, ChildPermissionMode, MultiAgentConfig, OpenAiClient, PhiAgent, PhiAgentConfig, ToolPolicy, base_agent_builder_with_excludes};
use phi_kernel_tools::local_shell::LocalShellTool;

use crate::gate::{VerifyEnforcementConfig, VerifyEnforcementMiddleware};
use crate::lsp::LspManager;
use crate::skills::{SkillResolver, default_skill_dirs};
use crate::tools::decompose::DecomposeTool;
use crate::tools::diagnostics::DiagnosticsTool;
use crate::tools::merge::MergeTool;
use crate::tools::workspace::WorkspaceTracker;
use crate::tools::{repomap::RepoMapTool, ripgrep::RipgrepTool, verify::VerifyTool};

/// Coding-oriented system prompt.
const SYSTEM_PROMPT: &str = r#"You are phiforge, an AI coding agent. You write, edit, and debug code inside a workspace.

Tools available:
- repo_map — get the codebase layout. With no argument it returns a directory skeleton (module → package tree, file counts); pass a workspace-relative `path` to get per-file symbols (classes, methods, fields). Use this FIRST to orient, then scope it to the area you're working in.
- search_content — search file contents with ripgrep (regex) to locate symbols or strings.
- read_file / write_file / edit_file / list_files — inspect and modify files (paths are workspace-relative).
- execute_command — run shell commands (e.g. the workspace's build/test/lint commands).
- verify — run a build/test command and get a terse error summary. With no `command` it auto-selects the workspace's build command (`cargo check`, `mvn -q compile`, `npx tsc --noEmit`, `make`, …). Prefer this for compiling.
- diagnostics — pull LSP errors/warnings for the workspace (fast, no recompile; rust-analyzer / typescript-language-server / clangd). Use after edits for a quick check; `verify` is the authoritative full check.
- decompose / merge — decompose splits a large task into parallel read-only investigation slices; merge reconciles changes and checks compilation (see below).
- update_plan — show the user a structured checklist (objective + steps + statuses) of what you'll do. Use for complex tasks (3+ steps); skip for simple/one-shot requests.

Note: tool output is capped (~16k chars); oversized output is rejected, not truncated.
read_file takes `offset` and `limit` (lines) — read files longer than ~300 lines in chunks.

How to work:
1. For complex tasks (3+ steps), call `update_plan` first to show the user a plan (objective + steps + statuses), then update it as each step's status changes. Skip for simple/one-shot requests.
2. Understand the request: call repo_map for the layout and search_content to locate symbols, then read the relevant files.
3. Edit with edit_file (or write_file for new files). For edit_file, `old_text` must match the file exactly and appear exactly once.
4. Verify your work: call `verify` (or `execute_command` with the workspace's build/test command). `verify` returns compact `file:line:col  code  message` errors.
5. When a command fails, read the error, fix the code, and re-run until it passes.

Multi-agent (for tasks with clearly independent parts):
Sub-agents are READ-ONLY investigators: they read, search, and report — they CANNOT write files or run mutating commands. You (the main agent) perform every edit yourself, so you never lose track of what changed.
1. Call `decompose` with the full task. It returns either `serial` (do it inline) or `parallel` with independent investigation slices.
2. If `parallel`: spawn one read-only sub-agent per slice — `spawn_agent` with `task_name` = slice name, `message` = "Context: <slice.context>\nInvestigate and report: <slice.task>". Then `wait_agent` for each (generous timeout_ms, e.g. 300000).
3. Read each sub-agent's report, then implement the changes yourself with edit_file / write_file.
4. Call `verify` (or `execute_command` with the workspace's build command) until the whole workspace compiles; fix anything failing and re-verify.

Be precise and minimal. Don't rewrite code that already works. When done, briefly report what you changed."#;

/// Build a phiforge agent bound to `workspace_root`.
///
/// `base_agent_builder` already registers the file tools (read/write/edit/list).
/// We add the shell, verify, search, and repo-map tools ourselves — none of them
/// are part of `base_agent_builder` (the phi CLI registers shell manually too).
///
/// Returns `(PhiAgent, SkillResolver)` — the resolver powers the `/skill` slash
/// command in the TUI loop.
pub fn build(
    llm_client: Arc<OpenAiClient>,
    approval: Arc<dyn ApprovalHandler>,
    policy: Option<Arc<dyn ToolPolicy>>,
    shell_timeout_ms: u64,
    workspace_root: PathBuf,
    writes_possible: bool,
) -> Result<(PhiAgent, SkillResolver)> {
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
        .register_tool(VerifyTool::new(&workspace_root, shell_timeout_ms))
        .register_tool(RipgrepTool::new(workspace_root.clone()))
        .register_tool(RepoMapTool::new(workspace_root.clone()));

    // Phase 4 multi-agent orchestration: `decompose` and `merge` share a
    // `WorkspaceTracker` so the latter can diff against the former's snapshot.
    let tracker = Arc::new(WorkspaceTracker::new());
    builder = builder
        .register_tool(DecomposeTool::new(llm, tracker.clone(), workspace_root.clone()))
        .register_tool(MergeTool::new(tracker, workspace_root.clone(), shell_timeout_ms));

    // LSP diagnostics (multi-server). `LspManager` lazily starts one server per
    // language (rust-analyzer / typescript-language-server / clangd) and shares
    // them with the `diagnostics` pull tool, which reads each `publishDiagnostics`
    // cache. A server that can't be started degrades gracefully to `verify`.
    builder = builder.register_tool(DiagnosticsTool::new(
        Arc::new(LspManager::new(workspace_root.clone())),
        workspace_root.clone(),
    ));

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
        // Option A: sub-agents are READ-ONLY investigators (they read/search/
        // report; the main agent writes everything). The hard gate is here —
        // excluding the three mutating tools a child must never hold — while the
        // framework only *suggests* read-only via `child_read_only` (below).
        // `decompose`/`merge` are additionally root-level orchestration tools: a
        // leaf agent has no `spawn_agent`, so handing it `decompose` would let it
        // plan parallel sub-agent work it cannot execute (the "fake completion"
        // bug). Exclude all five so children can only inspect.
        child_excluded_tools: vec![
            "decompose".to_string(),
            "merge".to_string(),
            "write_file".to_string(),
            "edit_file".to_string(),
            "execute_command".to_string(),
        ],
        // Children do narrow slices; cap their reasoning depth so a reasoning-heavy
        // model (deepseek-v4-pro) can't "think" itself into a runaway on long
        // multi-agent contexts.
        child_reasoning_effort: Some(ReasoningEffort::Low),
        // Redundant with the default, but explicit: children get the framework's
        // read-only nudge on top of the hard gate above.
        child_read_only: true,
        ..MultiAgentConfig::default()
    });

    // A policy is what makes approval meaningful (see approval.rs). Only `ask`
    // and `deny` modes carry one; `auto` leaves it unset, so every call is
    // approved without consulting the handler.
    if let Some(p) = policy {
        builder = builder.tool_policy(p);
    }

    // Phase 6a: forced-verify gate. When the agent edits files and then tries to
    // report "done" without running `verify` (or `merge`, which runs cargo check
    // itself), this middleware suppresses that final text and injects a nudge to
    // verify first — the "never hand back non-compiling code" promise, enforced
    // as phiforge policy (the framework stays neutral; see design §8.3). In
    // `deny` mode no writes can happen, so the gate is disabled (`writes_possible`).
    builder = builder.middleware(VerifyEnforcementMiddleware::new(VerifyEnforcementConfig {
        writes_possible,
        ..VerifyEnforcementConfig::default()
    }));

    let agent = PhiAgent::build(builder, PhiAgentConfig::default())?;

    // 构建 SkillResolver（扫描 `.claude/skills` 目录，供 /skill 斜杠命令使用）
    let skill_resolver = SkillResolver::from_dirs(&default_skill_dirs());
    if !skill_resolver.is_empty() {
        tracing::info!(
            count = skill_resolver.len(),
            names = %skill_resolver.skill_names().join(", "),
            "loaded skills for /skill command"
        );
    }

    Ok((agent, skill_resolver))
}
