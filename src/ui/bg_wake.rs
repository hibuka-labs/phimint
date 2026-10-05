//! Background-task auto-wake: turning finished shell tasks into a turn.
//!
//! The sibling of `child_results.rs` for the *other* kind of in-flight work.
//! When the main agent's turn ends with background shell tasks still running
//! (spawned via `execute_command` with `background: true`), the status drops
//! to `Waiting { bg: n }` — and when the last task ends, the agent must be
//! woken to fetch the outputs via `task_output` and report them. Without the
//! wake, the user stares at "done" while results pile up unread.
//!
//! Delivery policy (mirrors the child fan-in router, same bug class):
//!
//! - **Hold** while the agent is mid-turn (`agent_running`), while sub-agents
//!   are still running, or while any *bounded* background job is still
//!   running (`timeout_ms > 0`). An injection must never land mid-turn —
//!   held injections are exactly the fan-in injection bug (results not
//!   injected → hang). Holding also gives free batching: tasks finishing
//!   close together are reported in one wake. Indefinite daemons
//!   (`timeout_ms == 0`) deliberately do NOT hold the wake — a server never
//!   "finishes", so waiting on it would swallow the reports of jobs that did
//!   (the hold bug this line replaces).
//! - **Batch semantics** like the child watcher: the wake fires once every
//!   *bounded* job is terminal, listing every not-yet-reported outcome
//!   (done / timed out / error) in one synthetic run. A daemon joins a
//!   listing only when it itself reaches a terminal state.
//! - **Quiet window** (issue #30): a notification turn is a full-context
//!   turn, so a burst of completions must not cost one wake each. The wake
//!   waits out [`BG_WAKE_QUIET`] anchored on the *newest* terminal outcome —
//!   each new completion pushes the deadline out (debounce) — and fires the
//!   moment that deadline passes and the hold gates above are clear. The hold
//!   postpones a wake; it never restarts the window behind one.
//! - **Cancelled is not wake-worthy**: a user-initiated Ctrl+C cancel is
//!   already surfaced by the status-bar notice; waking the agent to narrate
//!   it would burn a turn the user never asked for. Done/TimedOut/Error are
//!   outcomes the agent must know about.
//! - **Facts before delivery**: `reported` is flipped before the synthetic
//!   run is handed to the loop, and every task is reported at most once — a
//!   later wake (tasks spawned during the report turn) lists only the new
//!   outcomes.
//! - **Freshness is the wake's own concern**: `take_bg_wake` reconciles the
//!   panel map from the registry before deciding (issue #29: the loop used to
//!   check the wake *before* the reconcile tick, so a just-finished task
//!   missed the first wake and arrived alone a full report-turn later).
//! - **The wake is self-sufficient**: each listing carries the task's output
//!   tail (≤2KB, UTF-8-safe). The registry entry may already be gone
//!   (consume-based GC after a `task_output` fetch, or the 30-min fallback) —
//!   the agent can still report from the tail (issue #35).
//!
//! The decision logic lives here (unit-tested); `run.rs` only executes the
//! side effects (`push_system`, `Cmd::Run`) and stays free of timing policy.

use std::time::{Duration, Instant};

use phi_kernel_tools::background_shell::{BackgroundTaskStatus, tail_utf8};

use super::app::{App, BackgroundTaskEntry};

/// One wake-worthy outcome, flattened for composition.
struct WakeItem {
    id: String,
    command: String,
    status: BackgroundTaskStatus,
    output_tail: String,
    consumed: bool,
    /// Terminal time — the quiet window anchors on the newest of these.
    finished_at: Option<Instant>,
}

/// Whether a terminal task outcome should wake the agent. `Cancelled` is
/// deliberately absent — see the module docs.
fn is_wake_worthy(status: &BackgroundTaskStatus) -> bool {
    matches!(
        status,
        BackgroundTaskStatus::Done
            | BackgroundTaskStatus::TimedOut
            | BackgroundTaskStatus::Error(_)
    )
}

