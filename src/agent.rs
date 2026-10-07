//! Build the phimint coding agent: system prompt + tool registration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use phi_agent::{
    ApprovalHandler, ChildPermissionMode, ControlConfig, DefaultGuard, DefaultGuardConfig,
    LocalShellTool, MaxTurnsNudgeConfig, MaxTurnsNudgeMiddleware, MemoryConfig, MultiAgentConfig,
    PhiAgent, PhiAgentConfig, ReasoningEffort, ReasoningOnlyAction, RepeatToolLimitConfig,
    RepeatToolLimitMiddleware, TokenBudgetConfig, TokenBudgetCore, ToolPolicy,
    base_agent_builder_no_compression, base_agent_builder_with_excludes,
};

use crate::context_rotation::TokenBudgetCompactor;
use crate::skills::{SkillResolver, SkillTelemetry, SkillTool, render_catalog};
use crate::tools::diagnostics::DiagnosticsTool;
use crate::tools::{repomap::RepoMapTool, ripgrep::RipgrepTool};
use code_intel::lsp::{ClientInfo, LspManager, LspServerSpec};
use phi_kernel_tools::background_shell::{BackgroundTaskRegistry, TaskCancelTool, TaskOutputTool};
use phi_kernel_tools::context_rotation::{
    HistoryStore, NotesStore, create_history_tools, create_notes_tools,
};

/// Coding-oriented system prompt (adapted from Codex).
const SYSTEM_PROMPT: &str = r#"You are phimint, a coding agent running in a terminal-based TUI. You are expected to be precise, safe, and helpful.

## Personality

Your default personality and tone is concise, direct, and friendly. You communicate efficiently, always keeping the user clearly informed about ongoing actions without unnecessary detail. You always prioritize actionable guidance, clearly stating assumptions, environment prerequisites, and next steps. Unless explicitly asked, you avoid excessively verbose explanations about your work.

## Tools available

- `repo_map` — get the codebase layout. No argument returns a directory skeleton; pass a workspace-relative `path` for per-file symbols (classes, methods, fields).
- `search_content` — search file contents with ripgrep (regex).
- `read_file` / `write_file` / `edit_file` / `list_files` — inspect and modify files (workspace-relative paths). For files under 300 lines, read the entire file at once without offset/limit. Only use pagination for very large files (over 500 lines).
- `execute_command` — run shell commands (build/test/lint). Runs inside a sandbox; destructive ops require user confirmation. Add `background: true` to run in background (returns task_id immediately).
- `task_output` — check or wait for a background task's result. Use `wait: true` to block until completion.
- `task_cancel` — terminate a running background task by task_id.
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
Sub-agents are read-only investigators by default (read/search/report). When a task must edit files or run mutating commands, request write capability for that spawn (`tools: "write"`, or a preset like "coder"/"tester") — in ask mode the user confirms each sub-agent write, and the popup names the requesting sub-agent.
- Prefer multiple sub-agents to parallelize your work. Time is a constraint so parallelism resolves the task faster.
- When spawning write-capable sub-agents, partition tasks by DISJOINT FILE SETS — never let two sub-agents modify the same file. State in each write-capable child's task text that file modifications must use `write_file`/`edit_file` — shell redirection (`echo >`, `sed -i`, `tee`) bypasses the gate and silently breaks the partition discipline. The write gate enforces this mechanically: a second sub-agent touching a held file fails with `file locked by <agent>`; when a child reports that error, re-partition the work yourself. If the holder has already finished (its spawn echo may say it recycled a finished agent), retrying the write through `write_file` works — the lock is released when the holder's task ends. Never route a retry through `execute_command` (shell redirection/writes) to dodge the lock: it bypasses the gate, silently corrupts the disjoint-file-set discipline, and the "conflict" you are working around is real information about a partitioning mistake.
- **Results are pushed to you automatically**: when every sub-agent has finished, all their full reports arrive together as one message and a new turn starts. There is no wait tool and you never need to poll. A `done` status means the report is held by the runtime — it is NOT lost and NOT a delivery failure; it is injected the instant your turn ends.
- **To wait, simply end your turn**: after spawning sub-agents, end your reply with a brief progress note and stop. Ending the turn is the ONLY way to receive their reports. Do NOT "use the waiting time" to investigate the topics you delegated — that duplicates the work you just paid sub-agents to do. End the turn FIRST, then continue your own work when the results arrive.
- NEVER call `list_agents` to check progress or wait. Polling burns tokens, does not make sub-agents finish faster, and repeated snapshots tell you nothing new. Their reports come to you whether you watch or not.
- NEVER use shell commands to pass time while sub-agents run (`sleep`, `wait`, watch loops, repeated no-op calls). Ending your turn is the ONLY wait mechanism — if you catch yourself waiting, end the turn.
- While sub-agents run, your only role is coordination: do not perform the work you delegated, do not send them follow-up nudges (e.g. "please finalize" — they are still working), and do not close them. Never hold the turn open to "keep an eye on" sub-agents, and never close agents just because their status looks quiet — a quiet `done` agent with a held report is healthy. Interrupting a running sub-agent mid-task only delays and fragments the results.
- If the user asks a question while sub-agents run, answer it first; coordination continues afterwards.
- When you have a plan with multiple steps, process them in parallel by spawning one agent per step when possible.

When done, briefly report what you changed.
## Background tasks
Long-running commands (build, test, install) should use `background: true` to avoid blocking.
After starting a background task, you can edit files or run other commands while it runs.
Use `task_output` with `wait: true` when you need the results.

