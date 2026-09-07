//! Build the phimint coding agent: system prompt + tool registration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use phi_agent::{
    ApprovalHandler, ChildPermissionMode, ControlConfig, DefaultGuard, DefaultGuardConfig,
    LocalShellTool, MaxTurnsNudgeConfig, MaxTurnsNudgeMiddleware, MultiAgentConfig, PhiAgent,
    PhiAgentConfig, ReasoningEffort, ReasoningOnlyAction, ToolPolicy, base_agent_builder_with_excludes,
};

use code_intel::lsp::{ClientInfo, LspManager, LspServerSpec};
use crate::skills::{SkillResolver, default_skill_dirs};
use crate::tools::diagnostics::DiagnosticsTool;
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
- Default to ASCII when editing or creating files. Only introduce non-ASCII or other Unicode characters when there is a clear justification and the file already uses them.
- Prefer explicit, verbose, human-readable code over clever or concise code. Write clear comments that explain what is going on if code is not self-explanatory.

### Validate your work
After editing, call `diagnostics` for a quick check. Before reporting done, ensure code compiles: run `cargo check` (or equivalent). For big changes, also run tests. Both must pass.

When testing, start as specific as possible to the code you changed, then make your way to broader tests as you build confidence.

When compilation fails, read the error, fix, and re-check. If a fix doesn't work after 3 attempts, explain what you tried and ask the user.

### Keep going
You are a coding agent. Please keep going until the query is completely resolved, before ending your turn and yielding back to the user. Only terminate your turn when you are sure that the problem is solved. Do NOT guess or make up an answer.

### Be efficient
- Prefer dedicated tools over shell: `read_file` instead of `cat`, `search_content` instead of `grep`, `execute_command` instead of shell for build/test. Shell bypasses output limits and tool controls.
- For bug fixes or logic changes, reproduce the issue first (failing test, small script, or direct command) before editing. Simple fixes (typos, configs, one-liners) can skip this.
- Do not re-derive facts already established in the conversation. Once the user confirms a decision, move on.
- Do not attempt to fix unrelated bugs or broken tests. It is not your responsibility to fix them. (You may mention them to the user in your final message though.)

### Safety
- For destructive operations (rm -rf, git push --force, dropping tables), confirm with the user first.
- When a tool call is denied, adjust your approach — don't retry the same call verbatim.

### Progress updates
For longer tasks requiring many tool calls, provide concise progress updates (1-2 sentences) recapping progress so far in plain language.

### Final message
Your final message should read naturally, like an update from a concise teammate. Be concise and factual — no filler or conversational commentary. Use present tense and active voice. When referencing files, include the path so the user can click to open.

