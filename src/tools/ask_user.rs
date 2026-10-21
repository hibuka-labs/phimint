//! `ask_user` — the text half of the design routing.
//!
//! A visual question goes to the design canvas (`canvas_push`); a conceptual
//! one — "do we do this at all", "which first", "which tech" — is a pick
//! between named candidates and gets a terminal picker instead of a prose
//! list. This tool only carries the question to the UI and waits for the
//! answer: the picker is `ui::picker`, and *when* to use the tool is the
//! design-canvas skill's call, not the tool's.
//!
//! The wait is human time, so the call pauses its execution clock
//! ([`ToolContext::pause_execution_clock`]) — the pipeline's tool budget must
//! not kill a question while the user is still reading it.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentError, AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// One candidate the user can pick. `id` is the model's stable key; `label`
/// is what the picker shows and what comes back as the answer.
#[derive(Debug, Clone)]
pub struct AskOption {
    pub id: String,
    pub label: String,
    pub note: Option<String>,
}

/// What the user did with a question.
#[derive(Debug, Clone)]
pub enum AskAnswer {
    Picked { id: String, label: String },
    Skipped,
}

/// A question handed to the UI, carrying the channel the answer comes back on.
#[derive(Debug)]
pub struct AskItem {
    pub question: String,
    pub options: Vec<AskOption>,
    pub answer_tx: oneshot::Sender<AskAnswer>,
}

/// The `ask_user` tool: enqueue a question and wait on the oneshot.
pub struct AskUserTool {
    request_tx: mpsc::UnboundedSender<AskItem>,
}

/// Build the tool and the receiver the run loop services. Register the tool
/// root-only (like `canvas_*`) — a sub-agent must not open pickers of its own.
pub fn new_ask_user_tool() -> (Arc<AskUserTool>, mpsc::UnboundedReceiver<AskItem>) {
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    (Arc::new(AskUserTool { request_tx }), request_rx)
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> &'static str {
        "Ask the user to pick one of a few options, in the terminal. Use for a conceptual \
            decision — should we do this, which first, which tech — where the answer is one of \
            2-4 candidates you can name. This is the text half of the design routing: anything \
            the user has to *see* (layout, styling, comparison) belongs on the design canvas \
            (`canvas_push`), and an open-ended question (a goal, a path, a number) belongs in \
            your reply text. Returns `picked: <label> (id: <id>)`, or `skipped` when the user \
            presses Esc — a skip is an answer too; carry on with what you have, never re-ask. \
            One live question at a time."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The decision to make, in one line and the user's language."
                },
                "options": {
                    "type": "array",
                    "description": "The candidates: 2-4 of them.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable id ([A-Za-z0-9_-]), echoed back on pick."
                            },
                            "label": {
                                "type": "string",
                                "description": "Short label shown in the picker, in the user's language."
                            },
                            "note": {
                                "type": "string",
                                "description": "Optional one-line elaboration under the label."
                            }
                        },
                        "required": ["id", "label"]
                    }
                }
            },
            "required": ["question", "options"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "ask_user".to_string(),
            description: "Concept-choice picker (text half of the design routing).".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if question.is_empty() {
            return Err(invalid("`question` is required and must be non-empty"));
        }
        let options = parse_options(args)?;

        let (answer_tx, answer_rx) = oneshot::channel();
        // Human time does not count against the tool budget.
        let _pause = ctx.pause_execution_clock();
        if self
            .request_tx
            .send(AskItem {
                question: question.to_string(),
                options,
                answer_tx,
            })
            .is_err()
        {
            // No UI attached (a detached run loop) — say so rather than hang.
            return Ok(vec![Content::text(
                "skipped: no terminal picker is attached to this session",
            )]);
        }

        tokio::select! {
            _ = ctx.cancel_token.cancelled() => Ok(vec![Content::text(
                "cancelled: the turn was stopped before the user answered",
            )]),
            answer = answer_rx => Ok(vec![Content::text(match answer {
                Ok(AskAnswer::Picked { id, label }) => format!("picked: {label} (id: {id})"),
                Ok(AskAnswer::Skipped) => "skipped: the user did not pick".to_string(),
                // The prompt was dropped — by a cancelled turn (the run loop
                // drains them) or by a UI that went away. Say which.
                Err(_) if ctx.cancel_token.is_cancelled() => {
                    "cancelled: the turn was stopped before the user answered".to_string()
                }
                Err(_) => "skipped: the picker closed without an answer".to_string(),
            })]),
        }
    }
}

fn invalid(raw: &str) -> AgentError {
    AgentError::ToolArgsInvalid {
        name: "ask_user".to_string(),
        raw: raw.to_string(),
    }
}

