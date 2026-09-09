//! Child-result presentation for the TUI loop (fan-in redesign).
//!
//! The delivery-timing policy — Progress display-only / Batch wake,
//! hold-until-idle, per-report truncation cap — is framework code now: it
//! lives in `agent-works` (`multi_agent::fan_in`) next to `ChildReport` /
//! `ChildResultEvent` and is re-exported by phi-agent. This module is the
//! thin presentation half that stayed behind: it maps the policy's data-only
//! routes to the TUI's words (Chinese copy) and synthetic runs.
//!
//! Session 20260903_0cf95e79: the watcher is the fan-in coordinator and emits
//! two kinds of events, so the old "hold every single result and flush at
//! turn end" policy split in two:
//!
//! - **Progress** — one child returned. Display-only, never injected into
//!   the parent's context (the parent is deliberately not woken). The
//!   Focus summary is for the user; without one, a plain status word.
//! - **Batch** — every child has returned. This is what wakes the parent:
//!   injected immediately when the agent is idle, held and flushed right
//!   after the turn ends when it is running.
//!
//! The decision logic used to be inline in `run_tui`'s `while` body with
//! zero test coverage — exactly where the Phase 2-4 dead-channel bug hid
//! (session 20260903_2438d139, P3). The timing policy itself is unit-tested
//! in agent-works; this module keeps the copy tests, and the caller executes
//! the side effects (`set_notice`, `push_system`, `Cmd::Run`).

use phi_agent::{ChildReport, ChildResultEvent};

/// What the caller should do with a watcher event.
#[derive(Debug)]
pub enum ChildResultRoute {
    /// Display-only: a progress line for the transcript (never injected).
    Notice {
        /// Transcript text.
        notice: String,
    },
    /// The parent is mid-turn: the batch was held until the turn ends; show
    /// a status-bar notice.
    Hold {
        /// Status-bar text.
        notice: String,
    },
    /// Start a synthetic run carrying the batch reports.
    Inject {
        /// Transcript notification.
        notice: String,
        /// Synthetic user input carrying the child report(s).
        input: String,
    },
}

/// Presentation adapter over the framework router
/// ([`phi_agent::ChildResultRouter`]): holds the delivery state and formats
/// its routes as TUI copy.
pub struct ChildResultRouter {
    inner: phi_agent::ChildResultRouter,
}

impl ChildResultRouter {
    pub fn new() -> Self {
        Self {
            inner: phi_agent::ChildResultRouter::new(),
        }
    }

    /// Route one freshly delivered watcher event.
    pub fn on_event(&mut self, agent_running: bool, event: ChildResultEvent) -> ChildResultRoute {
        match self.inner.on_event(agent_running, event) {
            phi_agent::ChildResultRoute::Progress {
                agent_path,
                status,
                summary,
            } => ChildResultRoute::Notice {
                notice: progress_notice(&agent_path, &status, summary.as_deref()),
            },
            phi_agent::ChildResultRoute::Held { held } => ChildResultRoute::Hold {
                notice: format!("所有子 agent 已返回（{held} 个），结果将在本轮结束后注入"),
            },
            phi_agent::ChildResultRoute::Batch { reports } => {
                let (notice, input) = compose_inject(&reports);
                ChildResultRoute::Inject { notice, input }
            }
        }
    }

    /// Drain reports held during the turn that just ended, as one batched
    /// synthetic run. Returns `None` while nothing is pending.
    pub fn flush_when_idle(&mut self) -> Option<ChildResultRoute> {
        let reports = self.inner.flush_when_idle()?;
        let (notice, input) = compose_inject(&reports);
        Some(ChildResultRoute::Inject { notice, input })
    }
}

/// Progress line for the transcript: Focus summary when available, plain
/// status word otherwise.
fn progress_notice(agent_path: &str, status: &str, summary: Option<&str>) -> String {
    let name = short_name(agent_path);
    match (status, summary) {
        ("ok", Some(s)) => format!("子 agent {name}：{s}"),
        ("ok", None) => format!("子 agent {name} 已完成"),
        ("error", Some(s)) => format!("子 agent {name} 出错：{s}"),
        ("error", None) => format!("子 agent {name} 执行出错"),
        ("closed", _) => format!("子 agent {name} 已关闭"),
        (other, _) => format!("子 agent {name} 状态：{other}"),
    }
}

/// Per-report injection cap, in characters — owned by the framework router
/// (see `phi_agent::ChildResultRouter::MAX_REPORT_CHARS` for the session
/// 20260904_c6559510 history).
const MAX_REPORT_CHARS: usize = phi_agent::ChildResultRouter::MAX_REPORT_CHARS;

/// Notification + synthetic input for a set of batch reports.
fn compose_inject(reports: &[ChildReport]) -> (String, String) {
    let notice = format!("{} 个子 agent 结果已注入上下文", reports.len());
    let input = reports
        .iter()
        .map(|r| {
            let (kept, total) = phi_agent::ChildResultRouter::clamp_report(&r.message);
            if total <= MAX_REPORT_CHARS {
                kept
            } else {
                format!(
                    "{kept}\n\n[! 报告过长已截断：{total}/{} 字符，关键结论可能在后段；如需细节请向该子 agent 追问]",
                    MAX_REPORT_CHARS
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    (notice, input)
}

/// Last path segment — notifications show `analyze-pi`, not `root/analyze-pi`.
fn short_name(agent_path: &str) -> &str {
    agent_path.rsplit('/').next().unwrap_or(agent_path)
}

#[cfg(test)]
#[path = "child_results_tests.rs"]
mod child_results_tests;
