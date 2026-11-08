use super::super::code_road::*;
use super::super::prompt::*;
use super::super::subagents::*;
use super::super::*;
use super::support::*;
use crate::shell::skills::SkillResolver;
use phi_agent::llm_trait::LlmProvider;
use phi_agent::{ChildToolCapability, resolve_capability};
use std::sync::{Arc, Mutex};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn built_agent_receives_catalog_in_live_prompt() {
    // The wiring guard: if the compose call is dropped from `build()`,
    // the model stops seeing the catalog and THIS test goes red —
    // exactly the dead-copy failure mode of session 20260903_9255c25e.
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = fixture_skill_dir(tmp.path());
    let prompts = system_prompts_received(vec![skill_dir]).await;

    let base = base_prompt();
    assert!(
        prompts.iter().any(|p| {
            p.starts_with(base.as_str())
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
        tools: Mutex::new(Vec::new()),
    });
    let (approval, policy, _approval_rx, _mode) =
        crate::approval::build_live_approval("auto", None);
    let (agent, resolver, _telemetry, _bg_registry, _canvas_activity) = build(
        provider.clone() as Arc<dyn LlmProvider>,
        approval,
        policy,
        1_000,
        workspace.path().to_path_buf(),
        1024,
        "low",
        "mock-model".to_string(),
        vec![skill_dir],
        None,
        None,
    )
    .await
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
    assert_eq!(r.scope, crate::shell::skills::SkillScope::Session);
    let active = vec![crate::shell::skills::ActiveSkillEntry {
        name: r.name.clone(),
        body: r.body.clone(),
    }];
    agent
        .set_system_prompt(
            &session,
            crate::shell::skills::append_active_skills(&bake_base, &active),
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

    let base = base_prompt();
    assert!(
        prompts
            .iter()
            .any(|p| p.starts_with(base.as_str()) && !p.contains("## Skills")),
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
    let src = include_str!("../subagents.rs");
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

    let base = base_prompt();
    assert!(
        prompts.iter().any(|p| {
            p.starts_with(base.as_str())
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

/// T9 (outside voice finding 4): the task-boundary guidance for parallel write sub-agents must stay resident.
#[test]
fn system_prompt_demands_disjoint_file_sets_for_parallel_writers() {
    assert!(
        SYSTEM_PROMPT.contains("DISJOINT FILE SETS"),
        "the parent prompt must demand disjoint file sets when dispatching parallel write tasks"
    );
    assert!(
        SYSTEM_PROMPT.contains("file locked by"),
        "the parent prompt must explain the write-gate naming error and its handling (re-partitioning)"
    );
}

/// The "read-only by default, explicit opt-in" wording replaces the old blanket read-only assertion.
#[test]
fn system_prompt_states_default_read_only_with_opt_in_write() {
    assert!(
        SYSTEM_PROMPT.contains("read-only investigators by default"),
        "the new wording that sub-agents are read-only by default must be present"
    );
    assert!(
        SYSTEM_PROMPT.contains("tools: \"write\""),
        "the parent prompt must tell the parent agent how to spell out the capability for a single spawn request"
    );
    // Session 20261029_0af8bfc6: the parent spawned three analysis
    // sub-agents with `tools: "researcher"` and each got a bare
    // read_file+list_files pair (the preset whitelist) — ~100 serial
    // read_file calls, no repo_map/search_content. The prompt must steer
    // read-only spawns to the default kit instead.
    assert!(
        SYSTEM_PROMPT.contains("For read-only investigation OMIT `tools`"),
        "the parent prompt must steer read-only spawns to the default read kit"
    );
    assert!(
        SYSTEM_PROMPT.contains("grinds a codebase file-by-file"),
        "the parent prompt must say why the researcher/reviewer presets are the wrong tool face for codebase work"
    );
    // Sub-agents read via their own read-only code cells (2026-10-29, the
    // codexec-in-children round): the prompt must state the split — cells
    // may read, writes are refused at the seam. D13 named exception
    // (child-kit mirror, same day): the sentence moved off the base
    // prompt into the tool-road child-kit rewrite — the base is the
    // no-cell face and may not promise cells — so the pin asserts the
    // prompt a live tool-road run composes.
    #[cfg(feature = "code-exec")]
    assert!(
        code_road_prompt(crate::model::config::CodeExecMode::Tool, "")
            .contains("every write is refused"),
        "the tool-road prompt must say a sub-agent's cells are read-only"
    );
    assert!(
        !SYSTEM_PROMPT.contains("every write is refused"),
        "the no-cell face must not promise cell semantics"
    );
}

/// The child kit on the tool-road prompt (2026-11-01: both vehicles ride
/// a child's menu, the read pair restored): the prompt must say the kit
/// carries `code_cell` alongside the read set, pin its read-only seam,
/// and pass the file-work dispatch rule down. The base prompt keeps the
/// read-set sentence intact (anchor proof — a missed anchor degrades to
/// the shorter truth).
#[cfg(feature = "code-exec")]
#[test]
fn tool_road_prompt_mirrors_the_child_kit() {
    let prompt = code_road_prompt(crate::model::config::CodeExecMode::Tool, "");
    for here in [
        "(`repo_map`, `search_content`, `read_file`, `list_files`, `code_cell`)",
        "every write is refused",
        "Pass the file-work dispatch rule down too: several files to read → one `read_file` with `paths: [...]`, a chain of file operations → one cell.",
    ] {
        assert!(prompt.contains(here), "tool-road prompt must teach: {here}");
    }
    // The base face keeps the read pair and the preset comparison.
    assert!(
        SYSTEM_PROMPT.contains("(`repo_map`, `search_content`, `read_file`, `list_files`)"),
        "the base prompt must promise the read pair every kit carries"
    );
    assert!(
        SYSTEM_PROMPT.contains("grinds a codebase file-by-file"),
        "the preset comparison is true on every road again and stays"
    );
}

/// T8+T11: single construction point of the child config (shared with
/// build). Since T11 the write switch is open in both modes; the
/// argument only picks the permission mode.
#[test]
fn child_config_wiring_write_open_and_notes_excluded() {
    let ask = child_multi_agent_config(true, &browser_tool_names(), 16_000);
    assert!(
        ask.allow_child_write,
        "D0: ask/deny (with policy) allows write children"
    );
    assert_eq!(
        ask.child_permission_mode,
        phi_agent::ChildPermissionMode::None,
        "approval-mode children go through the delegation chain"
    );
    let auto = child_multi_agent_config(false, &browser_tool_names(), 16_000);
    assert!(
        auto.allow_child_write,
        "T11 (2026-09-20 acceptance run): auto also allows write children"
    );
    assert_eq!(
        auto.child_permission_mode,
        phi_agent::ChildPermissionMode::Full,
        "no-policy callers have no delegation chain; children must keep Full permission (never None mode)"
    );
    // D3.1: the computation rule owns the nudge; phimint turns the global nudge off explicitly.
    assert!(!ask.child_read_only);
    assert!(!auto.child_read_only);
    // eng-review finding 1: mutating notes tools must be excluded (a sub-agent must not pollute parent notes).
    for t in ["notes_write_file", "notes_append_to_file"] {
        assert!(
            ask.child_excluded_tools.iter().any(|e| e == t),
            "{t} must be in child_excluded_tools"
        );
    }
    // Cells ride the child kit (2026-10-29): the read-only engine policy
    // is the gate (see `CellSessions`), not an exclusion entry.
    for cfg in [&ask, &auto] {
        assert!(
            !cfg.child_excluded_tools.iter().any(|e| e == "code_cell"),
            "sub-agents read via their own read-only code cells"
        );
    }
}

/// 2026-11-01: the §6.5 valve must reach child engines too — before this
/// wiring, `ToolConfig::default()` left children uncapped and a child's
/// 29k-char read sailed past the root's 16k cap.
#[test]
fn child_config_threads_the_tool_output_cap() {
    let cfg = child_multi_agent_config(true, &browser_tool_names(), 16_000);
    assert_eq!(
        cfg.child_max_tool_output_chars,
        Some(16_000),
        "children share the root's per-call tool-output valve"
    );
}

/// Both file vehicles ride every child kit (2026-11-01 — the inversion of
/// the 2026-10-29 child face): the serial read pair is never cut, whether
/// or not `code_cell` rides alongside it. The pair is not in
/// `write_tools`, so the write exemption never removes it either — a
/// write child reads via the pair or its cells and writes via the gated
/// write tools.
#[test]
fn child_read_pair_rides_every_kit() {
    let cfg = child_multi_agent_config(true, &browser_tool_names(), 16_000);
    for t in ["read_file", "list_files"] {
        assert!(
            !cfg.child_excluded_tools.iter().any(|e| e == t),
            "{t} is the single-file vehicle and stays on the child menu"
        );
    }
    let res = resolve_capability(
        Some(&ChildToolCapability::Write),
        cfg.control.autonomy,
        cfg.allow_child_write,
        &cfg.child_excluded_tools,
        &cfg.control.write_tools,
    );
    for t in ["read_file", "list_files"] {
        assert!(
            !res.excluded_tools.contains(t),
            "{t} stays on the menu even for a write child"
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
        "notes_write_file",
        "notes_append_to_file",
    ];
    for cfg in [
        child_multi_agent_config(true, &browser_tool_names(), 16_000),
        child_multi_agent_config(false, &browser_tool_names(), 16_000),
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
    let cfg = child_multi_agent_config(true, &browser_tool_names(), 16_000);
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
            "a write sub-agent needs {t}"
        );
    }
    // Only write_tools members are exempt -- notes.* is not in write_tools, so it stays excluded.
    assert!(res.excluded_tools.contains("notes_write_file"));
    assert!(res.excluded_tools.contains("task_output"));
    // Browser tools are not in write_tools either, so the write exemption
    // must not hand a child the shared Chrome session.
    for t in browser_tool_names() {
        assert!(
            res.excluded_tools.contains(&t),
            "{t} must stay excluded even for a write child"
        );
    }
    assert!(res.degraded_reason.is_none());
}

/// Every browser tool belongs to the parent alone. The suite is derived
/// from phi-tools, not listed here, so this pins "all of them" rather than
/// "the ones we remembered".
#[test]
fn browser_tools_never_reach_sub_agents() {
    let names = browser_tool_names();
    assert!(!names.is_empty(), "the browser suite must be non-empty");
    for cfg in [
        child_multi_agent_config(true, &names, 16_000),
        child_multi_agent_config(false, &names, 16_000),
    ] {
        for t in &names {
            assert!(
                cfg.child_excluded_tools.contains(t),
                "{t} must be in child_excluded_tools"
            );
        }
        // And the resolved set keeps them out for a default (read-only) spawn.
        let res = resolve_capability(
            Some(&ChildToolCapability::ReadOnly),
            cfg.control.autonomy,
            cfg.allow_child_write,
            &cfg.child_excluded_tools,
            &cfg.control.write_tools,
        );
        for t in &names {
            assert!(res.excluded_tools.contains(t), "{t} must stay excluded");
        }
    }
}

/// Write-child surface (T11 auto mode): a write request exempts the
/// write tools here too — auto's write switch opened with the
/// acceptance run, permission mode stays Full (see the
/// child_config_wiring test).
#[test]
fn auto_mode_write_request_exempts_write_tools() {
    let cfg = child_multi_agent_config(false, &browser_tool_names(), 16_000);
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

/// The host engine's sessions carry the `root` mark and draw from the
/// process-global counter — the code-cell factory keys policy and journal
/// off exactly this.
#[test]
fn root_sessions_carry_the_mark_and_unique_numbers() {
    let a = RootMarkedIds.generate();
    let b = RootMarkedIds.generate();
    for s in [&a, &b] {
        assert_eq!(
            s.external_id.as_deref(),
            Some(ROOT_SESSION_MARK),
            "host sessions must be marked root"
        );
    }
    assert_ne!(a.id, b.id, "numbers must not repeat within the process");
}

/// One row stream per resident session: the host's journal keeps its
/// historical name, a sub-agent's is namespaced by its session id.
#[cfg(feature = "code-exec")]
#[test]
fn cell_journals_split_by_session() {
    let root = SessionId::with_external_id(3, ROOT_SESSION_MARK);
    let child = SessionId::new(7);
    assert_eq!(CellSessions::journal_name(&root), "code_journal.jsonl");
    assert_eq!(CellSessions::journal_name(&child), "code_journal-7.jsonl");
}
