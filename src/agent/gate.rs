//! The verify gate: the "must-verify-before-done" half of the verification loop.
//!
//! The `verify` tool (in `crate::tools::verify`) supplies **evidence**; this
//! middleware supplies the **policy** that a hand-back without fresh evidence is
//! not allowed to stand unmarked. It is a consumer-side middleware — it uses
//! only the framework's neutral seams (`Middleware::on_post_llm` /
//! `on_user_message`) and pushes no "thou must verify" policy into the engine
//! (design §8.3: other businesses don't need this).
//!
//! Three rules dominate everything here.
//!
//! **1. Freshness, not presence.** The question is never "was `verify` called
//! at some point" — that is the exact bug that made the original gate lie
//! (`src/gate.rs` at `93ae5e6` cleared its dirty flag on *any* verify call, pass
//! or fail). The question is "is there a green light *newer than* the last
//! change", which [`EvidenceLedger::fresh_green`] answers.
//!
//! **2. Intercept once, then degrade to a visible marker.** `max_nudges` is 1
//! (the user's requirement: "verify 拦截一次就够了"). A second nag says the same
//! sentence twice and costs a turn the model could have spent compiling. After
//! that single interception the gate never blocks again — it appends an
//! [`Unverified::warning`] line to the reply so the *human* sees the gap. The
//! marker is a footnote, not a verdict: it distinguishes "lazy" (never checked)
//! from "tried and lost" (checked and failed) for free, because the ledger knows.
//!
//! **3. Never a false green.** Every misjudgement must fall on the side of
//! "keep blocking", never on the side of "passed". That is why edits are
//! recorded from *intent* (a write that failed still counts as an edit — a
//! spurious nag, not a spurious pass) and why an edit arriving in the same turn
//! as a `verify` is deferred to the next LLM turn, so that verify's evidence is
//! never credited with covering an edit that raced it.
//!
//! ### Why this does not fight `DefaultGuard`
//!
//! The seam is structurally in our favour. `handle_post_llm_result` handles
//! `follow_up_message` at `turn_dispatch.rs:242-247` — *before* the four-state
//! dispatch. So the turn on which this middleware sets `follow_up_message`
//! returns `TurnFlow::Continue` and **never reaches `guard.on_turn()`**. The two
//! cannot nudge on the same turn. The residual overlap is only on the degraded
//! turn (where we no longer set `follow_up_message` and the guard is free to do
//! its own completeness work) — and there we only append a marker, which is not
//! a nudge at all. See `notes/verify-gate-design.md` §8.1.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use code_intel::lang;
use phi_agent::{AgentResult, Middleware, PostLlmCtx, UserMessageCtx};
use serde_json::Value;

use crate::verify_ledger::{EvidenceLedger, Unverified, VerifyKind};

/// Tools that can change source. `code_cell` is here because a CodeAct cell's
/// filesystem side effects are opaque — the misjudgement direction must not be
/// a false pass (design D2).
const DIRTY_TOOLS: &[&str] = &["write_file", "edit_file", "code_cell"];

/// Tools that produce compile-level evidence. The `verify` tool records into the
/// ledger itself when it runs; this list is what tells the gate "a verification
/// is in flight", which is what defers same-turn edit recording (rule 3).
const VERIFY_TOOLS: &[&str] = &["verify", "merge"];

/// Gate configuration.
pub struct VerifyGateConfig {
    /// Interceptions allowed per user message. **1** — see rule 2 above and
    /// `notes/verify-gate-design.md` D1.
    pub max_nudges: usize,
    /// Self-identifying header on every interception. User-role (the loop's
    /// only neutral seam for continuing a turn) but it names itself, so both
    /// the completeness judge and a human reading the trace can tell it apart
    /// from real input (design §8.1).
    ///
    /// A header, not a whole message: what is actually wrong depends on the
    /// ledger's verdict ("you never ran it" and "you ran it and it failed" need
    /// different next steps), so the body comes from [`Unverified::nudge`] and
    /// this only says who is speaking.
    pub nudge_header: String,
    /// Whether writing is possible at all right now. In `deny` mode the write
    /// tools are refused outright, so nothing can go dirty and the gate would
    /// only ever be noise — it turns itself off instead.
    ///
    /// A **probe**, not a flag: the approval mode is a live switch (Shift+Tab
    /// cycles `auto` ⇄ `ask` mid-session), so answering this at construction
    /// time would go stale. Called once per LLM turn.
    pub writes_possible: WritesProbe,
}

/// Whether the run can currently write at all. See
/// [`VerifyGateConfig::writes_possible`].
pub type WritesProbe = Arc<dyn Fn() -> bool + Send + Sync>;