## Multi-agent (tasks with clearly independent parts)
Sub-agents are READ-ONLY (read/search/report, no writes or mutating commands). You perform all edits.
- Prefer multiple sub-agents to parallelize your work. Time is a constraint so parallelism resolves the task faster.
- **Results are pushed to you automatically**: when every sub-agent has finished, all their full reports arrive together as one message and a new turn starts. There is no wait tool and you never need to poll. A `done` status means the report is held by the runtime — it is NOT lost and NOT a delivery failure; it is injected the instant your turn ends.
- **To wait, simply end your turn**: after spawning sub-agents, end your reply with a brief progress note and stop. Ending the turn is the ONLY way to receive their reports. Do NOT "use the waiting time" to investigate the topics you delegated — that duplicates the work you just paid sub-agents to do. End the turn FIRST, then continue your own work when the results arrive.
- NEVER call `list_agents` to check progress or wait. Polling burns tokens, does not make sub-agents finish faster, and repeated snapshots tell you nothing new. Their reports come to you whether you watch or not.
- NEVER use shell commands to pass time while sub-agents run (`sleep`, `wait`, watch loops, repeated no-op calls). Ending your turn is the ONLY wait mechanism — if you catch yourself waiting, end the turn.
- While sub-agents run, your only role is coordination: do not perform the work you delegated, do not send them follow-up nudges (e.g. "please finalize" — they are still working), and do not close them. Never hold the turn open to "keep an eye on" sub-agents, and never close agents just because their status looks quiet — a quiet `done` agent with a held report is healthy. Interrupting a running sub-agent mid-task only delays and fragments the results.
- If the user asks a question while sub-agents run, answer it first; coordination continues afterwards.
- When you have a plan with multiple steps, process them in parallel by spawning one agent per step when possible.

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
    llm_client: Arc<dyn phi_agent::llm_trait::LlmProvider>,
    approval: Arc<dyn ApprovalHandler>,
    policy: Option<Arc<dyn ToolPolicy>>,
    shell_timeout_ms: u64,
    workspace_root: PathBuf,
    writes_possible: bool,
    thinking_budget: u64,
    reasoning_effort: &str,
    model: String,
) -> Result<(PhiAgent, SkillResolver)> {
    // Keep a handle for the guard below — `llm_client` itself is moved into the
    // builder here.
    let guard_client = Arc::clone(&llm_client);
    // PARKED with the verify gate below: the flag only feeds the (currently
    // commented-out) `VerifyEnforcementMiddleware` wiring. Keep the parameter
    // so re-enabling the gate needs no signature change.
    let _ = writes_possible;
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
        .register_tool(RipgrepTool::new(workspace_root.clone()))
        .register_tool(RepoMapTool::new(workspace_root.clone()))
        // TESTING: low max_turns to verify nudge feature
        .execution_max_turns(256);

    // LSP diagnostics (multi-server). `LspManager` lazily starts one server per
    // language (rust-analyzer / typescript-language-server / clangd) and shares
    // them with the `diagnostics` pull tool, which reads each `publishDiagnostics`
    // cache. A server that can't be started degrades gracefully to shell commands.
    // Routing (registry-backed resolver) and the client identity are injected —
    // code-intel doesn't know about "phimint".
    let lsp_manager = LspManager::new(
        workspace_root.clone(),
        ClientInfo {
            name: "phimint".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        },
        |p| {
            code_intel::lang::lsp_spec_for_path(&p.to_string_lossy()).map(|spec| LspServerSpec {
                command: spec.command.iter().map(|s| s.to_string()).collect(),
            })
        },
    );
    builder = builder.register_tool(DiagnosticsTool::new(
        Arc::new(lsp_manager),
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
        // Sub-agents are READ-ONLY investigators (they read/search/report;
        // the main agent writes everything). The hard gate is here —
        // excluding the three mutating tools a child must never hold — while the
        // framework only *suggests* read-only via `child_read_only` (below).
        child_excluded_tools: vec![
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
        // Hang guard (§9.2): a child stuck on one task is hard-stopped after
        // 10 min and an Error result is pushed to the parent — without this,
        // a hung child would never wake the push-based parent.
        control: ControlConfig {
            task_timeout: Some(std::time::Duration::from_secs(10 * 60)),
            ..ControlConfig::default()
        },
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
        // Judge failures (timeout / parse error) fail OPEN. Session
        // 20260903_0cf95e79: with a clientless judge every check "failed" and
        // fail-closed turned that into a block on every legitimate turn-end
        // (including the fan-in "end turn to wait for sub-agents" move).
        // Letting an occasional unverified answer through is far cheaper than
        // blocking correct behavior — the next turn self-corrects.
        judge_fail_open: true,
        ..DefaultGuardConfig::default()
    };
    // Wire the judge's LLM client — `new()` leaves it None, making the judge
    // permanently unable to reach a verdict (the root cause above).
    builder = builder.guard(DefaultGuard::with_llm_client(guard_config, guard_client));

    // Max turns nudge: firm message on the last 3 turns before the hard limit.
    builder = builder.middleware(MaxTurnsNudgeMiddleware::new(MaxTurnsNudgeConfig {
        threshold: 3,
        message: "You have nearly exhausted your turn budget. \
            Stop all tool calls immediately and provide your final answer now. \
            Summarize what was accomplished and any remaining tasks."
            .to_string(),
    }));

    // Phase 6a: forced-verify gate. When the agent edits files and then tries to
    // report "done" without running `verify`, this middleware suppresses that
    // final text and injects a nudge to verify first — the "never hand back
    // non-compiling code" promise, enforced as phimint policy (the framework
    // stays neutral; see design §8.3). In `deny` mode no writes can happen, so
    // the gate is disabled (`writes_possible`).
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

#[cfg(test)]
mod prompt_guard_tests {
    //! Guards the wiring lesson from session 20260903_9255c25e: the prompt the
    //! model actually sees is THIS file's inline `SYSTEM_PROMPT`. An earlier
    //! fix edited a dead duplicate (src/prompt/mod.rs) and never reached the
    //! model — the 65× `list_agents` polling loop in that session was the
    //! result. These assertions fail if the fan-in semantics drift out of the
    //! live text.

    use super::SYSTEM_PROMPT;

    #[test]
    fn live_prompt_carries_fan_in_wait_semantics() {
        assert!(
            SYSTEM_PROMPT.contains("Results are pushed to you automatically"),
            "system prompt must state that results are pushed"
        );
        assert!(
            SYSTEM_PROMPT.contains("To wait, simply end your turn"),
            "system prompt must tell the model that ending the turn IS the wait"
        );
        assert!(
            SYSTEM_PROMPT.contains("never need to poll"),
            "system prompt must say polling is unnecessary"
        );
        assert!(
            SYSTEM_PROMPT.contains("NEVER call `list_agents` to check progress or wait"),
            "system prompt must ban list_agents polling unconditionally \
             (session 20260904_c6559510: the model polled 52x)"
        );
    }

    #[test]
    fn live_prompt_has_no_conditional_wait_loophole() {
        // Session 20260904_c6559510: "if you have no other useful work" let
        // the model declare its own analysis "useful work", keep the turn
        // open, and poll. The wait instruction must be unconditional.
        assert!(
            !SYSTEM_PROMPT.contains("if you have no other useful work"),
            "the conditional-wait loophole is back — it lets the model hold \
             the turn open whenever it invents side work"
        );
        assert!(
            !SYSTEM_PROMPT.contains("while waiting"),
            "the polling ban must not be scoped to 'while waiting' — the \
             model reasoned it was 'working', not 'waiting', and ignored it"
        );
        assert!(
            SYSTEM_PROMPT.contains("do not send them follow-up nudges"),
            "system prompt must ban mid-flight nudges (trigger=true) that \
             created extra task rounds in session 20260904_c6559510"
        );
    }

    #[test]
    fn live_prompt_has_no_pre_fan_in_wait_semantics() {
        assert!(
            !SYSTEM_PROMPT.contains("wait for them before yielding"),
            "pre-fan-in wording is back — it makes the model poll list_agents \
             instead of ending the turn"
        );
    }

    #[test]
    fn live_prompt_bans_shell_wait_loops() {
        // Session 20260904_3eeb5610: with list_agents banned, the model
        // invented `sleep 30` + poll loops as its own wait mechanism instead
        // of ending the turn. The ban must be as unconditional as the
        // list_agents one.
        assert!(
            SYSTEM_PROMPT.contains("NEVER use shell commands to pass time"),
            "system prompt must ban sleep/wait shell tricks — ending the turn \
             is the only wait mechanism"
        );
        assert!(
            SYSTEM_PROMPT.contains("if you catch yourself waiting, end the turn"),
            "the ban must name the correct replacement action"
        );
    }

    #[test]
    fn live_prompt_explains_done_delivery_semantics() {
        // Session 20260904_841ed65b: a mid-turn parent saw `done` children
        // with no reports in context and concluded "the system didn't
        // deliver" — then killed the healthy agents. `done` must be defined
        // as held-for-injection, not as something to verify or fix.
        assert!(
            SYSTEM_PROMPT.contains("A `done` status means the report is held by the runtime"),
            "system prompt must define done = held by the runtime, pending \
             injection at turn end"
        );
        assert!(
            SYSTEM_PROMPT.contains("NOT lost and NOT a delivery failure"),
            "system prompt must preempt the 'system didn't deliver' misread"
        );
        assert!(
            SYSTEM_PROMPT.contains("Ending the turn is the ONLY way to receive their reports"),
            "system prompt must make ending the turn the sole delivery action"
        );
    }

    #[test]
    fn live_prompt_bans_killing_time_with_delegated_work() {
        // Session 20260904_841ed65b, divergence point at spawn+3s: the root
        // chose to "use the waiting time" investigating the very topics it
        // had just delegated, and never ended the turn. The rationalization
        // must be banned by name.
        assert!(
            SYSTEM_PROMPT.contains("use the waiting time"),
            "system prompt must ban 'using the waiting time' to redo delegated \
             work — that was the 841ed65b divergence point"
        );
        assert!(
            !SYSTEM_PROMPT.contains("analyze the shared context"),
            "the old '(e.g. analyzing the shared context)' example reads as a \
             sanctioned waiting activity — it must stay out"
        );
    }
}
