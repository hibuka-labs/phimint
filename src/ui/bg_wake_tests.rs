//! Tests for the background-task auto-wake policy (`bg_wake.rs`).
//!
//! Covers the delivery gates (root turn in flight / children running / tasks
//! running), wake-worthiness (Done/TimedOut/Error wake, Cancelled doesn't),
//! the report-once semantics, the synthetic-input composition (embedded
//! output tail, consumed annotation, bounded budgets), and the #29 freshness
//! invariants (synthetic-turn hold, just-finished tasks join the first wake).

use phi_kernel_tools::background_shell::{BackgroundTaskRegistry, BackgroundTaskStatus};
use tokio_util::sync::CancellationToken;

use super::super::app::{AgentStatus, App, Phase, SubAgentState, SubAgentStatus};
use super::{MAX_TAIL_PER_TASK, MAX_TAIL_PER_WAKE, mock_bg_entry, status_desc};

/// Insert a bounded background job (`timeout_ms > 0`) straight into the panel
/// (bypasses the registry; `reconcile_background_tasks` owns the registry path).
fn insert_bg(app: &mut App, id: &str, command: &str, status: BackgroundTaskStatus) {
    app.background_tasks
        .insert(id.to_string(), mock_bg_entry(id, command, status, 120_000));
}

/// Insert a daemon-style entry (`timeout_ms == 0`): runs until it dies.
fn insert_daemon(app: &mut App, id: &str, command: &str, status: BackgroundTaskStatus) {
    app.background_tasks
        .insert(id.to_string(), mock_bg_entry(id, command, status, 0));
}

/// Insert a finished job whose output tail / consumption flag are already set.
fn insert_with_tail(
    app: &mut App,
    id: &str,
    command: &str,
    status: BackgroundTaskStatus,
    tail: &str,
    consumed: bool,
) {
    let mut entry = mock_bg_entry(id, command, status, 120_000);
    entry.output_tail = tail.to_string();
    entry.consumed = consumed;
    app.background_tasks.insert(id.to_string(), entry);
}

fn running_child(app: &mut App, path: &str) {
    app.sub_agents.insert(
        path.to_string(),
        SubAgentState {
            name: path.to_string(),
            status: SubAgentStatus::Running,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: None,
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
}

// ── Delivery gates ────────────────────────────────────────────────────────

#[test]
fn wake_holds_while_agent_is_running() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
    );
    app.running = true;
    assert!(app.take_bg_wake().is_none(), "must not inject mid-turn");
    // Same state, agent idle → fires.
    app.running = false;
    assert!(app.take_bg_wake().is_some());
}

#[test]
fn wake_holds_for_synthetic_turns_too() {
    // Issue #29: `running` used to be set only on keyboard submit, so a wake
    // composed mid-run of a *synthetic* turn (bg wake / child inject) queued
    // behind the in-flight run and arrived alone minutes later. The hold must
    // look at the status machine too, not just the bool.
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
    );
    app.status = AgentStatus::Running {
        phase: Phase::Thinking,
    };
    assert!(
        app.take_bg_wake().is_none(),
        "synthetic turn in flight must hold"
    );

    // Turn over → the wake is due.
    app.status = AgentStatus::Idle;
    assert!(app.take_bg_wake().is_some());
}

#[test]
fn wake_holds_while_sub_agents_are_running() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
    );
    running_child(&mut app, "root/auth");
    assert!(app.take_bg_wake().is_none(), "sub-agents still in flight");

    // Child finishes → the wake is now due.
    app.mark_sub_agent_finished("root/auth");
    assert!(app.take_bg_wake().is_some());
}

