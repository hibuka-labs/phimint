//! Build the phimint coding agent: system prompt + tool registration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use agent_base::ReasoningEffort;
use agent_base::engine::max_turns_nudge::{MaxTurnsNudgeConfig, MaxTurnsNudgeMiddleware};
use agent_works::guard::{DefaultGuard, DefaultGuardConfig, ReasoningOnlyAction};
use phi_agent::{ApprovalHandler, ChildPermissionMode, MultiAgentConfig, PhiAgent, PhiAgentConfig, ToolPolicy, base_agent_builder_with_excludes};
use phi_kernel_tools::local_shell::LocalShellTool;

use crate::lsp::LspManager;
use crate::skills::{SkillResolver, default_skill_dirs};
use crate::tools::decompose::DecomposeTool;
use crate::tools::diagnostics::DiagnosticsTool;
use crate::tools::merge::MergeTool;
use crate::tools::workspace::WorkspaceTracker;
use crate::tools::{repomap::RepoMapTool, ripgrep::RipgrepTool};

/// Coding-oriented system prompt (adapted from Codex).
const SYSTEM_PROMPT: &str = r#"You are phimint, a coding agent running in a terminal-based TUI. You are expected to be precise, safe, and helpful.

## Personality

Your default personality and tone is concise, direct, and friendly. You communicate efficiently, always keeping the user clearly informed about ongoing actions without unnecessary detail. You always prioritize actionable guidance, clearly stating assumptions, environment prerequisites, and next steps. Unless explicitly asked, you avoid excessively verbose explanations about your work.

## Tools available

- `repo_map` — get the codebase layout. No argument returns a directory skeleton; pass a workspace-relative `path` for per-file symbols (classes, methods, fields).
- `search_content` — search file contents with ripgrep (regex).
- `read_file` / `write_file` / `edit_file` / `list_files` — inspect and modify files (workspace-relative paths). For files under 300 lines, read the entire file at once without offset/limit. Only use pagination for very large files (over 500 lines).
- `execute_command` — run shell commands (build/test/lint). Runs inside a sandbox; destructive ops require user confirmation.
- `diagnostics` — pull LSP errors/warnings (fast, no recompile). Use after editing for a quick check.
- `decompose` / `merge` — split large tasks into parallel read-only investigation slices; merge reconciles and verifies (see Multi-agent below).
- `update_plan` — structured checklist for complex tasks (3+ steps); skip for simple requests.

## How to work

### Orient first
Use `repo_map` for layout, `search_content` to locate symbols, then `read_file` on the relevant files. Don't read every source file — scope to the task. If the user's request is ambiguous, investigate with `search_content` and `repo_map` before asking. Only ask when genuinely unclear.

### Plan when needed
For complex tasks (3+ steps), call `update_plan` first and update statuses as you go. Keep steps concise (5–7 words each). Don't use plans for simple or single-step queries.

### Edit precisely
- Use `edit_file` for targeted changes. `old_text` must match exactly and appear exactly once.
- Use `write_file` for new files or full rewrites.
- Use `append_to_file` for adding to the end of existing files.
- Do not waste tokens by re-reading files after editing them — the tool call will fail if it didn't work.
- Match the surrounding code's style, naming, and comment density. Don't introduce a different idiom.

### Validate your work
After editing, call `diagnostics` for a quick check. Before reporting done, ensure code compiles: run `cargo check` (or equivalent). For big changes, also run tests. Both must pass.

When testing, start as specific as possible to the code you changed, then make your way to broader tests as you build confidence.

When compilation fails, read the error, fix, and re-check. If a fix doesn't work after 3 attempts, explain what you tried and ask the user.

### Keep going
You are a coding agent. Please keep going until the query is completely resolved, before ending your turn and yielding back to the user. Only terminate your turn when you are sure that the problem is solved. Do NOT guess or make up an answer.

### Be efficient
- Prefer dedicated tools over shell: `read_file` instead of `cat`, `search_content` instead of `grep`. Shell bypasses output limits and tool controls.
- For bug fixes or logic changes, reproduce the issue first (failing test, small script, or direct command) before editing. Simple fixes (typos, configs, one-liners) can skip this.
- Do not re-derive facts already established in the conversation. Once the user confirms a decision, move on.

### Safety
- For destructive operations (rm -rf, git push --force, dropping tables), confirm with the user first.
- When a tool call is denied, adjust your approach — don't retry the same call verbatim.
- Do not attempt to fix unrelated bugs or broken tests. It is not your responsibility to fix them. (You may mention them to the user in your final message though.)

### Progress updates
For longer tasks requiring many tool calls, provide concise progress updates (1-2 sentences) recapping progress so far in plain language.

### Final message
Your final message should read naturally, like an update from a concise teammate. Be concise and factual — no filler or conversational commentary. Use present tense and active voice. When referencing files, include the path so the user can click to open.

