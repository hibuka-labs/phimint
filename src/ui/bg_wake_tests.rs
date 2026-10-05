//! Tests for the background-task auto-wake policy (`bg_wake.rs`).
//!
//! Covers the delivery gates (root turn in flight / children running / tasks
//! running), wake-worthiness (Done/TimedOut/Error wake, Cancelled doesn't),
//! the report-once semantics, the synthetic-input composition (embedded
//! output tail, consumed annotation, bounded budgets), the #29 freshness
//! invariants (synthetic-turn hold, just-finished tasks join the first wake),
//! and the #30 aggregation quiet window (burst collapse, isolation latency,
//! deadline refresh).

use std::time::{Duration, Instant};

use phi_kernel_tools::background_shell::{BackgroundTaskRegistry, BackgroundTaskStatus};
use tokio_util::sync::CancellationToken;

use super::super::app::{AgentStatus, App, Phase, SubAgentState, SubAgentStatus};
use super::{BG_WAKE_QUIET, MAX_TAIL_PER_TASK, MAX_TAIL_PER_WAKE, mock_bg_entry, status_desc};

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

/// Insert a bounded job whose completion is stamped — the timestamp the quiet
/// window anchors on (production: the registry's `finished_at` at the terminal
/// transition). `mock_bg_entry` leaves it `None` (undated → no window).
fn insert_bg_finished_at(
    app: &mut App,
    id: &str,
    command: &str,
    status: BackgroundTaskStatus,
    finished_at: Instant,
) {
    let mut entry = mock_bg_entry(id, command, status, 120_000);
    entry.finished_at = Some(finished_at);
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
    assert!(
        app.take_bg_wake(Instant::now()).is_none(),
        "must not inject mid-turn"
    );
    // Same state, agent idle → fires.
    app.running = false;
    assert!(app.take_bg_wake(Instant::now()).is_some());
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
        app.take_bg_wake(Instant::now()).is_none(),
        "synthetic turn in flight must hold"
    );

    // Turn over → the wake is due.
    app.status = AgentStatus::Idle;
    assert!(app.take_bg_wake(Instant::now()).is_some());
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
    assert!(
        app.take_bg_wake(Instant::now()).is_none(),
        "sub-agents still in flight"
    );

    // Child finishes → the wake is now due.
    app.mark_sub_agent_finished("root/auth");
    assert!(app.take_bg_wake(Instant::now()).is_some());
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
        app.take_bg_wake(Instant::now()).is_none(),
        "batch semantics: wait for all"
    );

    // The slow task ends → one batched wake covering both.
    app.background_tasks.get_mut("bg_bbbb2222").unwrap().status = BackgroundTaskStatus::Done;
    let (_, input) = app
        .take_bg_wake(Instant::now())
        .expect("all terminal → wake");
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
        .take_bg_wake(Instant::now())
        .expect("daemon must not swallow the report");
    assert!(input.contains("bg_aaaa1111"));
    assert!(
        !input.contains("bg_daemon01"),
        "a still-running daemon is not an outcome"
    );
    assert!(
        input.contains("have finished"),
        "no 'all finished' claim: {input}"
    );
    assert!(
        !input.contains("all finished") && !input.contains("all tasks"),
        "a daemon may still be running"
    );
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
    let (_, input) = app.take_bg_wake(Instant::now()).expect("dead daemon wakes");
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
    assert!(
        app.take_bg_wake(Instant::now()).is_none(),
        "bounded job still in flight"
    );
}

#[test]
fn wake_needs_no_bg_tasks() {
    let mut app = App::new();
    assert!(app.take_bg_wake(Instant::now()).is_none());
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

    // The registry stamped both completions at their terminal transition, so
    // the first wake is due only after the quiet window; freshness (issue #29)
    // is what puts both of them in that one wake.
    let (_, input) = app
        .take_bg_wake(Instant::now() + BG_WAKE_QUIET)
        .expect("freshly finished tasks must join the first wake");
    assert!(input.contains(&one), "first task listed: {input}");
    assert!(
        input.contains(&two),
        "second task batched into the same wake: {input}"
    );
}

// ── Quiet window (issue #30) ──────────────────────────────────────────────

#[test]
fn burst_completions_collapse_into_one_wake() {
    // AC1: two completions within the window of each other produce exactly one
    // wake listing both — the second completion refreshes the deadline, so
    // the first never fires early and strands its sibling in a wake of its own.
    let t0 = Instant::now();
    let mut app = App::new();
    insert_bg_finished_at(
        &mut app,
        "bg_aaaa1111",
        "cargo publish a",
        BackgroundTaskStatus::Done,
        t0,
    );
    insert_bg_finished_at(
        &mut app,
        "bg_bbbb2222",
        "cargo publish b",
        BackgroundTaskStatus::Done,
        t0 + Duration::from_secs(10),
    );

    // At the first outcome's naive deadline the batch is still quieting down.
    assert!(app.take_bg_wake(t0 + BG_WAKE_QUIET).is_none());

    // Window stood → exactly one wake covering the burst …
    let (_, input) = app
        .take_bg_wake(t0 + Duration::from_secs(25))
        .expect("one wake for the burst");
    assert!(input.contains("bg_aaaa1111"));
    assert!(input.contains("bg_bbbb2222"));

    // … and the reported-at-most-once invariant still holds afterwards.
    assert!(app.take_bg_wake(t0 + Duration::from_secs(40)).is_none());
}