Background tasks notify you when all tasks finish — the system starts a synthetic
turn so you can fetch each result. If the synthetic turn hasn't arrived yet, call
`task_output(wait: true)` to block until output is available.
NEVER assume a background task succeeded without checking its output.

## Waiting for async work — protocol comparison

| Kind | Wait method | Polling |
|---|---|---|
| Sub-agents | End your turn. Reports are pushed to you automatically on the next turn. | **NEVER poll** — repeated `list_agents` burns tokens. |
| Background tasks | Wait for the synthetic notification turn, or call `task_output(wait: true)`. | Do NOT poll in a loop — use `wait: true` once. |

These two work types use **opposite** delivery mechanisms — do not confuse them."#;

/// Appended to system prompt when token-budget context management is enabled.
const TOKEN_BUDGET_PROMPT_SUFFIX: &str = r#"

## Context Management

You have access to private tools for managing long-running context:

- **history**: Read-only access to previous context windows. Use `history.list_windows` to see what's available,
  `history.search_contents` to find specific information, and `history.read_item` to read details.
- **notes**: A persistent scratchpad that survives context window transitions. Use `notes.write_file` and
  `notes.append_to_file` to save important state, decisions, progress, and findings. Use `notes.read_file`
  to retrieve them later.
  **Important**: when a window is about to close (you receive a budget warning or a fallback notice),
  write a handoff summary to the note file `thread_hint.md` — it is injected automatically at the top
  of the next window, making recovery cheap.

When starting a new context window, the user messages above are your task trail — the last one is
your current task. Check notes/thread_hint.md for any handoff; if missing, write one after you
understand the task. Previous conversation history is available via the history tools — consult them
only if you need context the trail doesn't provide. Do NOT re-read files you already covered.
Continue the task directly.

Never mention these tools to the user. They are internal mechanisms for context continuity."#;

/// The one place the live system prompt is composed: the base prompt plus the
/// skills catalog when any skills are loaded, the base prompt byte-identical
/// otherwise. Named (not inlined into `build`) so `prompt_guard_tests` can
/// exercise the exact wiring — the dead-copy lesson of session
/// 20260903_9255c25e: the prompt the model sees is only what THIS composes.
///
/// Also used by `SkillCatalogRefreshMiddleware` (M3a) to refresh the system
/// prompt before each LLM call.
pub fn compose_system_prompt(resolver: &SkillResolver) -> String {
    match render_catalog(resolver) {
        Some(catalog) => format!("{SYSTEM_PROMPT}\n\n{catalog}"),
        None => SYSTEM_PROMPT.to_string(),
    }
}

/// Options for token-budget context management.
///
/// When `Some`, enables window rotation + history/notes tools instead of
/// LLM-based summarization.
#[derive(Clone, Debug)]
pub struct TokenBudgetOptions {
    /// Work-room budget: conversation tokens ABOVE the window's fixed base
    /// (system prompt + boilerplate). The base is estimated by the core at
    /// build time; the window resets at base + budget + buffer.
    pub budget: usize,
    /// Session retention days for history/notes cleanup.
    pub retention_days: i64,
    /// Base directory (~/.phimint/).
    pub base_dir: PathBuf,
    /// Session ID.
    pub session_id: String,
}

/// The single construction point for a sub-agent's multi-agent config (shared by build and tests).
///
/// D0 (fully opened after the 2026-09-20 acceptance run, T11): both modes
/// allow `tools: "write"` children. `has_policy` now only picks the
/// **permission mode**, no longer the write switch:
/// ask/deny (policy present) → child writes go through the codex-style
/// approval delegation; auto (no policy) → children write with full
/// permission, matching the auto-mode parent's own approval-free behavior
/// (do NOT flip the call-site argument instead — that would push auto into
/// None mode too, and auto has no approval chain to delegate to). Default
/// spawns (no tools argument) stay read-only in both modes, pinned by
/// `critical_default_spawn_children_get_no_write_tools`.
///
/// D3.1: `child_read_only` is explicitly off -- the nudge is left to the
/// framework's per-child rule (write sub-agents are no longer fed read-only
/// discipline; read-only ones are nudged via the resolved exclusion set).
///
/// eng-review finding 1: `notes.write_file` / `notes.append_to_file` are
/// mutating tools in business_tools and must be excluded explicitly (a
/// sub-agent must not pollute parent notes). `history.*` is read-only: kept.
fn child_multi_agent_config(has_policy: bool) -> MultiAgentConfig {
    MultiAgentConfig {
        // Sub-agent permission follows the approval mode (codex-style
        // delegation lives in agent-works): auto (no policy) grants full; ask/deny restrict and escalate.
        child_permission_mode: if has_policy {
            ChildPermissionMode::None
        } else {
            ChildPermissionMode::Full
        },
        // D0 (T11, opened after the 2026-09-20 acceptance run): write
        // children enabled in both modes — ask delegates approvals
        // (verified on-device), auto writes with full permission
        // (consistent with its parent).
        allow_child_write: true,
        // Sub-agents are read-only investigators by default (read/search/report).
        // The hard gate is this exclusion table; write capability means write_tools
        // members are exempted from it (framework rule), so the table only lists tools no sub-agent should get.
        child_excluded_tools: vec![
            "write_file".to_string(),
            "edit_file".to_string(),
            "execute_command".to_string(),
            "task_output".to_string(),
            "task_cancel".to_string(),
            "notes.write_file".to_string(),
            "notes.append_to_file".to_string(),
        ],
        // Sub-agents get a narrow slice; reasoning depth is capped to stop them running away (the deepseek-v4-pro lesson).
        child_reasoning_effort: Some(ReasoningEffort::Low),
        // D3.1: the global nudge is off; the per-child rule owns it.
        child_read_only: false,
        // Hang guard (S9.2): a stuck sub-agent is hard-stopped after 10 minutes and its Error is pushed to the parent.
        control: ControlConfig {
            task_timeout: Some(std::time::Duration::from_secs(10 * 60)),
            ..ControlConfig::default()
        },
        ..MultiAgentConfig::default()
    }
}

