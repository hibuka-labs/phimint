//! Unit tests for [`ChildResultRouter`](super::ChildResultRouter) — the
//! delivery-timing policy for the fan-in event stream (session
//! 20260903_0cf95e79 redesign): Progress is display-only, Batch wakes the
//! parent exactly once per generation.

use super::{ChildResultRoute, ChildResultRouter};
use phi_agent::{ChildReport, ChildResultEvent};

fn report(path: &str, message: &str) -> ChildReport {
    ChildReport {
        agent_path: path.to_string(),
        status: "ok".to_string(),
        result: Some(message.to_string()),
        message: format!("[子 agent {path} 已完成]\n{message}"),
    }
}

fn progress(path: &str, status: &str, summary: Option<&str>) -> ChildResultEvent {
    ChildResultEvent::Progress {
        agent_path: path.to_string(),
        status: status.to_string(),
        summary: summary.map(|s| s.to_string()),
    }
}

fn batch(reports: Vec<ChildReport>) -> ChildResultEvent {
    ChildResultEvent::Batch { reports }
}

#[test]
fn progress_event_is_display_only() {
    let mut router = ChildResultRouter::new();

    match router.on_event(true, progress("root/analyze-pi", "ok", Some("完成了分析"))) {
        ChildResultRoute::Notice { notice } => {
            assert!(notice.contains("analyze-pi"), "{notice}");
            assert!(notice.contains("完成了分析"), "{notice}");
        }
        other => panic!("Progress must be display-only, got {other:?}"),
    }

    // Progress never injects, never accumulates.
    assert!(router.flush_when_idle().is_none());
}

#[test]
fn progress_without_summary_falls_back_to_status_word() {
    let mut router = ChildResultRouter::new();

    match router.on_event(false, progress("root/w", "ok", None)) {
        ChildResultRoute::Notice { notice } => {
            assert!(notice.contains("w"), "{notice}");
            assert!(notice.contains("已完成"), "{notice}");
        }
        other => panic!("expected Notice, got {other:?}"),
    }

    match router.on_event(false, progress("root/e", "error", None)) {
        ChildResultRoute::Notice { notice } => assert!(notice.contains("出错"), "{notice}"),
        other => panic!("expected Notice, got {other:?}"),
    }

    match router.on_event(false, progress("root/c", "closed", None)) {
        ChildResultRoute::Notice { notice } => assert!(notice.contains("已关闭"), "{notice}"),
        other => panic!("expected Notice, got {other:?}"),
    }
}

#[test]
fn notice_uses_short_name_without_root_prefix() {
    let mut router = ChildResultRouter::new();
    match router.on_event(false, progress("root/analyze-deepseek", "ok", None)) {
        ChildResultRoute::Notice { notice } => {
            assert!(notice.contains("analyze-deepseek"), "{notice}");
            assert!(!notice.contains("root/"), "{notice}");
        }
        other => panic!("expected Notice, got {other:?}"),
    }
}

#[test]
fn batch_when_idle_injects_immediately() {
    let mut router = ChildResultRouter::new();

    match router.on_event(
        false,
        batch(vec![
            report("root/a", "report a"),
            report("root/b", "report b"),
        ]),
    ) {
        ChildResultRoute::Inject { notice, input } => {
            assert!(notice.contains("2"), "{notice}");
            let (first, second) = input
                .split_once("\n\n")
                .expect("two reports joined by a blank line");
            assert!(first.contains("report a"));
            assert!(second.contains("report b"));
        }
        other => panic!("idle agent must inject now, got {other:?}"),
    }

    // Nothing was held.
    assert!(router.flush_when_idle().is_none());
}

#[test]
fn batch_when_running_holds_until_turn_ends() {
    let mut router = ChildResultRouter::new();

    match router.on_event(true, batch(vec![report("root/a", "report a")])) {
        ChildResultRoute::Hold { notice } => {
            assert!(notice.contains("本轮结束"), "{notice}");
        }
        other => panic!("running agent must hold the batch, got {other:?}"),
    }

    // The turn ends → held reports flush as one synthetic run.
    match router.flush_when_idle() {
        Some(ChildResultRoute::Inject { input, .. }) => {
            assert!(input.contains("report a"));
        }
        other => panic!("expected batched inject, got {other:?}"),
    }

    // Drained — a second flush is a no-op.
    assert!(router.flush_when_idle().is_none());
}

