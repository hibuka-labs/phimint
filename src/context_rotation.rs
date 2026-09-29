//! Context rotation — the application shell.
//!
//! The decision state machine lives in `agent_works::rotation_policy`
//! ([`TokenBudgetCore`]: phases, base overhead, message assembly). This
//! module is the thin [`ContextCompaction`] shell around it that owns the
//! phimint-specific I/O:
//!
//! - **Archive**: on [`TokenBudgetAction::Reset`], write the outgoing
//!   window to history.
//! - **Thread hint**: read the model's own handoff note and seed the
//!   fresh window with it.
//! - **Mechanical handoff**: extract activity ledger from outgoing window.
//! - **TUI notification**: a global handle exposes the reset count so the
//!   UI loop can announce window rotations.
//!
//! agent-base stays strategy-free: it calls [`ContextCompaction::compact`]
//! every Continue point and validates the output against the sendability
//! contract; this shell (via the core) supplies the strategy.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use phi_agent::{
    ChatMessage, CompactionKind, CompactionOutcome, ContextCompaction, SessionId, TokenBudgetAction,
    TokenBudgetCore, clear_messages_jsonl, estimate_messages_tokens,
};

use phi_kernel_tools::context_rotation::{HistoryStore, NotesStore, read_thread_hint, extract_ledger};

/// Global handle to the token-budget compactor for TUI access.
/// Set during agent build; the TUI checks `reset_count()` after each turn
/// to display a window-rotation notification.
static TOKEN_BUDGET_COMPACTOR: std::sync::Mutex<Option<Arc<TokenBudgetCompactor>>> =
    std::sync::Mutex::new(None);

/// Get the current window reset count from the global compactor.
/// Returns 0 if token-budget is not enabled.
pub fn window_reset_count() -> usize {
    TOKEN_BUDGET_COMPACTOR
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|c| c.reset_count()))
        .unwrap_or(0)
}

/// Whether the futility brake has paused window rotation (work budget too
/// small to hold one turn of work). Returns false if token-budget is not
/// enabled.
pub fn futility_braked() -> bool {
    TOKEN_BUDGET_COMPACTOR
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|c| c.core().braked()))
        .unwrap_or(false)
}

/// Store the compactor handle globally (called during agent build).
pub fn set_global_compactor(compactor: Arc<TokenBudgetCompactor>) {
    if let Ok(mut guard) = TOKEN_BUDGET_COMPACTOR.lock() {
        *guard = Some(compactor);
    }
}

/// Startup notice when the requested budget was clamped to the viability
/// floor (v4 A-side: tell the user the true minimum instead of silently
/// swallowing the value). Set during agent build; run.rs drains it into
/// the transcript.
static CLAMP_NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_clamp_notice(notice: String) {
    if let Ok(mut guard) = CLAMP_NOTICE.lock() {
        *guard = Some(notice);
    }
}

/// Take the clamp notice (None afterwards), if token-budget is not enabled
/// or no clamp happened, returns None.
pub fn take_clamp_notice() -> Option<String> {
    CLAMP_NOTICE.lock().ok().and_then(|mut g| g.take())
}

/// Application shell around [`TokenBudgetCore`] (see module docs).
pub struct TokenBudgetCompactor {
    core: TokenBudgetCore,
    /// Base directory for storage (~/.phimint/).
    base_dir: PathBuf,
    /// Session ID for this compactor instance.
    session_id: String,
    /// Agent name (used for notes path and thread hint lookup).
    agent_name: String,
    /// Number of window resets that have occurred (TUI notification).
    reset_count: AtomicUsize,
}

impl TokenBudgetCompactor {
    pub fn new(core: TokenBudgetCore, base_dir: PathBuf, session_id: String, agent_name: String) -> Self {
        Self {
            core,
            base_dir,
            session_id,
            agent_name,
            reset_count: AtomicUsize::new(0),
        }
    }

    /// The decision core (window id, base overhead, config access).
    pub fn core(&self) -> &TokenBudgetCore {
        &self.core
    }

    /// Number of window resets that have occurred.
    pub fn reset_count(&self) -> usize {
        self.reset_count.load(Ordering::Relaxed)
    }

    /// Append a nudge at the END, right after the latest tool result —
    /// that is where the model's attention is. Prepending buries the nudge
    /// at position 0, thousands of tokens before the work in progress, and
    /// the model ignores it (session 20260908_2ecc530a: 14 fallbacks sent,
    /// 0 handoffs written). Prior messages pass through untouched.
    fn append_nudge(
        messages: &[ChatMessage],
        nudge: ChatMessage,
        kind: CompactionKind,
    ) -> CompactionOutcome {
        let mut result = Vec::with_capacity(messages.len() + 1);
        result.extend_from_slice(messages);
        result.push(nudge);
        CompactionOutcome {
            kind,
            messages: result,
        }
    }
}

