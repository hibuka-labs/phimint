use super::*;
use crate::model::config::{ModelConfig, TierConfig};
use crate::model::picker_rows;
use crate::model::switch;
use phi_agent_tui::PickAnswer;

/// The loud note a switch earns when the store has no config file
/// behind it (the test stores never do — they are hermetic).
const NO_CONFIG_NOTE: &str =
    "\n! config.json not saved: no config file found (pick is session-only)";

fn setup() -> (Arc<Mutex<ModelStore>>, Arc<PhimintRouter>) {
    setup_with_config(ModelConfig {
        base_url: Some("https://api.openai.com/v1".to_string()),
        api_key: Some("test-key".to_string()),
        protocol: Some("openai".to_string()),
        main: TierConfig::Simple("gpt-4".to_string()),
        lite: Some(TierConfig::Simple("gpt-3.5-turbo".to_string())),
        advanced: Some(TierConfig::Simple("gpt-4-turbo".to_string())),
        model: None,
        scene_tiers: None,
        update: Default::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    })
}

fn setup_with_config(config: ModelConfig) -> (Arc<Mutex<ModelStore>>, Arc<PhimintRouter>) {
    // Create a mock provider for testing
    struct MockProvider;

    #[async_trait::async_trait]
    impl phi_agent::llm_trait::LlmProvider for MockProvider {
        async fn stream(
            &self,
            _request: phi_agent::llm_trait::ChatRequest,
        ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError> {
            unimplemented!()
        }

        async fn chat(
            &self,
            _request: phi_agent::llm_trait::ChatRequest,
        ) -> Result<phi_agent::llm_trait::ChatResponse, phi_agent::llm_trait::LlmError> {
            unimplemented!()
        }

        fn capabilities(&self) -> phi_agent::llm_trait::Capabilities {
            phi_agent::llm_trait::Capabilities::default()
        }

        fn info(&self) -> phi_agent::llm_trait::ProviderInfo {
            phi_agent::llm_trait::ProviderInfo {
                name: "mock".to_string(),
                model: "mock-model".to_string(),
                version: None,
            }
        }
    }

    let provider = Arc::new(MockProvider);
    let model_store = Arc::new(Mutex::new(ModelStore::new(config, provider)));
    let router = Arc::new(PhimintRouter::new());
    (model_store, router)
}

// Every test runs under a tokio runtime on purpose: the handler executes
// on the turn pump (a tokio task), so it must await its locks — the old
// `blocking_lock` panicked right here ("cannot block the current thread
// from within a runtime"), killing the agent task on any /model-family
// command.

#[tokio::test]
async fn test_model_command_show_main() {
    let (model_store, router) = setup();
    let response = handle_model_command("/model", &model_store, &router, None, None).await;
    assert_eq!(response, Some("Current main model: gpt-4".to_string()));
}

// ── /focus state machine (routing decision 2, SC-3) ──────────────────

#[tokio::test]
async fn focus_on_serves_the_lite_tier_and_flips_the_router_flag() {
    let (model_store, router) = setup();
    assert!(!router.is_focus_mode());
    let response = handle_model_command("/focus", &model_store, &router, None, None).await;
    let text = response.expect("focus answers");
    assert!(text.contains("Focus mode: on"), "got: {text}");
    assert!(text.contains("`lite`"), "the answer names the tier: {text}");
    assert!(router.is_focus_mode(), "router owns the focus state");
    assert_eq!(router.route("main_chat"), "lite");
}

#[tokio::test]
async fn focus_off_unwinds_to_the_effective_main() {
    let (model_store, router) = setup();
    handle_model_command("/focus", &model_store, &router, None, None).await;
    let response = handle_model_command("/focus", &model_store, &router, None, None).await;
    let text = response.expect("focus answers");
    assert!(text.contains("Focus mode: off"), "got: {text}");
    assert!(text.contains("`main`"), "the answer names the tier: {text}");
    assert!(!router.is_focus_mode());
    assert_eq!(router.route("main_chat"), "main");
}

#[tokio::test]
async fn model_during_focus_moves_the_store_but_holds_the_engine() {
    let (model_store, router) = setup();
    handle_model_command("/focus", &model_store, &router, None, None).await;
    let response =
        handle_model_command("/model gpt-4-turbo", &model_store, &router, None, None).await;
    let text = response.expect("model answers");
    assert!(
        text.contains("focus is on"),
        "the hold note must be visible: {text}"
    );
    // The store still moves — focus only holds the engine.
    let store = model_store.lock().await;
    assert_eq!(store.tier_model("main"), "gpt-4-turbo");
}

#[tokio::test]
async fn model_without_focus_carries_no_hold_note() {
    let (model_store, router) = setup();
    let response =
        handle_model_command("/model gpt-4-turbo", &model_store, &router, None, None).await;
    let text = response.expect("model answers");
    assert!(!text.contains("focus is on"), "got: {text}");
}

#[tokio::test]
async fn focus_persists_nothing() {
    // `/focus` is a session overlay: no config file behind these stores,
    // so any persistence attempt would surface as a loud note.
    let (model_store, router) = setup();
    let response = handle_model_command("/focus", &model_store, &router, None, None).await;
    let text = response.expect("focus answers");
    assert!(
        !text.contains("config.json"),
        "focus must not touch persistence: {text}"
    );
}

/// Value: protects=the `/focus` answer stays honest when the focus tier
/// cannot produce a provider (a broken tier endpoint) — the user is told
/// the engine did not move instead of reading a clean "Focus mode: on";
/// fails_when=the error path panics or the answer claims the engine
/// switched while the chat still serves the old model;
/// why_new=every other `/focus` test runs a healthy tier, so the
/// provider-error branch is uncovered; seam=none; no_claim=the healthy
/// focus state machine (proven above) and the engine swap itself (the
/// booted TUI, pty scenario 14).
#[tokio::test]
async fn focus_reports_an_unusable_tier_instead_of_claiming_a_switch() {
    let (model_store, router) = setup_with_config(ModelConfig {
        base_url: Some("https://api.openai.com/v1".to_string()),
        api_key: Some("test-key".to_string()),
        protocol: Some("openai".to_string()),
        main: TierConfig::Simple("gpt-4".to_string()),
        // A tier whose endpoint is unusable: the provider factory
        // refuses it, so the focus leg cannot build a client.
        lite: Some(TierConfig::Full {
            model: "gpt-3.5-turbo".to_string(),
            base_url: Some(String::new()),
            api_key: Some("test-key".to_string()),
            protocol: Some("openai".to_string()),
        }),
        advanced: Some(TierConfig::Simple("gpt-4-turbo".to_string())),
        model: None,
        scene_tiers: None,
        update: Default::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    });
    let response = handle_model_command("/focus", &model_store, &router, None, None).await;
    let text = response.expect("focus answers");
    assert!(text.contains("Focus mode: on"), "got: {text}");
    assert!(
        text.contains("engine unchanged"),
        "the answer must say the engine did not move: {text}"
    );
    assert!(
        router.is_focus_mode(),
        "the focus flag still flips — the tier failure is not a refusal"
    );
}

#[tokio::test]
async fn test_model_command_set_main() {
    let (model_store, router) = setup();
    let response =
        handle_model_command("/model gpt-4-turbo", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some(format!(
            "Main model set to: gpt-4-turbo (was gpt-4){NO_CONFIG_NOTE}"
        ))
    );

    // Verify the model was actually set
    let store = model_store.lock().await;
    assert_eq!(store.tier_model("main"), "gpt-4-turbo");
}

#[tokio::test]
async fn test_lite_command_show() {
    let (model_store, router) = setup();
    let response = handle_model_command("/lite", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some("Current lite model: gpt-3.5-turbo".to_string())
    );
}

#[tokio::test]
async fn test_lite_command_set() {
    let (model_store, router) = setup();
    let response =
        handle_model_command("/lite gpt-4o-mini", &model_store, &router, None, None).await;
    assert_eq!(response, Some("Lite model set to: gpt-4o-mini".to_string()));
}

#[tokio::test]
async fn test_advanced_command_show() {
    let (model_store, router) = setup();
    let response = handle_model_command("/advanced", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some("Current advanced model: gpt-4-turbo".to_string())
    );
}

#[tokio::test]
async fn test_advanced_command_set() {
    let (model_store, router) = setup();
    let response =
        handle_model_command("/advanced o1-preview", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some("Advanced model set to: o1-preview".to_string())
    );
}

#[tokio::test]
async fn test_models_command() {
    let (model_store, router) = setup();
    let response = handle_model_command("/models", &model_store, &router, None, None).await;
    assert!(response.is_some());
    let response = response.unwrap();
    assert!(response.contains("main: gpt-4 (current)"));
    assert!(response.contains("lite: gpt-3.5-turbo"));
    assert!(response.contains("advanced: gpt-4-turbo"));
    assert!(response.contains("Focus mode: off"));
}

/// protects: bare `/model` parks the command on the pick queue and the
/// human's choice flows through the one apply path — rows arrive first,
/// the store lands on the picked tuple, the confirmation names both
/// names. fails_when: the pick is never pushed, the answer is ignored, or
/// the applied model differs from the answered row. no_claim: the modal
/// rendering or filter behavior (phi-agent-tui's cards own those).
#[tokio::test]
async fn bare_model_parks_on_the_pick_queue_and_applies_the_answer() {
    let (model_store, router) = setup();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (store_handle, router_handle) = (Arc::clone(&model_store), Arc::clone(&router));
    let command = tokio::spawn(async move {
        handle_model_command("/model", &store_handle, &router_handle, Some(&tx), None).await
    });

    let item = rx.recv().await.expect("the pick modal is pushed");
    assert_eq!(item.title, "Model");
    assert!(
        item.rows
            .iter()
            .any(|row| row.label.contains("gpt-4-turbo")),
        "rows = the configured models: {:?}",
        item.rows
    );
    item.answer_tx
        .send(PickAnswer::Chosen {
            id: "gpt-4-turbo".to_string(),
            literal: false,
        })
        .expect("the command is parked on the answer");
    let response = command.await.unwrap();
    assert_eq!(
        response,
        Some(format!(
            "Main model set to: gpt-4-turbo (was gpt-4){NO_CONFIG_NOTE}"
        ))
    );
    assert_eq!(model_store.lock().await.tier_model("main"), "gpt-4-turbo");
}

/// protects: Esc answers `Cancelled` and the command reports "model
/// unchanged" with the store untouched — a cancel never half-applies.
/// fails_when: a cancelled pick changes the model or parks the command
/// forever. no_claim: what the product does with the freed turn.
#[tokio::test]
async fn a_cancelled_pick_changes_nothing() {
    let (model_store, router) = setup();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (store_handle, router_handle) = (Arc::clone(&model_store), Arc::clone(&router));
    let command = tokio::spawn(async move {
        handle_model_command("/model", &store_handle, &router_handle, Some(&tx), None).await
    });

    let item = rx.recv().await.expect("the pick modal is pushed");
    item.answer_tx
        .send(PickAnswer::Cancelled)
        .expect("the command is parked on the answer");
    let response = command.await.unwrap();
    assert_eq!(response, Some("model unchanged".to_string()));
    assert_eq!(model_store.lock().await.tier_model("main"), "gpt-4");
}

/// protects: a typed name the config does not list takes the literal
/// fallback — only the name swaps, the endpoint stays the main one — and
/// the confirmation says so, with a spelling warning for a name nothing
/// vouches for. fails_when: a literal name silently changes the endpoint,
/// the swap reports the wrong "was", or the warning is missing.
/// no_claim: whether the name exists at the provider.
#[tokio::test]
async fn a_typed_unknown_name_keeps_the_endpoint() {
    let (model_store, router) = setup();
    let response =
        handle_model_command("/model brand-new", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some(format!(
            "Main model set to: brand-new (was gpt-4)\n  \
             streaming: on -> unknown\n  tools: on -> unknown\n  \
             vision: on -> unknown\n  thinking: off -> unknown{NO_CONFIG_NOTE}\n\
             ! `brand-new` is not a declared or known model name — double-check the spelling"
        )),
        "the confirm names both models and the capability bits that differ"
    );
    let store = model_store.lock().await;
    assert_eq!(store.tier_model("main"), "brand-new");
    assert_eq!(
        store.tier_tuple("main").1,
        "https://api.openai.com/v1",
        "the literal fallback keeps the main endpoint"
    );
}

/// protects: a real switch persists the pick to the config file's
/// `model` selection pointer and nothing else — comments, sibling keys
/// and the `main` definition survive byte-for-byte (the「model list
/// shrinks」regression) — the confirmation marks the save, and a second
/// switch overwrites the pointer value. fails_when: the file stays
/// untouched, the `main` definition is rewritten, or the save is missing
/// from the confirm line. no_claim: what a config file may otherwise
/// contain (`model::persist` fixtures own the splice shapes).
#[tokio::test]
async fn a_real_switch_writes_only_the_model_pointer() {
    let dir = std::env::temp_dir().join(format!("phimint-cmd-persist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        "{\n  // keep me\n  \"main\": \"gpt-4\",\n  \"base_url\": \"https://api.openai.com/v1\",\n  \"api_key\": \"test-key\",\n  \"protocol\": \"openai\"\n}\n",
    )
    .unwrap();

    let (model_store, router) = setup();
    model_store.lock().await.set_config_path(Some(path.clone()));
    let response =
        handle_model_command("/model gpt-4-turbo", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some("Main model set to: gpt-4-turbo (was gpt-4)\n  saved to config.json".to_string())
    );
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("// keep me"), "{after}");
    assert!(after.contains("\"main\": \"gpt-4\""), "{after}");
    assert!(after.contains("\"model\": \"gpt-4-turbo\""), "{after}");

    let response =
        handle_model_command("/model brand-new", &model_store, &router, None, None).await;
    assert_eq!(
        response.unwrap().lines().next(),
        Some("Main model set to: brand-new (was gpt-4-turbo)"),
        "a second switch tracks the pick"
    );
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("\"model\": \"brand-new\""), "{after}");
    assert!(
        after.contains("\"main\": \"gpt-4\""),
        "the definition never moves: {after}"
    );

    // Re-picking the live tuple: "unchanged", no note, no rewrite.
    let response =
        handle_model_command("/model brand-new", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some("Main model unchanged: brand-new".to_string()),
        "the same pick is a no-op, and the file is not rewritten"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), after);

    std::fs::remove_dir_all(&dir).ok();
}