impl Default for VerifyGateConfig {
    fn default() -> Self {
        Self {
            max_nudges: 1,
            nudge_header: "[gate] 这不是用户输入，是门禁提醒。".to_string(),
            writes_possible: Arc::new(|| true),
        }
    }
}

/// Per-user-message gate state. Reset by [`VerifyGateMiddleware::on_user_message`].
#[derive(Debug, Default)]
struct GateState {
    /// Interceptions spent in this user message.
    nudges: usize,
    /// Paths seen in write-tool *intent* this turn but not yet counted as edits.
    ///
    /// Deferred one LLM turn so that a `verify` issued in the same turn as the
    /// write is never credited with covering it (rule 3). Flushed at the top of
    /// the next [`on_post_llm`](Middleware::on_post_llm).
    deferred_edits: Vec<PathBuf>,
    /// Whether we have already declined to nudge because a fresh green light
    /// exists — so the budget reset is logged once, not per turn.
    saw_fresh_green: bool,
}

/// The verify gate (see the module docs).
pub struct VerifyGateMiddleware {
    config: VerifyGateConfig,
    ledger: Arc<Mutex<EvidenceLedger>>,
    state: Mutex<GateState>,
}

impl VerifyGateMiddleware {
    pub fn new(config: VerifyGateConfig, ledger: Arc<Mutex<EvidenceLedger>>) -> Self {
        Self {
            config,
            ledger,
            state: Mutex::new(GateState::default()),
        }
    }
}

#[async_trait]
impl Middleware for VerifyGateMiddleware {
    /// Each user message is one "edit → verify" cycle. This is the R1 half of
    /// the reset rule (`notes/verify-gate-design.md` D1′); the R3 half — reset on
    /// a *passing* verify — falls out of `fresh_green` and is applied in
    /// `on_post_llm`.
    async fn on_user_message(&self, _ctx: &mut UserMessageCtx) -> AgentResult<()> {
        let mut st = self.state.lock().expect("gate state lock poisoned");
        st.nudges = 0;
        st.deferred_edits.clear();
        st.saw_fresh_green = false;
        Ok(())
    }

    async fn on_post_llm(&self, ctx: &mut PostLlmCtx) -> AgentResult<()> {
        if !(self.config.writes_possible)() {
            return Ok(());
        }

        // ── 1. Fold in edits deferred from the previous turn. ──────────────
        // Doing this first is what makes an edit that raced a same-turn `verify`
        // land *after* that verify's evidence in the ledger, so the evidence
        // cannot cover it.
        let (pending_edits, in_flight_verify) = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            let mut edits: Vec<PathBuf> = std::mem::take(&mut st.deferred_edits);

            // ── 2. Classify this turn's actions. ───────────────────────────
            let mut saw_verify = false;
            for action in ctx.actions() {
                let name = action.name.as_str();
                if VERIFY_TOOLS.contains(&name) {
                    saw_verify = true;
                } else if DIRTY_TOOLS.contains(&name) {
                    if let Some(p) = edit_path(action.raw_args().as_ref()) {
                        if lang::is_code_path(&p) {
                            edits.push(PathBuf::from(p));
                        }
                    } else {
                        // No resolvable path (a CodeAct cell, or arguments we
                        // could not read). Conservative: count it as an edit.
                        edits.push(PathBuf::from("."));
                    }
                }
            }

            // A verify in *this* turn executes after this hook, so its evidence
            // cannot cover this turn's edits. Defer them to the next turn.
            if saw_verify {
                st.deferred_edits.extend(edits);
                (Vec::new(), true)
            } else {
                (edits, false)
            }
        };

        let fresh_green = {
            let mut ledger = self.ledger.lock().expect("ledger lock poisoned");
            if !pending_edits.is_empty() {
                ledger.record_edit(&pending_edits);
            }
            ledger.fresh_green(VerifyKind::Check)
        };

        // R3 half of the reset: a *passing* verify settles the batch and returns
        // the nudge budget. A failed verify deliberately does not — the change is
        // still unproven, and spending the budget again would nag about the same
        // unsolved problem. (`fresh_green` is the whole predicate; no separate
        // bookkeeping is needed to know the green light is current.)
        {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            if fresh_green && !st.saw_fresh_green {
                st.nudges = 0;
                st.saw_fresh_green = true;
            } else if !fresh_green {
                st.saw_fresh_green = false;
            }
        }

        // A verification in flight: the model is already doing the right thing.
        // Never intercept a turn that ends in a verify call.
        if in_flight_verify {
            return Ok(());
        }