#[test]
fn wake_holds_while_any_bg_task_is_running() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "fast task",
        BackgroundTaskStatus::Done,
    );
    insert_bg(
        &mut app,
        "bg_bbbb2222",
        "slow task",
        BackgroundTaskStatus::Running,
    );
    assert!(
        app.take_bg_wake().is_none(),
        "batch semantics: wait for all"
    );

    // The slow task ends → one batched wake covering both.
    app.background_tasks.get_mut("bg_bbbb2222").unwrap().status = BackgroundTaskStatus::Done;
    let (_, input) = app.take_bg_wake().expect("all terminal → wake");
    assert!(input.contains("bg_aaaa1111"));
    assert!(input.contains("bg_bbbb2222"));
}

#[test]
fn daemon_running_does_not_hold_the_wake() {
    // Regression (session 20260922_6d262d0f): a daemon (`timeout_ms: 0`,
    // e.g. `mvn spring-boot:run`) never reaches a terminal state, so a hold
    // on "every task terminal" swallowed the report of a finished bounded
    // job forever.
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
    );
    insert_daemon(
        &mut app,
        "bg_daemon01",
        "mvn spring-boot:run",
        BackgroundTaskStatus::Running,
    );
    let (_, input) = app
        .take_bg_wake()
        .expect("daemon must not swallow the report");
    assert!(input.contains("bg_aaaa1111"));
    assert!(
        !input.contains("bg_daemon01"),
        "a still-running daemon is not an outcome"
    );
    assert!(
        input.contains("以下后台任务已结束"),
        "no 'all finished' claim: {input}"
    );
    assert!(!input.contains("全部结束"), "a daemon may still be running");
}

#[test]
fn daemon_death_wakes_on_its_own() {
    // A dead server is exactly the outcome the agent must investigate.
    let mut app = App::new();
    insert_daemon(
        &mut app,
        "bg_daemon01",
        "mvn spring-boot:run",
        BackgroundTaskStatus::Error("exited".into()),
    );
    let (_, input) = app.take_bg_wake().expect("dead daemon wakes");
    assert!(input.contains("bg_daemon01"));
}

#[test]
fn bounded_job_still_holds_a_daemon_death() {
    // Batch discipline is unchanged for bounded work: with a job in flight
    // the wake waits — even for a daemon that already died — one report.
    let mut app = App::new();
    insert_daemon(
        &mut app,
        "bg_daemon01",
        "mvn spring-boot:run",
        BackgroundTaskStatus::Done,
    );
    insert_bg(
        &mut app,
        "bg_bbbb2222",
        "slow job",
        BackgroundTaskStatus::Running,
    );
    assert!(app.take_bg_wake().is_none(), "bounded job still in flight");
}

#[test]
fn wake_needs_no_bg_tasks() {
    let mut app = App::new();
    assert!(app.take_bg_wake().is_none());
}

// ── Freshness (issue #29) ─────────────────────────────────────────────────

#[test]
fn just_finished_tasks_join_the_first_wake() {
    // The wake must decide on *fresh* state. The loop used to take the wake
    // before the reconcile tick, so a task that went terminal between two
    // ticks missed the first wake and arrived alone a full report-turn later.
    let registry = BackgroundTaskRegistry::new(4);
    let mut app = App::new();
    app.set_background_registry(registry.clone());

    let one = registry
        .register("echo one", None, CancellationToken::new(), None, 120_000)
        .unwrap();
    registry.update_status(&one, BackgroundTaskStatus::Done);
    let two = registry
        .register("echo two", None, CancellationToken::new(), None, 120_000)
        .unwrap();
    registry.update_status(&two, BackgroundTaskStatus::Done);

    // Panel map is empty — nothing has been reconciled into it yet.
    assert!(
        app.background_tasks.is_empty(),
        "precondition: map is stale"
    );

    let (_, input) = app
        .take_bg_wake()
        .expect("freshly finished tasks must join the first wake");
    assert!(input.contains(&one), "first task listed: {input}");
    assert!(
        input.contains(&two),
        "second task batched into the same wake: {input}"
    );
}

// ── Wake-worthiness ───────────────────────────────────────────────────────