/// Whether the panel map may drop this entry yet (display lifecycle).
///
/// An unreported wake-worthy outcome owes the agent a report — the 3s
/// display reap and the registry-stale cleanup must never eat that debt
/// (issue #29: a sibling finished ≥3s earlier used to be reaped in the same
/// reconcile pass that flipped the batch to Idle, and the upsert guard made
/// the loss permanent). `reported` tasks and non-wake-worthy ones
/// (`Cancelled`, still running) carry no debt and are droppable.
pub(crate) fn may_reap(entry: &BackgroundTaskEntry) -> bool {
    entry.reported || !is_wake_worthy(&entry.status)
}

/// Aggregation quiet window (issue #30): after the newest terminal outcome,
/// wait this long for its siblings before composing the wake. Notification
/// turns are full-context turns (~185K input tokens at the session's end), so
/// one wake per completion is a tax — session 20260927_4180845c paid 10 wakes
/// in 9 minutes for 100–500-char status reports. 15s collapses a completion
/// burst into one listing while an isolated completion still wakes within the
/// window of its terminal state.
pub(crate) const BG_WAKE_QUIET: Duration = Duration::from_secs(15);

impl App {
    /// Consume the background-task wake, if one is due.
    ///
    /// Reconciles the panel map from the registry first, so a task that just
    /// went terminal is always part of the *first* wake after its finish —
    /// batching is decided on fresh state, never on a tick-lagged cache.
    ///
    /// Returns `Some((transcript_notice, synthetic_input))` when the quiet
    /// window has stood ([`BG_WAKE_QUIET`], issue #30), every *bounded* job is
    /// terminal (daemons are ignored — they never finish on their own),
    /// nothing is in flight (no root turn queued or running, no sub-agents),
    /// and at least one wake-worthy outcome has not been reported yet. Marks
    /// the reported flag before returning so a task can never be reported
    /// twice; returns `None` otherwise (the caller just retries on the next
    /// tick).
    ///
    /// `now` is the decision clock: the quiet-window deadline is compared
    /// against it, so tests pin the window without sleeping (same seam as
    /// [`is_writing_hint`](super::app::is_writing_hint)).
    pub(crate) fn take_bg_wake(&mut self, now: Instant) -> Option<(String, String)> {
        // Freshness first — even when held, so the map stops lagging.
        let _ = self.reconcile_background_tasks();

        // Hold: an injection must never land mid-turn — including synthetic
        // turns (issue #29: `running` used to be set only on keyboard submit,
        // so wakes composed mid-run and queued behind the in-flight run,
        // arriving alone minutes later). Also hold while sub-agents or
        // bounded jobs run, to race neither the fan-in batch. Daemons
        // (`timeout_ms == 0`) don't hold.
        if self.running
            || matches!(self.status, super::app::AgentStatus::Running { .. })
            || self.running_sub_agents() > 0
            || self.bg_running_split().0 > 0
        {
            return None;
        }

        let ready: Vec<WakeItem> = self
            .background_tasks
            .values()
            .filter(|t| !t.reported && is_wake_worthy(&t.status))
            .map(|t| WakeItem {
                id: t.id.clone(),
                command: t.command.clone(),
                status: t.status.clone(),
                output_tail: t.output_tail.clone(),
                consumed: t.consumed,
                finished_at: t.finished_at,
            })
            .collect();
        if ready.is_empty() {
            return None;
        }

        // Quiet window (issue #30): anchor on the newest terminal outcome so a
        // sibling finishing mid-window pushes the deadline out — one wake for
        // the whole burst. An outcome with no `finished_at` (production stamps
        // every terminal transition) counts as already past the window: the
        // window may delay a wake, never lose one.
        if let Some(newest) = ready.iter().filter_map(|t| t.finished_at).max()
            && now.saturating_duration_since(newest) < BG_WAKE_QUIET
        {
            return None;
        }

        // Facts before delivery: flip the flag before the caller hands the
        // synthetic run to the agent task, so a crash between the two steps
        // loses a report, never duplicates one.
        for task in self.background_tasks.values_mut() {
            if is_wake_worthy(&task.status) {
                task.reported = true;
            }
        }

        Some(compose_wake(&ready))
    }
}

/// Cap for a raw error string embedded in the wake listing (AC: the wake is
/// bounded — an error message must not balloon the synthetic turn).
const MAX_ERROR_DESC: usize = 200;