        // ── 3. Is this a hand-back? ───────────────────────────────────────
        // Only a text-only turn with no pending actions is the model saying
        // "I'm done". Anything else is mid-work.
        if ctx.has_action_intent() || ctx.full_text.is_empty() {
            return Ok(());
        }

        // ── 4. Does anything need verifying at all? ───────────────────────
        // No edits → the question never comes up. A workspace with no compile
        // layer → nothing to verify *against*; nagging would be noise and
        // claiming evidence would be a lie. Say neither.
        //
        // Resolved once, here, and carried into both branches below: the
        // interception has to name *why* it is blocking (see
        // [`Unverified::nudge`]), and asking the ledger twice would risk the
        // two halves of one decision disagreeing.
        let unv = {
            let ledger = self.ledger.lock().expect("ledger lock poisoned");
            ledger.unverified(VerifyKind::Check)
        };
        let unv = match unv {
            None => return Ok(()), // fresh green light — nothing to do
            Some(Unverified::NoCompileLayer) => return Ok(()),
            Some(Unverified::NeverChecked { edited }) if edited.is_empty() => {
                return Ok(());
            }
            Some(unv) => unv,
        };

        // ── 5. Intercept once, then degrade to a visible marker. ──────────
        let nudges = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            if st.nudges < self.config.max_nudges {
                st.nudges += 1;
                st.nudges
            } else {
                0
            }
        };

        if nudges > 0 {
            tracing::info!(
                session_id = ctx.session_id.id,
                turn = ctx.turn_count,
                nudge = nudges,
                max_nudges = self.config.max_nudges,
                reason = unv.nudge(),
                "verify gate: suppressing an unverified hand-back"
            );
            // Drop the premature "done" and force one more turn. Because
            // `follow_up_message` short-circuits at `turn_dispatch.rs:242`, the
            // guard is skipped for this turn entirely — no double nudge.
            ctx.skip_push = true;
            ctx.follow_up_message =
                Some(format!("{} {}", self.config.nudge_header, unv.nudge()));
            return Ok(());
        }

        // Already spent the one interception. Let the reply through and append
        // the warning where the *human* will read it. Not a nudge, so the guard
        // is free to do its own work on this turn.
        {
            let mut ledger = self.ledger.lock().expect("ledger lock poisoned");
            ledger.clear_pending();
        }
        let warning = unv.warning();
        if marker_already_present(&ctx.full_text, &warning) {
            // The model can copy the marker — it is in its own context from the
            // reply we degraded a moment ago, and the first smoke run did
            // exactly that, producing two footnotes for one problem. One is
            // enough.
            tracing::warn!(
                session_id = ctx.session_id.id,
                turn = ctx.turn_count,
                "verify gate: reply already carries the marker, not appending again"
            );
            return Ok(());
        }
        tracing::warn!(
            session_id = ctx.session_id.id,
            turn = ctx.turn_count,
            "verify gate: degraded to a marker after max nudges"
        );
        ctx.full_text.push_str("\n\n");
        ctx.full_text.push_str(&warning);

        Ok(())
    }
}

