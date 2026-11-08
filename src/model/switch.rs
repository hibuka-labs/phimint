//! The model-switch road (the `/model` family's two answer paths): the pick
//! modal's flow and the typed-name flow, both ending in [`apply_main_model`].
//!
//! This is the one place an effective main model changes. Split out of
//! `shell::commands` (the command surface) so the switch use-case — resolve,
//! create provider, install, hot-swap, persist the pointer — reads as one
//! unit against the rest of the model domain (`store`, `persist`,
//! `picker_rows`).

use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::model::picker_rows;
use crate::model::store::ModelStore;
use phi_agent_tui::{PickAnswer, PickItem};

/// Bare `/model`: push the candidate rows over the pick queue and wait for
/// the human's answer (the documented [`CommandAction`](phi_agent_tui::CommandAction)
/// park-on-oneshot contract). Esc cancels without touching anything; a closed
/// queue degrades to the show line instead of parking forever.
pub(crate) async fn pick_main_model(
    model_store: &Arc<Mutex<ModelStore>>,
    pick_tx: &mpsc::UnboundedSender<PickItem>,
    agent: Option<&Arc<phi_agent::PhiAgent>>,
) -> Option<String> {
    // Rows under the lock, guard dropped BEFORE the await — the answer path
    // locks the store again.
    let rows = {
        let store = model_store.lock().await;
        picker_rows::pick_rows(&store, &picker_rows::build_candidates(&store))
    };
    let (answer_tx, answer_rx) = oneshot::channel();
    if pick_tx
        .send(PickItem {
            title: "Model".to_string(),
            rows,
            answer_tx,
        })
        .is_err()
    {
        let store = model_store.lock().await;
        return Some(format!("Current main model: {}", store.tier_model("main")));
    }
    match answer_rx.await {
        // The「clear selection」row is an action, never a model name — it
        // runs the clear flow and never reaches `resolve_selection`.
        Ok(PickAnswer::Chosen { id, .. }) if id == picker_rows::CLEAR_SELECTION_ID => {
            Some(clear_main_model(model_store, agent).await)
        }
        Ok(PickAnswer::Chosen { id, .. }) => Some(apply_main_model(id, model_store, agent).await),
        // Esc, or the shell shutting down under us: nothing changed.
        Ok(PickAnswer::Cancelled) | Err(_) => Some("model unchanged".to_string()),
    }
}

/// The「clear selection」answer: drop this session's selection and roll the
/// effective main back — to the configured `main` definition when the file's
/// `model` pointer WAS the selection (its member is deleted too), or to the
/// file's standing pointer when the live selection merely overrode it (a
/// CLI `--model`; that member survives and the confirm line says so). The
/// engine swap takes the same turn-safe lane as [`apply_main_model`]. The
/// store lock only guards the store — the engine swap and the file surgery
/// run outside it (sync file I/O must never sit on an async lock).
pub(crate) async fn clear_main_model(
    model_store: &Arc<Mutex<ModelStore>>,
    agent: Option<&Arc<phi_agent::PhiAgent>>,
) -> String {
    let (from, to, provider, config_path, deletes_file_pointer, kept_pointer) = {
        let mut store = model_store.lock().await;
        let from = store.tier_tuple("main");
        let config_path = store.config_path().map(|p| p.to_path_buf());
        let file_pointer = crate::model::persist::pointer_tuple_at(config_path.as_deref());
        let file_is_selection =
            crate::model::persist::file_pointer_is_selection(config_path.as_deref(), &from);
        // The file member goes only when it IS this session's selection; a
        // standing pointer the session merely overrode is not ours to delete.
        let deletes_file_pointer = file_is_selection == Some(true);
        let to = match &file_pointer {
            Some(pointer) if !deletes_file_pointer => pointer.clone(),
            _ => store.configured_main(),
        };
        let provider =
            match store.apply_picked(to.0.clone(), to.1.clone(), to.2.clone(), to.3.clone()) {
                Ok(provider) => provider,
                Err(err) => return format!("! could not clear selection: {err}"),
            };
        store.clear_pointer();
        let kept_pointer = (!deletes_file_pointer).then_some(file_pointer).flatten();
        (
            from,
            to,
            provider,
            config_path,
            deletes_file_pointer,
            kept_pointer,
        )
    };
    let deferred = agent
        .map(|a| a.runtime().llm_engine().set_provider_when_idle(provider))
        .unwrap_or(false);
    let diffs = picker_rows::capability_diffs((&from.0, &from.1, &from.2), (&to.0, &to.1, &to.2));
    let unchanged = from == to;
    let head = if unchanged {
        format!("Main model unchanged: {} (selection cleared)", to.0)
    } else {
        picker_rows::confirm_line(&from.0, &to.0, unchanged, &diffs)
    };
    let tail = if deletes_file_pointer {
        // The clear always reports its delete: even when the tuple did not
        // move (a pointer naming the configured main), the key deletion is
        // real work.
        crate::model::persist::note(
            false,
            crate::model::persist::clear_selection(config_path.as_deref()),
            deferred,
        )
    } else if let Some(pointer) = kept_pointer {
        let mut out = format!("\n  config.json pointer kept (names {})", pointer.0);
        if deferred {
            out.push_str("\n  applies after the current turn");
        }
        out
    } else if config_path.is_none() {
        // No config behind the store: the same loud note the apply road
        // gives — the selection was never persistable.
        crate::model::persist::note(
            false,
            Err(crate::model::persist::PersistError::NoConfigFile),
            deferred,
        )
    } else if deferred {
        "\n  applies after the current turn".to_string()
    } else {
        String::new()
    };
    format!("{head}{tail}")
}

