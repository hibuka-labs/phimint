//! The wrap-up nudge: "you have been working without saying anything".
//!
//! The mirror image of `DefaultGuardConfig::reasoning_only_nudge`, which fires
//! when the model thinks without acting. This one fires when the model acts
//! without talking: [`Middleware::on_post_llm`] sees the turn's `full_text`,
//! and a run of turns that produce none is a run the user is watching in
//! silence. Session 20261115_39c267a3 ran 45 of those and never reported.
//!
//! Three rules govern it, the same three the verify gate is built on.
//!
//! **1. The signal is silence, not length.** Turn count is the wrong lever —
//! `execution_max_turns` already owns that, and a real coding task legitimately
//! runs long. What is wrong is running long *without a word*. `text_len == 0`
//! is directly visible here and needs no tool-result plumbing.
//!
//! **2. Nudge once per user message, then degrade.**
//! [`WrapUpNudgeConfig::max_silent_turns`] is the threshold; after one nudge the
//! counter restarts rather than firing again mid-task. A second nag says the same
//! sentence twice and costs a turn the model could have spent reporting (the verify
//! gate's D1, `max_nudges = 1`).
//!
//! **3. The nudge is advice, not a block.** It names the way out — report, or
//! say what is still pending — instead of ordering a stop. An ordered stop
//! mid-investigation loses work; an advisory one costs nothing if ignored.
//!
//! `20` is measured, not picked: across 226 real Claude Code requests the
//! longest run of silent turns had median 19, 85% landed in 15–19, and exactly
//! one exceeded 20. On our own runaways it reaches 100. See
//! `notes/prompt-redesign-design.md` §14.2.

use std::sync::Mutex;

use async_trait::async_trait;
use phi_agent::{AgentResult, Middleware, PostLlmCtx, UserMessageCtx};

/// When and what the wrap-up nudge says. See the module docs for why each
/// field is what it is.
pub struct WrapUpNudgeConfig {
    /// Consecutive turns with no reply text before the nudge fires. `20` is
    /// calibrated against healthy behaviour, not guessed — see module docs.
    pub max_silent_turns: usize,
    /// Self-identifying header so the nudge never reads as the user speaking
    /// (same shape as `VerifyGateConfig::nudge_header`).
    pub nudge_header: String,
    /// The advice. Must name the way out rather than order a stop.
    pub message: String,
}

impl Default for WrapUpNudgeConfig {
    fn default() -> Self {
        Self {
            max_silent_turns: 20,
            nudge_header: "[wrap-up] This is not user input - it is a nudge.".to_string(),
            message: "You have not said anything to the user in a while. You likely have \
                 enough to report — write your report now, or say what you are still \
                 waiting for."
                .to_string(),
        }
    }
}

#[derive(Default)]
struct WrapUpState {
    /// Consecutive turns whose reply text was empty.
    silent_turns: usize,
    /// Nudges spent in the current user message.
    nudges: usize,
}

/// See the module docs.
pub struct WrapUpNudgeMiddleware {
    config: WrapUpNudgeConfig,
    state: Mutex<WrapUpState>,
}

impl WrapUpNudgeMiddleware {
    pub fn new(config: WrapUpNudgeConfig) -> Self {
        Self {
            config,
            state: Mutex::new(WrapUpState::default()),
        }
    }
}

#[async_trait]
impl Middleware for WrapUpNudgeMiddleware {
    /// One user message is one stretch of work and carries exactly one
    /// nudge budget (the verify gate's R1). Folding the counter here is what
    /// keeps a long task from being nagged once per silent stretch.
    async fn on_user_message(&self, _ctx: &mut UserMessageCtx) -> AgentResult<()> {
        let mut st = self.state.lock().expect("wrap-up state lock poisoned");
        st.silent_turns = 0;
        st.nudges = 0;
        Ok(())
    }