fn parse_options(args: &Value) -> AgentResult<Vec<AskOption>> {
    let Some(items) = args.get("options").and_then(Value::as_array) else {
        return Err(invalid("`options` must be an array"));
    };
    if items.len() < 2 {
        return Err(invalid("`options` needs at least two candidates"));
    }
    let mut options = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if id.is_empty() || label.is_empty() {
            return Err(invalid("every option needs a non-empty `id` and `label`"));
        }
        if options.iter().any(|o: &AskOption| o.id == id) {
            return Err(invalid("option `id`s must be unique"));
        }
        options.push(AskOption {
            id: id.to_string(),
            label: label.to_string(),
            note: item
                .get("note")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string),
        });
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> (Arc<AskUserTool>, mpsc::UnboundedReceiver<AskItem>) {
        new_ask_user_tool()
    }

    fn two_options() -> Value {
        json!({
            "question": "先做哪个？",
            "options": [
                {"id": "a", "label": "搜索", "note": "先打通找文件"},
                {"id": "b", "label": "登录"}
            ]
        })
    }

    fn text_of(out: &[Content]) -> String {
        out.iter()
            .map(|c| match c {
                Content::Text { text } => text.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_pick_comes_back_as_the_users_label() {
        let (tool, mut rx) = tool();
        let args = two_options();

        let call = tokio::spawn({
            let tool = tool.clone();
            let args = args.clone();
            async move { tool.call(&args, &ToolContext::for_test()).await }
        });

        let item = rx.recv().await.expect("question reaches the UI");
        assert_eq!(item.question, "先做哪个？");
        assert_eq!(item.options.len(), 2);
        assert_eq!(item.options[0].note.as_deref(), Some("先打通找文件"));
        item.answer_tx
            .send(AskAnswer::Picked {
                id: "b".into(),
                label: "登录".into(),
            })
            .unwrap();

        let out = call.await.unwrap().unwrap();
        assert_eq!(text_of(&out), "picked: 登录 (id: b)");
    }

    #[tokio::test]
    async fn esc_is_an_answer_too() {
        let (tool, mut rx) = tool();
        let call = tokio::spawn({
            let tool = tool.clone();
            let args = two_options();
            async move { tool.call(&args, &ToolContext::for_test()).await }
        });

        let item = rx.recv().await.unwrap();
        item.answer_tx.send(AskAnswer::Skipped).unwrap();

        let out = call.await.unwrap().unwrap();
        assert_eq!(text_of(&out), "skipped: the user did not pick");
    }

    #[tokio::test]
    async fn a_stopped_turn_does_not_wait_for_a_picker() {
        let (tool, mut rx) = tool();
        let ctx = ToolContext::for_test();
        let call = tokio::spawn({
            let tool = tool.clone();
            let args = two_options();
            async move { tool.call(&args, &ctx).await }
        });

        let item = rx.recv().await.unwrap();
        // The user never answers; the turn is cancelled instead.
        drop(item);
        let out = call.await.unwrap().unwrap();
        assert!(
            text_of(&out).contains("picker closed"),
            "a dropped prompt must still return: {}",
            text_of(&out)
        );
    }

    #[tokio::test]
    async fn cancelling_the_turn_returns_instead_of_hanging() {
        let (tool, mut rx) = tool();
        let ctx = ToolContext::for_test();
        let token = ctx.cancel_token.clone();
        let call = tokio::spawn({
            let tool = tool.clone();
            let args = two_options();
            async move { tool.call(&args, &ctx).await }
        });

        let _item = rx.recv().await.unwrap();
        token.cancel();
        let out = call.await.unwrap().unwrap();
        assert!(
            text_of(&out).contains("cancelled"),
            "a cancelled turn must release the tool: {}",
            text_of(&out)
        );
    }

    #[tokio::test]
    async fn a_lonely_option_is_not_a_choice() {
        let (tool, _rx) = tool();
        let args = json!({
            "question": "做不做？",
            "options": [{"id": "a", "label": "做"}]
        });
        let err = tool
            .call(&args, &ToolContext::for_test())
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("at least two"),
            "expected a rejection, got {err}"
        );
    }

    #[tokio::test]
    async fn an_empty_question_is_not_a_choice() {
        let (tool, _rx) = tool();
        let args = json!({
            "question": "  ",
            "options": [{"id": "a", "label": "a"}, {"id": "b", "label": "b"}]
        });
        let err = tool
            .call(&args, &ToolContext::for_test())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("question"), "got {err}");
    }

    #[tokio::test]
    async fn duplicate_ids_are_rejected() {
        let (tool, _rx) = tool();
        let args = json!({
            "question": "哪个？",
            "options": [{"id": "a", "label": "a"}, {"id": "a", "label": "b"}]
        });
        let err = tool
            .call(&args, &ToolContext::for_test())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unique"), "got {err}");
    }

    #[test]
    fn the_tool_face_tells_the_model_where_the_canvas_ends() {
        let (tool, _rx) = tool();
        let description = tool.description();
        assert!(description.contains("canvas_push"), "{description}");
        assert!(description.contains("skipped"), "{description}");
        assert!(description.contains("never re-ask"), "{description}");
        assert_eq!(tool.name(), "ask_user");
    }
}
