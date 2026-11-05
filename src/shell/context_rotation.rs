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

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use phi_agent::{
    ChatMessage, CompactionKind, CompactionOutcome, ContextCompaction, HintSlot, SessionId,
    TokenBudgetAction, TokenBudgetCore, estimate_messages_tokens, persist_window_messages,
};

use phi_kernel_tools::context_rotation::{
    HistoryStore, NotesStore, SessionScope, extract_artifact_index, extract_ledger,
    extract_plan_state, read_thread_hint,
};

/// Note names shown on the hint slot's one-line notes index (see
/// [`TokenBudgetCompactor::notes_index_line`]).
const NOTES_INDEX_MAX: usize = 8;

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
///
/// This mutates process-global state: in production there is exactly one
/// agent per process, so the last install wins and is the only one. Tests
/// build agents in parallel, so a test that installs a compactor and a test
/// that reads the global must serialize on
/// `global_compactor_test_lock` — otherwise the reader sees the writer's
/// compactor (a different reset count) and fails at random.
pub fn set_global_compactor(compactor: Arc<TokenBudgetCompactor>) {
    if let Ok(mut guard) = TOKEN_BUDGET_COMPACTOR.lock() {
        *guard = Some(compactor);
    }
}

/// Re-point every session-keyed store to a resumed session — the `/resume`
/// switch. Called from `Wire::session_boot`, the one per-session seam the
/// shell re-arms every session iteration. The compactor carries the shared
/// [`SessionScope`] that the history/notes stores and the code-cell factory
/// also hold (build.rs hands out one handle), so a single re-point here moves
/// them all; the rotation counter is reseeded from the target's archive
/// alongside. No compactor installed (token budget off) means no
/// session-keyed stores exist — nothing to do.
pub fn repoint_current_session(session_id: &str) {
    let compactor = TOKEN_BUDGET_COMPACTOR
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    match compactor {
        Some(compactor) => compactor.repoint(session_id),
        None => tracing::debug!(
            "repoint_current_session: no token-budget compactor installed, nothing to re-point"
        ),
    }
}

/// Test-only lock serializing every test that installs or observes the
/// process-global compactor handle.
///
/// A tokio mutex, because one of the two holders is an async test and a
/// `std` guard must not be held across an await. The sync holder takes it
/// with `blocking_lock`; the async holder awaits it.
#[cfg(test)]
pub(crate) fn global_compactor_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &LOCK
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
    /// The live session's storage scope — history, notes and the session dir
    /// resolve their paths through it at use time, so one re-point (the
    /// `/resume` switch) moves every subsequent write. A frozen session id
    /// kept writing the abandoned bootstrap session's trees after a switch
    /// (2026-11-04) — the misrouting this scope exists to prevent.
    scope: Arc<SessionScope>,
    /// Agent name (used for notes path and thread hint lookup).
    agent_name: String,
    /// Number of window resets that have occurred (TUI notification).
    reset_count: AtomicUsize,
}

impl TokenBudgetCompactor {
    pub fn new(core: TokenBudgetCore, scope: Arc<SessionScope>, agent_name: String) -> Self {
        let this = Self {
            core,
            scope,
            agent_name,
            reset_count: AtomicUsize::new(0),
        };
        // Seed the rotation counter from the target's archive: a resumed
        // session may already hold windows 001..N, and a counter restarting
        // at 1 would truncate `windows/001.jsonl` on the next rotation.
        this.reseed_window_id();
        this
    }

    /// Serve a different session — the `/resume` switch. Every store sharing
    /// this compactor's scope follows; the rotation counter is reseeded from
    /// the target's archive so numbering continues instead of clobbering.
    pub fn repoint(&self, session_id: &str) {
        self.scope.repoint(session_id);
        self.reseed_window_id();
    }

    /// Continue the current session's archive numbering: the next rotation
    /// archives as `last_window_id + 1` (filenames on disk are the authority
    /// — see [`HistoryStore::last_window_id`]).
    fn reseed_window_id(&self) {
        let next = HistoryStore::new_scoped(Arc::clone(&self.scope)).last_window_id() + 1;
        self.core.state().window_id.store(next, Ordering::Relaxed);
    }

