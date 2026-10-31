//! The [`InteractionChannel`] on phimint's product surfaces (M3 T8).
//!
//! consent rides the live approval gate — the same TUI popup, mode switch and
//! ledger the tool path uses (OV-6: consent UX must not regress on the code
//! backend); ask rides the `ask_user` picker queue. One cancel token, the
//! run's: a stopped turn releases both.
//!
//! Human time is unbounded on this product (the `ask_user` tool pauses its
//! execution clock for the same reason), so [`LiveInteractionChannel::exec_ctx`]
//! clears the library watchdog and leans on the run's cancel token alone.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{
    AgentError, ApprovalDecision, ApprovalHandler, ApprovalRequest, AskAnswer, AskOption, ExecCtx,
    InteractionChannel, InteractionError,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::approval::LiveApprovalGate;
use crate::tools::ask_user::AskItem;

/// The ExecCtx interaction channel over phimint's live product surfaces.
pub struct LiveInteractionChannel {
    gate: Arc<LiveApprovalGate>,
    ask_tx: mpsc::UnboundedSender<AskItem>,
}

impl LiveInteractionChannel {
    /// Wire to the live approval gate and the `ask_user` picker queue
    /// (`AskUserTool::ask_sender`).
    pub fn new(gate: Arc<LiveApprovalGate>, ask_tx: mpsc::UnboundedSender<AskItem>) -> Self {
        Self { gate, ask_tx }
    }

    /// The product's execution context: the run's cancel token, this channel,
    /// and no interaction watchdog — human time is unbounded here (the run's
    /// cancel token and the detached-queue error guard the dead cases).
    pub fn exec_ctx(
        cancel: CancellationToken,
        gate: Arc<LiveApprovalGate>,
        ask_tx: mpsc::UnboundedSender<AskItem>,
    ) -> ExecCtx {
        ExecCtx::new(cancel, Arc::new(Self::new(gate, ask_tx))).with_interact_timeout(None)
    }
}

#[async_trait]
impl InteractionChannel for LiveInteractionChannel {
    async fn request_consent(
        &self,
        request: ApprovalRequest,
        cancel: &CancellationToken,
    ) -> Result<ApprovalDecision, InteractionError> {
        // The gate is the kernel's approval throat: same popup, same ledger.
        self.gate
            .approve(request, cancel.clone())
            .await
            .map_err(channel_error)
    }

    async fn ask_user(
        &self,
        question: &str,
        options: &[AskOption],
        cancel: &CancellationToken,
    ) -> Result<AskAnswer, InteractionError> {
        let (answer_tx, answer_rx) = oneshot::channel();
        let item = AskItem {
            question: question.to_string(),
            options: options.to_vec(),
            answer_tx,
        };
        if self.ask_tx.send(item).is_err() {
            // No picker attached (a detached run loop) — say so rather than hang.
            return Err(InteractionError::Channel(
                "no terminal picker is attached to this session".into(),
            ));
        }
        tokio::select! {
            biased;
            answer = answer_rx => answer.map_err(|_| {
                InteractionError::Channel("the picker closed without an answer".into())
            }),
            _ = cancel.cancelled() => Err(InteractionError::Cancelled),
        }
    }
}

