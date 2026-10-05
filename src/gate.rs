//! Enforced verify gate (Phase 6a): a consumer-side middleware, zero framework changes.
//!
//! **PARKED**: the wiring is commented out wholesale in `agent.rs` (see the
//! note there), so this module is dead in the bin build. Parked, not torn out:
//! impl and tests stay; `#![allow(dead_code)]` below only silences that.
#![allow(dead_code)]
//!
//! Enforcement half of the promise "never hand you code that will not
//! compile". When the agent touched a code file (`write_file` / `edit_file`)
//! this turn without running `verify`, its text-only "done" reply is
//! suppressed (`skip_push`) and a reminder forces verification. After
//! `max_nudges` refusals it degrades to a trailing "Unverified" warning on
//! the final reply -- never blocking forever (the deadlock escape hatch).
//!
//! Depends only on neutral framework hooks: `Middleware::on_post_llm`
//! (`skip_push` / `follow_up_message`) and `on_user_message` (per-turn reset).
//! No "must verify" policy is pushed into the framework (design S8.3).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use phi_agent::{AgentError, AgentResult, Middleware, PostLlmCtx, UserMessageCtx};
use serde_json::Value;

/// Write tools that dirty the workspace.
const DIRTY_TOOLS: [&str; 2] = ["write_file", "edit_file"];

/// Tools that verify the workspace and clear dirty.
const VERIFY_TOOLS: [&str; 1] = ["verify"];

/// Whether a write call lands on a file that affects `cargo check`.
///
/// Only code files set dirty: writing README/docs cannot break the build and must not trigger enforcement.
/// An unparseable path conservatively returns `true` -- verify once too often rather than miss one.
fn is_code_edit(args: &str) -> bool {
    let path = serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|v| v.get("path").and_then(Value::as_str).map(str::to_owned));
    match path {
        Some(p) => code_intel::lang::is_code_path(&p),
        None => true,
    }
}

/// Gate configuration.
pub struct VerifyEnforcementConfig {
    /// How many consecutive nudges before giving up and failing the turn.
    pub max_nudges: usize,
    /// Reminder injected as a User message, forcing verify first.
    pub nudge_message: String,
    /// Whether writes can happen this turn. In `deny` mode write tools are all
    /// rejected, dirty can never set, and the gate is off -- it must not punish a read-only agent.
    pub writes_possible: bool,
}

impl Default for VerifyEnforcementConfig {
    fn default() -> Self {
        Self {
            max_nudges: 3,
            nudge_message: "STOP. You edited code/config files but did not run `verify`. \
                 You MUST call `verify` NOW before saying done. \
                 Do not write any more text — just call verify."
                .to_string(),
            writes_possible: true,
        }
    }
}

/// Per-turn gate state: reset by `on_user_message`, advanced in `on_post_llm`.
#[derive(Default)]
struct GateState {
    dirty: bool,
    nudges: usize,
}

/// The enforced verify gate (see the module docs).
pub struct VerifyEnforcementMiddleware {
    config: VerifyEnforcementConfig,
    state: Arc<Mutex<GateState>>,
}

impl VerifyEnforcementMiddleware {
    pub fn new(config: VerifyEnforcementConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(GateState::default())),
        }
    }
}

#[async_trait]
impl Middleware for VerifyEnforcementMiddleware {
    /// Each user message starts a fresh "edit -> verify" cycle.
    async fn on_user_message(&self, _ctx: &mut UserMessageCtx) -> AgentResult<()> {
        let mut st = self.state.lock().expect("gate state lock poisoned");
        st.dirty = false;
        st.nudges = 0;
        Ok(())
    }

    async fn on_post_llm(&self, ctx: &mut PostLlmCtx) -> AgentResult<()> {
        if !self.config.writes_possible {
            return Ok(());
        }

        // 1) Update dirty from the tool calls in this response: write tools set it, verify tools clear it.
        let (dirty, nudges) = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            for (_id, name, args) in &ctx.tool_calls {
                if DIRTY_TOOLS.contains(&name.as_str()) && is_code_edit(args) {
                    st.dirty = true;
                } else if VERIFY_TOOLS.contains(&name.as_str()) {
                    st.dirty = false;
                    st.nudges = 0; // reset the nudge counter after a verify run
                }
            }
            (st.dirty, st.nudges)
        };