/// The one way a main model changes: resolve the name to its endpoint tuple
/// (a configured candidate's own endpoint, or the literal fallback — the
/// typed name on the current main endpoint), create the provider FIRST (an
/// error leaves the store untouched), then install and hot-swap the live
/// engine. A real change is persisted to `config.json`'s `model` selection
/// pointer so the next boot does not revert it (feedback 2026-11-07) — the
/// tier definitions are never rewritten (see [`crate::model::persist`]) and
/// a CLI-supplied endpoint is never persisted with it. The confirmation
/// names what changed, plus only the capability bits that actually differ
/// (registry facts; "unknown" is never guessed) — and warns when the name is
/// one nothing vouches for. [`picker_rows::CLEAR_SELECTION_ID`] arriving
/// here (the typed road) is a clear, never a name.
pub(crate) async fn apply_main_model(
    picked: String,
    model_store: &Arc<Mutex<ModelStore>>,
    agent: Option<&Arc<phi_agent::PhiAgent>>,
) -> String {
    // The clear sentinel is an action, never a model name: the picker road
    // intercepts it, and so must the typed road — a literal sentinel would
    // otherwise be persisted as a model name.
    if picked == picker_rows::CLEAR_SELECTION_ID {
        return clear_main_model(model_store, agent).await;
    }
    let (from, to, provider, config_path, runtime, unvalidated) = {
        let mut store = model_store.lock().await;
        let from = store.tier_tuple("main");
        let to = picker_rows::resolve_selection(&store, &picked);
        let unvalidated = picker_rows::is_unvalidated_literal(&store, &picked);
        let provider =
            match store.apply_picked(to.0.clone(), to.1.clone(), to.2.clone(), to.3.clone()) {
                Ok(provider) => provider,
                Err(err) => return format!("! could not set main model: {err}"),
            };
        let config_path = store.config_path().map(|p| p.to_path_buf());
        // The runtime defaults (CLI-merged) travel out of the lock with the
        // pick: a CLI-supplied endpoint is session policy, never file
        // content (`persist::render_pointer_value` enforces it).
        let runtime = (
            store.config().base_url.clone().unwrap_or_default(),
            store.config().api_key.clone().unwrap_or_default(),
            store.config().protocol.clone().unwrap_or_default(),
        );
        (from, to, provider, config_path, runtime, unvalidated)
    };
    // Hot-swap and file surgery outside the lock. While a turn streams, the
    // swap lands when that turn's run ends — it must finish on the old
    // provider (mid-run tool-continuation steps would otherwise mix two
    // models in one conversation). `deferred` drives the confirm line's
    // timing note.
    let deferred = agent
        .map(|a| a.runtime().llm_engine().set_provider_when_idle(provider))
        .unwrap_or(false);
    let diffs = picker_rows::capability_diffs((&from.0, &from.1, &from.2), (&to.0, &to.1, &to.2));
    let unchanged = from == to;
    let saved = if unchanged {
        Ok(())
    } else {
        crate::model::persist::persist_to(
            config_path.as_deref(),
            &to,
            (&runtime.0, &runtime.1, &runtime.2),
        )
    };
    let mut out = format!(
        "{}{}",
        picker_rows::confirm_line(&from.0, &to.0, unchanged, &diffs),
        crate::model::persist::note(unchanged, saved, deferred)
    );
    if !unchanged && unvalidated {
        out.push_str(&format!(
            "\n! `{}` is not a declared or known model name — double-check the spelling",
            to.0
        ));
    }
    out
}