/// Whether the reply already carries this exact marker.
///
/// Matching is on whitespace-normalised text so a re-wrapped copy still counts.
/// Anything *other* than a faithful copy — a different warning, a warning about
/// something else — is left alone and ours is appended anyway: a missing
/// correct marker is worse than a duplicated one, so this deliberately errs
/// towards appending.
fn marker_already_present(text: &str, warning: &str) -> bool {
    normalize(text).contains(&normalize(warning))
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `path` argument of a write tool call, if it names one.
///
/// Returns `None` for a payload we cannot read or one with no `path` (a CodeAct
/// cell carries `{"code": …}`) — callers treat that as an edit rather than
/// guessing it away.
fn edit_path(raw_args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw_args).ok()?;
    v.get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::{Action, FinishReason, ParseOutcome, SessionId};

    /// A PostLlmCtx holding `actions`, with `full_text` as the reply.
    fn ctx(full_text: &str, actions: Vec<Action>) -> PostLlmCtx {
        let parse = if actions.is_empty() {
            ParseOutcome::Empty
        } else {
            ParseOutcome::Actions(actions)
        };
        PostLlmCtx {
            session_id: SessionId::new(1),
            full_text: full_text.to_string(),
            parse,
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

    fn write(path: &str) -> Action {
        Action::from_tool_call(
            "c1",
            "write_file",
            format!(r#"{{"path":"{path}","content":"x"}}"#),
        )
    }

    fn cell() -> Action {
        Action::from_tool_call("c2", "code_cell", r#"{"code":"print(1)"}"#)
    }

    fn verify() -> Action {
        Action::from_tool_call("c3", "verify", r#"{}"#)
    }

    fn gate() -> (VerifyGateMiddleware, Arc<Mutex<EvidenceLedger>>) {
        let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
        ledger
            .lock()
            .unwrap()
            .set_compile_layer(true);
        (VerifyGateMiddleware::new(VerifyGateConfig::default(), Arc::clone(&ledger)), ledger)
    }

    #[test]
    fn edit_path_reads_the_path_argument() {
        assert_eq!(
            edit_path(r#"{"path":"src/a.rs","content":"x"}"#),
            Some("src/a.rs".to_string())
        );
        assert_eq!(edit_path(r#"{"code":"print(1)"}"#), None);
        assert_eq!(edit_path("not json"), None);
    }

    #[tokio::test]
    async fn a_hand_back_with_no_edits_is_never_intercepted() {
        let (mw, _ledger) = gate();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_none(), "nothing was edited");
        assert!(!c.skip_push);
    }

    #[tokio::test]
    async fn a_docs_only_edit_is_not_a_code_change() {
        // The user's hard rule: prose and loose config need no compile check.
        let (mw, _ledger) = gate();
        let mut c = ctx("", vec![write("README.md")]);
        mw.on_post_llm(&mut c).await.unwrap();

        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_none(), "README.md is not code");
    }

    #[tokio::test]
    async fn a_code_edit_without_verification_is_intercepted_once() {
        let (mw, _ledger) = gate();

        // Turn 1: the edit (deferred — it lands next turn).
        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_none(), "a write turn is not a hand-back");

        // Turn 2: hand back. The deferred edit is now counted → intercept.
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.skip_push, "the premature done is suppressed");
        let msg = c.follow_up_message.expect("one nudge");
        assert!(msg.contains("[gate]"), "must be self-identifying: {msg}");
    }

    #[tokio::test]
    async fn the_second_hand_back_degrades_to_a_marker_not_a_nudge() {
        // The user's requirement: intercept once is enough.
        let (mw, _ledger) = gate();

        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_some(), "first interception");

        let mut c = ctx("all done, really", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(
            c.follow_up_message.is_none(),
            "never nags twice about the same batch"
        );
        assert!(!c.skip_push, "the reply must reach the user");
        assert!(
            c.full_text.contains("⚠️"),
            "degraded to a visible marker: {}",
            c.full_text
        );
    }

    #[tokio::test]
    async fn the_marker_distinguishes_laziness_from_failure() {
        let (mw, ledger) = gate();
        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();

        // A failed check after the edit: "tried and lost", not "lazy".
        ledger.lock().unwrap().record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(101),
            error_count: 1,
            warning_count: 0,
            summary: String::new(),
        });

        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(
            c.full_text.contains("未通过"),
            "should say checked-and-failed: {}",
            c.full_text
        );
        assert!(!c.full_text.contains("未见验证"), "{}", c.full_text);
    }

    #[tokio::test]
    async fn the_nudge_names_the_verdict_not_just_verify() {
        // The first smoke run: the model ran `verify`, it failed, it said so —
        // and the gate still answered "please run verify", which cost a turn
        // and asked for something already done.
        let (mw, ledger) = gate();
        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();

        ledger
            .lock()
            .unwrap()
            .record_verify(crate::verify_ledger::VerifyVerdict {
                kind: VerifyKind::Check,
                command: "cargo check".to_string(),
                exit_code: Some(101),
                error_count: 2,
                warning_count: 0,
                summary: String::new(),
            });

        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        let msg = c.follow_up_message.expect("one nudge");
        assert!(msg.contains("[gate]"), "must be self-identifying: {msg}");
        assert!(msg.contains("未通过"), "{msg}");
        assert!(msg.contains("exit 101"), "{msg}");
        assert!(
            !msg.contains("请先执行"),
            "it already ran verify — don't ask again: {msg}"
        );
    }

    #[tokio::test]
    async fn the_marker_is_not_appended_twice() {
        // The marker lives in the model's output stream, so the model can copy
        // it back — the first smoke run showed two footnotes for one problem.
        let (mw, _ledger) = gate();
        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done, really", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        let marker = c.full_text.trim().to_string();
        assert_eq!(marker.matches("⚠️").count(), 1, "{marker}");

        // Replay that marker as the model's own text on a fresh run.
        let (mw, _ledger) = gate();
        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_some(), "first interception");

        let mut c = ctx(&format!("done\n\n{marker}"), vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert_eq!(
            c.full_text.matches("⚠️").count(),
            1,
            "a copied marker must not be doubled: {}",
            c.full_text
        );
    }

    #[tokio::test]
    async fn a_verify_in_the_same_turn_as_the_edit_does_not_cover_it() {
        // Rule 3: never a false green. The verify runs after this hook, so its
        // evidence must not be credited with a write it raced.
        let (mw, ledger) = gate();

        let mut c = ctx("", vec![write("src/a.rs"), verify()]);
        mw.on_post_llm(&mut c).await.unwrap();

        // The verify passes — but for code that predates the edit.
        ledger.lock().unwrap().record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(0),
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        });

        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(
            c.follow_up_message.is_some(),
            "the racing verify must not count as a green light"
        );
    }

    #[tokio::test]
    async fn a_passing_verify_returns_the_nudge_budget() {
        // D1′: reset on a *passing* verify (R3), so a later edit gets its own
        // interception rather than inheriting a spent budget.
        let (mw, ledger) = gate();

        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_some());

        // Verify passes for real.
        let mut c = ctx("", vec![verify()]);
        mw.on_post_llm(&mut c).await.unwrap();
        ledger.lock().unwrap().record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(0),
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        });

        // Hand back now: fresh green → no interception at all.
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_none(), "a green light settles it");

        // A NEW edit after the green light gets its own budget back.
        let mut c = ctx("", vec![write("src/b.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(
            c.follow_up_message.is_some(),
            "a new batch deserves a new interception"
        );
    }

    #[tokio::test]
    async fn a_turn_ending_in_a_verify_is_never_intercepted() {
        // The model is doing the right thing. Don't punish it mid-flight.
        let (mw, _ledger) = gate();
        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();

        let mut c = ctx("let me check it first", vec![verify()]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_none());
    }

    #[tokio::test]
    async fn a_workspace_without_a_compile_layer_is_left_alone() {
        let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
        ledger.lock().unwrap().set_compile_layer(false);
        let mw = VerifyGateMiddleware::new(VerifyGateConfig::default(), Arc::clone(&ledger));

        let mut c = ctx("", vec![write("src/a.js")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();

        assert!(c.follow_up_message.is_none(), "nothing to verify against");
        assert!(!c.full_text.contains("⚠️"), "and no noise either: {}", c.full_text);
    }

    #[tokio::test]
    async fn deny_mode_switches_the_gate_off() {
        let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
        ledger.lock().unwrap().set_compile_layer(true);
        let mw = VerifyGateMiddleware::new(
            VerifyGateConfig {
                writes_possible: Arc::new(|| false),
                ..VerifyGateConfig::default()
            },
            Arc::clone(&ledger),
        );

        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();

        assert!(c.follow_up_message.is_none());
        assert!(!c.full_text.contains("⚠️"));
    }

    #[tokio::test]
    async fn the_writes_probe_is_re_read_every_turn() {
        // The approval mode is a live switch (Shift+Tab) — a construction-time
        // snapshot would go stale and gate a run that can no longer write.
        let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
        ledger.lock().unwrap().set_compile_layer(true);
        let can_write = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let probe = Arc::clone(&can_write);
        let mw = VerifyGateMiddleware::new(
            VerifyGateConfig {
                writes_possible: Arc::new(move || probe.load(std::sync::atomic::Ordering::SeqCst)),
                ..VerifyGateConfig::default()
            },
            Arc::clone(&ledger),
        );

        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();

        // Shift+Tab → deny mid-run: the gate must stop watching.
        can_write.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_none(), "reads the switch live");
    }

    #[tokio::test]
    async fn a_new_user_message_resets_the_cycle() {
        let (mw, _ledger) = gate();
        let mut c = ctx("", vec![write("src/a.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_some());

        // Spend the budget, then a new user message arrives.
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(c.follow_up_message.is_none());

        let mut uctx = UserMessageCtx {
            session_id: SessionId::new(1),
            user_input: "now do this instead".to_string(),
        };
        mw.on_user_message(&mut uctx).await.unwrap();

        let mut c = ctx("", vec![write("src/b.rs")]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(
            c.follow_up_message.is_some(),
            "a new task gets a fresh budget"
        );
    }

    #[tokio::test]
    async fn an_unresolvable_edit_path_still_counts() {
        // A CodeAct cell carries no path. Conservative direction: treat it as
        // an edit rather than guessing it away (rule 3).
        let (mw, _ledger) = gate();
        let mut c = ctx("", vec![cell()]);
        mw.on_post_llm(&mut c).await.unwrap();
        let mut c = ctx("all done", vec![]);
        mw.on_post_llm(&mut c).await.unwrap();
        assert!(
            c.follow_up_message.is_some(),
            "an opaque code action is an edit until proven otherwise"
        );
    }
}