    async fn on_post_llm(&self, ctx: &mut PostLlmCtx) -> AgentResult<()> {
        if !ctx.full_text.is_empty() {
            let mut st = self.state.lock().expect("wrap-up state lock poisoned");
            st.silent_turns = 0;
            return Ok(());
        }

        let nudge = {
            let mut st = self.state.lock().expect("wrap-up state lock poisoned");
            st.silent_turns += 1;
            if st.silent_turns >= self.config.max_silent_turns && st.nudges == 0 {
                st.nudges += 1;
                st.silent_turns = 0;
                true
            } else {
                false
            }
        };
        if !nudge {
            return Ok(());
        }

        tracing::info!(
            session_id = ctx.session_id.id,
            turn = ctx.turn_count,
            max_silent_turns = self.config.max_silent_turns,
            "wrap-up nudge: no reply text for N turns in a row"
        );
        ctx.follow_up_message = Some(format!(
            "{} {}",
            self.config.nudge_header, self.config.message
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::{Action, ActionFormat, FinishReason, ParseOutcome, SessionId};

    fn ctx(full_text: &str) -> PostLlmCtx {
        PostLlmCtx {
            session_id: SessionId::new(1),
            full_text: full_text.to_string(),
            parse: ParseOutcome::Empty,
            available_tools: vec![],
            turn_count: 1,
            total_actions: 0,
            nudge_count: 0,
            turn_actions: 0,
            skip_push: false,
            follow_up_message: None,
            finish_reason: FinishReason::Stop,
        }
    }

    fn silent_turn() -> PostLlmCtx {
        let mut c = ctx("");
        c.parse = ParseOutcome::Actions(vec![Action {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "src/lib.rs"}),
            identity: String::new(),
            format: ActionFormat::ToolCall,
        }]);
        c
    }

    async fn run(mw: &WrapUpNudgeMiddleware, c: &mut PostLlmCtx) -> Option<String> {
        mw.on_post_llm(c).await.unwrap();
        c.follow_up_message.clone()
    }

    #[tokio::test]
    async fn a_nudge_arrives_after_the_threshold_of_silent_turns() {
        let mw = WrapUpNudgeMiddleware::new(WrapUpNudgeConfig {
            max_silent_turns: 3,
            ..Default::default()
        });
        for i in 0..2 {
            assert!(run(&mw, &mut silent_turn()).await.is_none(), "turn {i}");
        }
        let msg = run(&mw, &mut silent_turn()).await;
        assert!(msg.is_some(), "the third silent turn must nudge");
    }

    #[tokio::test]
    async fn a_turn_that_speaks_resets_the_run() {
        let mw = WrapUpNudgeMiddleware::new(WrapUpNudgeConfig {
            max_silent_turns: 3,
            ..Default::default()
        });
        run(&mw, &mut silent_turn()).await;
        run(&mw, &mut silent_turn()).await;
        // One word to the user, and the run starts over.
        assert!(run(&mw, &mut ctx("here is where I am.")).await.is_none());
        assert!(run(&mw, &mut silent_turn()).await.is_none());
        assert!(run(&mw, &mut silent_turn()).await.is_none());
        assert!(
            run(&mw, &mut silent_turn()).await.is_some(),
            "the counter must restart after the model speaks, not accumulate"
        );
    }

    #[tokio::test]
    async fn one_nudge_per_user_message() {
        // The verify gate's D1: a second nag says the same sentence twice and
        // costs a turn the model could have spent reporting.
        let mw = WrapUpNudgeMiddleware::new(WrapUpNudgeConfig {
            max_silent_turns: 2,
            ..Default::default()
        });
        assert!(run(&mw, &mut silent_turn()).await.is_none());
        assert!(run(&mw, &mut silent_turn()).await.is_some());
        // Another silent stretch inside the same user message: no second nudge.
        for _ in 0..5 {
            assert!(run(&mw, &mut silent_turn()).await.is_none());
        }

        let mut uc = phi_agent::UserMessageCtx {
            session_id: SessionId::new(1),
            user_input: "next question".to_string(),
        };
        mw.on_user_message(&mut uc).await.unwrap();
        assert!(
            run(&mw, &mut silent_turn()).await.is_none()
                && run(&mw, &mut silent_turn()).await.is_some(),
            "a new user message must re-arm the budget"
        );
    }

    #[test]
    fn the_nudge_advises_and_marks_itself() {
        let c = WrapUpNudgeConfig::default();
        assert!(
            c.message.contains("report"),
            "the advice must name the way out: {0}",
            c.message
        );
        assert!(
            c.message.contains("or say what you are still waiting for"),
            "and offer the other exit, so an unfinished task is not forced to stop: {0}",
            c.message
        );
        assert!(
            !c.message.contains("Stop all tool calls"),
            "it must not order a stop — that is MaxTurnsNudge's job at the hard cap: {0}",
            c.message
        );
        assert!(
            c.nudge_header.contains("not user input"),
            "the header must keep the nudge from reading as the user: {0}",
            c.nudge_header
        );
    }

    #[test]
    fn the_default_threshold_is_the_measured_one() {
        // 226 real requests: median longest silent run 19, 85% in 15-19, one
        // over 20. Anything lower nags healthy behaviour.
        assert_eq!(WrapUpNudgeConfig::default().max_silent_turns, 20);
    }
}
