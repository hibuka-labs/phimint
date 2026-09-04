//! Child-result delivery policy for the TUI loop (fan-in redesign).
//!
//! Session 20260903_0cf95e79: the watcher is now the fan-in coordinator and
//! emits two kinds of events, so the old "hold every single result and
//! flush at turn end" policy split in two:
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
//! (session 20260903_2438d139, P3). [`ChildResultRouter`] is pure state plus
//! decisions; the caller executes the side effects (`set_notice`,
//! `push_system`, `Cmd::Run`), so the timing policy is unit-testable without
//! a terminal.

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

/// Owns batch reports held during a turn and decides their delivery timing.
pub struct ChildResultRouter {
    pending_reports: Vec<ChildReport>,
}

impl ChildResultRouter {
    pub fn new() -> Self {
        Self {
            pending_reports: Vec::new(),
        }
    }

    /// Route one freshly delivered watcher event.
    pub fn on_event(&mut self, agent_running: bool, event: ChildResultEvent) -> ChildResultRoute {
        match event {
            ChildResultEvent::Progress {
                agent_path,
                status,
                summary,
            } => ChildResultRoute::Notice {
                notice: progress_notice(&agent_path, &status, summary.as_deref()),
            },
            ChildResultEvent::Batch { reports } => {
                if agent_running {
                    self.pending_reports.extend(reports);
                    ChildResultRoute::Hold {
                        notice: format!(
                            "所有子 agent 已返回（{} 个），结果将在本轮结束后注入",
                            self.pending_reports.len()
                        ),
                    }
                } else {
                    let (notice, input) = compose_inject(&reports);
                    ChildResultRoute::Inject { notice, input }
                }
            }
        }
    }

    /// Drain reports held during the turn that just ended, as one batched
    /// synthetic run. Returns `None` while nothing is pending.
    pub fn flush_when_idle(&mut self) -> Option<ChildResultRoute> {
        if self.pending_reports.is_empty() {
            return None;
        }
        let reports = std::mem::take(&mut self.pending_reports);
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

/// Per-report injection cap, in characters. The whole batch must stay well
/// under the session's `max_message_tokens` safety valve — session
/// 20260904_c6559510: a 212,996-char / 53,276-token batch (extra rounds
/// created by the parent's own nudges) exceeded the valve and was silently
/// popped; the parent then synthesized from memory. ~4 chars/token for
/// mixed CJK/EN, so 24,000 chars ≈ 6k tokens/report; even 8 reports land
/// near 50k tokens, far under the 120k valve.
const MAX_REPORT_CHARS: usize = 24_000;

/// Notification + synthetic input for a set of batch reports.
fn compose_inject(reports: &[ChildReport]) -> (String, String) {
    let notice = format!("{} 个子 agent 结果已注入上下文", reports.len());
    let input = reports
        .iter()
        .map(|r| {
            if r.message.chars().count() <= MAX_REPORT_CHARS {
                r.message.clone()
            } else {
                let truncated: String = r.message.chars().take(MAX_REPORT_CHARS).collect();
                format!(
                    "{truncated}\n\n[⚠️ 报告过长已截断：{}/{} 字符，关键结论可能在后段；如需细节请向该子 agent 追问]",
                    r.message.chars().count(),
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
