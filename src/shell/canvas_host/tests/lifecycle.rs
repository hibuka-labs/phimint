use super::super::*;
use super::*;

// ── lifecycle bookkeeping ───────────────────────────────────────────────────

#[test]
fn status_shows_nothing_before_a_canvas_exists() {
    let host = CanvasHost::with_launch_fn(mpsc::unbounded_channel().0, |_| LaunchOutcome::Spawned);
    assert_eq!(host.status_line(&facts()), None);
}

#[test]
fn status_walks_the_lifecycle_faces() {
    let host = CanvasHost::with_launch_fn(mpsc::unbounded_channel().0, |_| LaunchOutcome::Spawned);
    host.note_canvas("http://127.0.0.1:4242", 4242);
    let text = |facts: &AnswerFacts| host.status_line(facts).unwrap().text;

    // Bound, no screen yet, consent open.
    assert_eq!(text(&facts()), "canvas ● localhost:4242 · 待同意");

    host.on_push(&receipt(1, "布局选择", 3, false));
    assert_eq!(host.poll_consent(), None); // nothing pending
    // Screen up, still waiting for the y.
    assert_eq!(text(&facts()), "canvas ● localhost:4242 · 待同意");
    host.request_user_launch("http://127.0.0.1:4242"); // consent settled
    assert_eq!(text(&facts()), "canvas ● localhost:4242 · 3 个选项待选");

    // Answer consumed → 空闲.
    let mut answered = facts();
    answered.answered.insert(1);
    assert_eq!(text(&answered), "canvas ● localhost:4242 · 空闲");

    host.note_closed();
    assert_eq!(text(&answered), "画布已关闭 · 可重开");
    host.note_failed();
    let line = host.status_line(&answered).unwrap();
    assert_eq!(line.text, "canvas ● 已断开 (ctrl+b 重开)");
    assert!(line.alert);
}

#[test]
fn push_recording_is_idempotent_per_seq() {
    let host = CanvasHost::with_launch_fn(mpsc::unbounded_channel().0, |_| LaunchOutcome::Spawned);
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));
    host.on_push(&receipt(1, "布局选择", 3, false)); // polled replay of the same push
    host.on_push(&receipt(2, "配色", 0, true));
    host.request_user_launch("http://127.0.0.1:4242"); // consent settled

    // The newest screen is current — a display-only one reads 展示中.
    assert_eq!(
        host.status_line(&facts()).unwrap().text,
        "canvas ● localhost:4242 · 展示中"
    );

    // The history is not doubled: the waiting summary's 待定 list would name
    // screen 1 twice if the replayed receipt had been recorded again.
    assert!(
        host.on_root_turn_end(&facts(), false).is_none(),
        "fresh screen"
    );
    let env = host
        .on_root_turn_end(&facts(), false)
        .expect("display-only screen is done being looked at");
    assert_eq!(
        env.body_html.matches("布局选择").count(),
        1,
        "one 待定 entry, not two"
    );
}

#[test]
fn turn_end_pushes_the_waiting_screen_once() {
    let host = CanvasHost::with_launch_fn(mpsc::unbounded_channel().0, |_| LaunchOutcome::Spawned);
    host.note_canvas("http://127.0.0.1:4242", 4242);

    // Turn 0: the agent pushes a 3-option screen and answers nothing yet.
    host.on_push(&receipt(1, "布局选择", 3, false));
    assert!(
        host.on_root_turn_end(&facts(), false).is_none(),
        "fresh screen, unanswered — leave it up"
    );

    // Turn 1: the pick was consumed during this turn's answer.
    host.on_answered(&["(画布选择) 已选 B · 顶部横向导航".to_string()]);
    let mut answered = facts();
    answered.answered.insert(1);
    let env = host
        .on_root_turn_end(&answered, false)
        .expect("done screen ⇒ waiting screen");
    assert_eq!(env.title, "继续在终端讨论…");
    assert!(env.body_html.contains("已定"), "{}", env.body_html);
    host.on_push(&receipt(2, "继续在终端讨论…", 0, true));
    host.on_waiting_push(2);

    // Turn 2: the waiting screen must not repaint itself.
    assert!(host.on_root_turn_end(&answered, false).is_none());
}

#[test]
fn turn_end_with_a_drained_click_leaves_the_pick_visible() {
    let host = CanvasHost::with_launch_fn(mpsc::unbounded_channel().0, |_| LaunchOutcome::Spawned);
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));
    host.on_root_turn_end(&facts(), false);

    // The click arrived mid-turn and the boundary drain just started the user
    // turn answering it — no waiting screen (row 15).
    let mut answered = facts();
    answered.answered.insert(1);
    assert!(host.on_root_turn_end(&answered, true).is_none());
}

#[test]
fn waiting_summary_carries_the_picks_and_the_open_questions() {
    let host = CanvasHost::with_launch_fn(mpsc::unbounded_channel().0, |_| LaunchOutcome::Spawned);
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));
    host.on_answered(&["(画布选择) 已选 B · 顶部横向导航".to_string()]);
    host.on_push(&receipt(2, "配色方向", 2, false)); // never answered
    host.on_push(&receipt(3, "参考 mockup", 0, true)); // display-only, current

    // Screen 1's answer was sent (mailbox facts say so), screen 2 never was.
    let mut answered = facts();
    answered.answered.insert(1);

    // Screen 3 landed during turn 0 — the first end-of-turn must not wipe it.
    assert!(
        host.on_root_turn_end(&answered, false).is_none(),
        "fresh mockup"
    );
    let env = host
        .on_root_turn_end(&answered, false)
        .expect("stale display-only ⇒ waiting");
    assert!(
        env.body_html.contains("已选 B · 顶部横向导航"),
        "{}",
        env.body_html
    );
    assert!(
        env.body_html.contains("配色方向 · 2 个选项"),
        "{}",
        env.body_html
    );
    assert!(
        !env.body_html.contains("布局选择"),
        "an answered screen belongs in 已定 only: {}",
        env.body_html
    );
    assert!(
        !env.body_html.contains("参考 mockup"),
        "no look-only noise: {}",
        env.body_html
    );
}

#[test]
fn close_reason_is_the_canvas_side_not_the_hosts() {
    // Sanity: the failure shape the host reacts to is phi-canvas's
    // `is_failed()` (bare shutdown) — the host only mirrors the phase it is
    // told about. This test pins the enum it is told.
    assert_ne!(CloseReason::Idle, CloseReason::SessionEnded);
    assert_ne!(CloseReason::UserClosed, CloseReason::SessionEnded);
}