/// Build a phimint agent bound to `workspace_root`.
///
/// `build` output: the agent plus the shared handles the shell keeps alive.
pub type BuildOutput = (
    PhiAgent,
    Arc<SkillResolver>,
    Arc<SkillTelemetry>,
    Arc<BackgroundTaskRegistry>,
);

/// `base_agent_builder` already registers the file tools (read/write/edit/list).
/// We add the shell, search, and repo-map tools ourselves — none of them
/// are part of `base_agent_builder` (the phi CLI registers shell manually too).
///
/// `skill_dirs` are the directories scanned for skills; they feed both the
/// system-prompt catalog and the returned resolver (the `/skill` slash
/// command). Parameterized so tests can point the scan at a fixture directory.
///
/// Returns `(PhiAgent, SkillResolver)` — the resolver powers the `/skill` slash
/// command in the TUI loop.
// Builder-style constructor: a params struct is a post-release refactor.
#[allow(clippy::too_many_arguments)]
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
    skill_dirs: Vec<PathBuf>,
    token_budget_opts: Option<TokenBudgetOptions>,
) -> Result<BuildOutput> {
    // Skills catalog (skill-injection design D1): the resolver must exist
    // BEFORE the builder — the catalog joins the system prompt at build time,
    // not after the agent is constructed. Wrapped in Arc so the `skill` tool
    // and the TUI's `/` path share one instance.
    let skill_resolver = Arc::new(SkillResolver::from_dirs(&skill_dirs));
    let skill_telemetry = Arc::new(SkillTelemetry::new());

    // Background task registry: shared between LocalShellTool, TaskOutputTool,
    // TaskCancelTool, and the TUI tick loop.
    let bg_registry = BackgroundTaskRegistry::new(4); // max 4 concurrent bg tasks
    if !skill_resolver.is_empty() {
        tracing::info!(
            count = skill_resolver.len(),
            names = %skill_resolver.skill_names().join(", "),
            "loaded skills for /skill command"
        );
    }
    let mut system_prompt = compose_system_prompt(&skill_resolver);

    // Keep a handle for the guard below — `llm_client` itself is moved into the
    // builder here.
    let guard_client = Arc::clone(&llm_client);
    // PARKED with the verify gate below: the flag only feeds the (currently
    // commented-out) `VerifyEnforcementMiddleware` wiring. Keep the parameter
    // so re-enabling the gate needs no signature change.
    let _ = writes_possible;

    // Token-budget context management: when enabled, use window rotation +
    // history/notes tools instead of LLM-based summarization.
    let mut tool_output_cap = 16_000usize;
    let builder = if let Some(ref tb_opts) = token_budget_opts {
        // Append context management instructions to system prompt
        system_prompt.push_str(TOKEN_BUDGET_PROMPT_SUFFIX);

        let builder = base_agent_builder_no_compression(
            llm_client,
            vec!["target".to_string(), "node_modules".to_string()],
        );

        // Register TokenBudgetCompactor. `budget` is WORK ROOM — space above
        // the window's fixed base (system prompt + boilerplate), which the
        // core estimates here ONCE from the just-composed prompt. Reminder /
        // buffer thresholds scale proportionally (20% / 10%).
        let tb_config = TokenBudgetConfig::with_work_budget(tb_opts.budget);
        let core = TokenBudgetCore::new(tb_config, Some(&system_prompt));
        tracing::info!(
            work_budget = core.config().work_budget,
            base_overhead = core.base_overhead(),
            hard_limit = core.hard_limit(),
            "token-budget window rotation enabled"
        );

        // v4 A-side: the user asked for a budget below the viability floor.
        // Don't silently swallow it — surface the clamp (and the true
        // minimum) to the TUI at startup (run.rs reads the notice).
        if core.config().work_budget > tb_opts.budget {
            let notice = format!(
                "token-budget {} below viable minimum {} (system prompt ~{}); \
                 clamped. Mechanical handoff active; expect window rotation. \
                 Recommended >= {}",
                tb_opts.budget,
                core.config().work_budget,
                core.base_overhead(),
                TokenBudgetConfig::default().work_budget,
            );
            crate::context_rotation::set_clamp_notice(notice.clone());
            tracing::warn!("{}", notice);
        }

        // v4 B-side: bound one tool result to a third of the work room
        // (session 20260909_e7053736: untruncated 3-4k-token reads at a
        // 6.5k room left space for exactly one). The kernel tools already
        // self-truncate to `ToolContext::max_output_chars` — lowering the
        // builder cap is the whole mechanism, no wrapper needed. Chars ≈
        // tokens × 4 (the estimator's Latin rate); the 16k comfort cap
        // still wins at normal budgets.
        tool_output_cap = 16_000.min(core.max_result_tokens().saturating_mul(4));

        let compactor = Arc::new(TokenBudgetCompactor::new(
            core,
            tb_opts.base_dir.clone(),
            tb_opts.session_id.clone(),
            "main".to_string(),
        ));

        // Store globally so the TUI can check reset_count
        crate::context_rotation::set_global_compactor(Arc::clone(&compactor));

        // Register history + notes tools
        let history_store = Arc::new(HistoryStore::new(&tb_opts.base_dir, &tb_opts.session_id));
        let notes_store = Arc::new(NotesStore::new(
            &tb_opts.base_dir,
            &tb_opts.session_id,
            "main",
        ));

        let mut builder = builder.context_compactor(compactor);
        for tool in create_history_tools(history_store) {
            builder = builder.register_tool_arc(Arc::from(tool));
        }
        for tool in create_notes_tools(notes_store) {
            builder = builder.register_tool_arc(Arc::from(tool));
        }
        builder
    } else {
        base_agent_builder_with_excludes(
            llm_client,
            vec!["target".to_string(), "node_modules".to_string()],
        )
    };

    // Agent instruction files (CLAUDE.md): user-level + project-level.
    // phimint uses Claude Code compatible paths for seamless migration.
    // Claude Code scans all three locations; we do the same.
    let agent_instructions_paths = {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("."));

        vec![
            home.join(".claude").join("CLAUDE.md"), // user-level (lowest priority)
            std::path::PathBuf::from(".claude/CLAUDE.md"), // project-level (.claude/)
            std::path::PathBuf::from("CLAUDE.md"),  // project-level (root)
        ]
    };

    let mut builder = builder
        .system_prompt(system_prompt)
        .approval_handler(approval)
        // phimint already injects the skill catalog in compose_system_prompt --
        // agent-works' LazySkillPrompter must not append a second "## Available Skills".
        .disable_skill_prompt_injection()
        // Inject CLAUDE.md (user-level + project-level) into system prompt
        .agent_instructions_paths(agent_instructions_paths)
        // Persistent auto-memory (Phase 9c): Claude Code compatible storage at
        // `~/.claude/projects/<slug>/memory/` (slug = this workspace path), so
        // phimint and Claude Code share one memory directory. At build time the
        // framework registers the four `memory_*` tools and appends the
        // MEMORY.md index snapshot + tool guidance after the CLAUDE.md section.
        // A missing directory is fine: the index renders as an empty-index
        // placeholder and the first `memory_write` creates it. Memory tools are
        // registered on the parent only — they never join `business_tools`, so
        // the read-only sub-agent gate needs no extra exclusion entries.
        .memory_config(MemoryConfig::claude_compatible(&workspace_root))
        // base_agent_builder caps tool output at 4000 chars and REJECTS (rather
        // than truncates) anything larger. That is too small to read a normal
        // source file — read_file's own default limit is 2000 *lines*, so a
        // ~100-line file already overflows the char cap. Raise it to fit a few
        // hundred lines; the system prompt tells the agent to paginate beyond.
        // (Token-budget mode lowers it further — see tool_output_cap above.)
        .max_tool_output_chars(tool_output_cap)
        .register_tool(LocalShellTool::new(shell_timeout_ms).with_registry(bg_registry.clone()))
        .register_tool(TaskOutputTool::new(bg_registry.clone()))
        .register_tool(TaskCancelTool::new(bg_registry.clone()))
        .register_tool(RipgrepTool::new(workspace_root.clone()))
        .register_tool(RepoMapTool::new(workspace_root.clone()))
        // skill tool (skill-injection D2): model-initiated skill body loader.
        .register_tool(SkillTool::new(
            Arc::clone(&skill_resolver),
            Arc::clone(&skill_telemetry),
        ))
        // Per-turn catalog refresh (skill-injection M3a): in-place swap of the
        // catalog section before each LLM call — the builder injected one copy
        // at build time, this middleware keeps it current without duplication.
        .middleware(crate::skills::SkillCatalogRefreshMiddleware::new(
            Arc::clone(&skill_resolver),
        ))
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

    // Single construction point for the child multi-agent config (the
    // argument now only picks the permission mode; the write switch is
    // fully open since T11 — see the function docs).
    builder = builder.with_multi_agent(child_multi_agent_config(policy.is_some()));

    // A policy is what makes approval meaningful (see approval.rs). Only `ask`
    // and `deny` modes carry one; `auto` leaves it unset, so every call is
    // approved without consulting the handler.
    if let Some(p) = policy {
        builder = builder.tool_policy(p);
    }

    // Custom guard: inject "stop thinking, act now" nudge for reasoning-only responses
    // Use DisableThinking strategy to handle reasoning-only loops
    let guard_config = DefaultGuardConfig {
        reasoning_only_nudge:
            "STOP THINKING. You have been reasoning too long without taking action. \
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
        // Production sessions (2026-09-22, d84374ca/bfef1017) show the judge
        // model routinely exceeds the 10s default — every session ended with
        // "judge timeout after 10s" and a permanently ineffective judge.
        // 2026-09-30 (20260930_b81e3aec): still timed out at 20s on a fast
        // flash model. 32s is the compromise — slower third-party models need
        // the headroom; beyond that the judge is not worth waiting for.
        judge_timeout_secs: 32,
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

    // Repeat-tool limit: break poll loops — repeated identical tool calls.
    // Nudge at 5 identical calls (per fingerprint, cumulative per run);
    // hard-block at 10 (discards pending calls, forces text-only summary).
    builder = builder.middleware(RepeatToolLimitMiddleware::new(
        RepeatToolLimitConfig::default(),
    ));

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
            tracing::warn!(
                effort = reasoning_effort,
                "unknown reasoning effort, using Medium"
            );
            ReasoningEffort::Medium
        }
    };

    let agent = PhiAgent::build(
        builder,
        PhiAgentConfig {
            enable_thinking: true,
            thinking_budget: Some(thinking_budget),
            thinking_effort: effort,
            model,
            ..PhiAgentConfig::default()
        },
    )?;

    Ok((agent, skill_resolver, skill_telemetry, bg_registry))
}