/// Status suffix for the wake listing: what the agent will learn without
/// calling `task_output` (so it can prioritize a failed task first).
fn status_desc(status: &BackgroundTaskStatus) -> String {
    match status {
        BackgroundTaskStatus::Done => "done".to_string(),
        BackgroundTaskStatus::TimedOut => "timed out".to_string(),
        BackgroundTaskStatus::Cancelled => "cancelled".to_string(),
        BackgroundTaskStatus::Error(e) => {
            let trimmed = e.trim();
            if trimmed.chars().count() <= MAX_ERROR_DESC {
                format!("error: {trimmed}")
            } else {
                let head: String = trimmed.chars().take(MAX_ERROR_DESC).collect();
                format!("error: {head}…")
            }
        }
        BackgroundTaskStatus::Running => "running".to_string(),
    }
}

/// Command for the wake listing, capped so a pathological 4 KB command line
/// can't blow up the synthetic message.
fn short_command(command: &str) -> String {
    const MAX: usize = 120;
    let trimmed = command.trim();
    if trimmed.chars().count() <= MAX {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(MAX).collect();
        format!("{head}…")
    }
}

/// Per-task output tail budget inside one wake (issue #35 AC3). Also the
/// capture cap when the panel snapshots a task's tail at terminal time.
pub(crate) const MAX_TAIL_PER_TASK: usize = 2 * 1024;
/// Total tail budget across one wake; when the batch is wide, the per-task
/// share shrinks so the synthetic turn stays bounded.
pub(crate) const MAX_TAIL_PER_WAKE: usize = 8 * 1024;

/// Notification + synthetic input for one background wake.
///
/// Each listing carries its task's output tail (UTF-8-safe, ≤2KB/task and
/// ≤8KB/wake) so the agent can report even when `task_output` can no longer
/// fetch the entry (consumed by an earlier fetch, or GC'd past the fallback
/// TTL). Consumed tasks say so — the tail *is* the report source there.
fn compose_wake(items: &[WakeItem]) -> (String, String) {
    let n = items.len();
    let notice = format!("{n} background task(s) finished; waking the agent to report");
    let per_task = MAX_TAIL_PER_TASK.min(MAX_TAIL_PER_WAKE / n.max(1));
    let listing = items
        .iter()
        .map(|t| {
            let head = format!(
                "- {}: `{}` ({})",
                t.id,
                short_command(&t.command),
                status_desc(&t.status)
            );
            let tail = tail_utf8(&t.output_tail, per_task);
            let consumed_note = if t.consumed {
                " (output already consumed by task_output)"
            } else {
                ""
            };
            if tail.is_empty() {
                format!("{head}\n  (no output{consumed_note})")
            } else {
                format!("{head}\n  output tail{consumed_note}:\n----\n{tail}\n----")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let input = format!(
        "[System notice] Background tasks you started have finished ({n}):\n{listing}\n\n\
         Report to the user first, summarizing from the output tails above. Call \
         task_output(task_id) for full output if you need it (the task is finished, so \
         wait=false is enough; if it was reclaimed you get not_found, so trust the tail). \
         Do not re-run these commands."
    );
    (notice, input)
}

/// Test-only seam: build a panel entry directly (the registry path needs a
/// live `BackgroundTaskRegistry` + tokio runtime). `timeout_ms == 0` builds a
/// daemon entry; anything else is a bounded job. `finished_at` is left `None`
/// (undated → treated as past the quiet window, i.e. immediately due); tests
/// that exercise window behavior stamp it explicitly.
#[cfg(test)]
pub(crate) fn mock_bg_entry(
    id: &str,
    command: &str,
    status: BackgroundTaskStatus,
    timeout_ms: u64,
) -> super::app::BackgroundTaskEntry {
    super::app::BackgroundTaskEntry {
        id: id.to_string(),
        command: command.to_string(),
        timeout_ms,
        status,
        started_at: std::time::Instant::now(),
        finished_at: None,
        reported: false,
        output_tail: String::new(),
        consumed: false,
    }
}

#[cfg(test)]
#[path = "bg_wake_tests.rs"]
mod bg_wake_tests;