#[test]
fn cancelled_tasks_never_wake() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "sleep 30",
        BackgroundTaskStatus::Cancelled,
    );
    assert!(
        app.take_bg_wake().is_none(),
        "user cancelled it deliberately"
    );
}

#[test]
fn cancelled_is_filtered_from_a_mixed_wake() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_cccc3333",
        "sleep 30",
        BackgroundTaskStatus::Cancelled,
    );
    insert_bg(
        &mut app,
        "bg_dddd4444",
        "cargo build",
        BackgroundTaskStatus::Done,
    );
    let (_, input) = app.take_bg_wake().expect("the done task still wakes");
    assert!(input.contains("bg_dddd4444"));
    assert!(
        !input.contains("bg_cccc3333"),
        "cancelled task must not be listed"
    );
}

#[test]
fn timed_out_and_error_are_wake_worthy() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_eeee5555",
        "long job",
        BackgroundTaskStatus::TimedOut,
    );
    insert_bg(
        &mut app,
        "bg_ffff6666",
        "broken job",
        BackgroundTaskStatus::Error("spawn failed".into()),
    );
    let (_, input) = app.take_bg_wake().expect("failures must reach the agent");
    assert!(input.contains("bg_eeee5555"));
    assert!(input.contains("bg_ffff6666"));
    assert!(input.contains("超时"));
    assert!(input.contains("spawn failed"));
}

// ── Report-once semantics ─────────────────────────────────────────────────

#[test]
fn wake_reports_each_task_exactly_once() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
    );
    let (notice, input) = app.take_bg_wake().expect("first wake fires");
    assert!(notice.contains('1'), "notice carries the count: {notice}");
    assert!(input.contains("bg_aaaa1111"));
    assert!(
        input.contains("task_output"),
        "agent is told how to fetch output: {input}"
    );
    assert!(
        input.contains("不要重新运行"),
        "agent is told not to re-run: {input}"
    );
    assert!(app.take_bg_wake().is_none(), "already reported");

    // A NEW task spawned later (during the report turn) wakes on its own.
    insert_bg(
        &mut app,
        "bg_bbbb2222",
        "next job",
        BackgroundTaskStatus::Done,
    );
    let (_, input) = app.take_bg_wake().expect("new task wakes");
    assert!(input.contains("bg_bbbb2222"));
    assert!(!input.contains("bg_aaaa1111"), "old task not re-listed");
}

// ── Composition details ───────────────────────────────────────────────────

#[test]
fn long_commands_are_capped() {
    let mut app = App::new();
    let long_cmd = format!("echo {}", "x".repeat(500));
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        &long_cmd,
        BackgroundTaskStatus::Done,
    );
    let (_, input) = app.take_bg_wake().unwrap();
    assert!(input.len() < long_cmd.len(), "listing must be capped");
    assert!(input.contains("…"), "truncation marker present");
}

#[test]
fn status_desc_covers_all_variants() {
    assert_eq!(status_desc(&BackgroundTaskStatus::Done), "已完成");
    assert_eq!(status_desc(&BackgroundTaskStatus::TimedOut), "超时");
    assert_eq!(status_desc(&BackgroundTaskStatus::Cancelled), "已取消");
    assert_eq!(status_desc(&BackgroundTaskStatus::Running), "运行中");
    assert_eq!(
        status_desc(&BackgroundTaskStatus::Error("boom".into())),
        "出错：boom"
    );
}

