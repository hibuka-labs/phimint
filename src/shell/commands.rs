//! The product's slash-command face (T5d): the `CommandSpec` table the
//! shell dispatches, and the action closures behind it.
//!
//! The shell owns the dispatch mechanism — first-word matching on the turn
//! pump, local answers settling as system messages — while WHAT a command
//! does is product policy and lives here: the model-tier family in
//! [`crate::shell::model_command`], the tool-socket board in
//! `crate::external`. Nothing in this file reaches into shell state; each
//! action closes over the product handles it needs and answers through
//! [`ShellFlow`].

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::external::state::ExternalState;
use crate::model::router::PhimintRouter;
use crate::model::store::ModelStore;
use crate::shell::model_command;
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
                match model_command::handle_model_command(
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
