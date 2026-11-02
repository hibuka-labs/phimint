use super::code_road::RootMarkedIds;
#[cfg(feature = "code-exec")]
use super::code_road::{
    CellSessions, CodeRoadEngine, code_artifacts_dir, code_exec_journal, code_session_dir,
};
use super::prompt::{TOKEN_BUDGET_PROMPT_SUFFIX, compose_system_prompt_for};
use super::subagents::child_multi_agent_config;
use super::*;
use phi_agent::AgentBuilder;
use std::path::Path;

/// Build a phimint agent bound to `workspace_root`.
///
/// `build` output: the agent plus the shared handles the shell keeps alive.
pub type BuildOutput = (
    PhiAgent,
    Arc<SkillResolver>,
    Arc<SkillTelemetry>,
    Arc<BackgroundTaskRegistry>,
    Arc<crate::tools::canvas::CanvasActivity>,
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
///
/// Builder-style constructor: a params struct is a post-release refactor.
/// `async` because the code-exec engine handshakes its worker at construction.
/// The tool-road face of [`build_with_code_exec`] (today's shipped behavior).
#[allow(clippy::too_many_arguments)]
pub async fn build(
    llm_client: Arc<dyn phi_agent::llm_trait::LlmProvider>,
    approval: Arc<dyn ApprovalHandler>,
    policy: Option<Arc<dyn ToolPolicy>>,
    shell_timeout_ms: u64,
    workspace_root: PathBuf,
    thinking_budget: u64,
    reasoning_effort: &str,
    model: String,
    skill_dirs: Vec<PathBuf>,
    token_budget_opts: Option<TokenBudgetOptions>,
) -> Result<BuildOutput> {
    build_with_code_exec(
        CodeExecMode::Tool,
        None,
        llm_client,
        approval,
        policy,
        shell_timeout_ms,
        workspace_root,
        thinking_budget,
        reasoning_effort,
        model,
        skill_dirs,
        token_budget_opts,
    )
    .await
}

/// [`build`] with the code road chosen at the assembly point (M3 T10d layer ③):
/// `code_exec.mode` decides what code wiring this runtime gets, and `interact`
/// is the act road's interaction slot — armed per session by `shell_wire`'s
/// boot closure with that session's `ask_user` picker queue.
#[allow(clippy::too_many_arguments)]
pub async fn build_with_code_exec(
    code_road: CodeExecMode,
    // Consumed only by the act road's mount; the no-engine build (layer ②)
    // has no act road to arm.
    #[cfg_attr(not(feature = "code-exec"), allow(unused_variables))] interact: Option<
        Arc<phi_agent_tui::InteractSlot>,
    >,
    llm_client: Arc<dyn phi_agent::llm_trait::LlmProvider>,
    approval: Arc<dyn ApprovalHandler>,
    policy: Option<Arc<dyn ToolPolicy>>,
    shell_timeout_ms: u64,
    workspace_root: PathBuf,
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
    // The shared active-skill set rides along on the resolver: phimint marks
    // names at activation/resume, the tool and catalog consult the same set
    // (single-injection guard, survey P4).
    let skill_active = ActiveSkillSet::default();
    let skill_resolver =
        Arc::new(SkillResolver::from_dirs(&skill_dirs).with_active_set(skill_active));
    let skill_telemetry = Arc::new(SkillTelemetry::new());

    // Canvas liveness for the browser tool face: created here (the middleware
    // needs it at build time), bound per session by `run`, read before
    // every LLM call. See `browser_face`.
    let canvas_activity = Arc::new(crate::tools::canvas::CanvasActivity::new());

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
    // Code road engine assembly (codexec-design §11, M3 T10d layer ③),
    // hoisted above prompt composition (提示面同源, sdk-assembly §3.4): the
    // act prompt's ns seed is `prompt_face()` of the very builder that will
    // install the packs — one pack list, one declaration source. The
    // `code-exec` feature compiles the whole block out (layer ②): no
    // feature, no engine in the binary. Config mode decides which code
    // wiring this runtime gets; without a session context
    // (`token_budget_opts` — always Some in real runs) nothing mounts. The
    // engine itself is built later, once the agent builder exists to
    // receive the tool/backend.
    #[cfg(feature = "code-exec")]
    let code_engine: Option<CodeRoadEngine> =
        match (code_road.resolved(), token_budget_opts.as_ref()) {
            (CodeExecMode::Off, _) | (_, None) => None,
            (CodeExecMode::Tool, Some(tb_opts)) => {
                // M2 road: `code_cell` on the tool-call protocol. The whole step
                // already passes phimint's approval gate (`code_cell` classifies
                // Sensitive — see approval.rs), so the cell's own mechanics ride
                // it; what the step cannot express on its own is a *device*
                // effect, so the device pack's methods carry their own consent
                // posture (posture B: reads and pointer moves flow, every
                // mutating input asks a human — see `desk_consent`).
                let limits = Limits::default();
                let engine = ExecutorBuilder::new(code_exec_journal(tb_opts)?)
                    .with_policy(ConsentPolicy::tool_road())
                    .with_limits(limits)
                    .with_ns(PhiNsShell::builtin())
                    .with_artifacts(code_artifacts_dir(tb_opts));
                // desk.* unmounted — see the note at the factory above.
                // The consent bridge: a held device action reaches the human
                // through the product's approval popup — the same queue, mode
                // switch and ledger a tool call uses. No bridge (no slot) means
                // a held action stays a hold: fail-closed, never a silent run.
                // One handle for both ends (the engine's mount and the
                // factory's fallback engine), so a standing grant the human
                // gives is remembered by the same bridge.
                let consent = interact
                    .as_ref()
                    .map(|slot| Arc::new(ConsentBridge::new(Arc::clone(slot))));
                let engine = match consent.as_ref() {
                    Some(bridge) => {
                        engine.with_consent(Arc::clone(bridge) as Arc<dyn InteractionBridge>)
                    }
                    None => engine,
                };
                Some(CodeRoadEngine::Tool {
                    engine,
                    limits,
                    consent,
                })
            }
            (CodeExecMode::Act, Some(tb_opts)) => {
                // M3 road: text turns are code steps through the action backend
                // (T10c). No tool-gate approval sits on this road, so the engine
                // policy IS the human gate: a cell's own mechanics ride (the
                // journal audits every effect), and every external-effect class
                // suspends to the consent popup through the interaction channel —
                // `sh.run` today, `desk.*` now that the device pack mounts (the
                // device methods follow the same posture as the tool road —
                // reads and pointer moves flow, mutating input asks), the net
                // pack when it installs. The `ExecCtxBridge` is one handle on
                // both ends: long-lived in the engine, armed per step by the
                // adapter.
                let bridge = Arc::new(ExecCtxBridge::new());
                let limits = Limits::default();
                let engine = ExecutorBuilder::new(code_exec_journal(tb_opts)?)
                    .with_policy(ConsentPolicy::act_road())
                    .with_limits(limits)
                    .with_ns(PhiNsShell::builtin())
                    .with_artifacts(code_artifacts_dir(tb_opts))
                    .with_interaction(bridge.clone());
                // desk.* unmounted — see the note at the factory above.
                Some(CodeRoadEngine::Act {
                    engine,
                    limits,
                    bridge,
                })
            }
        };

    // The ns seed the act section splices in — the engine's own face, or
    // nothing when no engine mounts (off road / no session context /
    // kill-switch build).
    #[cfg(feature = "code-exec")]
    let ns_face = code_engine
        .as_ref()
        .map(CodeRoadEngine::prompt_face)
        .unwrap_or_default();
    #[cfg(not(feature = "code-exec"))]
    let ns_face = String::new();

    // The file-op face key (`file_face`): the code road's engine really
    // assembled — tool (`code_cell`) or act alike, and the startup probe has
    // already guaranteed a Python for it. With no engine (off road,
    // kill-switch build, no session context) there is no code vehicle to
    // manage and the triad is the only face left.
    #[cfg(feature = "code-exec")]
    let code_engine_live = code_engine.is_some();
    #[cfg(not(feature = "code-exec"))]
    let code_engine_live = false;

    let mut system_prompt = compose_system_prompt_for(&skill_resolver, code_road, &ns_face);

    // Keep a handle for the guard below — `llm_client` itself is moved into the
    // builder here.
    let guard_client = Arc::clone(&llm_client);

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
            crate::shell::context_rotation::set_clamp_notice(notice.clone());
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
        crate::shell::context_rotation::set_global_compactor(Arc::clone(&compactor));

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

    let agent_instructions_paths = agent_instructions_paths();

    let mut builder = builder
        .system_prompt(system_prompt)
        .approval_handler(approval)
        // Session numbering is process-global (a host and its sub-agents are
        // several engines in one process — per-engine counters handed every
        // one of them a "session 1"), and the host's own sessions carry the
        // `root` mark the code-cell factory reads to pick the right policy.
        .session_id_generator(Arc::new(RootMarkedIds))
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
        .middleware(crate::shell::skills::SkillCatalogRefreshMiddleware::new(
            Arc::clone(&skill_resolver),
        ))
        // Browser tool-face loading: the 22 browser tools register below, this
        // middleware narrows the face to the canvas minimum while the design
        // canvas is live and announces the change in the system prompt. The
        // canvas opens mid-session, so the composition is per-turn — the
        // activity is bound per session by `run` (see `browser_face`).
        .middleware(crate::shell::browser_face::BrowserToolFaceMiddleware::new(
            Arc::clone(&canvas_activity),
        ))
        // File-op faces (file_face): a dead code road drops `code_cell` from
        // the wire, and the `PHIMINT_FILE_TOOL_FACE=code` rollback drops the
        // triad while code owns file work — registered unconditionally, a
        // no-op on the off road and the kill-switch build.
        .middleware(crate::shell::file_face::FileToolFaceMiddleware::new(
            code_engine_live,
            crate::shell::file_face::code_owns_file_tools(),
        ))
        // TESTING: low max_turns to verify nudge feature
        .execution_max_turns(256);

    builder = register_diagnostics(builder, &workspace_root);

    let (next, browser_tool_names) = register_browser_tools(builder);
    builder = next;

    // Code road attachment: the engine was assembled above (its face already
    // seeded the act prompt); here it lands on the agent builder — the
    // `code_cell` tool (M2 road) or the CodeAct action backend (M3 road).
    #[cfg(feature = "code-exec")]
    if let Some(code_engine) = code_engine {
        match code_engine {
            CodeRoadEngine::Tool {
                engine,
                limits,
                consent,
            } => {
                // One resident engine per session, pooled behind the tool (M2
                // tool road): the host session keeps the `AllowAll` executor
                // this eager build produced (it doubles as the startup probe),
                // and every sub-agent session opens its own audit-sealed
                // read-only engine on first use — see `CellSessions`.
                let face = engine.prompt_face();
                let executor = engine
                    .build(move |registry| PythonWorker::spawn(registry, limits))
                    .await
                    .context("code-exec engine must assemble")?;
                let session_dir = token_budget_opts
                    .as_ref()
                    .map(code_session_dir)
                    .context("code-exec needs a session context")?;
                let factory = CellSessions::new(session_dir, limits, face, executor, consent);
                builder = builder.register_tool(CodeCellTool::per_session(
                    phi_agent::code_cell::TOOL_NAME,
                    Arc::new(factory),
                ));
            }
            CodeRoadEngine::Act {
                engine,
                limits,
                bridge,
            } => {
                let executor = engine
                    .build(move |registry| PythonWorker::spawn(registry, limits))
                    .await
                    .context("code-exec engine must assemble")?;
                // The observation face wears the same cap the tool road's
                // render does — one model-face budget, both roads.
                builder = builder.action_backend(Box::new(
                    CodeActExecutor::new(executor, bridge).with_observation_cap(tool_output_cap),
                ));
                if let Some(slot) = interact {
                    // Human time is unbounded on this product (the run's
                    // cancel token is the guard) — phimint's TUI shape.
                    builder = builder.interaction(slot).interaction_timeout(None);
                }
            }
        }
    }

    // Single construction point for the child multi-agent config (the
    // argument now only picks the permission mode; the write switch is
    // fully open since T11 — see the function docs).
    builder = builder.with_multi_agent(child_multi_agent_config(
        policy.is_some(),
        &browser_tool_names,
        tool_output_cap,
    ));

    // A policy is what makes approval meaningful (see approval.rs). Only `ask`
    // and `deny` modes carry one; `auto` leaves it unset, so every call is
    // approved without consulting the handler.
    if let Some(p) = policy {
        builder = builder.tool_policy(p);
    }

    let guard_config = guard_config();
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

    let effort = reasoning_effort_from(reasoning_effort);

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

    Ok((
        agent,
        skill_resolver,
        skill_telemetry,
        bg_registry,
        canvas_activity,
    ))
}

/// The CLAUDE.md files injected into the system prompt, lowest priority first.
fn agent_instructions_paths() -> Vec<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    vec![
        home.join(".claude").join("CLAUDE.md"), // user-level (lowest priority)
        PathBuf::from(".claude/CLAUDE.md"),     // project-level (.claude/)
        PathBuf::from("CLAUDE.md"),             // project-level (root)
    ]
}

