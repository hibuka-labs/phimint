//! The late-armed [`InteractionChannel`] slot (M3 T10d).
//!
//! The kernel mounts its interaction channel at build time, but the `ask_user`
//! picker queue is born per session inside `ui::run.rs`. This slot holds the
//! live approval gate from build and takes the ask sender when the session
//! arms it; every call delegates to [`LiveInteractionChannel`], so suspension
//! discipline (answer / cancel / detached-queue guard) stays single-sourced
//! there.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use phi_agent::{
    ApprovalDecision, ApprovalRequest, AskAnswer, AskOption, InteractionChannel, InteractionError,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::approval::LiveApprovalGate;
use crate::interaction::LiveInteractionChannel;
use crate::tools::ask_user::AskItem;

/// Where the live cancel token comes from: a source, not a token — the
/// runtime resets its token before every turn, so a token captured at build
/// time would be stale by the time a human is asked.
type CancelSource = Arc<dyn Fn() -> CancellationToken + Send + Sync>;

/// The build-time-mountable, run-time-armed interaction channel.
pub struct InteractSlot {
    gate: Arc<LiveApprovalGate>,
    ask: Mutex<Option<mpsc::UnboundedSender<AskItem>>>,
    /// The run's live cancel token, once the runtime exists.
    cancel: Mutex<Option<CancelSource>>,
}

impl InteractSlot {
    /// A slot over the live approval gate. The ask sender arrives later
    /// ( [`Self::arm`] ), when the session's picker queue exists.
    pub fn new(gate: Arc<LiveApprovalGate>) -> Self {
        Self {
            gate,
            ask: Mutex::new(None),
            cancel: Mutex::new(None),
        }
    }

    /// Bind this session's picker queue (the `ask_user` tool's sender).
    /// Called once per session iteration; re-arming swaps the queue.
    pub fn arm(&self, ask_tx: mpsc::UnboundedSender<AskItem>) {
        *self.ask.lock().expect("interact slot lock") = Some(ask_tx);
    }

    /// Bind the runtime's cancel token source — the token the consent popup
    /// is released by when the turn is cancelled.
    pub fn arm_cancel(&self, source: CancelSource) {
        *self.cancel.lock().expect("interact slot lock") = Some(source);
    }

    /// The current run's cancel token. With nothing armed it is a fresh
    /// token that never fires: a popup asked outside a turn still answers,
    /// it just cannot be released by a turn that does not exist.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel
            .lock()
            .expect("interact slot lock")
            .as_ref()
            .map(|source| source())
            .unwrap_or_default()
    }

    /// The real channel for one call. Unarmed, the ask sender is a closed
    /// channel: consent still rides the gate, and an ask takes
    /// `LiveInteractionChannel`'s own detached-picker error instead of
    /// hanging.
    fn channel(&self) -> LiveInteractionChannel {
        let ask_tx = self
            .ask
            .lock()
            .expect("interact slot lock")
            .clone()
            .unwrap_or_else(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                drop(rx);
                tx
            });
        LiveInteractionChannel::new(Arc::clone(&self.gate), ask_tx)
    }
}

#[async_trait]
impl InteractionChannel for InteractSlot {
    async fn request_consent(
        &self,
        request: ApprovalRequest,
        cancel: &CancellationToken,
    ) -> Result<ApprovalDecision, InteractionError> {
        self.channel().request_consent(request, cancel).await
    }

    async fn ask_user(
        &self,
        question: &str,
        options: &[AskOption],
        cancel: &CancellationToken,
    ) -> Result<AskAnswer, InteractionError> {
        self.channel().ask_user(question, options, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::build_live_approval;
    use crate::tools::ask_user::new_ask_user_tool;

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
        vec![AskOption {
            id: "go".into(),
            label: "Go".into(),
            note: None,
        }]
    }

    /// The gate is live from build — consent works before any session armed
    /// the ask queue.
    #[tokio::test]
    async fn consent_works_before_the_slot_is_armed() {
        let (gate, _policy, mut queue_rx, _switch) = build_live_approval("ask", None);
        let slot = InteractSlot::new(gate);

        let call = tokio::spawn(async move {
            InteractionChannel::request_consent(&slot, request(), &CancellationToken::new()).await
        });
        let item = queue_rx.recv().await.unwrap();
        item.decision_tx.send(ApprovalDecision::AllowOnce).unwrap();

        assert_eq!(call.await.unwrap().unwrap(), ApprovalDecision::AllowOnce);
    }

    /// Unarmed ask must fail closed through the delegated channel's own
    /// detached-queue guard — never hang, never auto-answer.
    #[tokio::test]
    async fn an_unarmed_ask_fails_closed() {
        let (gate, _policy, _queue_rx, _switch) = build_live_approval("ask", None);
        let slot = InteractSlot::new(gate);

        let err = slot
            .ask_user("做不做？", &options(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(&err, InteractionError::Channel(_)), "{err}");
    }

    /// Once armed, an ask rides the session's picker queue end to end.
    #[tokio::test]
    async fn an_armed_ask_reaches_the_picker() {
        let (gate, _policy, _queue_rx, _switch) = build_live_approval("ask", None);
        let (ask_tool, mut ask_rx) = new_ask_user_tool();
        let slot = InteractSlot::new(gate);
        slot.arm(ask_tool.ask_sender());

        let call = tokio::spawn(async move {
            InteractionChannel::ask_user(&slot, "先做哪个？", &options(), &CancellationToken::new())
                .await
        });
        let item = ask_rx.recv().await.unwrap();
        assert_eq!(item.question, "先做哪个？");
        item.answer_tx
            .send(AskAnswer::Picked {
                id: "go".into(),
                label: "Go".into(),
            })
            .unwrap();

        assert_eq!(
            call.await.unwrap().unwrap(),
            AskAnswer::Picked {
                id: "go".into(),
                label: "Go".into(),
            }
        );
    }

    /// Re-arming swaps the queue: the session switch's picker gets the ask,
    /// the old one's receiver sees nothing.
    #[tokio::test]
    async fn rearming_swaps_the_picker_queue() {
        let (gate, _policy, _queue_rx, _switch) = build_live_approval("ask", None);
        let (old_tool, mut old_rx) = new_ask_user_tool();
        let (new_tool, mut new_rx) = new_ask_user_tool();
        let slot = InteractSlot::new(gate);
        slot.arm(old_tool.ask_sender());
        slot.arm(new_tool.ask_sender());

        let call = tokio::spawn(async move {
            InteractionChannel::ask_user(&slot, "哪个？", &options(), &CancellationToken::new())
                .await
        });
        let item = new_rx.recv().await.unwrap();
        assert!(
            old_rx.try_recv().is_err(),
            "the old picker must be out of the loop"
        );
        item.answer_tx.send(AskAnswer::Skipped).unwrap();

        assert_eq!(call.await.unwrap().unwrap(), AskAnswer::Skipped);
    }
}