## Multi-agent (tasks with clearly independent parts)
Sub-agents are READ-ONLY (read/search/report, no writes or mutating commands). You perform all edits.
1. Call `decompose` — returns `serial` (do inline) or `parallel` with investigation slices.
2. If `parallel`: spawn one sub-agent per slice, wait for reports, then implement changes yourself.
3. Ensure the whole workspace compiles; fix as needed.

When done, briefly report what you changed."#;

/// Build a phimint agent bound to `workspace_root`.
///
/// `base_agent_builder` already registers the file tools (read/write/edit/list).
/// We add the shell, search, and repo-map tools ourselves — none of them
/// are part of `base_agent_builder` (the phi CLI registers shell manually too).
///
/// Returns `(PhiAgent, SkillResolver)` — the resolver powers the `/skill` slash
/// command in the TUI loop.
pub fn build(
    llm_client: Arc<dyn agent_base::llm_trait::LlmProvider>,
    approval: Arc<dyn ApprovalHandler>,
    policy: Option<Arc<dyn ToolPolicy>>,
    shell_timeout_ms: u64,
    workspace_root: PathBuf,
    writes_possible: bool,
    thinking_budget: u64,
    reasoning_effort: &str,
    model: String,
) -> Result<(PhiAgent, SkillResolver)> {
    let llm_for_decompose = llm_client.clone();
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
        // .register_tool(VerifyTool::new(&workspace_root, shell_timeout_ms))
        .register_tool(RipgrepTool::new(workspace_root.clone()))
        .register_tool(RepoMapTool::new(workspace_root.clone()))
        // TESTING: low max_turns to verify nudge feature
        .execution_max_turns(256);

    // Phase 4 multi-agent orchestration: `decompose` and `merge` share a
    // `WorkspaceTracker` so the latter can diff against the former's snapshot.
    let tracker = Arc::new(WorkspaceTracker::new());
    builder = builder
        .register_tool(DecomposeTool::new(llm_for_decompose, tracker.clone(), workspace_root.clone()))
        .register_tool(MergeTool::new(tracker, workspace_root.clone(), shell_timeout_ms));

    // LSP diagnostics (multi-server). `LspManager` lazily starts one server per
    // language (rust-analyzer / typescript-language-server / clangd) and shares
    // them with the `diagnostics` pull tool, which reads each `publishDiagnostics`
    // cache. A server that can't be started degrades gracefully to shell commands.
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

    // Custom guard: inject "stop thinking, act now" nudge for reasoning-only responses
    // Use DisableThinking strategy to handle reasoning-only loops
    let guard_config = DefaultGuardConfig {
        reasoning_only_nudge: "STOP THINKING. You have been reasoning too long without taking action. \
            IMMEDIATELY call a tool or provide your final answer. Do NOT produce more reasoning. \
            Just DO something NOW."
            .to_string(),
        reasoning_only_max_strikes: 2, // Fail faster after 2 reasoning-only turns
        reasoning_only_action: ReasoningOnlyAction::DisableThinking, // Disable thinking instead of failing
        disable_thinking_nudge: "Thinking has been disabled due to excessive reasoning. \
            You MUST now either call a tool or write your final answer. \
            Do NOT attempt to reason further. Just DO something NOW."
            .to_string(),
        ..DefaultGuardConfig::default()
    };
    builder = builder.guard(DefaultGuard::new(guard_config));

    // Max turns nudge: firm message on the last 3 turns before the hard limit.
    builder = builder.middleware(MaxTurnsNudgeMiddleware::new(MaxTurnsNudgeConfig {
        threshold: 3,
        message: "You have nearly exhausted your turn budget. \
            Stop all tool calls immediately and provide your final answer now. \
            Summarize what was accomplished and any remaining tasks."
            .to_string(),
    }));

    // Phase 6a: forced-verify gate. When the agent edits files and then tries to
    // report "done" without running `verify` (or `merge`, which runs cargo check
    // itself), this middleware suppresses that final text and injects a nudge to
    // verify first — the "never hand back non-compiling code" promise, enforced
    // as phimint policy (the framework stays neutral; see design §8.3). In
    // `deny` mode no writes can happen, so the gate is disabled (`writes_possible`).
    // builder = builder.middleware(VerifyEnforcementMiddleware::new(VerifyEnforcementConfig {
    //     writes_possible,
    //     ..VerifyEnforcementConfig::default()
    // }));

    // Parse reasoning effort from CLI string
    let effort = match reasoning_effort.to_lowercase().as_str() {
        "none" => ReasoningEffort::None,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        _ => {
            tracing::warn!(effort = reasoning_effort, "unknown reasoning effort, using Medium");
            ReasoningEffort::Medium
        }
    };

    let agent = PhiAgent::build(builder, PhiAgentConfig {
        enable_thinking: true,
        thinking_budget: Some(thinking_budget),
        thinking_effort: effort,
        model,
        ..PhiAgentConfig::default()
    })?;

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
