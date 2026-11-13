//! The model-tier command face (T5d): `/model` \[name\] · `/lite` \[name\] ·
//! `/advanced` \[name\] · `/models` · `/focus` — the family's local semantics,
//! never a model turn.
//!
//! Split out of [`crate::shell::commands`], which owns the `CommandSpec`
//! table and its action closures: WHAT each command does is product policy
//! and lives here. Nothing in this file reaches into shell state; each
//! action closes over the product handles it needs and answers as plain
//! text (the table wraps it into [`phi_agent_tui::ShellFlow`]).

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::model::router::PhimintRouter;
use crate::model::store::ModelStore;
use crate::model::switch;
use phi_agent_tui::PickItem;

/// The honest timing note `/model` earns while focus is on: the store moved
/// but the engine did not (routing decision 2). Describes standing state,
/// so it stays true for every outcome of the command.
const FOCUS_HOLD_NOTE: &str = "\n  focus is on: the engine keeps serving the focus tier; main changes take effect when focus ends";

/// Bare `/model` opens the shell's pick modal (rows = the configured models)
/// when a pick queue is wired; a typed `/model <name>` sets the main model
/// directly. Both roads end in [`switch::apply_main_model`] — the picked
/// row, the typed name, and the literal fallback install the same way. A
/// swap persists to `config.json` (the confirm line says so) and stages the
/// engine hot-swap: when a turn is streaming, it finishes on the old
/// provider and the new one answers afterwards.
///
/// Async because it runs on the turn pump: the old `blocking_lock` here
/// panicked the agent task the moment one of these commands was used
/// ("cannot block the current thread from within a runtime") — the tests in
/// this module run under a tokio runtime to pin that.
pub(super) async fn handle_model_command(
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
        // Focus on (routing decision 2): `/model` updates the store and the
        // pointer, but the engine leg gets NO handle — the chat keeps
        // serving the focus tier and unwinds to the new main when focus
        // ends. `router.focus_mode` is the one and only focus state (SC-3):
        // no second flag lives anywhere else.
        let focus = router.is_focus_mode();
        let engine_leg = if focus { None } else { agent };
        if args.is_empty() {
            match pick_tx {
                Some(tx) => {
                    let mut out = switch::pick_main_model(model_store, tx, engine_leg).await;
                    if focus && let Some(out) = out.as_mut() {
                        out.push_str(FOCUS_HOLD_NOTE);
                    }
                    out
                }
                None => {
                    // Headless degrade (and the show line): no shell to pick
                    // with, so the command answers with the current model.
                    let store = model_store.lock().await;
                    Some(format!("Current main model: {}", store.tier_model("main")))
                }
            }
        } else {
            let mut out = switch::apply_main_model(args.to_string(), model_store, engine_leg).await;
            if focus {
                out.push_str(FOCUS_HOLD_NOTE);
            }
            Some(out)
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
    // /focus - toggle focus mode (session-scoped, never persisted: the
    // `model` pointer belongs to /model alone, routing decision 2).
    else if input == "/focus" {
        let new_state = !router.is_focus_mode();
        router.set_focus_mode(new_state);
        // The engine follows the scene table: focus on → the main chat
        // serves the `lite` tier, off → back to the effective main. Both
        // resolve through `route("main_chat")` — the table is the single
        // adjudication surface (review A-1), and nothing is written to the
        // store or the config file here.
        let (tier, model, provider) = {
            let mut store = model_store.lock().await;
            let tier = router.route("main_chat").to_string();
            let model = store.tier_model(&tier).to_string();
            let provider = store.get_or_create_provider(&tier);
            (tier, model, provider)
        };
        // The engine swap runs outside the store lock (switch.rs rule).
        let swap_note = match provider {
            Ok(provider) => {
                let deferred = agent
                    .map(|a| a.runtime().llm_engine().set_provider_when_idle(provider))
                    .unwrap_or(false);
                if deferred {
                    " (applies after the current turn)".to_string()
                } else {
                    String::new()
                }
            }
            Err(err) => format!(" — ! engine unchanged: {err}"),
        };
        Some(format!(
            "Focus mode: {} — the main chat serves `{}` (`{}`){}",
            if new_state { "on" } else { "off" },
            tier,
            model,
            swap_note,
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