#[test]
fn isolated_completion_wakes_within_the_window() {
    // AC2: a lone completion must not wait past the window.
    let t0 = Instant::now();
    let mut app = App::new();
    insert_bg_finished_at(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
        t0,
    );

    assert!(
        app.take_bg_wake(t0 + Duration::from_millis(14_900))
            .is_none(),
        "still inside the window"
    );
    assert!(
        app.take_bg_wake(t0 + BG_WAKE_QUIET).is_some(),
        "due at the window boundary"
    );
}

#[test]
fn new_terminal_event_refreshes_the_deadline() {
    // A completion inside the window pushes the deadline out to its own finish
    // + window — one wake covers both instead of a wake each.
    let t0 = Instant::now();
    let mut app = App::new();
    insert_bg_finished_at(
        &mut app,
        "bg_aaaa1111",
        "job one",
        BackgroundTaskStatus::Done,
        t0,
    );
    assert!(
        app.take_bg_wake(t0 + Duration::from_secs(14)).is_none(),
        "first outcome still quieting"
    );

    // Sibling finishes at t0+14s → the deadline moves to t0+29s.
    insert_bg_finished_at(
        &mut app,
        "bg_bbbb2222",
        "job two",
        BackgroundTaskStatus::Done,
        t0 + Duration::from_secs(14),
    );
    assert!(
        app.take_bg_wake(t0 + Duration::from_secs(28)).is_none(),
        "deadline refreshed by the new terminal event"
    );
    let (_, input) = app
        .take_bg_wake(t0 + Duration::from_secs(29))
        .expect("one wake for both");
    assert!(input.contains("bg_aaaa1111"));
    assert!(input.contains("bg_bbbb2222"));
}

#[test]
fn hold_is_a_gate_not_a_fresh_window() {
    // The hold is a precondition, not a substitute (issue #30): it postpones
    // a wake but must not restart the quiet window behind it — a completion
    // held through a turn wakes as soon as the turn ends, not a window later.
    let t0 = Instant::now();
    let mut app = App::new();
    insert_bg_finished_at(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
        t0,
    );
    app.running = true;
    // The turn runs past the deadline …
    assert!(
        app.take_bg_wake(t0 + Duration::from_secs(60)).is_none(),
        "mid-turn injection is forbidden"
    );
    // … and ends: the wake is already due, no re-quieten.
    app.running = false;
    assert!(app.take_bg_wake(t0 + Duration::from_secs(61)).is_some());
}

#[test]
fn undated_outcomes_skip_the_quiet_window() {
    // `finished_at: None` exists only on test mocks (production stamps every
    // terminal transition) and is treated as already past the window: the
    // window may delay a wake, never lose or hold one indefinitely. The dated
    // path is covered by the tests above.
    let mut app = App::new();
    insert_bg(
        &mut app,
        "bg_aaaa1111",
        "cargo test",
        BackgroundTaskStatus::Done,
    );
    assert!(app.take_bg_wake(Instant::now()).is_some());
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
        app.take_bg_wake(Instant::now()).is_none(),
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
    let (_, input) = app
        .take_bg_wake(Instant::now())
        .expect("the done task still wakes");
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
    let (_, input) = app
        .take_bg_wake(Instant::now())
        .expect("failures must reach the agent");
    assert!(input.contains("bg_eeee5555"));
    assert!(input.contains("bg_ffff6666"));
    assert!(input.contains("timed out"));
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
    let (notice, input) = app.take_bg_wake(Instant::now()).expect("first wake fires");
    assert!(notice.contains('1'), "notice carries the count: {notice}");
    assert!(input.contains("bg_aaaa1111"));
    assert!(
        input.contains("task_output"),
        "agent is told how to fetch output: {input}"
    );
    assert!(
        input.contains("Do not re-run"),
        "agent is told not to re-run: {input}"
    );
    assert!(
        app.take_bg_wake(Instant::now()).is_none(),
        "already reported"
    );

    // A NEW task spawned later (during the report turn) wakes on its own.
    insert_bg(
        &mut app,
        "bg_bbbb2222",
        "next job",
        BackgroundTaskStatus::Done,
    );
    let (_, input) = app.take_bg_wake(Instant::now()).expect("new task wakes");
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
    let (_, input) = app.take_bg_wake(Instant::now()).unwrap();
    assert!(input.len() < long_cmd.len(), "listing must be capped");
    assert!(input.contains("…"), "truncation marker present");
}

#[test]
fn status_desc_covers_all_variants() {
    assert_eq!(status_desc(&BackgroundTaskStatus::Done), "done");
    assert_eq!(status_desc(&BackgroundTaskStatus::TimedOut), "timed out");
    assert_eq!(status_desc(&BackgroundTaskStatus::Cancelled), "cancelled");
    assert_eq!(status_desc(&BackgroundTaskStatus::Running), "running");
    assert_eq!(
        status_desc(&BackgroundTaskStatus::Error("boom".into())),
        "error: boom"
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

    let (_, input) = app.take_bg_wake(Instant::now()).expect("wake fires");
    assert!(
        input.contains("all tests passed"),
        "tail is embedded: {input}"
    );
    assert!(
        input.contains("fresh tail"),
        "consumed task's tail is embedded: {input}"
    );
    assert!(
        input.contains("output tail"),
        "tail section marker: {input}"
    );
    assert!(
        input.contains("output already consumed by task_output"),
        "consumed task is annotated: {input}"
    );
    assert_eq!(
        input.matches("output already consumed").count(),
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

    let (_, input) = app.take_bg_wake(Instant::now()).expect("wake fires");
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