#[test]
fn wake_carries_each_tasks_output_tail() {
    // Issue #35: the wake is self-sufficient — the registry entry may already
    // be gone (consume-GC or the 30-min fallback), so the outcome rides in
    // the wake itself. Consumed tasks say so: the tail *is* the report source.
    let mut app = App::new();
    insert_with_tail(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
        "all tests passed\n",
        false,
    );
    insert_with_tail(
        &mut app,
        "bg_bbbb2222",
        "cargo build",
        BackgroundTaskStatus::Done,
        "fresh tail\n",
        true,
    );

    let (_, input) = app.take_bg_wake().expect("wake fires");
    assert!(
        input.contains("all tests passed"),
        "tail is embedded: {input}"
    );
    assert!(
        input.contains("fresh tail"),
        "consumed task's tail is embedded: {input}"
    );
    assert!(input.contains("输出 tail"), "tail section marker: {input}");
    assert!(
        input.contains("输出此前已被 task_output 取走"),
        "consumed task is annotated: {input}"
    );
    assert_eq!(
        input.matches("输出此前已被").count(),
        1,
        "only the consumed task is annotated: {input}"
    );
}

#[test]
fn wake_tail_budget_is_bounded_and_utf8_safe() {
    // AC3: the wake is bounded. Five fat multi-byte tails share an 8KB wake
    // budget (per-task share shrinks), a raw error string is capped, and no
    // truncation may split a code point (no U+FFFD in the composed turn).
    let mut app = App::new();
    let fat_tail = "中".repeat(4000); // 12KB of 3-byte code points
    insert_with_tail(
        &mut app,
        "bg_job0001",
        "job-1",
        BackgroundTaskStatus::Done,
        &fat_tail,
        false,
    );
    insert_with_tail(
        &mut app,
        "bg_job0002",
        "job-2",
        BackgroundTaskStatus::Done,
        &fat_tail,
        false,
    );
    insert_with_tail(
        &mut app,
        "bg_job0003",
        "job-3",
        BackgroundTaskStatus::Done,
        &fat_tail,
        false,
    );
    insert_with_tail(
        &mut app,
        "bg_job0004",
        "job-4",
        BackgroundTaskStatus::Done,
        &fat_tail,
        false,
    );
    insert_with_tail(
        &mut app,
        "bg_job0005",
        "job-5",
        BackgroundTaskStatus::Error("E".repeat(500)),
        &fat_tail,
        false,
    );

    let (_, input) = app.take_bg_wake().expect("wake fires");
    assert!(
        !input.contains('\u{FFFD}'),
        "truncation must never split a code point"
    );

    // Per-task tail share = min(2KB, 8KB / 5) bytes → each tail keeps only
    // whole '中' chars that fit that share.
    let per_task = MAX_TAIL_PER_TASK.min(MAX_TAIL_PER_WAKE / 5);
    let expected_chars = 5 * (per_task / 3);
    assert_eq!(
        input.matches('中').count(),
        expected_chars,
        "tails share the wake budget"
    );

    // The raw error string is capped at MAX_ERROR_DESC chars …
    assert_eq!(
        input.matches('E').count(),
        200,
        "error description is capped"
    );
    // … and the whole synthetic turn stays within budget + fixed overhead.
    assert!(
        input.len() < MAX_TAIL_PER_WAKE + 2 * 1024,
        "wake must stay bounded: {} bytes",
        input.len()
    );
}

// ── Status-machine integration ────────────────────────────────────────────

#[test]
fn waiting_count_follows_bg_tasks_and_drops_to_idle() {
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "one",
        BackgroundTaskStatus::Running,
    );
    insert_bg(
        &mut app,
        "bg_bbbb2222",
        "two",
        BackgroundTaskStatus::Running,
    );
    app.status = AgentStatus::Waiting { running: 0, bg: 2 };

    // Refresh path: finishing a sub-agent entry refreshes both counts; use a
    // no-op id so only the refresh effect is observed.
    app.background_tasks.get_mut("bg_aaaa1111").unwrap().status = BackgroundTaskStatus::Done;
    app.mark_sub_agent_finished("__noop_refresh__");
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 0, bg: 1 }
    ));

    app.background_tasks.get_mut("bg_bbbb2222").unwrap().status = BackgroundTaskStatus::Done;
    app.mark_sub_agent_finished("__noop_refresh__");
    assert_eq!(app.status, AgentStatus::Idle);
}
