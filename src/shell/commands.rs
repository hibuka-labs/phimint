//! The product's slash-command face (T5d): the `CommandSpec` table the
//! shell dispatches, and the handlers behind it.
//!
//! The shell owns the dispatch mechanism — first-word matching on the turn
//! pump, local answers settling as system messages — while WHAT a command
//! does is product policy and lives here: model tiers, the tool-socket
//! board. Nothing in this file reaches into shell state; each action closes
//! over the product handles it needs and answers through [`ShellFlow`].

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::external::state::ExternalState;
use crate::model::router::PhimintRouter;
use crate::model::store::ModelStore;
use crate::model::switch;
use phi_agent_tui::{CommandAction, CommandSpec, PickItem, ShellFlow};

/// Build the product's slash-command table: one row per wire name, the
/// (name, help) rows feeding the `/` picker.
///
/// Two action closures cover all eight names — the model-tier family shares
/// one handler, the tool-socket family another (the old inline chain, one
/// dispatch move later).
///
/// Listing policy (product wording): a family's ACTION commands are listed
/// in the `/` picker — a command the picker does not show is a command
/// nobody finds (feedback #15: the model family was typed-only and invisible
/// exactly when someone went looking for it). Read-only listing commands are
/// typed-only: the picker already lists the things they would list, so a
/// second listing among the rows is noise (feedback 2026-11-07: `/mo` showed
/// `model` and `models` side by side).
///
/// `pick_tx` is the shell's pick-modal queue (bare `/model` parks on it); a
/// `None` (headless) degrades to the plain show line. `agent` hot-swaps the
/// live engine's provider after a model swap.
pub fn command_table(
    agent: Arc<phi_agent::PhiAgent>,
    model_store: Arc<Mutex<ModelStore>>,
    router: Arc<PhimintRouter>,
    external: Arc<Mutex<ExternalState>>,
    pick_tx: Option<mpsc::UnboundedSender<PickItem>>,
) -> Vec<CommandSpec> {
    /// `mid_turn` marks the model-tier family: typed while a turn streams,
    /// these run at once through the mid-turn lane (the pick pops now), while
    /// the tool-socket family keeps the classic post-turn queue.
    fn spec(
        name: &str,
        help: &str,
        in_picker: bool,
        mid_turn: bool,
        action: CommandAction,
    ) -> CommandSpec {
        CommandSpec {
            name: name.to_string(),
            help: help.to_string(),
            in_picker,
            action,
            mid_turn,
        }
    }

    // Model-tier face: /model /lite /advanced /models /focus — pick or set
    // the tier models, list them, toggle focus mode.
    let model_action: CommandAction = {
        let model_store = Arc::clone(&model_store);
        let router = Arc::clone(&router);
        let pick_tx = pick_tx.clone();
        let model_agent = Some(agent.clone());
        Arc::new(move |input: String| {
            let model_store = Arc::clone(&model_store);
            let router = Arc::clone(&router);
            let pick_tx = pick_tx.clone();
            let agent = model_agent.clone();
            Box::pin(async move {
                match handle_model_command(
                    &input,
                    &model_store,
                    &router,
                    pick_tx.as_ref(),
                    agent.as_ref(),
                )
                .await
                {
                    Some(text) => ShellFlow::Answer(text),
                    None => ShellFlow::Unhandled,
                }
            })
        })
    };

    // tool-socket P3 face: /connect /tools /plugin — local semantics
    // executed by ExternalState, never a model turn. Commands serialize on
    // the pump, so the socket board sees one at a time; a plugin install
    // blocks its action until the human answers the full-facts confirm
    // popup. A namespace hit with broken syntax shows that namespace's
    // usage line instead of forwarding a half-command as a turn.
    let socket_action: CommandAction = {
        let external = Arc::clone(&external);
        let agent = Arc::clone(&agent);
        Arc::new(move |input: String| {
            let external = Arc::clone(&external);
            let agent = Arc::clone(&agent);
            Box::pin(async move {
                if let Some(cmd) = crate::external::commands::parse_command(&input) {
                    let host = crate::external::PhiToolHost { agent };
                    let mut state = external.lock().await;
                    let confirm = state.confirm_gate();
                    let response = state.handle(&host, confirm.as_ref(), cmd).await;
                    ShellFlow::Answer(response)
                } else if let Some(usage) = crate::external::commands::usage_for(&input) {
                    ShellFlow::Answer(usage.to_string())
                } else {
                    ShellFlow::Unhandled
                }
            })
        })
    };

    vec![
        // Model-tier family: the action commands are listed in the picker.
        // All five take the mid-turn lane (see `spec`).
        spec(
            "model",
            "Pick or set the main model",
            true,
            true,
            model_action.clone(),
        ),
        spec(
            "lite",
            "Show or set the lite model",
            true,
            true,
            model_action.clone(),
        ),
        spec(
            "advanced",
            "Show or set the advanced model",
            true,
            true,
            model_action.clone(),
        ),
        // Typed-only listing: the picker rows already ARE the model list.
        spec(
            "models",
            "List configured models and focus mode",
            false,
            true,
            model_action.clone(),
        ),
        spec("focus", "Toggle focus mode", true, true, model_action),
        // Tool-socket family: listed in the picker, classic lane.
        spec(
            "connect",
            "Connect an MCP server (catalog name or paste config)",
            true,
            false,
            socket_action.clone(),
        ),
        spec(
            "tools",
            "List external tools / toggle on-off",
            true,
            false,
            socket_action.clone(),
        ),
        spec(
            "plugin",
            "List, install, or remove plugins",
            true,
            false,
            socket_action,
        ),
    ]
}

