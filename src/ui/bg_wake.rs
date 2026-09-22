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
//! - **Cancelled is not wake-worthy**: a user-initiated Ctrl+C cancel is
//!   already surfaced by the status-bar notice; waking the agent to narrate
//!   it would burn a turn the user never asked for. Done/TimedOut/Error are
//!   outcomes the agent must know about.
//! - **Facts before delivery**: `reported` is flipped before the synthetic
//!   run is handed to the loop, and every task is reported at most once — a
//!   later wake (tasks spawned during the report turn) lists only the new
//!   outcomes.
//!
//! The decision logic lives here (unit-tested); `run.rs` only executes the
//! side effects (`push_system`, `Cmd::Run`) and stays free of timing policy.

use phi_kernel_tools::background_shell::BackgroundTaskStatus;

use super::app::App;

/// One wake-worthy outcome, flattened for composition.
struct WakeItem {
    id: String,
    command: String,
    status: BackgroundTaskStatus,
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

impl App {
    /// Consume the background-task wake, if one is due.
    ///
    /// Returns `Some((transcript_notice, synthetic_input))` when every
    /// *bounded* job is terminal (daemons are ignored — they never finish on
    /// their own), nothing else is in flight (agent idle, no sub-agents), and
    /// at least one wake-worthy outcome has not been reported yet. Marks the
    /// reported flag before returning so a task can never be reported twice;
    /// returns `None` otherwise (the caller just retries on the next tick).
    pub(crate) fn take_bg_wake(&mut self, agent_running: bool) -> Option<(String, String)> {
        // Hold: an injection must never land mid-turn, and a wake while
        // sub-agents or bounded jobs are still running would race the fan-in
        // batch and split the report across turns. Daemons don't hold.
        if agent_running
            || self.running_sub_agents() > 0
            || self.bg_running_split().0 > 0
        {
            return None;
        }

        let ready: Vec<WakeItem> = self.background_tasks.values()
            .filter(|t| !t.reported && is_wake_worthy(&t.status))
            .map(|t| WakeItem {
                id: t.id.clone(),
                command: t.command.clone(),
                status: t.status.clone(),
            })
            .collect();
        if ready.is_empty() {
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

/// Status suffix for the wake listing: what the agent will learn without
/// calling `task_output` (so it can prioritize a failed task first).
fn status_desc(status: &BackgroundTaskStatus) -> String {
    match status {
        BackgroundTaskStatus::Done => "已完成".to_string(),
        BackgroundTaskStatus::TimedOut => "超时".to_string(),
        BackgroundTaskStatus::Cancelled => "已取消".to_string(),
        BackgroundTaskStatus::Error(e) => format!("出错：{e}"),
        BackgroundTaskStatus::Running => "运行中".to_string(),
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

/// Notification + synthetic input for one background wake.
fn compose_wake(items: &[WakeItem]) -> (String, String) {
    let n = items.len();
    let notice = format!("{n} 个后台任务已结束，唤醒 agent 获取输出");
    let listing = items
        .iter()
        .map(|t| format!("- {}：`{}`（{}）", t.id, short_command(&t.command), status_desc(&t.status)))
        .collect::<Vec<_>>()
        .join("\n");
    let input = format!(
        "[系统通知] 你启动的以下后台任务已结束（{n} 个）：\n{listing}\n\n\
         请逐个调用 task_output(task_id) 获取完整输出（任务已结束，wait=false 即可），\
         汇总后向用户汇报结果。不要重新运行这些命令。"
    );
    (notice, input)
}

/// Test-only seam: build a panel entry directly (the registry path needs a
/// live `BackgroundTaskRegistry` + tokio runtime). `timeout_ms == 0` builds a
/// daemon entry; anything else is a bounded job.
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
    }
}

#[cfg(test)]
#[path = "bg_wake_tests.rs"]
mod bg_wake_tests;