#[cfg(test)]
mod prompt_guard_tests {
    //! Guards the wiring lesson from session 20260903_9255c25e: the prompt the
    //! model actually sees is THIS file's inline `SYSTEM_PROMPT`. An earlier
    //! fix edited a dead duplicate (src/prompt/mod.rs) and never reached the
    //! model — the 65× `list_agents` polling loop in that session was the
    //! result. These assertions fail if the fan-in semantics drift out of the
    //! live text.

    use super::{MemoryConfig, SYSTEM_PROMPT, compose_system_prompt};
    use crate::skills::SkillResolver;

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

    // ── Skills catalog wiring (skill-injection design M1) ──
    //
    // Same lesson as above, one level up: the catalog must reach the prompt
    // the MODEL actually receives. `compose_system_prompt` is guarded at the
    // function level (these tests), and the two `built_agent_*` tests below
    // run a real `build()` + turn against a capturing mock provider — delete
    // the compose call in `build()` and those go red.

    use futures_util;
    use phi_agent::llm_trait::{
        Capabilities, ChatMessage, ChatRequest, ChatResponse, ChatStream, FinishReason, LlmError,
        LlmProvider, ProviderInfo, StreamChunk, UsageInfo,
    };
    use std::sync::{Arc, Mutex};

    /// LLM stub that records the system prompt of every request it receives
    /// and answers with a tool-free "done" (the turn ends immediately).
    struct CapturingMockProvider {
        system_prompts: Mutex<Vec<String>>,
    }