    /// The decision core (window id, base overhead, config access).
    pub fn core(&self) -> &TokenBudgetCore {
        &self.core
    }

    /// Number of window resets that have occurred.
    pub fn reset_count(&self) -> usize {
        self.reset_count.load(Ordering::Relaxed)
    }

    /// One-line index of the notes directory for the hint slot: the fresh
    /// window knows what exists without spending tool calls listing it
    /// (session 20261103_4206f951: every window paid list/read calls to
    /// rediscover the same files). One line, capped — the slot is attention,
    /// not a directory dump.
    fn notes_index_line(&self) -> Option<String> {
        let notes = NotesStore::new_scoped(Arc::clone(&self.scope), &self.agent_name);
        let mut files = notes.list_files(None, NOTES_INDEX_MAX + 1);
        if files.is_empty() {
            return None;
        }
        let more = files.len() > NOTES_INDEX_MAX;
        files.truncate(NOTES_INDEX_MAX);
        let mut names: Vec<String> = files
            .iter()
            .map(|f| {
                if f.chars().count() > 48 {
                    let head: String = f.chars().take(48).collect();
                    format!("{head}…")
                } else {
                    f.clone()
                }
            })
            .collect();
        if more {
            names.push("+more".to_string());
        }
        Some(format!("notes/: {}", names.join(", ")))
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
                let store = HistoryStore::new_scoped(Arc::clone(&self.scope));
                if let Err(e) = store.archive_window(previous_window, messages) {
                    tracing::warn!(
                        session_id = %self.scope.session_id(),
                        window_id = previous_window,
                        error = %e,
                        "failed to archive window messages"
                    );
                } else {
                    tracing::info!(
                        session_id = %self.scope.session_id(),
                        window_id = previous_window,
                        message_count = messages.len(),
                        "archived window messages to history"
                    );
                    // `messages.jsonl` is NOT deleted here. It used to be, to
                    // stop a resume re-injecting the already-archived window —
                    // but that left the session with no restorable snapshot at
                    // all: unlisted by the picker and blank on resume whenever
                    // the process died before the next turn-end persist (and
                    // both rotated sessions did, on the same tool panic).
                    // Instead the file is rewritten below with just the fresh
                    // window, which is both non-reinjecting AND restorable.
                }

                // 2. The model's own handoff note, if it wrote one. Its
                //    presence drives the seed variant in the core: with a
                //    handoff the seed points at the hint slot above, without
                //    one it says to write the note — no guaranteed
                //    `thread_hint.md` probe either way.
                let handoff = read_thread_hint(
                    self.scope.base_dir(),
                    &self.scope.session_id(),
                    &self.agent_name,
                )
                .map(|hint| format!("<thread_hint>\n{hint}\n</thread_hint>"));

                // 2b. Mechanical activity ledger (v4): extracted from the
                //    outgoing window with zero model participation — the
                //    fallback nudge is advisory, this is the guarantee
                //    (session 20260909_e7053736: 9/9 nudges ignored, 10/10
                //    windows re-read the same files). Mirrored into notes
                //    so `notes_list_files` is never empty at the minimum
                //    budget. Best effort on both I/O.
                let notes = NotesStore::new_scoped(Arc::clone(&self.scope), &self.agent_name);
                let ledger = extract_ledger(messages);
                if let Some(text) = &ledger
                    && let Err(e) = notes.write_file("activity_ledger.md", text)
                {
                    tracing::warn!(error = %e, "failed to mirror activity ledger to notes");
                }
                // 2b-2. Plan state (privileged channel): the live
                //    `update_plan` objective+steps, verbatim and uncapped —
                //    the display-only protocol forgets everything at a
                //    rotation while the task is still on its checklist.
                //    Mirrored to notes/plan_state.md; a window that made no
                //    plan call reads the mirror back, so the plan survives
                //    rotation until the model actually changes it.
                let extracted_plan = extract_plan_state(messages);
                let plan = extracted_plan.clone().or_else(|| {
                    notes
                        .read_file("plan_state.md", None, None)
                        .ok()
                        .filter(|text| !text.trim().is_empty())
                });
                if let Some(text) = &extracted_plan
                    && let Err(e) = notes.write_file("plan_state.md", text)
                {
                    tracing::warn!(error = %e, "failed to mirror plan state to notes");
                }
                // 2c. Mechanical artifact index (M3c): what the outgoing
                //    window spilled to the artifact store. The bodies travel
                //    with the archive but stay fetchable (`obs.fetch('<id>')`)
                //    — the index is their part in the fresh window.
                let artifact_index = extract_artifact_index(messages);
                // 2d. Notes index: computed AFTER the mirrors above, so the
                //    line reflects what the notes directory holds now. Slot
                //    order (handoff, plan, notes index, artifact index,
                //    ledger — the ledger LAST, freshest, closest to the seed
                //    where attention is) is core policy.
                let notes_index = self.notes_index_line();
                let hint = HintSlot {
                    handoff,
                    plan,
                    notes_index,
                    artifact_index,
                    ledger,
                };

                // 3. Assemble + commit the fresh window.
                let new_messages = self
                    .core
                    .build_reset_messages(messages, hint, previous_window);
                let new_window = self.core.commit_reset();
                self.reset_count.fetch_add(1, Ordering::Relaxed);

                // 3b. Persist the fresh window immediately. `messages.jsonl`
                //     must always hold *something* restorable: the turn-end
                //     persist is the only other writer, and a rotation is
                //     usually followed by more work in this turn — dying in
                //     between (a tool panic, a crash) used to leave the
                //     session invisible to the resume picker.
                let session_dir = self.scope.session_dir();
                if let Err(e) = std::fs::create_dir_all(&session_dir) {
                    tracing::warn!(error = %e, "failed to create session dir for fresh window");
                }
                if let Err(e) = persist_window_messages(&session_dir, &new_messages) {
                    tracing::warn!(error = %e, "failed to persist fresh window after reset");
                }

                tracing::info!(
                    session_id = %self.scope.session_id(),
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
        let scope = Arc::new(SessionScope::new(tmp.path(), "test_session"));
        TokenBudgetCompactor::new(core, scope, "main".to_string())
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
        assert!(
            matches!(&result[0], ChatMessage::System { content, ephemeral: false }
            if content.contains("helpful assistant"))
        );
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

    /// A reset must leave `messages.jsonl` holding the fresh window. It used
    /// to *delete* the file (so a resume could not re-inject the archived
    /// window), which left a session that rotated and then died with nothing
    /// to resume from — unlisted by the picker and blank on resume. Both
    /// rotated sessions in `~/.phimint` ended exactly that way.
    #[test]
    fn reset_leaves_messages_jsonl_restorable() {
        let tmp = TempDir::new().unwrap();
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs(shell.core(), reset_point);

        let _ = shell.compact_sync(&msgs).expect("fallback must fire"); // hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);

        let messages_path = tmp
            .path()
            .join("sessions")
            .join("test_session")
            .join("messages.jsonl");
        let stored = std::fs::read_to_string(&messages_path)
            .expect("messages.jsonl must survive a window reset");
        assert!(
            stored.lines().any(|l| !l.trim().is_empty()),
            "messages.jsonl must be non-blank after a reset, got: {stored:?}"
        );

        // The fresh window, not a re-injection of the archived one: the
        // trail's user message is kept, the outgoing work blob is not.
        assert!(
            stored.contains("x"),
            "fresh window must carry the user trail"
        );
        let archived = std::fs::read_to_string(
            tmp.path()
                .join("history")
                .join("test_session")
                .join("windows")
                .join("001.jsonl"),
        )
        .unwrap();
        let outgoing_len = archived.lines().count();
        assert!(outgoing_len > 0);
        assert!(
            stored.lines().count() < outgoing_len + 5,
            "messages.jsonl should hold the fresh window only"
        );
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
        match &result[5] {
            ChatMessage::User { content, .. } => {
                assert_eq!(
                    content, "Reconstruct state.",
                    "handoff present → seed variant"
                );
            }
            _ => panic!("expected seed message"),
        }
    }

    /// ③ The hint slot carries a one-line notes index computed AFTER the
    /// ledger mirror: the fresh window knows what the notes directory holds
    /// without spending tool calls listing it.
    #[test]
    fn reset_hint_slot_carries_notes_index() {
        let tmp = TempDir::new().unwrap();
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs_with_tools(shell.core(), reset_point);

        assert!(shell.compact_sync(&msgs).is_some()); // fallback hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);
        // The mirror lands first, so the index lists the fresh ledger.
        match &result.messages[2] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("notes/: activity_ledger.md"), "{content}");
            }
            _ => panic!("expected hint slot"),
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
        let near = work_msgs(shell.core(), cfg.work_budget - cfg.reminder_threshold + 20);
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
        let over = work_msgs(shell.core(), cfg.work_budget + cfg.fallback_buffer / 2);
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
        // Mirrored into notes — `notes_list_files` is never empty at the
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
    fn reset_hint_carries_plan_state_and_mirrors_it() {
        let tmp = TempDir::new().unwrap();
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let mut msgs = work_msgs_with_tools(shell.core(), reset_point);
        msgs.push(ChatMessage::Assistant {
            content: None,
            reasoning_content: None,
            thinking_signature: None,
            tool_calls: Some(vec![phi_agent::llm_trait::ToolCallMessage {
                id: "p1".into(),
                name: "update_plan".into(),
                arguments: serde_json::json!({
                    "objective": "Ship the flipbook export",
                    "plan": [{"step": "wire the exporter", "status": "in_progress"}],
                })
                .to_string(),
            }]),
        });

        assert!(shell.compact_sync(&msgs).is_some()); // fallback hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        let result = result.messages;
        match &result[2] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("<plan>"), "{content}");
                assert!(
                    content.contains("Objective: Ship the flipbook export"),
                    "{content}"
                );
                assert!(
                    content.contains("- [in_progress] wire the exporter"),
                    "{content}"
                );
                // Plan travels with the hint, before navigation and the ledger.
                let plan_pos = content.find("<plan>").unwrap();
                let ledger_pos = content.find("<activity_ledger>").unwrap();
                assert!(plan_pos < ledger_pos, "plan must come before the ledger");
            }
            _ => panic!("expected combined hint message"),
        }
        // Mirrored to notes: the plan outlives the window it came from even
        // if the model never calls the tool again.
        let mirrored = tmp
            .path()
            .join("notes")
            .join("test_session")
            .join("main")
            .join("plan_state.md");
        let text = std::fs::read_to_string(&mirrored).unwrap();
        assert!(
            text.contains("Objective: Ship the flipbook export"),
            "{text}"
        );
    }

    #[test]
    fn plan_state_survives_a_quiet_window_via_the_mirror() {
        // Window N carried a plan and mirrored it; window N+1 has no
        // update_plan call at all — the mirror keeps the plan in the hint
        // (the display-only protocol would otherwise forget it here).
        let tmp = TempDir::new().unwrap();
        let hint_dir = tmp.path().join("notes").join("test_session").join("main");
        std::fs::create_dir_all(&hint_dir).unwrap();
        std::fs::write(
            hint_dir.join("plan_state.md"),
            "<plan>\nObjective: Ship the flipbook export\n- [in_progress] wire the exporter\n</plan>",
        )
        .unwrap();

        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs_with_tools(shell.core(), reset_point);

        assert!(shell.compact_sync(&msgs).is_some()); // fallback hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        match &result.messages[2] {
            ChatMessage::System { content, .. } => {
                assert!(
                    content.contains("Objective: Ship the flipbook export"),
                    "{content}"
                );
            }
            _ => panic!("expected combined hint message"),
        }
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
        // The global handle is process-wide: any other test that builds an
        // agent with token-budget options installs its own compactor and
        // would overwrite ours mid-test (the reader then sees that other
        // compactor's reset count and the assertion fails at random).
        let _guard = global_compactor_test_lock().blocking_lock();
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

    /// Regression (2026-11-04 incident): after a `/resume` switch the
    /// rotation archive and the fresh-window `messages.jsonl` must land in
    /// the RESUMED session's trees — not the abandoned bootstrap one — and
    /// the window counter must continue the target's archive numbering
    /// instead of truncating `windows/001.jsonl`.
    #[test]
    fn repoint_routes_rotation_and_resumes_numbering() {
        let tmp = TempDir::new().unwrap();
        // The resumed session's past life: one archived window.
        let resumed_windows = tmp.path().join("history/resumed/windows");
        std::fs::create_dir_all(&resumed_windows).unwrap();
        std::fs::write(
            resumed_windows.join("001.jsonl"),
            "{\"window\":1,\"index\":0,\"role\":\"user\",\"content\":\"old\",\"ts\":\"t\"}\n",
        )
        .unwrap();

        // A compactor built on the then-live bootstrap session...
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let scope = Arc::new(SessionScope::new(tmp.path(), "bootstrap"));
        let shell = TokenBudgetCompactor::new(core, scope, "main".to_string());

        // ...switches to the resumed session before any rotation.
        shell.repoint("resumed");

        let msgs = work_msgs(shell.core(), reset_point);
        let _ = shell.compact_sync(&msgs).expect("fallback must fire"); // hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);

        // The archive continues the resumed session's numbering (002), and
        // the existing 001 survives byte-for-byte...
        let archived_001 = std::fs::read_to_string(resumed_windows.join("001.jsonl")).unwrap();
        assert!(
            archived_001.contains("old"),
            "existing archive must survive"
        );
        assert!(
            resumed_windows.join("002.jsonl").exists(),
            "rotation must continue the target's numbering, not restart at 001"
        );
        // ...the fresh window's messages.jsonl lands beside the resumed
        // session's files, and the bootstrap tree stays untouched.
        assert!(tmp.path().join("sessions/resumed/messages.jsonl").exists());
        assert!(!tmp.path().join("history/bootstrap").exists());
        assert!(!tmp.path().join("sessions/bootstrap").exists());
    }

    /// Boot-resume shape: the compactor is constructed on a session that
    /// already holds archived windows — the counter must start past them so
    /// the first rotation archives 002, not a truncating rewrite of 001.
    #[test]
    fn construction_seeds_window_id_from_existing_archive() {
        let tmp = TempDir::new().unwrap();
        let windows = tmp.path().join("history/test_session/windows");
        std::fs::create_dir_all(&windows).unwrap();
        std::fs::write(
            windows.join("001.jsonl"),
            "{\"window\":1,\"index\":0,\"role\":\"user\",\"content\":\"old\",\"ts\":\"t\"}\n",
        )
        .unwrap();

        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let shell = shell_with(core, &tmp);
        let msgs = work_msgs(shell.core(), reset_point);
        let _ = shell.compact_sync(&msgs).expect("fallback must fire"); // hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);

        assert!(
            windows.join("002.jsonl").exists(),
            "the first rotation after a resume must not rewrite 001"
        );
    }

    /// The production seam: `Wire::session_boot` calls this every session
    /// iteration. It must reach the compactor installed globally (the same
    /// instance the stores share their scope with).
    #[test]
    fn global_repoint_forwards_to_the_compactor() {
        let _guard = global_compactor_test_lock().blocking_lock();
        let tmp = TempDir::new().unwrap();
        let core = small_core();
        let reset_point = core.hard_limit() - core.base_overhead();
        let scope = Arc::new(SessionScope::new(tmp.path(), "bootstrap"));
        let shell = Arc::new(TokenBudgetCompactor::new(core, scope, "main".to_string()));
        set_global_compactor(Arc::clone(&shell));

        repoint_current_session("resumed");

        let msgs = work_msgs(shell.core(), reset_point);
        let _ = shell.compact_sync(&msgs).expect("fallback must fire"); // hold
        let result = shell.compact_sync(&msgs).expect("reset must fire");
        assert_eq!(result.kind, CompactionKind::Reset);
        assert!(
            tmp.path()
                .join("history/resumed/windows/001.jsonl")
                .exists(),
            "the global seam must move the compactor's writes"
        );
    }
}
