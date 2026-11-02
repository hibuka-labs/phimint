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

use crate::external::state::ExternalState;
use crate::model::router::PhimintRouter;
use crate::model::store::ModelStore;
use phi_agent_tui::{CommandAction, CommandSpec, ShellFlow};

/// Build the product's slash-command table: one row per wire name, the
/// (name, help) rows feeding the `/` picker.
///
/// Two action closures cover all eight names — the model-tier family shares
/// one handler, the tool-socket family another (the old inline chain, one
/// dispatch move later).
///
/// Listing policy (product wording): the model-tier family is **typed-only**
/// (`in_picker: false`) — the curated `/` face is skills + resume/upgrade +
/// the tool-socket family, exactly as before the shell sinking (a one-flag
/// flip per row opens a command up to the picker).
pub fn command_table(
    agent: Arc<phi_agent::PhiAgent>,
    model_store: Arc<Mutex<ModelStore>>,
    router: Arc<PhimintRouter>,
    external: Arc<Mutex<ExternalState>>,
) -> Vec<CommandSpec> {
    fn spec(name: &str, help: &str, in_picker: bool, action: CommandAction) -> CommandSpec {
        CommandSpec {
            name: name.to_string(),
            help: help.to_string(),
            in_picker,
            action,
        }
    }

    // Model-tier face: /model /lite /advanced /models /focus — show or set
    // the tier models, list them, toggle focus mode.
    let model_action: CommandAction = {
        let model_store = Arc::clone(&model_store);
        let router = Arc::clone(&router);
        Arc::new(move |input: String| {
            let model_store = Arc::clone(&model_store);
            let router = Arc::clone(&router);
            Box::pin(async move {
                match handle_model_command(&input, &model_store, &router).await {
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
        // Model-tier family: typed-only (out of the picker's curated rows).
        spec(
            "model",
            "Show or set the main model",
            false,
            model_action.clone(),
        ),
        spec(
            "lite",
            "Show or set the lite model",
            false,
            model_action.clone(),
        ),
        spec(
            "advanced",
            "Show or set the advanced model",
            false,
            model_action.clone(),
        ),
        spec(
            "models",
            "List configured models and focus mode",
            false,
            model_action.clone(),
        ),
        spec("focus", "Toggle focus mode", false, model_action),
        // Tool-socket family: listed in the picker.
        spec(
            "connect",
            "Connect an MCP server (catalog name or paste config)",
            true,
            socket_action.clone(),
        ),
        spec(
            "tools",
            "List external tools / toggle on-off",
            true,
            socket_action.clone(),
        ),
        spec(
            "plugin",
            "List, install, or remove plugins",
            true,
            socket_action,
        ),
    ]
}

/// /model [name] · /lite [name] · /advanced [name] · /models · /focus — the
/// model-tier command face (local semantics, never a model turn).
///
/// Async because it runs on the turn pump: the old `blocking_lock` here
/// panicked the agent task the moment one of these commands was used
/// ("cannot block the current thread from within a runtime") — the tests in
/// this module run under a tokio runtime to pin that.
async fn handle_model_command(
    input: &str,
    model_store: &Arc<Mutex<ModelStore>>,
    router: &Arc<PhimintRouter>,
) -> Option<String> {
    let input = input.trim();

    // /model [name] - show or set main model
    if input == "/model" || input.starts_with("/model ") {
        let args = input.strip_prefix("/model").unwrap_or("").trim();
        if args.is_empty() {
            // Show current main model
            let store = model_store.lock().await;
            let model = store.tier_model("main");
            Some(format!("Current main model: {}", model))
        } else {
            // Set main model
            let mut store = model_store.lock().await;
            store.set_tier_model("main", args.to_string());
            Some(format!("Main model set to: {}", args))
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
            response.push_str(&format!("  {}: {}\n", tier, model));
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

    fn setup() -> (Arc<Mutex<ModelStore>>, Arc<PhimintRouter>) {
        let config = ModelConfig {
            base_url: Some("https://api.openai.com/v1".to_string()),
            api_key: Some("test-key".to_string()),
            protocol: Some("openai".to_string()),
            main: TierConfig::Simple("gpt-4".to_string()),
            lite: Some(TierConfig::Simple("gpt-3.5-turbo".to_string())),
            advanced: Some(TierConfig::Simple("gpt-4-turbo".to_string())),
            scene_tiers: None,
            update: Default::default(),
            ui: Default::default(),
            code_exec: Default::default(),
        };

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
        let response = handle_model_command("/model", &model_store, &router).await;
        assert_eq!(response, Some("Current main model: gpt-4".to_string()));
    }

    #[tokio::test]
    async fn test_model_command_set_main() {
        let (model_store, router) = setup();
        let response = handle_model_command("/model gpt-4-turbo", &model_store, &router).await;
        assert_eq!(response, Some("Main model set to: gpt-4-turbo".to_string()));

        // Verify the model was actually set
        let store = model_store.lock().await;
        assert_eq!(store.tier_model("main"), "gpt-4-turbo");
    }

    #[tokio::test]
    async fn test_lite_command_show() {
        let (model_store, router) = setup();
        let response = handle_model_command("/lite", &model_store, &router).await;
        assert_eq!(
            response,
            Some("Current lite model: gpt-3.5-turbo".to_string())
        );
    }

    #[tokio::test]
    async fn test_lite_command_set() {
        let (model_store, router) = setup();
        let response = handle_model_command("/lite gpt-4o-mini", &model_store, &router).await;
        assert_eq!(response, Some("Lite model set to: gpt-4o-mini".to_string()));
    }

    #[tokio::test]
    async fn test_advanced_command_show() {
        let (model_store, router) = setup();
        let response = handle_model_command("/advanced", &model_store, &router).await;
        assert_eq!(
            response,
            Some("Current advanced model: gpt-4-turbo".to_string())
        );
    }

    #[tokio::test]
    async fn test_advanced_command_set() {
        let (model_store, router) = setup();
        let response = handle_model_command("/advanced o1-preview", &model_store, &router).await;
        assert_eq!(
            response,
            Some("Advanced model set to: o1-preview".to_string())
        );
    }

    #[tokio::test]
    async fn test_models_command() {
        let (model_store, router) = setup();
        let response = handle_model_command("/models", &model_store, &router).await;
        assert!(response.is_some());
        let response = response.unwrap();
        assert!(response.contains("main: gpt-4"));
        assert!(response.contains("lite: gpt-3.5-turbo"));
        assert!(response.contains("advanced: gpt-4-turbo"));
        assert!(response.contains("Focus mode: off"));
    }

    #[tokio::test]
    async fn test_focus_command_toggle() {
        let (model_store, router) = setup();

        // Toggle focus on
        let response = handle_model_command("/focus", &model_store, &router).await;
        assert_eq!(response, Some("Focus mode: on".to_string()));
        assert!(router.is_focus_mode());

        // Toggle focus off
        let response = handle_model_command("/focus", &model_store, &router).await;
        assert_eq!(response, Some("Focus mode: off".to_string()));
        assert!(!router.is_focus_mode());
    }

    #[tokio::test]
    async fn test_non_model_command() {
        let (model_store, router) = setup();
        let response = handle_model_command("hello", &model_store, &router).await;
        assert_eq!(response, None);
    }

    #[tokio::test]
    async fn test_model_command_with_extra_spaces() {
        let (model_store, router) = setup();
        let response =
            handle_model_command("  /model   gpt-4-turbo  ", &model_store, &router).await;
        assert_eq!(response, Some("Main model set to: gpt-4-turbo".to_string()));
    }

    /// Dispatch-level contract: a `/models` submit WITH arguments is not a
    /// command hit (the handler only takes bare `/models`) — it must fall
    /// through as `Unhandled`, never be answered or swallowed. Same for a
    /// longer word that merely starts with a command name.
    #[tokio::test]
    async fn declined_inputs_fall_through_as_unhandled() {
        let (model_store, router) = setup();
        assert_eq!(
            handle_model_command("/models foo bar", &model_store, &router).await,
            None
        );
        assert_eq!(
            handle_model_command("/modelling", &model_store, &router).await,
            None
        );
    }
}