#[async_trait]
impl ContextCompaction for TokenBudgetCompactor {
    async fn compact(
        &self,
        _session_id: &SessionId,
        messages: &[ChatMessage],
    ) -> Option<CompactionOutcome> {
        let total_tokens = estimate_messages_tokens(messages);
        match self.core.evaluate(total_tokens) {
            TokenBudgetAction::None => None,

            TokenBudgetAction::Reminder(msg) => {
                Some(Self::append_nudge(messages, msg, CompactionKind::Reminder))
            }
            TokenBudgetAction::Fallback(msg) => {
                Some(Self::append_nudge(messages, msg, CompactionKind::Fallback))
            }

            TokenBudgetAction::Reset {
                previous_window, ..
            } => {
                // 1. Archive the outgoing window (best effort — a failed
                //    archive is logged and the rotation proceeds; the data
                //    is lost from history but the session stays healthy).
                let store = HistoryStore::new(&self.base_dir, &self.session_id);
                if let Err(e) = store.archive_window(previous_window, messages) {
                    tracing::warn!(
                        session_id = %self.session_id,
                        window_id = previous_window,
                        error = %e,
                        "failed to archive window messages"
                    );
                } else {
                    tracing::info!(
                        session_id = %self.session_id,
                        window_id = previous_window,
                        message_count = messages.len(),
                        "archived window messages to history"
                    );
                    // Archive succeeded — clear messages.jsonl so that a
                    // future resume does not re-inject the already-archived
                    // window (the history tool can retrieve it).
                    let session_dir = self.base_dir.join("sessions").join(&self.session_id);
                    if let Err(e) = clear_messages_jsonl(&session_dir) {
                        tracing::warn!(error = %e, "failed to clear messages.jsonl after archive");
                    }
                }

                // 2. The model's own handoff note, if it wrote one.
                let thread_hint = read_thread_hint(
                    &self.base_dir,
                    &self.session_id,
                    &self.agent_name,
                )
                .map(|hint| format!("<thread_hint>\n{hint}\n</thread_hint>"));

                // 2b. Mechanical activity ledger (v4): extracted from the
                //    outgoing window with zero model participation — the
                //    fallback nudge is advisory, this is the guarantee
                //    (session 20260909_e7053736: 9/9 nudges ignored, 10/10
                //    windows re-read the same files). Mirrored into notes
                //    so `notes.list_files` is never empty at the minimum
                //    budget. Best effort on both I/O.
                let ledger = extract_ledger(messages);
                if let Some(text) = &ledger {
                    let notes = NotesStore::new(
                        &self.base_dir,
                        &self.session_id,
                        &self.agent_name,
                    );
                    if let Err(e) = notes.write_file("activity_ledger.md", text) {
                        tracing::warn!(error = %e, "failed to mirror activity ledger to notes");
                    }
                }
                // Ledger goes LAST in the slot — freshest, closest to the
                // seed where attention is.
                let thread_hint = match (thread_hint, ledger) {
                    (Some(hint), Some(ledger)) => Some(format!("{hint}\n\n{ledger}")),
                    (Some(hint), None) => Some(hint),
                    (None, ledger) => ledger,
                };

                // 3. Assemble + commit the fresh window.
                let new_messages =
                    self.core
                        .build_reset_messages(messages, thread_hint, previous_window);
                let new_window = self.core.commit_reset();
                self.reset_count.fetch_add(1, Ordering::Relaxed);

                tracing::info!(
                    session_id = %self.session_id,
                    from_window = previous_window,
                    to_window = new_window,
                    new_message_count = new_messages.len(),
                    "window reset complete"
                );
                Some(CompactionOutcome {
                    kind: CompactionKind::Reset,
                    messages: new_messages,
                })
            }
        }
    }

