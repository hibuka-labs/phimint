//! Tests for the background-task auto-wake policy (`bg_wake.rs`).
//!
//! Covers the delivery gates (agent running / children running / tasks
//! running), wake-worthiness (Done/TimedOut/Error wake, Cancelled doesn't),
//! the report-once semantics, and the synthetic-input composition.

use phi_kernel_tools::background_shell::BackgroundTaskStatus;

use super::super::app::{AgentStatus, App, SubAgentState, SubAgentStatus};
use super::{mock_bg_entry, status_desc};

/// Insert a background entry straight into the panel (bypasses the registry;
/// `reconcile_background_tasks` owns the registry path).
fn insert_bg(app: &mut App, id: &str, command: &str, status: BackgroundTaskStatus) {
    app.background_tasks.insert(id.to_string(), mock_bg_entry(id, command, status));
}

fn running_child(app: &mut App, path: &str) {
    app.sub_agents.insert(path.to_string(), SubAgentState {
        name: path.to_string(),
        status: SubAgentStatus::Running,
        files: Vec::new(),
        started_at: std::time::Instant::now(),
        completed_at: None,
        last_tool_at: std::time::Instant::now(),
        events: Vec::new(),
    });
}

// ── Delivery gates ────────────────────────────────────────────────────────

#[test]
fn wake_holds_while_agent_is_running() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_aaaa1111", "cargo test", BackgroundTaskStatus::Done);
    assert!(app.take_bg_wake(true).is_none(), "must not inject mid-turn");
    // Same state, agent idle → fires.
    assert!(app.take_bg_wake(false).is_some());
}

#[test]
fn wake_holds_while_sub_agents_are_running() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_aaaa1111", "cargo test", BackgroundTaskStatus::Done);
    running_child(&mut app, "root/auth");
    assert!(app.take_bg_wake(false).is_none(), "sub-agents still in flight");

    // Child finishes → the wake is now due.
    app.mark_sub_agent_finished("root/auth");
    assert!(app.take_bg_wake(false).is_some());
}

#[test]
fn wake_holds_while_any_bg_task_is_running() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_aaaa1111", "fast task", BackgroundTaskStatus::Done);
    insert_bg(&mut app, "bg_bbbb2222", "slow task", BackgroundTaskStatus::Running);
    assert!(app.take_bg_wake(false).is_none(), "batch semantics: wait for all");

    // The slow task ends → one batched wake covering both.
    app.background_tasks.get_mut("bg_bbbb2222").unwrap().status = BackgroundTaskStatus::Done;
    let (_, input) = app.take_bg_wake(false).expect("all terminal → wake");
    assert!(input.contains("bg_aaaa1111"));
    assert!(input.contains("bg_bbbb2222"));
}

#[test]
fn wake_needs_no_bg_tasks() {
    let mut app = App::new();
    assert!(app.take_bg_wake(false).is_none());
}

// ── Wake-worthiness ───────────────────────────────────────────────────────

#[test]
fn cancelled_tasks_never_wake() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_aaaa1111", "sleep 30", BackgroundTaskStatus::Cancelled);
    assert!(app.take_bg_wake(false).is_none(), "user cancelled it deliberately");
}

#[test]
fn cancelled_is_filtered_from_a_mixed_wake() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_cccc3333", "sleep 30", BackgroundTaskStatus::Cancelled);
    insert_bg(&mut app, "bg_dddd4444", "cargo build", BackgroundTaskStatus::Done);
    let (_, input) = app.take_bg_wake(false).expect("the done task still wakes");
    assert!(input.contains("bg_dddd4444"));
    assert!(!input.contains("bg_cccc3333"), "cancelled task must not be listed");
}

#[test]
fn timed_out_and_error_are_wake_worthy() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_eeee5555", "long job", BackgroundTaskStatus::TimedOut);
    insert_bg(&mut app, "bg_ffff6666", "broken job", BackgroundTaskStatus::Error("spawn failed".into()));
    let (_, input) = app.take_bg_wake(false).expect("failures must reach the agent");
    assert!(input.contains("bg_eeee5555"));
    assert!(input.contains("bg_ffff6666"));
    assert!(input.contains("超时"));
    assert!(input.contains("spawn failed"));
}

// ── Report-once semantics ─────────────────────────────────────────────────

#[test]
fn wake_reports_each_task_exactly_once() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_aaaa1111", "cargo test", BackgroundTaskStatus::Done);
    let (notice, input) = app.take_bg_wake(false).expect("first wake fires");
    assert!(notice.contains('1'), "notice carries the count: {notice}");
    assert!(input.contains("bg_aaaa1111"));
    assert!(input.contains("task_output"), "agent is told how to fetch output: {input}");
    assert!(input.contains("不要重新运行"), "agent is told not to re-run: {input}");
    assert!(app.take_bg_wake(false).is_none(), "already reported");

    // A NEW task spawned later (during the report turn) wakes on its own.
    insert_bg(&mut app, "bg_bbbb2222", "next job", BackgroundTaskStatus::Done);
    let (_, input) = app.take_bg_wake(false).expect("new task wakes");
    assert!(input.contains("bg_bbbb2222"));
    assert!(!input.contains("bg_aaaa1111"), "old task not re-listed");
}

// ── Composition details ───────────────────────────────────────────────────

#[test]
fn long_commands_are_capped() {
    let mut app = App::new();
    let long_cmd = format!("echo {}", "x".repeat(500));
    insert_bg(&mut app, "bg_aaaa1111", &long_cmd, BackgroundTaskStatus::Done);
    let (_, input) = app.take_bg_wake(false).unwrap();
    assert!(input.len() < long_cmd.len(), "listing must be capped");
    assert!(input.contains("…"), "truncation marker present");
}

#[test]
fn status_desc_covers_all_variants() {
    assert_eq!(status_desc(&BackgroundTaskStatus::Done), "已完成");
    assert_eq!(status_desc(&BackgroundTaskStatus::TimedOut), "超时");
    assert_eq!(status_desc(&BackgroundTaskStatus::Cancelled), "已取消");
    assert_eq!(status_desc(&BackgroundTaskStatus::Running), "运行中");
    assert_eq!(status_desc(&BackgroundTaskStatus::Error("boom".into())), "出错：boom");
}

// ── Status-machine integration ────────────────────────────────────────────

#[test]
fn waiting_count_follows_bg_tasks_and_drops_to_idle() {
    let mut app = App::new();
    insert_bg(&mut app, "bg_aaaa1111", "one", BackgroundTaskStatus::Running);
    insert_bg(&mut app, "bg_bbbb2222", "two", BackgroundTaskStatus::Running);
    app.status = AgentStatus::Waiting { running: 0, bg: 2 };

    // Refresh path: finishing a sub-agent entry refreshes both counts; use a
    // no-op id so only the refresh effect is observed.
    app.background_tasks.get_mut("bg_aaaa1111").unwrap().status = BackgroundTaskStatus::Done;
    app.mark_sub_agent_finished("__noop_refresh__");
    assert!(matches!(app.status, AgentStatus::Waiting { running: 0, bg: 1 }));

    app.background_tasks.get_mut("bg_bbbb2222").unwrap().status = BackgroundTaskStatus::Done;
    app.mark_sub_agent_finished("__noop_refresh__");
    assert_eq!(app.status, AgentStatus::Idle);
}