#[test]
fn multiple_batches_flush_together_in_order() {
    let mut router = ChildResultRouter::new();

    // Two generations complete while the parent is busy.
    assert!(matches!(
        router.on_event(true, batch(vec![report("root/a", "gen one")])),
        ChildResultRoute::Hold { .. }
    ));
    assert!(matches!(
        router.on_event(true, batch(vec![report("root/b", "gen two")])),
        ChildResultRoute::Hold { .. }
    ));

    match router.flush_when_idle() {
        Some(ChildResultRoute::Inject { notice, input }) => {
            assert!(notice.contains("2"), "{notice}");
            let (first, second) = input.split_once("\n\n").expect("joined in order");
            assert!(first.contains("gen one"));
            assert!(second.contains("gen two"));
        }
        other => panic!("expected batched inject, got {other:?}"),
    }
}

#[test]
fn progress_between_batches_does_not_break_flush() {
    let mut router = ChildResultRouter::new();

    assert!(matches!(
        router.on_event(true, batch(vec![report("root/a", "one")])),
        ChildResultRoute::Hold { .. }
    ));
    // A lone Progress lands while a batch is held — display only.
    assert!(matches!(
        router.on_event(true, progress("root/x", "ok", None)),
        ChildResultRoute::Notice { .. }
    ));
    match router.flush_when_idle() {
        Some(ChildResultRoute::Inject { input, .. }) => {
            assert!(input.contains("one"));
            assert!(!input.contains("root/x"), "Progress must not be injected");
        }
        other => panic!("expected batched inject, got {other:?}"),
    }
}

#[test]
fn after_flush_new_batch_is_injected_not_stale() {
    let mut router = ChildResultRouter::new();

    assert!(matches!(
        router.on_event(true, batch(vec![report("root/first", "one")])),
        ChildResultRoute::Hold { .. }
    ));
    assert!(router.flush_when_idle().is_some());

    // Next generation arrives while idle → inject immediately, never
    // accumulating into a stale batch.
    match router.on_event(false, batch(vec![report("root/second", "two")])) {
        ChildResultRoute::Inject { input, .. } => assert!(input.contains("two")),
        other => panic!("expected immediate inject, got {other:?}"),
    }
    assert!(router.flush_when_idle().is_none());
}

#[test]
fn flush_without_pending_is_a_noop() {
    let mut router = ChildResultRouter::new();
    assert!(router.flush_when_idle().is_none());
}

/// Session 20260904_c6559510: a 212,996-char batch exceeded the session's
/// `max_message_tokens` valve and was silently popped — the parent saw zero
/// reports and hallucinated a synthesis. Per-report truncation must keep
/// every injection far under the valve, and the truncation must be visible
/// to the model (so it knows to ask the child for details).
#[test]
fn oversized_report_is_truncated_with_visible_marker() {
    let long = "x".repeat(super::MAX_REPORT_CHARS + 5_000);
    let mut router = ChildResultRouter::new();

    match router.on_event(false, batch(vec![report("root/big", &long)])) {
        ChildResultRoute::Inject { input, .. } => {
            let total = input.chars().count();
            assert!(
                total <= super::MAX_REPORT_CHARS + 200,
                "injected report must be capped: {total} chars"
            );
            assert!(
                input.contains("报告过长已截断"),
                "truncation marker must be visible to the model"
            );
        }
        other => panic!("expected inject, got {other:?}"),
    }
}

#[test]
fn report_under_cap_is_passed_through_verbatim() {
    let mut router = ChildResultRouter::new();
    match router.on_event(false, batch(vec![report("root/a", "report a")])) {
        ChildResultRoute::Inject { input, .. } => {
            assert!(
                input.contains("report a") && !input.contains("报告过长已截断"),
                "short report must not be touched"
            );
        }
        other => panic!("expected inject, got {other:?}"),
    }
}