    fn token_count_hint(&self, _session_id: &SessionId) -> Option<usize> {
        // We don't have access to messages here; fall back to the react loop's
        // own estimation. Return None so it uses ContextWindowManager.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use phi_agent::TokenBudgetConfig;

    fn shell_with(core: TokenBudgetCore, tmp: &TempDir) -> TokenBudgetCompactor {
        TokenBudgetCompactor::new(
            core,
            tmp.path().to_path_buf(),
            "test_session".to_string(),
            "main".to_string(),
        )
    }

    fn small_core() -> TokenBudgetCore {
        // Minimal boilerplate so the base stays well under the reset point.
        // Budget 200; trail capped at50 so the fresh window (base + trail)
        // always fits under the hard limit (base + 200 + 20).
        let config = TokenBudgetConfig {
            work_budget: 200,
            reminder_threshold: 40,
            fallback_buffer: 20,
            guidance_message: "Use notes/history.".to_string(),
            seed_message: "Reconstruct state.".to_string(),
            user_trail_max_tokens: 50,
            ..Default::default()
        };
        TokenBudgetCore::new(config, Some("You are a helpful assistant."))
    }

    /// Synchronous wrapper for compact in tests (no actual async I/O needed).
    trait CompactSync {
        fn compact_sync(&self, messages: &[ChatMessage]) -> Option<CompactionOutcome>;
    }

    impl CompactSync for TokenBudgetCompactor {
        fn compact_sync(&self, messages: &[ChatMessage]) -> Option<CompactionOutcome> {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let sid = SessionId {
                id: 1,
                external_id: None,
            };
            rt.block_on(self.compact(&sid, messages))
        }
    }

    fn work_msgs(core: &TokenBudgetCore, work_tokens: usize) -> Vec<ChatMessage> {
        // Production shape: the conversation list starts with the System
        // prompt message (which a reset preserves). The core subtracts the
        // fixed base from the total, so size the user message to base + work.
        let total = core.base_overhead() + work_tokens;
        vec![
            ChatMessage::system("You are a helpful assistant."),
            ChatMessage::user("x".repeat(total * 4)),
        ]
    }

    /// Same as [`work_msgs`] plus an assistant turn with a real tool call,
    /// so the ledger extractor has something to hand off.
    fn work_msgs_with_tools(core: &TokenBudgetCore, work_tokens: usize) -> Vec<ChatMessage> {
        let total = core.base_overhead() + work_tokens;
        vec![
            ChatMessage::system("You are a helpful assistant."),
            ChatMessage::user("x".repeat(total * 4)),
            ChatMessage::Assistant {
                content: Some("reading the readme".into()),
                reasoning_content: None,
                thinking_signature: None,
                tool_calls: Some(vec![phi_agent::llm_trait::ToolCallMessage {
                    id: "t1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "README.md"}).to_string(),
                }]),
            },
        ]
    }

    #[test]
    fn reset_archives_and_installs_fresh_window() {
        let tmp = TempDir::new().unwrap();
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs(shell.core(), reset_point);

        // One-turn hold: the first crossing asks for the handoff...
        let hold = shell.compact_sync(&msgs).expect("fallback must fire");
        assert_eq!(hold.kind, CompactionKind::Fallback);
        match hold.messages.last().unwrap() {
            ChatMessage::System { content, .. } => assert!(content.contains("closing")),
            _ => panic!("expected fallback message appended at the END"),
        }
        // ...the next check rotates: archive + fresh window.
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);
        let result = result.messages;
        // Fresh window: system prompt + window info + user trail + guidance + seed
        assert_eq!(result.len(), 5);
        assert!(matches!(&result[0], ChatMessage::System { content, ephemeral: false }
            if content.contains("helpful assistant")));
        assert!(matches!(&result[1], ChatMessage::System { content, .. }
            if content.contains("context_window")));
        // Trail preserves the real user message from the outgoing window.
        assert!(matches!(&result[2], ChatMessage::User { content, .. }
            if content.starts_with("x")));
        assert!(matches!(result.last().unwrap(), ChatMessage::User { .. }));
        assert_eq!(shell.reset_count(), 1);

        // Archive exists for window 001 — full outgoing window incl. system
        let jsonl = tmp
            .path()
            .join("history")
            .join("test_session")
            .join("windows")
            .join("001.jsonl");
        assert!(jsonl.exists());
        assert_eq!(std::fs::read_to_string(&jsonl).unwrap().lines().count(), 2);
    }