    impl CapturingMockProvider {
        fn record(&self, request: &ChatRequest) {
            if let Some(ChatMessage::System { content, .. }) = request.messages.first() {
                self.system_prompts.lock().unwrap().push(content.clone());
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for CapturingMockProvider {
        async fn stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
            self.record(&request);
            let chunks: Vec<Result<StreamChunk, LlmError>> = vec![
                Ok(StreamChunk::Text("done".to_string())),
                Ok(StreamChunk::Stop {
                    finish_reason: Some("stop".to_string()),
                }),
            ];
            Ok(ChatStream::new(Box::pin(futures_util::stream::iter(
                chunks,
            ))))
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
            self.record(&request);
            Ok(ChatResponse {
                content: "done".to_string(),
                reasoning_content: None,
                thinking_signature: None,
                tool_calls: vec![],
                usage: UsageInfo::default(),
                finish_reason: FinishReason::Stop,
                raw: None,
            })
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_streaming: true,
                supports_tools: true,
                ..Default::default()
            }
        }

        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "mock".to_string(),
                model: "mock-model".to_string(),
                version: None,
            }
        }
    }

    /// Fixture: one skill in `<tmp>/skills/catalog-skill/SKILL.md`.
    fn fixture_skill_dir(tmp: &std::path::Path) -> std::path::PathBuf {
        let dir = tmp.join("skills").join("catalog-skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: catalog-skill\ndescription: guard fixture skill\n\
             user-invocable: true\n---\n\nfixture body",
        )
        .unwrap();
        tmp.join("skills")
    }

    /// Build a real agent via `build()` against a self-managed workspace and
    /// run one turn against the capturing mock, returning every system prompt
    /// the LLM received.
    async fn system_prompts_received(skill_dirs: Vec<std::path::PathBuf>) -> Vec<String> {
        let workspace = tempfile::tempdir().unwrap();
        system_prompts_received_in(workspace.path(), skill_dirs).await
    }

    /// Same as [`system_prompts_received`] but with a caller-owned workspace —
    /// needed when an assertion must reference the exact workspace the agent
    /// was built against (e.g. the memory root slug, Phase 9c).
    async fn system_prompts_received_in(
        workspace: &std::path::Path,
        skill_dirs: Vec<std::path::PathBuf>,
    ) -> Vec<String> {
        let provider = Arc::new(CapturingMockProvider {
            system_prompts: Mutex::new(Vec::new()),
        });
        let (approval, policy) = crate::approval::build_approval("auto");
        let (agent, _resolver, _telemetry, _bg_registry) = super::build(
            provider.clone() as Arc<dyn LlmProvider>,
            approval,
            policy,
            1_000,
            workspace.to_path_buf(),
            true,
            1024,
            "low",
            "mock-model".to_string(),
            skill_dirs,
            None,
        )
        .unwrap();
        let session = agent.create_session().await;
        agent
            .run_turn(session, "hello", |_ev| Ok(()))
            .await
            .unwrap();

        let prompts = provider.system_prompts.lock().unwrap().clone();
        assert!(
            !prompts.is_empty(),
            "mock provider must receive at least one request"
        );
        prompts
    }

    #[test]
    fn compose_appends_catalog_when_skills_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = fixture_skill_dir(tmp.path());
        let resolver = crate::skills::SkillResolver::from_dirs(&[skill_dir]);

        let prompt = compose_system_prompt(&resolver);
        assert!(
            prompt.starts_with(SYSTEM_PROMPT),
            "catalog must be appended AFTER the base prompt"
        );
        assert!(
            prompt[SYSTEM_PROMPT.len()..].contains("## Skills"),
            "{prompt}"
        );
        assert!(
            prompt.contains("- catalog-skill: guard fixture skill\n"),
            "{prompt}"
        );
        assert!(prompt.contains("### How to use skills"), "{prompt}");
        assert!(
            prompt.contains(
                "Skills the user activated with a slash command remain in effect for the whole session"
            ),
            "{prompt}"
        );
    }

    #[test]
    fn compose_byte_identical_without_skills() {
        // M1 acceptance: an empty skill environment must not perturb the
        // prompt AT ALL — byte-identical to the pre-change baseline.
        let tmp = tempfile::tempdir().unwrap();
        let resolver = crate::skills::SkillResolver::from_dirs(&[tmp.path().join("nonexistent")]);
        assert_eq!(compose_system_prompt(&resolver), SYSTEM_PROMPT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn built_agent_receives_catalog_in_live_prompt() {
        // The wiring guard: if the compose call is dropped from `build()`,
        // the model stops seeing the catalog and THIS test goes red —
        // exactly the dead-copy failure mode of session 20260903_9255c25e.
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = fixture_skill_dir(tmp.path());
        let prompts = system_prompts_received(vec![skill_dir]).await;

        assert!(
            prompts.iter().any(|p| {
                p.starts_with(SYSTEM_PROMPT)
                    && p.contains("## Skills")
                    && p.contains("- catalog-skill: guard fixture skill\n")
            }),
            "the live request must carry base prompt + catalog; got {} request(s)",
            prompts.len()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_scope_bake_reaches_live_prompt() {
        // Skill-lifetime v3 wiring guard: activating a session-scope skill
        // must change what the LLM sees — the request AFTER the bake carries
        // the Active Skills section, the request before it does not. Mirrors
        // agent_loop's Session branch (activate → compose → set_system_prompt
        // → run the raw command) against the real runtime.
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = fixture_skill_dir(tmp.path());
        let workspace = tempfile::tempdir().unwrap();

        let provider = Arc::new(CapturingMockProvider {
            system_prompts: Mutex::new(Vec::new()),
        });
        let (approval, policy) = crate::approval::build_approval("auto");
        let (agent, resolver, _telemetry, _bg_registry) = super::build(
            provider.clone() as Arc<dyn LlmProvider>,
            approval,
            policy,
            1_000,
            workspace.path().to_path_buf(),
            true,
            1024,
            "low",
            "mock-model".to_string(),
            vec![skill_dir],
            None,
        )
        .unwrap();
        let session = agent.create_session().await;
        agent
            .run_turn(session.clone(), "warmup", |_ev| Ok(()))
            .await
            .unwrap();

        // The pristine build-time prompt (catalog + CLAUDE.md + memory index)
        // — exactly what agent_loop captures once before any bake. Baking
        // must APPEND to this, never recompose from the resolver.
        let bake_base = agent
            .system_prompt()
            .await
            .expect("phimint build always sets a system prompt");

        // Session-branch dispatch: activate, re-bake, run the raw command.
        let input = "/catalog-skill".to_string();
        let r = resolver
            .resolve_with_meta(&input)
            .expect("fixture mismatch");
        assert_eq!(r.scope, crate::skills::SkillScope::Session);
        let active = vec![crate::skills::ActiveSkillEntry {
            name: r.name.clone(),
            body: r.body.clone(),
        }];
        agent
            .set_system_prompt(
                &session,
                crate::skills::append_active_skills(&bake_base, &active),
            )
            .await
            .unwrap();
        agent.run_turn(session, &input, |_ev| Ok(())).await.unwrap();

        let prompts = provider.system_prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2, "two turns → two requests");
        assert!(
            !prompts[0].contains("## Active Skills"),
            "pre-bake request must not carry the section: {}",
            prompts[0]
        );
        // starts_with(bake_base) proves the bake preserved EVERYTHING the
        // builder injected (catalog, CLAUDE.md, memory index) — recomposing
        // from the resolver would strip them and fail here.
        assert!(
            prompts[1].starts_with(&bake_base),
            "post-bake prompt must extend the pristine build-time prompt verbatim"
        );
        assert!(
            prompts[1].contains("## Active Skills") && prompts[1].contains("fixture body"),
            "post-bake request must carry the baked section: {}",
            prompts[1]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn built_agent_without_skills_receives_base_prompt_verbatim() {
        // Wiring-level version of the byte-identical acceptance: build() with
        // an (empty) skill dir must send the untouched SYSTEM_PROMPT. The
        // runtime appends its own fixed guidance after whatever we pass, so
        // the assertion is: our base prompt verbatim up front, and NO catalog
        // fragment anywhere. Exact byte-identity of the composed prompt is
        // pinned by `compose_byte_identical_without_skills`.
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("skills");
        std::fs::create_dir_all(&empty).unwrap();
        let prompts = system_prompts_received(vec![empty]).await;

        assert!(
            prompts
                .iter()
                .any(|p| p.starts_with(SYSTEM_PROMPT) && !p.contains("## Skills")),
            "with no skills the live prompt must be the base prompt with no catalog fragment"
        );
    }

    // ── Skill catalog double-injection guard ──

    #[test]
    fn compose_system_prompt_has_exactly_one_skills_heading() {
        // build() calls compose_system_prompt which embeds one catalog copy.
        // If disable_skill_prompt_injection() were missing, agent-works'
        // LazySkillPrompter would append a second "## Available Skills" section
        // at build() time — the model would see two separate skill listings.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills").join("review");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: review\ndescription: d\nuser-invocable: true\n---\n\nreview body",
        )
        .unwrap();
        let resolver = SkillResolver::from_dirs(&[tmp.path().join("skills")]);
        let prompt = compose_system_prompt(&resolver);
        assert_eq!(
            prompt.matches("## Skills").count(),
            1,
            "compose_system_prompt must produce exactly one ## Skills heading"
        );
        assert!(
            !prompt.contains("## Available Skills"),
            "compose_system_prompt must not contain agent-works' '## Available Skills' heading"
        );
    }

    // ── Integration: disable_skill_prompt_injection end-to-end ──

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn built_agent_has_no_duplicate_skills_heading() {
        // Verifies disable_skill_prompt_injection() end-to-end: the model
        // must see exactly one "## Skills" heading and zero "## Available Skills"
        // headings. The unit test compose_system_prompt_has_exactly_one_skills_heading
        // covers the pure function; this covers the full build() → run_turn() path.
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = fixture_skill_dir(tmp.path());
        let prompts = system_prompts_received(vec![skill_dir]).await;

        for (i, p) in prompts.iter().enumerate() {
            let skills_count = p.matches("## Skills").count();
            assert_eq!(
                skills_count, 1,
                "request {i}: expected exactly one '## Skills' heading, found {skills_count}"
            );
            assert!(
                !p.contains("## Available Skills"),
                "request {i}: must not contain agent-works' '## Available Skills' heading"
            );
        }
    }

    #[test]
    fn system_prompt_base_has_no_available_skills_heading() {
        // Red-team guard: the raw SYSTEM_PROMPT constant must never contain
        // the agent-works LazySkillPrompter heading — that would mean the
        // base prompt itself carries a second catalog section.
        assert!(
            !SYSTEM_PROMPT.contains("## Available Skills"),
            "SYSTEM_PROMPT must not contain agent-works' '## Available Skills' heading"
        );
    }

    // ── Background task prompt guards ──

    #[test]
    fn prompt_has_task_output_description() {
        assert!(
            SYSTEM_PROMPT.contains("task_output"),
            "system prompt must mention task_output tool"
        );
        assert!(
            SYSTEM_PROMPT.contains("wait: true"),
            "system prompt must describe wait: true usage"
        );
    }

    #[test]
    fn prompt_has_task_cancel_description() {
        assert!(
            SYSTEM_PROMPT.contains("task_cancel"),
            "system prompt must mention task_cancel tool"
        );
    }

    #[test]
    fn prompt_has_background_tasks_section() {
        assert!(
            SYSTEM_PROMPT.contains("## Background tasks"),
            "system prompt must have a ## Background tasks section"
        );
    }

    #[test]
    fn prompt_has_background_true_keyword() {
        assert!(
            SYSTEM_PROMPT.contains("background: true"),
            "system prompt must mention background: true"
        );
    }

    #[test]
    fn prompt_explains_background_notification_semantics() {
        // bg_wake actually pushes a synthetic turn; the prompt now
        // accurately reflects this (session 20260914_50cf809d found the
        // old "do NOT notify" wording contradicted the mechanism and
        // contributed to the protocol confusion with sub-agents).
        assert!(
            SYSTEM_PROMPT.contains("Background tasks notify you when all tasks finish"),
            "system prompt must accurately describe bg_wake notification"
        );
        assert!(
            SYSTEM_PROMPT.contains("NEVER assume a background task succeeded"),
            "system prompt must ban assuming success without checking output"
        );
    }

    #[test]
    fn child_excluded_tools_include_background_tools() {
        // Read the source to verify the wiring (compile-time guard is harder;
        // this is a source-level guard that catches accidental removal).
        let src = include_str!("agent.rs");
        assert!(
            src.contains("\"task_output\""),
            "child_excluded_tools must include task_output"
        );
        assert!(
            src.contains("\"task_cancel\""),
            "child_excluded_tools must include task_cancel"
        );
    }

    // ── Phase 9c memory wiring guards ──

    #[test]
    fn system_prompt_base_has_no_memory_section() {
        // The memory section is appended by the FRAMEWORK at build time
        // (apply_memory_config, after the CLAUDE.md injection) — it must never
        // be baked into the base constant, or the skills-catalog guards above
        // and the no-skills byte-identical baseline would gain a second moving
        // part.
        assert!(
            !SYSTEM_PROMPT.contains("## Memory"),
            "SYSTEM_PROMPT must not inline the memory section; the framework \
             appends it when memory_config is wired"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn built_agent_receives_memory_prompt_for_its_workspace() {
        // Wiring guard: with memory_config wired in build(), the live request
        // must carry the memory section anchored to THIS workspace's
        // Claude-Code-compatible root (~/.claude/projects/<slug>/memory/).
        // apply_memory_config registers the four memory_* tools and injects
        // the prompt in ONE code path — the section being present transitively
        // proves the tools are registered.
        //
        // Regression (9c acceptance): the tmp workspace has no memory
        // directory and no CLAUDE.md — startup must stay graceful, with the
        // empty-index placeholder instead of an error.
        let tmp = tempfile::tempdir().unwrap();
        let expected_root = MemoryConfig::claude_compatible(tmp.path())
            .memory_root
            .display()
            .to_string();
        let prompts = system_prompts_received_in(tmp.path(), vec![tmp.path().join("skills")]).await;

        assert!(
            prompts.iter().any(|p| {
                p.starts_with(SYSTEM_PROMPT)
                    && p.contains("## Memory")
                    && p.contains("memory_write")
                    && p.contains(&expected_root)
            }),
            "the live prompt must carry base prompt + memory section naming \
             this workspace's memory root {expected_root}; got {} request(s)",
            prompts.len()
        );
        // Empty-index placeholder proves the no-memory-directory regression:
        // startup renders the friendly placeholder, never a failure.
        assert!(
            prompts.iter().any(|p| p.contains("no memories yet")),
            "with no memory directory the prompt must contain the empty-index \
             placeholder (graceful first-use startup)"
        );
    }

    use phi_agent::{ChildToolCapability, resolve_capability};

    /// T9 (outside voice finding 4): the task-boundary guidance for parallel write sub-agents must stay resident.
    #[test]
    fn system_prompt_demands_disjoint_file_sets_for_parallel_writers() {
        assert!(
            SYSTEM_PROMPT.contains("DISJOINT FILE SETS"),
            "父 prompt 必须要求按文件不相交分派并行写任务"
        );
        assert!(
            SYSTEM_PROMPT.contains("file locked by"),
            "父 prompt 必须解释写门指名错误及其处置（重新分界）"
        );
    }

    /// The "read-only by default, explicit opt-in" wording replaces the old blanket read-only assertion.
    #[test]
    fn system_prompt_states_default_read_only_with_opt_in_write() {
        assert!(
            SYSTEM_PROMPT.contains("read-only investigators by default"),
            "子 agent 默认只读的新表述必须存在"
        );
        assert!(
            SYSTEM_PROMPT.contains("tools: \"write\""),
            "父 prompt 必须告诉父 agent 如何为单个 spawn 请求写能力"
        );
    }

    /// T8+T11: single construction point of the child config (shared with
    /// build). Since T11 the write switch is open in both modes; the
    /// argument only picks the permission mode.
    #[test]
    fn child_config_wiring_write_open_and_notes_excluded() {
        let ask = super::child_multi_agent_config(true);
        assert!(
            ask.allow_child_write,
            "D0: ask/deny (with policy) allows write children"
        );
        assert_eq!(
            ask.child_permission_mode,
            phi_agent::ChildPermissionMode::None,
            "approval-mode children go through the delegation chain"
        );
        let auto = super::child_multi_agent_config(false);
        assert!(
            auto.allow_child_write,
            "T11 (2026-09-20 acceptance run): auto also allows write children"
        );
        assert_eq!(
            auto.child_permission_mode,
            phi_agent::ChildPermissionMode::Full,
            "auto has no approval chain; children must keep Full permission (never None mode)"
        );
        // D3.1: the computation rule owns the nudge; phimint turns the global nudge off explicitly.
        assert!(!ask.child_read_only);
        assert!(!auto.child_read_only);
        // eng-review finding 1: mutating notes tools must be excluded (a sub-agent must not pollute parent notes).
        for t in ["notes.write_file", "notes.append_to_file"] {
            assert!(
                ask.child_excluded_tools.iter().any(|e| e == t),
                "{t} must be in child_excluded_tools"
            );
        }
    }

    /// CRITICAL regression (design S5/S8): a default spawn (no `tools` arg ->
    /// read_only) must have a resolved exclusion set covering **all** write tools
    /// (notes.* included) -- this pins the behaviour for read-only scenarios like /review.
    #[test]
    fn critical_default_spawn_children_get_no_write_tools() {
        let write_tools = ["write_file", "edit_file", "execute_command"];
        let mutating = [
            "write_file",
            "edit_file",
            "execute_command",
            "notes.write_file",
            "notes.append_to_file",
        ];
        for cfg in [
            super::child_multi_agent_config(true),
            super::child_multi_agent_config(false),
        ] {
            for cap in [None, Some(ChildToolCapability::ReadOnly)] {
                let res = resolve_capability(
                    cap.as_ref(),
                    cfg.control.autonomy,
                    cfg.allow_child_write,
                    &cfg.child_excluded_tools,
                    &cfg.control.write_tools,
                );
                for t in mutating {
                    assert!(
                        res.excluded_tools.contains(t),
                        "{t} must stay excluded for default spawn (cap={cap:?})"
                    );
                }
                // Defensive: write_tools really is the default trio (this assertion flags a framework default change for review).
                for t in write_tools {
                    assert!(cfg.control.write_tools.iter().any(|w| w == t));
                }
            }
        }
    }

    /// The write sub-agent surface (D0, ask mode): under a write request the write tools are exempt and registerable.
    #[test]
    fn ask_mode_write_request_exempts_write_tools() {
        let cfg = super::child_multi_agent_config(true);
        let res = resolve_capability(
            Some(&ChildToolCapability::Write),
            cfg.control.autonomy,
            cfg.allow_child_write,
            &cfg.child_excluded_tools,
            &cfg.control.write_tools,
        );
        for t in ["write_file", "edit_file", "execute_command"] {
            assert!(!res.excluded_tools.contains(t), "write 子 agent 需 {t}");
        }
        // Only write_tools members are exempt -- notes.* is not in write_tools, so it stays excluded.
        assert!(res.excluded_tools.contains("notes.write_file"));
        assert!(res.excluded_tools.contains("task_output"));
        assert!(res.degraded_reason.is_none());
    }

    /// Write-child surface (T11 auto mode): a write request exempts the
    /// write tools here too — auto's write switch opened with the
    /// acceptance run, permission mode stays Full (see the
    /// child_config_wiring test).
    #[test]
    fn auto_mode_write_request_exempts_write_tools() {
        let cfg = super::child_multi_agent_config(false);
        let res = resolve_capability(
            Some(&ChildToolCapability::Write),
            cfg.control.autonomy,
            cfg.allow_child_write,
            &cfg.child_excluded_tools,
            &cfg.control.write_tools,
        );
        for t in ["write_file", "edit_file", "execute_command"] {
            assert!(
                !res.excluded_tools.contains(t),
                "auto write child needs {t}"
            );
        }
        assert!(res.degraded_reason.is_none());
    }
}