        // 2) Only "text-only, wants to stop, with unverified changes" needs blocking.
        if ctx.is_tool_call || ctx.full_text.is_empty() || !dirty {
            return Ok(());
        }

        // 3) Over the limit -> fail outright and end the turn.
        if nudges >= self.config.max_nudges {
            tracing::warn!(
                session_id = ctx.session_id.id,
                max_nudges = self.config.max_nudges,
                "verify gate: max nudges reached, failing turn"
            );
            return Err(AgentError::Internal(
                "verify gate: refused to run verify after multiple attempts".to_string(),
            ));
        }

        // 4) Veto this reply, inject the reminder, force one more loop.
        let new_nudges = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            st.nudges += 1;
            st.nudges
        };
        tracing::info!(
            session_id = ctx.session_id.id,
            nudge = new_nudges,
            "verify gate: suppressing unverified done, injecting verify nudge"
        );
        ctx.skip_push = true;
        ctx.follow_up_message = Some(self.config.nudge_message.clone());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::{FinishReason, SessionId};

    /// Build an `(id, name, args)` tool call (args default to `{}`).
    fn call(name: &str) -> (String, String, String) {
        call_with_args(name, "{}")
    }

    /// Build a tool call with explicit args.
    fn call_with_args(name: &str, args: &str) -> (String, String, String) {
        (format!("call_{name}"), name.to_string(), args.to_string())
    }

    /// Build a write tool call carrying a `path`.
    fn write_call(tool: &str, path: &str) -> (String, String, String) {
        call_with_args(tool, &format!(r#"{{"path": "{path}"}}"#))
    }

    /// Build a `PostLlmCtx` with defaults everywhere but `is_tool_call`/`full_text`/`tool_calls`.
    fn ctx(
        is_tool_call: bool,
        full_text: &str,
        tool_calls: Vec<(String, String, String)>,
    ) -> PostLlmCtx {
        PostLlmCtx {
            session_id: SessionId::new(1),
            full_text: full_text.to_string(),
            is_tool_call,
            tool_calls,
            available_tools: vec![],
            turn_count: 1,
            total_tool_calls: 0,
            nudge_count: 0,
            turn_tool_calls: 0,
            skip_push: false,
            follow_up_message: None,
            finish_reason: FinishReason::Stop,
        }
    }

    #[test]
    fn config_defaults() {
        let cfg = VerifyEnforcementConfig::default();
        assert_eq!(cfg.max_nudges, 3);
        assert!(cfg.writes_possible);
        assert!(cfg.nudge_message.contains("verify"));
    }

    #[tokio::test]
    async fn vetoes_unverified_done() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());
        let expected = VerifyEnforcementConfig::default().nudge_message;

        // write_file marks dirty (the tool call itself is not blocked).
        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // The text-only "done" that follows is vetoed.
        let mut done = ctx(false, "All done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
        assert_eq!(done.follow_up_message, Some(expected));
    }

    #[tokio::test]
    async fn edit_file_also_marks_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("edit_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
    }

    #[tokio::test]
    async fn doc_edit_does_not_mark_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![write_call("write_file", "README.md")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "wrote README, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push, "doc-only edits must not trigger the gate");
    }

    #[tokio::test]
    async fn code_edit_marks_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![write_call("write_file", "src/lib.rs")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "wrote src/lib.rs, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
    }

    #[tokio::test]
    async fn multilang_code_edit_marks_dirty() {
        for path in ["src/Foo.java", "src/a.ts", "src/b.cpp"] {
            let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

            let mut edit = ctx(true, "", vec![write_call("edit_file", path)]);
            mw.on_post_llm(&mut edit).await.unwrap();

            let mut done = ctx(false, "done", vec![]);
            mw.on_post_llm(&mut done).await.unwrap();
            assert!(done.skip_push, "{path} edit must trigger the gate");
        }
    }

    #[tokio::test]
    async fn cargo_manifest_marks_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![write_call("write_file", "Cargo.toml")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "bumped dep, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
    }

    #[test]
    fn is_code_path_classifies() {
        assert!(code_intel::lang::is_code_path("src/lib.rs"));
        assert!(code_intel::lang::is_code_path("build.rs"));
        assert!(code_intel::lang::is_code_path("Cargo.toml"));
        assert!(code_intel::lang::is_code_path("Cargo.lock"));
        assert!(code_intel::lang::is_code_path("docs/Cargo.toml"));
        assert!(code_intel::lang::is_code_path("Foo.java"));
        assert!(code_intel::lang::is_code_path("src/a.ts"));
        assert!(code_intel::lang::is_code_path("src/b.tsx"));
        assert!(code_intel::lang::is_code_path("src/c.cpp"));
        assert!(!code_intel::lang::is_code_path("README.md"));
        assert!(!code_intel::lang::is_code_path("docs/notes.txt"));
        assert!(!code_intel::lang::is_code_path("src/"));
    }

    #[test]
    fn missing_path_conservatively_marks_dirty() {
        // An unparseable path errs toward verifying.
        assert!(is_code_edit("{}"));
        assert!(is_code_edit("not json"));
    }

    #[tokio::test]
    async fn verify_clears_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut vfy = ctx(true, "", vec![call("verify")]);
        mw.on_post_llm(&mut vfy).await.unwrap();

        let mut done = ctx(false, "verified, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
        assert!(done.follow_up_message.is_none());
    }

    #[tokio::test]
    async fn only_write_tools_mark_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        // Read-only/search tools must not trip the gate.
        let mut read = ctx(true, "", vec![call("read_file"), call("search_content")]);
        mw.on_post_llm(&mut read).await.unwrap();

        let mut done = ctx(false, "just read, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push, "read-only tools must not trigger the gate");
    }

    #[tokio::test]
    async fn no_veto_when_clean() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        // No writes this turn, so a text-only done is let through.
        let mut done = ctx(false, "nothing changed, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
    }

    #[tokio::test]
    async fn no_veto_on_tool_call() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // Still working (tool calls) is not blocked, dirty or not.
        let mut work = ctx(true, "", vec![call("read_file")]);
        mw.on_post_llm(&mut work).await.unwrap();
        assert!(!work.skip_push);
    }

    #[tokio::test]
    async fn no_veto_on_empty_text() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // Empty text is not a "done", so it is not blocked (matches ToolEnforcementMiddleware).
        let mut empty = ctx(false, "", vec![]);
        mw.on_post_llm(&mut empty).await.unwrap();
        assert!(!empty.skip_push);
    }

    #[tokio::test]
    async fn fails_after_max_nudges() {
        let config = VerifyEnforcementConfig {
            max_nudges: 1,
            ..VerifyEnforcementConfig::default()
        };
        let mw = VerifyEnforcementMiddleware::new(config);

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // First nudge (at the limit).
        let mut first = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut first).await.unwrap();
        assert!(first.skip_push);

        // Second one: fail outright.
        let mut second = ctx(false, "done", vec![]);
        let result = mw.on_post_llm(&mut second).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("refused to run verify")
        );
    }

    #[tokio::test]
    async fn on_user_message_resets_state() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        // The edit marks dirty.
        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // A new user message starts a new cycle.
        let mut u = UserMessageCtx {
            session_id: SessionId::new(1),
            user_input: "hi".into(),
        };
        mw.on_user_message(&mut u).await.unwrap();

        // No edits in the new cycle, so done is not blocked.
        let mut done = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
    }

    #[tokio::test]
    async fn disabled_when_writes_not_possible() {
        let config = VerifyEnforcementConfig {
            writes_possible: false,
            ..VerifyEnforcementConfig::default()
        };
        let mw = VerifyEnforcementMiddleware::new(config);

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
    }
}
