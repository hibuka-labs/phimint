//! The init tool face is pinned (2026-11-20 用户拍板).
//!
//! phimint mounts a fixed set at init and only names the rest (`deferred.rs`)
//! — the model loads those on demand via `activate_tool`. Growing the
//! mounted set is an explicit decision: this file's count IS the decision
//! record. Reference point: Claude Code mounts 13; fewer is better.

use super::super::*;
use super::support::*;
use phi_agent::llm_trait::LlmProvider;
use std::sync::{Arc, Mutex};

/// The init face, counted on the production-shaped default build (token
/// budget + model store + interact slot + code-exec). 2026-11-20: measured
/// 23 — the face the 2026-11-16 session decided (「挂载 23 / 延迟 15」;
/// a blind-regex edit had silently left 8 zombie deferrals behind and the
/// wire read 15 until they were removed). To mount one more tool, change
/// this number **on purpose** and update `notes/tool-face-design.md`.
const INIT_MOUNTED_TOOLS: usize = 23;

/// Value: protects=the fixed init face — a tool creeping onto the wire
/// without a decision is the 44-tool face coming back. fails_when=the
/// mounted count moves without this number moving. why_new=the face is a
/// product decision (2026-11-20), not an accident of registration order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_init_tool_face_is_pinned() {
    // Token-budget builds install the process-global compactor; hold the
    // lock so no concurrent test swaps the handle underneath us.
    let _guard = crate::shell::context_rotation::global_compactor_test_lock()
        .lock()
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let provider = Arc::new(CapturingMockProvider {
        system_prompts: Mutex::new(Vec::new()),
        tools: Mutex::new(Vec::new()),
    });
    let (approval, policy, _approval_rx, _mode) =
        crate::approval::build_live_approval("auto", None);
    let model_store = Arc::new(tokio::sync::Mutex::new(
        crate::model::store::ModelStore::new(
            crate::model::config::ModelConfig {
                base_url: Some("https://api.example.com/v1".to_string()),
                api_key: Some("sk-test".to_string()),
                protocol: Some("openai".to_string()),
                main: crate::model::config::TierConfig::Simple("mock-model".to_string()),
                lite: None,
                advanced: None,
                model: None,
                scene_tiers: None,
                update: Default::default(),
                ui: Default::default(),
                code_exec: Default::default(),
            },
            provider.clone() as Arc<dyn LlmProvider>,
        ),
    ));
    // The act road's interaction slot (`ask_user` mounts on it) — the live
    // build always passes one, and the face count differs without it.
    let interact_slot = Arc::new(phi_agent_tui::InteractSlot::new(
        Arc::clone(&approval) as Arc<dyn phi_agent::ApprovalHandler>
    ));
    let (agent, _resolver, _telemetry, _bg_registry, _canvas_activity) =
        crate::agent::build::build_with_code_exec(
            crate::model::config::CodeExecMode::Tool,
            Some(interact_slot),
            provider.clone() as Arc<dyn LlmProvider>,
            approval,
            policy,
            1_000,
            tmp.path().to_path_buf(),
            1024,
            "low",
            "mock-model".to_string(),
            vec![tmp.path().join("skills")],
            Some(TokenBudgetOptions {
                budget: 210_000,
                retention_days: 30,
                base_dir: tmp.path().to_path_buf(),
                session_id: "tool-face-pin".to_string(),
            }),
            Some(Arc::clone(&model_store)),
            None,
            None,
        )
        .await
        .unwrap();

    // `ask_user` is armed per session at shell boot (`src/shell/wire.rs`),
    // not at build — the decided face counts it, so the pin registers it the
    // same way the boot does and counts it too.
    let (ask_user_tool, _ask_rx) = crate::tools::ask_user::new_ask_user_tool();
    agent
        .runtime()
        .tools_mut()
        .write()
        .await
        .register_arc(ask_user_tool);
    let session = agent.create_session().await;
    agent
        .run_turn(session, "hello", |_ev| Ok(()))
        .await
        .unwrap();

    let tools = provider.tools.lock().unwrap().clone();
    let names: Vec<String> = tools
        .iter()
        .map(|d| {
            d["function"]["name"]
                .as_str()
                .unwrap_or_else(|| panic!("tool definition without a name: {d}"))
                .to_string()
        })
        .collect();

    // The deferred set is named-only at init: nothing in it rides the wire,
    // and the opener that loads it does.
    for (name, _) in crate::agent::DEFERRED_TOOLS {
        assert!(
            !names.iter().any(|n| n == name),
            "deferred `{name}` must not ride the init face"
        );
    }
    assert!(
        names.iter().any(|n| n == "activate_tool"),
        "the opener must ride the init face"
    );
    assert_eq!(
        names.len(),
        INIT_MOUNTED_TOOLS,
        "the init face is fixed at {INIT_MOUNTED_TOOLS} mounted tools (2026-11-20). \
         Adding one is a decision: update this number and notes/tool-face-design.md. \
         Mounted now: {names:?}"
    );
}