/// Register the LSP-backed `diagnostics` tool.
fn register_diagnostics(builder: AgentBuilder, workspace_root: &Path) -> AgentBuilder {
    let lsp_manager = LspManager::new(
        workspace_root.to_path_buf(),
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
    builder.register_tool(DiagnosticsTool::new(
        Arc::new(lsp_manager),
        workspace_root.to_path_buf(),
    ))
}

/// Register the canvas self-check browser tool group; returns its names for the
/// sub-agent exclusion list.
fn register_browser_tools(builder: AgentBuilder) -> (AgentBuilder, Vec<String>) {
    let browser_tools = phi_tools::browser_tools_groups(
        &BrowserToolset::lazy(BrowserLaunchOptions::default()),
        &[phi_tools::BrowserToolGroup::Canvas],
    );
    let names: Vec<String> = browser_tools
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    let mut builder = builder;
    for tool in browser_tools {
        builder = builder.register_tool_arc(tool);
    }
    (builder, names)
}

/// The reasoning-loop guard: nudge, then disable thinking, failing open.
fn guard_config() -> DefaultGuardConfig {
    DefaultGuardConfig {
        reasoning_only_nudge:
            "STOP THINKING. You have been reasoning too long without taking action. \
            IMMEDIATELY call a tool or provide your final answer. Do NOT produce more reasoning. \
            Just DO something NOW."
                .to_string(),
        reasoning_only_max_strikes: 2,
        reasoning_only_action: ReasoningOnlyAction::DisableThinking,
        disable_thinking_nudge: "Thinking has been disabled due to excessive reasoning. \
            You MUST now either call a tool or write your final answer. \
            Do NOT attempt to reason further. Just DO something NOW."
            .to_string(),
        // Judge failures fail OPEN (session 20260903_0cf95e79): fail-closed
        // blocked every legitimate turn-end when the judge had no client.
        judge_fail_open: true,
        // 32s: production judges exceeded 10s and even 20s (20260930_b81e3aec).
        judge_timeout_secs: 32,
        ..DefaultGuardConfig::default()
    }
}

/// Parse the CLI's reasoning-effort string; unknown values fall back to Medium.
fn reasoning_effort_from(name: &str) -> ReasoningEffort {
    match name.to_lowercase().as_str() {
        "none" => ReasoningEffort::None,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        _ => {
            tracing::warn!(effort = name, "unknown reasoning effort, using Medium");
            ReasoningEffort::Medium
        }
    }
}