    #[test]
    fn thread_hint_from_notes_seeds_new_window() {
        let tmp = TempDir::new().unwrap();
        let hint_dir = tmp.path().join("notes").join("test_session").join("main");
        std::fs::create_dir_all(&hint_dir).unwrap();
        std::fs::write(hint_dir.join("thread_hint.md"), "remember the flipbook").unwrap();

        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs(shell.core(), reset_point);

        assert!(shell.compact_sync(&msgs).is_some()); // fallback hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);
        let result = result.messages;
        // sys + window_info + thread_hint + user trail + guidance + seed
        assert_eq!(result.len(), 6, "expected 6 messages");
        match &result[2] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("thread_hint"));
                assert!(content.contains("remember the flipbook"));
            }
            _ => panic!("expected thread hint message"),
        }
        match &result[2] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("thread_hint"));
                assert!(content.contains("remember the flipbook"));
            }
            _ => panic!("expected thread hint message"),
        }
    }

    #[test]
    fn below_budget_noop_and_no_global_side_effects() {
        let tmp = TempDir::new().unwrap();
        let shell = shell_with(small_core(), &tmp);
        let msgs = work_msgs(shell.core(), 10);
        assert!(shell.compact_sync(&msgs).is_none());
        assert_eq!(shell.reset_count(), 0);
    }

    /// Issue #33: the outcome must carry the action kind so agent-base can
    /// log a nudge append as an append, not as a compaction. One assertion
    /// per phase; the append case also pins the byte-identical passthrough
    /// (same inputs → prior messages untouched). Band positions derive from
    /// the *effective* config — `TokenBudgetCore::new` may clamp the room to
    /// the viability floor, so hard-coded token counts would miss the bands.
    #[test]
    fn outcome_kind_tracks_action_phase() {
        // Phase 1 — within the reminder band: Reminder; the prior history
        // passes through byte-identical, exactly one message appended.
        let tmp = TempDir::new().unwrap();
        let shell = shell_with(small_core(), &tmp);
        let cfg = shell.core().config();
        let near = work_msgs(
            shell.core(),
            cfg.work_budget - cfg.reminder_threshold + 20,
        );
        let input = near.clone();
        let outcome = shell.compact_sync(&near).expect("reminder must fire");
        assert_eq!(outcome.kind, CompactionKind::Reminder);
        assert_eq!(outcome.messages.len(), input.len() + 1);
        for (a, b) in outcome.messages.iter().zip(input.iter()) {
            assert_eq!(
                serde_json::to_string(a).unwrap(),
                serde_json::to_string(b).unwrap(),
                "a nudge append must leave prior messages byte-identical"
            );
        }

        // Phase 2 — budget exhausted, inside the fallback buffer (fresh
        // shell: a reminder already sent short-circuits to None).
        let tmp = TempDir::new().unwrap();
        let shell = shell_with(small_core(), &tmp);
        let cfg = shell.core().config();
        let over = work_msgs(
            shell.core(),
            cfg.work_budget + cfg.fallback_buffer / 2,
        );
        let outcome = shell.compact_sync(&over).expect("fallback must fire");
        assert_eq!(outcome.kind, CompactionKind::Fallback);
        assert_eq!(outcome.messages.len(), over.len() + 1);

        // Phase 3 — buffer exhausted too: one-turn hold, then the rotation.
        let tmp = TempDir::new().unwrap();
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let out = work_msgs(shell.core(), reset_point);
        let hold = shell.compact_sync(&out).expect("hold must fire");
        assert_eq!(hold.kind, CompactionKind::Fallback);
        let rotated = shell.compact_sync(&out).expect("reset must fire");
        assert_eq!(rotated.kind, CompactionKind::Reset);
        assert_eq!(shell.reset_count(), 1);
    }

    #[test]
    fn reset_injects_ledger_and_mirrors_it_to_notes() {
        let tmp = TempDir::new().unwrap();
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs_with_tools(shell.core(), reset_point);

        assert!(shell.compact_sync(&msgs).is_some()); // fallback hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);
        let result = result.messages;
        // sys + window_info + ledger slot + user trail + guidance + seed
        assert_eq!(result.len(), 6);
        match &result[2] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("<activity_ledger>"));
                assert!(content.contains("- read README.md"));
                assert!(content.contains("Where you left off: reading the readme"));
            }
            _ => panic!("expected ledger message"),
        }
        // Mirrored into notes — `notes.list_files` is never empty at the
        // minimum budget.
        let mirrored = tmp
            .path()
            .join("notes")
            .join("test_session")
            .join("main")
            .join("activity_ledger.md");
        let text = std::fs::read_to_string(&mirrored).unwrap();
        assert!(text.contains("- read README.md"));
    }

    #[test]
    fn ledger_appends_after_llm_thread_hint() {
        let tmp = TempDir::new().unwrap();
        let hint_dir = tmp.path().join("notes").join("test_session").join("main");
        std::fs::create_dir_all(&hint_dir).unwrap();
        std::fs::write(hint_dir.join("thread_hint.md"), "the flipbook plan").unwrap();

        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs_with_tools(shell.core(), reset_point);

        assert!(shell.compact_sync(&msgs).is_some()); // fallback hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        let result = result.messages;
        match &result[2] {
            ChatMessage::System { content, .. } => {
                // The model's semantic hint is preserved, not overwritten,
                // and the fresh mechanical ledger rides along.
                let hint_pos = content.find("the flipbook plan").unwrap();
                let ledger_pos = content.find("<activity_ledger>").unwrap();
                assert!(hint_pos < ledger_pos, "ledger must come after the hint");
            }
            _ => panic!("expected combined hint message"),
        }
    }

    #[test]
    fn global_handle_reports_reset_count() {
        let tmp = TempDir::new().unwrap();
        let shell = Arc::new(shell_with(small_core(), &tmp));
        set_global_compactor(Arc::clone(&shell));
        assert_eq!(window_reset_count(), 0);

        let reset_point = shell.core().hard_limit() - shell.core().base_overhead();
        let msgs = work_msgs(shell.core(), reset_point);
        shell.compact_sync(&msgs).expect("fallback must fire"); // hold
        shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(window_reset_count(), 1);
    }
}