/// protects: the「clear selection」answer deletes the `model` pointer,
/// rolls the effective main back to the configured `main` definition and
/// says so — the file keeps its comments and definitions. fails_when:
/// the pointer survives the clear, the model does not roll back, or the
/// clear reports the wrong state. no_claim: restart behavior (the
/// pointer-less file is by construction the next boot's input).
#[tokio::test]
async fn clear_selection_rolls_back_to_the_configured_main() {
    let dir = std::env::temp_dir().join(format!("phimint-cmd-clear-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        "{\n  // keep me\n  \"main\": \"gpt-4\",\n  \"base_url\": \"https://api.openai.com/v1\",\n  \"api_key\": \"test-key\",\n  \"protocol\": \"openai\"\n}\n",
    )
    .unwrap();

    let (model_store, router) = setup();
    model_store.lock().await.set_config_path(Some(path.clone()));
    let response =
        handle_model_command("/model gpt-4-turbo", &model_store, &router, None, None).await;
    assert_eq!(
        response,
        Some("Main model set to: gpt-4-turbo (was gpt-4)\n  saved to config.json".to_string())
    );

    // The picker offers the clear row exactly now, and the answer rolls
    // everything back.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (store_handle, router_handle) = (Arc::clone(&model_store), Arc::clone(&router));
    let command = tokio::spawn(async move {
        handle_model_command("/model", &store_handle, &router_handle, Some(&tx), None).await
    });
    let item = rx.recv().await.expect("the pick modal is pushed");
    assert!(
        item.rows
            .iter()
            .any(|row| row.id == picker_rows::CLEAR_SELECTION_ID),
        "a live selection offers the clear row: {:?}",
        item.rows
    );
    item.answer_tx
        .send(PickAnswer::Chosen {
            id: picker_rows::CLEAR_SELECTION_ID.to_string(),
            literal: false,
        })
        .expect("the command is parked on the answer");
    let response = command.await.unwrap();
    assert_eq!(
        response,
        Some("Main model set to: gpt-4 (was gpt-4-turbo)\n  saved to config.json".to_string())
    );

    {
        let store = model_store.lock().await;
        assert_eq!(
            store.tier_model("main"),
            "gpt-4",
            "back to the configured main"
        );
    }
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("// keep me"), "{after}");
    assert!(after.contains("\"main\": \"gpt-4\""), "{after}");
    assert!(!after.contains("\"model\""), "the pointer is gone: {after}");

    std::fs::remove_dir_all(&dir).ok();
}

/// protects: clearing over a pointer that names the configured main says
/// "unchanged (selection cleared)" and still deletes the key — the clear
/// reports its file work even when the tuple did not move. fails_when:
/// the copy pretends a change happened or the key survives. no_claim:
/// how a pointer came to equal the default.
#[tokio::test]
async fn clear_selection_over_an_unchanged_pointer_still_deletes_the_key() {
    let dir = std::env::temp_dir().join(format!("phimint-cmd-clear2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        "{\n  \"main\": \"gpt-4\",\n  \"model\": \"gpt-4\"\n}\n",
    )
    .unwrap();

    let (model_store, _router) = setup_with_config(ModelConfig {
        base_url: Some("https://api.openai.com/v1".to_string()),
        api_key: Some("test-key".to_string()),
        protocol: Some("openai".to_string()),
        main: TierConfig::Simple("gpt-4".to_string()),
        lite: None,
        advanced: None,
        model: Some(TierConfig::Simple("gpt-4".to_string())),
        scene_tiers: None,
        update: Default::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    });
    model_store.lock().await.set_config_path(Some(path.clone()));

    let response = switch::clear_main_model(&model_store, None).await;
    assert_eq!(
        response,
        "Main model unchanged: gpt-4 (selection cleared)\n  saved to config.json"
    );
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(!after.contains("\"model\""), "the key is gone: {after}");
    assert!(after.contains("\"main\": \"gpt-4\""), "{after}");

    std::fs::remove_dir_all(&dir).ok();
}

/// protects: the pick modal can never strand the command — a closed
/// queue (the shell is gone) degrades to the plain show line, and an
/// answer that never arrives (the modal dropped) reads as "model
/// unchanged". fails_when: bare `/model` parks forever, or a dead
/// queue makes the command hang the turn pump. no_claim: the shell's
/// own queue lifecycle (phi-agent-tui owns it).
#[tokio::test]
async fn a_dead_pick_queue_never_strands_the_command() {
    let (model_store, router) = setup();

    // A closed queue: the receiver is gone before the command runs.
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let response = handle_model_command("/model", &model_store, &router, Some(&tx), None).await;
    assert_eq!(
        response,
        Some("Current main model: gpt-4".to_string()),
        "a closed queue degrades to the show line"
    );

    // A live queue whose answer never comes: the modal is dropped
    // without answering.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (store_handle, router_handle) = (Arc::clone(&model_store), Arc::clone(&router));
    let command = tokio::spawn(async move {
        handle_model_command("/model", &store_handle, &router_handle, Some(&tx), None).await
    });
    let item = rx.recv().await.expect("the pick modal is pushed");
    drop(item.answer_tx);
    let response = command.await.unwrap();
    assert_eq!(response, Some("model unchanged".to_string()));
    assert_eq!(model_store.lock().await.tier_model("main"), "gpt-4");
}

#[tokio::test]
async fn test_focus_command_toggle() {
    let (model_store, router) = setup();

    // Toggle focus on — the answer now also names the serving tier/model
    // (T5), so assert the stable head, not the full sentence.
    let response = handle_model_command("/focus", &model_store, &router, None, None).await;
    let text = response.expect("focus answers");
    assert!(text.starts_with("Focus mode: on"), "got: {text}");
    assert!(router.is_focus_mode());

    // Toggle focus off
    let response = handle_model_command("/focus", &model_store, &router, None, None).await;
    let text = response.expect("focus answers");
    assert!(text.starts_with("Focus mode: off"), "got: {text}");
    assert!(!router.is_focus_mode());
}

#[tokio::test]
async fn test_non_model_command() {
    let (model_store, router) = setup();
    let response = handle_model_command("hello", &model_store, &router, None, None).await;
    assert_eq!(response, None);
}

#[tokio::test]
async fn test_model_command_with_extra_spaces() {
    let (model_store, router) = setup();
    let response = handle_model_command(
        "  /model   gpt-4-turbo  ",
        &model_store,
        &router,
        None,
        None,
    )
    .await;
    assert_eq!(
        response,
        Some(format!(
            "Main model set to: gpt-4-turbo (was gpt-4){NO_CONFIG_NOTE}"
        ))
    );
}

/// Dispatch-level contract: a `/models` submit WITH arguments is not a
/// command hit (the handler only takes bare `/models`) — it must fall
/// through as `Unhandled`, never be answered or swallowed. Same for a
/// longer word that merely starts with a command name.
#[tokio::test]
async fn declined_inputs_fall_through_as_unhandled() {
    let (model_store, router) = setup();
    assert_eq!(
        handle_model_command("/models foo bar", &model_store, &router, None, None).await,
        None
    );
    assert_eq!(
        handle_model_command("/modelling", &model_store, &router, None, None).await,
        None
    );
}

/// protects:「clear selection」never deletes a file pointer that is not
/// this session's selection — a standing pointer the session merely
/// overrode (a CLI `--model`) survives in the file byte-for-byte, the
/// roll lands on it, and the confirm line says the pointer was kept
/// rather than claiming a save. fails_when: the member is deleted
/// anyway, the roll lands elsewhere, or the kept state reads as "saved".
/// no_claim: how the override got applied (the CLI merge is config's
/// card).
#[tokio::test]
async fn clear_keeps_a_file_pointer_that_is_not_this_sessions_selection() {
    let dir = std::env::temp_dir().join(format!("phimint-cmd-clear3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        "{\n  \"main\": \"gpt-4\",\n  \"base_url\": \"https://api.openai.com/v1\",\n  \"api_key\": \"test-key\",\n  \"protocol\": \"openai\",\n  \"model\": \"standing-pick\"\n}\n",
    )
    .unwrap();

    // Boot with the file pointer in force, then a CLI-style override
    // displaces it in the live table (the file is not rewritten).
    let (model_store, _router) = setup_with_config(ModelConfig {
        base_url: Some("https://api.openai.com/v1".to_string()),
        api_key: Some("test-key".to_string()),
        protocol: Some("openai".to_string()),
        main: TierConfig::Simple("gpt-4".to_string()),
        lite: None,
        advanced: None,
        model: Some(TierConfig::Simple("standing-pick".to_string())),
        scene_tiers: None,
        update: Default::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    });
    model_store.lock().await.set_config_path(Some(path.clone()));
    model_store
        .lock()
        .await
        .apply_picked(
            "cli-pick".to_string(),
            "https://api.openai.com/v1".to_string(),
            "test-key".to_string(),
            "openai".to_string(),
        )
        .expect("provider creation succeeds");

    let response = switch::clear_main_model(&model_store, None).await;
    assert_eq!(
        response,
        "Main model set to: standing-pick (was cli-pick)\n  config.json pointer kept (names standing-pick)"
    );
    {
        let store = model_store.lock().await;
        assert_eq!(store.tier_model("main"), "standing-pick");
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\n  \"main\": \"gpt-4\",\n  \"base_url\": \"https://api.openai.com/v1\",\n  \"api_key\": \"test-key\",\n  \"protocol\": \"openai\",\n  \"model\": \"standing-pick\"\n}\n",
        "the standing pointer is byte-for-byte untouched"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// protects: the internal clear-selection sentinel is an action on the
/// typed road too — `/model __clear_selection__` runs the clear flow
/// (rolls back, deletes the pointer) and is never persisted as a model
/// name. fails_when: the typed sentinel lands in the tier table or the
/// file as a literal name. no_claim: the picker road's own interception
/// (its card above owns that).
#[tokio::test]
async fn the_clear_sentinel_typed_literally_runs_the_clear_flow() {
    let dir = std::env::temp_dir().join(format!("phimint-cmd-sentinel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        "{\n  \"main\": \"gpt-4\",\n  \"base_url\": \"https://api.openai.com/v1\",\n  \"api_key\": \"test-key\",\n  \"protocol\": \"openai\",\n  \"model\": \"gpt-4-turbo\"\n}\n",
    )
    .unwrap();

    // Boot with the pointer in force, so the clear has a real selection
    // to drop.
    let (model_store, router) = setup_with_config(ModelConfig {
        base_url: Some("https://api.openai.com/v1".to_string()),
        api_key: Some("test-key".to_string()),
        protocol: Some("openai".to_string()),
        main: TierConfig::Simple("gpt-4".to_string()),
        lite: None,
        advanced: None,
        model: Some(TierConfig::Simple("gpt-4-turbo".to_string())),
        scene_tiers: None,
        update: Default::default(),
        ui: Default::default(),
        code_exec: Default::default(),
    });
    model_store.lock().await.set_config_path(Some(path.clone()));

    let response = handle_model_command(
        "/model __clear_selection__",
        &model_store,
        &router,
        None,
        None,
    )
    .await;
    assert_eq!(
        response,
        Some("Main model set to: gpt-4 (was gpt-4-turbo)\n  saved to config.json".to_string())
    );
    {
        let store = model_store.lock().await;
        assert_eq!(store.tier_model("main"), "gpt-4");
    }
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(!after.contains("__clear_selection__"), "{after}");
    assert!(!after.contains("\"model\""), "the pointer is gone: {after}");

    std::fs::remove_dir_all(&dir).ok();
}

/// protects: the spelling warning rides the confirm line exactly for a
/// name nothing vouches for — not a configured model. fails_when: the
/// warning is missing on an unvalidated literal or appears on a declared
/// one. no_claim: whether the name exists at the provider.
#[tokio::test]
async fn the_spelling_warning_fires_only_on_names_nothing_vouches_for() {
    let (model_store, router) = setup();
    let response = handle_model_command(
        "/model definitely-not-a-model",
        &model_store,
        &router,
        None,
        None,
    )
    .await
    .expect("the command answers");
    assert!(
        response.contains("is not a declared or known model name"),
        "{response}"
    );

    let response = handle_model_command("/model gpt-4-turbo", &model_store, &router, None, None)
        .await
        .expect("the command answers");
    assert!(
        !response.contains("is not a declared or known model name"),
        "{response}"
    );
}