/// /model [name] · /lite [name] · /advanced [name] · /models · /focus — the
/// model-tier command face (local semantics, never a model turn).
///
/// Bare `/model` opens the shell's pick modal (rows = the configured models)
/// when a pick queue is wired; a typed `/model <name>` sets the main model
/// directly. Both roads end in [`switch::apply_main_model`] — the picked row, the
/// typed name, and the literal fallback install the same way. A swap
/// persists to `config.json` (the confirm line says so) and stages the
/// engine hot-swap: when a turn is streaming, it finishes on the old
/// provider and the new one answers afterwards.
///
/// Async because it runs on the turn pump: the old `blocking_lock` here
/// panicked the agent task the moment one of these commands was used
/// ("cannot block the current thread from within a runtime") — the tests in
/// this module run under a tokio runtime to pin that.
async fn handle_model_command(
    input: &str,
    model_store: &Arc<Mutex<ModelStore>>,
    router: &Arc<PhimintRouter>,
    pick_tx: Option<&mpsc::UnboundedSender<PickItem>>,
    agent: Option<&Arc<phi_agent::PhiAgent>>,
) -> Option<String> {
    let input = input.trim();

    // /model [name] - pick or set main model
    if input == "/model" || input.starts_with("/model ") {
        let args = input.strip_prefix("/model").unwrap_or("").trim();
        if args.is_empty() {
            match pick_tx {
                Some(tx) => switch::pick_main_model(model_store, tx, agent).await,
                None => {
                    // Headless degrade (and the show line): no shell to pick
                    // with, so the command answers with the current model.
                    let store = model_store.lock().await;
                    Some(format!("Current main model: {}", store.tier_model("main")))
                }
            }
        } else {
            Some(switch::apply_main_model(args.to_string(), model_store, agent).await)
        }
    }
    // /lite [name] - show or set lite model
    else if input == "/lite" || input.starts_with("/lite ") {
        let args = input.strip_prefix("/lite").unwrap_or("").trim();
        if args.is_empty() {
            let store = model_store.lock().await;
            let model = store.tier_model("lite");
            Some(format!("Current lite model: {}", model))
        } else {
            let mut store = model_store.lock().await;
            store.set_tier_model("lite", args.to_string());
            Some(format!("Lite model set to: {}", args))
        }
    }
    // /advanced [name] - show or set advanced model
    else if input == "/advanced" || input.starts_with("/advanced ") {
        let args = input.strip_prefix("/advanced").unwrap_or("").trim();
        if args.is_empty() {
            let store = model_store.lock().await;
            let model = store.tier_model("advanced");
            Some(format!("Current advanced model: {}", model))
        } else {
            let mut store = model_store.lock().await;
            store.set_tier_model("advanced", args.to_string());
            Some(format!("Advanced model set to: {}", args))
        }
    }
    // /models - list all configured models
    else if input == "/models" {
        let store = model_store.lock().await;
        let tiers = store.list_tiers();
        let mut response = String::from("Configured models:\n");
        for (tier, model) in tiers {
            if tier == "main" {
                response.push_str(&format!("  {}: {} (current)\n", tier, model));
            } else {
                response.push_str(&format!("  {}: {}\n", tier, model));
            }
        }
        response.push_str(&format!(
            "Focus mode: {}",
            if router.is_focus_mode() { "on" } else { "off" }
        ));
        Some(response)
    }
    // /focus - toggle focus mode
    else if input == "/focus" {
        let new_state = !router.is_focus_mode();
        router.set_focus_mode(new_state);
        Some(format!(
            "Focus mode: {}",
            if new_state { "on" } else { "off" }
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
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
            ) -> Result<phi_agent::llm_trait::ChatStream, phi_agent::llm_trait::LlmError>
            {
                unimplemented!()
            }

            async fn chat(
                &self,
                _request: phi_agent::llm_trait::ChatRequest,
            ) -> Result<phi_agent::llm_trait::ChatResponse, phi_agent::llm_trait::LlmError>
            {
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

        // Toggle focus on
        let response = handle_model_command("/focus", &model_store, &router, None, None).await;
        assert_eq!(response, Some("Focus mode: on".to_string()));
        assert!(router.is_focus_mode());

        // Toggle focus off
        let response = handle_model_command("/focus", &model_store, &router, None, None).await;
        assert_eq!(response, Some("Focus mode: off".to_string()));
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

        let response =
            handle_model_command("/model gpt-4-turbo", &model_store, &router, None, None)
                .await
                .expect("the command answers");
        assert!(
            !response.contains("is not a declared or known model name"),
            "{response}"
        );
    }
}