/// The kernel's approval errors onto the channel's error face.
fn channel_error(e: AgentError) -> InteractionError {
    match e {
        AgentError::Cancelled => InteractionError::Cancelled,
        other => InteractionError::Channel(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::build_live_approval;
    use crate::tools::ask_user::new_ask_user_tool;

    // The T8 scenarios (consent / ask_user / interrupt) on the real product
    // surfaces — the same queues the TUI drains.

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            title: "write_file".into(),
            message: "Write file: src/lib.rs".into(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Sensitive,
            raw: None,
            source: None,
        }
    }

    fn options() -> Vec<AskOption> {
        vec![
            AskOption {
                id: "search".into(),
                label: "搜索".into(),
                note: Some("先打通找文件".into()),
            },
            AskOption {
                id: "login".into(),
                label: "登录".into(),
                note: None,
            },
        ]
    }

    #[tokio::test]
    async fn consent_rides_the_live_approval_queue() {
        let (gate, _policy, mut queue_rx, _switch) = build_live_approval("ask", None);
        let (ask_tool, _ask_rx) = new_ask_user_tool();
        let ctx =
            LiveInteractionChannel::exec_ctx(CancellationToken::new(), gate, ask_tool.ask_sender());

        let call = tokio::spawn(async move { ctx.request_consent(request()).await });
        let item = queue_rx
            .recv()
            .await
            .expect("the prompt reaches the TUI queue");
        assert_eq!(item.request.title, "write_file");
        item.decision_tx.send(ApprovalDecision::AllowOnce).unwrap();

        assert_eq!(call.await.unwrap().unwrap(), ApprovalDecision::AllowOnce);
    }

    /// consent UX must not regress: auto mode answers without a popup.
    #[tokio::test]
    async fn auto_mode_answers_consent_without_a_popup() {
        let (gate, _policy, mut queue_rx, _switch) = build_live_approval("auto", None);
        let (ask_tool, _ask_rx) = new_ask_user_tool();
        let ctx =
            LiveInteractionChannel::exec_ctx(CancellationToken::new(), gate, ask_tool.ask_sender());

        assert_eq!(
            ctx.request_consent(request()).await.unwrap(),
            ApprovalDecision::AllowOnce
        );
        assert!(queue_rx.try_recv().is_err(), "auto must not pop the queue");
    }

    #[tokio::test]
    async fn ask_rides_the_ask_user_picker() {
        let (gate, _policy, _queue_rx, _switch) = build_live_approval("ask", None);
        let (ask_tool, mut ask_rx) = new_ask_user_tool();
        let ctx =
            LiveInteractionChannel::exec_ctx(CancellationToken::new(), gate, ask_tool.ask_sender());

        let call = tokio::spawn(async move { ctx.ask_user("先做哪个？", &options()).await });
        let item = ask_rx
            .recv()
            .await
            .expect("the question reaches the picker");
        assert_eq!(item.question, "先做哪个？");
        assert_eq!(item.options.len(), 2);
        assert_eq!(item.options[0].note.as_deref(), Some("先打通找文件"));
        item.answer_tx
            .send(AskAnswer::Picked {
                id: "login".into(),
                label: "登录".into(),
            })
            .unwrap();

        assert_eq!(
            call.await.unwrap().unwrap(),
            AskAnswer::Picked {
                id: "login".into(),
                label: "登录".into(),
            }
        );
    }

    #[tokio::test]
    async fn a_skipped_pick_is_an_answer_too() {
        let (gate, _policy, _queue_rx, _switch) = build_live_approval("ask", None);
        let (ask_tool, mut ask_rx) = new_ask_user_tool();
        let ctx =
            LiveInteractionChannel::exec_ctx(CancellationToken::new(), gate, ask_tool.ask_sender());

        let call = tokio::spawn(async move { ctx.ask_user("做不做？", &options()).await });
        let item = ask_rx.recv().await.unwrap();
        item.answer_tx.send(AskAnswer::Skipped).unwrap();

        assert_eq!(call.await.unwrap().unwrap(), AskAnswer::Skipped);
    }

    #[tokio::test]
    async fn interrupt_releases_a_suspended_consent() {
        let (gate, _policy, mut queue_rx, _switch) = build_live_approval("ask", None);
        let (ask_tool, _ask_rx) = new_ask_user_tool();
        let token = CancellationToken::new();
        let ctx = LiveInteractionChannel::exec_ctx(token.clone(), gate, ask_tool.ask_sender());

        let call = tokio::spawn(async move { ctx.request_consent(request()).await });
        let _item = queue_rx.recv().await.unwrap(); // prompt is up; nobody answers
        token.cancel();

        assert!(matches!(
            call.await.unwrap(),
            Err(InteractionError::Cancelled)
        ));
    }

    /// The detached case is the product-side deadlock guard: no picker
    /// attached must fail fast, never suspend forever.
    #[tokio::test]
    async fn a_detached_picker_fails_instead_of_hanging() {
        let (gate, _policy, _queue_rx, _switch) = build_live_approval("ask", None);
        let (ask_tool, ask_rx) = new_ask_user_tool();
        let ask_tx = ask_tool.ask_sender();
        drop(ask_rx);
        let ctx = LiveInteractionChannel::exec_ctx(CancellationToken::new(), gate, ask_tx);

        let err = ctx.ask_user("做不做？", &options()).await.unwrap_err();
        assert!(matches!(&err, InteractionError::Channel(_)), "{err}");
    }
}
