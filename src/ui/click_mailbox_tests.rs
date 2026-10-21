//! Tests for the click mailbox (M2: mailbox → boundary → user answer).
//!
//! Covers the 1B acceptance rows: synthesis formats (row 6), clicks queued
//! during chained tool calls (row 7), debt-not-lost + report-once (row 8),
//! the coalesce boundary, composer merge semantics (row 10), multi-select
//! toggle/confirm/skip (row 11), and slash hygiene (row 12) — plus one
//! end-to-end wire test over the real canvas server.

use std::time::{Duration, Instant};

use phi_canvas::{CanvasEvent, EventAction, ScreenEnvelope};

use super::{CLICK_COALESCE, ClickMailbox};

fn envelope() -> ScreenEnvelope {
    ScreenEnvelope::parse(&serde_json::json!({
        "title": "大屏顶部导航，哪个布局更专业？",
        "options": [
            { "id": "a", "label": "侧边栏导航" },
            { "id": "b", "label": "顶部横向导航" },
            { "id": "c", "label": "混合式" }
        ],
        "multiselect": false
    }))
    .unwrap()
}

fn multi_envelope() -> ScreenEnvelope {
    let mut env = envelope();
    env.multiselect = true;
    env
}

fn display_envelope() -> ScreenEnvelope {
    ScreenEnvelope::parse(&serde_json::json!({
        "title": "配色方向",
        "options": []
    }))
    .unwrap()
}

fn event(screen: u64, action: EventAction) -> CanvasEvent {
    CanvasEvent {
        seq: 1,
        screen,
        at_ms: 0,
        action,
    }
}

fn select(screen: u64, id: &str) -> CanvasEvent {
    event(
        screen,
        EventAction::Select {
            option_id: id.to_string(),
        },
    )
}

// --- row 6: synthesis formats -----------------------------------------------

#[test]
fn single_select_synthesizes_m2d_format() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );
}

#[test]
fn skip_synthesizes_locked_text() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &event(1, EventAction::Skip), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 跳过 · 不做选择".to_string())
    );
}

#[test]
fn correction_prefix_only_after_submit() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));
    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );

    // Same screen again: "提交后再点同一屏" → correction, new message, no rollback.
    let t1 = t0 + Duration::from_secs(5);
    mailbox.push_event(t1, &select(1, "c"), Some(&env));
    assert_eq!(
        mailbox.take_submit(t1 + CLICK_COALESCE, false, false),
        Some("(画布选择) 更正：已选 c · 混合式".to_string())
    );
}

#[test]
fn skip_after_submit_is_also_a_correction() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    let _ = mailbox.take_submit(t0 + CLICK_COALESCE, false, false);

    mailbox.push_event(t0, &event(1, EventAction::Skip), Some(&env));
    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 更正：跳过 · 不做选择".to_string())
    );
}

#[test]
fn display_only_screen_produces_no_message() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = display_envelope();
    for action in [
        EventAction::Select {
            option_id: "a".into(),
        },
        EventAction::Confirm,
        EventAction::Skip,
    ] {
        mailbox.push_event(t0, &event(1, action), Some(&env));
    }
    assert!(!mailbox.has_pending());
    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_secs(10), false, false),
        None
    );
}

#[test]
fn multiple_due_screens_compose_one_message() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0, &select(2, "b"), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 a · 侧边栏导航\n(画布选择) 已选 b · 顶部横向导航".to_string())
    );
}

#[test]
fn the_same_pick_twice_is_one_answer() {
    // The model re-pushes the same question mid-turn (v2) and cannot see the
    // click on the earlier screen — the user answers both and both queue. One
    // question, one answer: identical lines collapse into the newest.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));
    mailbox.push_event(t0, &select(2, "b"), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );
}

#[test]
fn different_picks_on_two_versions_keep_the_trail() {
    // The user changed their mind between the two versions: both answers are
    // real, in order — the last line is the live one.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));
    mailbox.push_event(t0, &select(2, "c"), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 b · 顶部横向导航\n(画布选择) 已选 c · 混合式".to_string())
    );
}

// --- rows 6 + 12: label hygiene --------------------------------------------

#[test]
fn label_sanitize_strips_newlines_and_leading_slash() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    // Built directly: the envelope validator rejects control chars, and this
    // sanitize is the defense-in-depth layer behind it (M2c).
    let env = ScreenEnvelope {
        title: "x".to_string(),
        subtitle: None,
        options: vec![
            phi_canvas::OptionItem {
                id: "a".to_string(),
                label: "/quit\nevil".to_string(),
                note: None,
            },
            phi_canvas::OptionItem {
                id: "b".to_string(),
                label: "ok".to_string(),
                note: None,
            },
        ],
        body_html: String::new(),
        screen: None,
        multiselect: false,
        reference: None,
        escape: None,
        interstitial: false,
    };
    mailbox.push_event(t0, &select(1, "a"), Some(&env));

    let message = mailbox
        .take_submit(t0 + CLICK_COALESCE, false, false)
        .expect("pick must synthesize");
    assert_eq!(message, "(画布选择) 已选 a · ／quit evil");
    // Row 12: synthesized text can never trip slash-command interception.
    assert!(!message.starts_with('/'));
    assert!(!message.contains('\n'));
}

#[test]
fn every_synthesized_form_avoids_slash_interception() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = ScreenEnvelope::parse(&serde_json::json!({
        "title": "x",
        "options": [{ "id": "a", "label": "/model" }]
    }))
    .unwrap();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    let message = mailbox
        .take_submit(t0 + CLICK_COALESCE, false, false)
        .unwrap();
    assert!(!message.starts_with('/'));

    mailbox.push_event(t0, &event(1, EventAction::Skip), Some(&env));
    let skip = mailbox.take_submit(t0, false, false).unwrap();
    assert!(!skip.starts_with('/'));
}

// --- row 11: multi-select toggle / confirm / skip ---------------------------

#[test]
fn multiselect_confirm_click_order_ids_and_labels() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    mailbox.push_event(t0, &select(1, "c"), Some(&env));
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));

    // Click order in the ids, same order in the labels (M2d).
    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 已选 c、a · 混合式, 侧边栏导航".to_string())
    );
}

#[test]
fn multiselect_reclick_toggles_off() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0, &select(1, "c"), Some(&env));
    mailbox.push_event(t0, &select(1, "a"), Some(&env)); // toggle-off
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 已选 c · 混合式".to_string())
    );
}

#[test]
fn multiselect_confirm_without_selection_is_skip() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 跳过 · 不做选择".to_string())
    );
}

#[test]
fn multiselect_confirm_freezes_set_later_toggles_need_new_confirm() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));
    // Toggles after the confirm are correction material, not part of the
    // frozen answer — and without a new confirm they stay queued.
    mailbox.push_event(t0, &select(1, "b"), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 已选 a · 侧边栏导航".to_string())
    );
    assert!(mailbox.has_pending(), "unconfirmed toggle must stay queued");

    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));
    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 更正：已选 a、b · 侧边栏导航, 顶部横向导航".to_string())
    );
}

#[test]
fn skip_clears_multiselect_toggles() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0, &event(1, EventAction::Skip), Some(&env));

    // Skip answers "nothing" and empties the toggle set.
    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 跳过 · 不做选择".to_string())
    );

    // The cleared set means a later empty confirm is another "nothing" (a
    // correction line) — not the toggled option resurrected.
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));
    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 更正：跳过 · 不做选择".to_string())
    );
}

#[test]
fn multiselect_confirm_has_no_coalesce_window() {
    // M2b: the 1s window is single-select only; Confirm means "I'm done".
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0, false, false),
        Some("(画布选择) 已选 a · 侧边栏导航".to_string())
    );
}

// --- rows 6 + 9: coalesce window -------------------------------------------

#[test]
fn single_coalesce_reports_only_the_last_click() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0 + Duration::from_millis(100), &select(1, "b"), Some(&env));

    // One message, last pick only — after the (debounced) window closes.
    assert_eq!(
        mailbox.take_submit(
            t0 + Duration::from_millis(100) + CLICK_COALESCE,
            false,
            false
        ),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );
}

#[test]
fn coalesce_window_debounces_on_newer_click() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0 + Duration::from_millis(500), &select(1, "c"), Some(&env));

    // Anchored on the newest click: still inside the window at first-click+800ms.
    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        None,
        "re-click extends the window (debounce)"
    );
    assert_eq!(
        mailbox.take_submit(
            t0 + Duration::from_millis(500) + CLICK_COALESCE,
            false,
            false
        ),
        Some("(画布选择) 已选 c · 混合式".to_string())
    );
}

// --- rows 7 + 8: mailbox while the agent is busy ---------------------------

#[test]
fn clicks_during_busy_are_held_then_drained_at_turn_end() {
    // Row 7 (M2 core): push then chain tools — clicks must queue, not vanish,
    // and not inject into the active turn.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));

    // Chained tool calls: the turn keeps running long past the window.
    for offset in [100, 500, 1_000, 5_000] {
        assert_eq!(
            mailbox.take_submit(t0 + Duration::from_millis(offset), true, false),
            None,
            "no injection into the active turn"
        );
        assert!(mailbox.has_pending(), "debt must survive the turn");
    }

    // Turn end is the boundary — drain is immediate, no quiet window.
    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_millis(5_100), false, false),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );
}

#[test]
fn queued_screens_report_frozen_answers_not_live_toggles() {
    // The busy-click ack face (`已选待送`) must say "picked, waiting for the
    // boundary" — and must *not* fire while the user is still toggling a
    // multi-select (that interaction is not an answer yet).
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    let multi = multi_envelope();

    mailbox.push_event(t0, &select(1, "b"), Some(&env));
    mailbox.push_event(t0, &select(2, "a"), Some(&multi));
    assert_eq!(
        mailbox.queued_screens(),
        vec![1],
        "single-select pick is frozen; a lone multi-toggle is not"
    );
    assert_eq!(
        mailbox.pending_screens(),
        vec![1, 2],
        "both still owe something"
    );

    // Confirm freezes the multi-select set → both screens are queued now.
    mailbox.push_event(t0, &event(2, EventAction::Confirm), Some(&multi));
    assert_eq!(mailbox.queued_screens(), vec![1, 2]);

    // Turn end drains → nothing left queued.
    assert!(
        mailbox
            .take_submit(t0 + Duration::from_millis(100), true, false)
            .is_none(),
        "busy still holds the debt"
    );
    assert_eq!(mailbox.queued_screens(), vec![1, 2], "held, still queued");
    let _ = mailbox.take_submit(t0 + Duration::from_secs(1), false, false);
    assert!(
        mailbox.queued_screens().is_empty(),
        "delivered → ack clears"
    );
}

#[test]
fn multiselect_clicks_during_busy_accumulate_without_loss() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    for id in ["a", "b", "c"] {
        mailbox.push_event(t0, &select(1, id), Some(&env));
    }
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));
    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_secs(30), true, false),
        None
    );

    // Everything survives to the boundary, in click order.
    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_secs(31), false, false),
        Some("(画布选择) 已选 a、b、c · 侧边栏导航, 顶部横向导航, 混合式".to_string())
    );
}

#[test]
fn debt_is_consumed_exactly_once() {
    // Row 8: debt-not-lost + report-once accounting (the `bg_wake` borrow).
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    let multi = multi_envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));
    mailbox.push_event(t0, &select(2, "b"), Some(&multi));
    mailbox.push_event(t0, &event(2, EventAction::Confirm), Some(&multi));
    mailbox.push_event(t0, &event(3, EventAction::Skip), Some(&env));

    let message = mailbox
        .take_submit(t0 + Duration::from_secs(60), false, false)
        .expect("all four clicks must drain together");
    assert_eq!(
        message,
        "(画布选择) 已选 a · 侧边栏导航\n\
         (画布选择) 已选 b · 顶部横向导航\n\
         (画布选择) 跳过 · 不做选择"
    );
    assert!(!mailbox.has_pending());
    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_secs(61), false, false),
        None
    );
}

// --- row 10: composer merge -------------------------------------------------

#[test]
fn enter_merges_typed_text_first_blank_line_then_pick() {
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));

    let typed = "我喜欢第二个，但是导航要更紧凑";
    assert_eq!(
        mailbox.take_for_enter(typed),
        Some(format!("{typed}\n\n(画布选择) 已选 b · 顶部横向导航"))
    );
}

#[test]
fn composer_holds_auto_submit_so_the_merge_can_happen() {
    // M2e: drain never touches the composer buffer — while typed text waits
    // for Enter, the pick stays queued instead of auto-submitting alone.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));

    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_secs(5), false, true),
        None
    );
    assert!(mailbox.has_pending(), "pick waits for the Enter boundary");

    // Composer cleared without submitting: the idle boundary fires normally.
    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_secs(6), false, false),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );
}

#[test]
fn enter_flushes_a_pick_still_inside_the_coalesce_window() {
    // "以 Enter 为界": everything clicked before Enter joins its message.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "c"), Some(&env));

    assert_eq!(
        mailbox.take_for_enter("就这个"),
        Some("就这个\n\n(画布选择) 已选 c · 混合式".to_string())
    );
}

#[test]
fn enter_first_later_click_is_a_new_message() {
    // M2e rule 4: Enter is the boundary — a late pick is its own user message,
    // never a rollback of what was sent.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));
    assert_eq!(
        mailbox.take_for_enter("先按这个做"),
        Some("先按这个做\n\n(画布选择) 已选 b · 顶部横向导航".to_string())
    );

    let t1 = t0 + Duration::from_millis(200); // "晚到" — any time after Enter
    mailbox.push_event(t1, &select(1, "c"), Some(&env));
    assert_eq!(
        mailbox.take_submit(t1 + CLICK_COALESCE, false, false),
        Some("(画布选择) 更正：已选 c · 混合式".to_string())
    );
}

#[test]
fn slash_submit_skips_the_merge_and_keeps_the_debt() {
    // M2c: a `/`-leading Enter is a slash action — merging would mask the
    // command behind pick text. Picks stay queued for the next boundary.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &select(1, "b"), Some(&env));

    assert_eq!(mailbox.take_for_enter("/resume"), None);
    assert!(mailbox.has_pending(), "picks survive the slash submit");
    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );
}

#[test]
fn enter_does_not_commit_unconfirmed_multiselect_toggles() {
    // M2b: multi-select commits only on Confirm; Enter neither merges nor
    // consumes the toggle set — the user can still confirm (or skip) later.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = multi_envelope();
    mailbox.push_event(t0, &select(1, "a"), Some(&env));

    assert_eq!(
        mailbox.take_for_enter("先看下配色"),
        None,
        "unconfirmed toggles are not a committed answer"
    );
    // The caller submits the typed text alone; toggles stay queued for Confirm.
    assert!(mailbox.has_pending(), "toggles stay queued for Confirm");
}

// --- robustness ------------------------------------------------------------

#[test]
fn unknown_screen_falls_back_to_id_label_and_single_semantics() {
    // A click is a user answer: bookkeeping gaps must not drop it.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    mailbox.push_event(t0, &select(99, "x7"), None);

    assert_eq!(
        mailbox.take_submit(t0 + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 x7 · x7".to_string())
    );
}

#[test]
fn confirm_on_single_select_screen_is_ignored() {
    // Server-side Confirm on a single-select screen is invalid (400); the
    // mailbox must not mis-synthesize one if it ever sees the stray event.
    let mut mailbox = ClickMailbox::new();
    let t0 = Instant::now();
    let env = envelope();
    mailbox.push_event(t0, &event(1, EventAction::Confirm), Some(&env));

    assert!(!mailbox.has_pending());
    assert_eq!(
        mailbox.take_submit(t0 + Duration::from_secs(5), false, false),
        None
    );
}

// --- end-to-end over the real canvas wire ----------------------------------

#[tokio::test]
async fn canvas_wire_click_becomes_user_answer() {
    // The prototype thesis: a browser click (real POST → broadcast) becomes a
    // clean user answer at the boundary. Raw HTTP/1.1 bytes, like phi-canvas's
    // security matrix, so the real hyper stack is on the path.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let tmp = tempfile::tempdir().unwrap();
    let config = phi_canvas::CanvasConfig::new(tmp.path().join("canvas"), "sess-m2");
    let canvas = phi_canvas::Canvas::open(config).await.unwrap();
    let env = envelope();
    let receipt = canvas.push(env.clone()).await.unwrap();
    let mut rx = canvas.subscribe();

    let port = canvas.port();
    let body = format!(
        r#"{{"type":"select","screen":{},"option_id":"b"}}"#,
        receipt.screen
    );
    let request = format!(
        "POST /events HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Origin: http://127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         X-Canvas-Token: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        canvas.session_token(),
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to canvas");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 204"),
        "accepted click: {response}"
    );

    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("event must reach the subscriber")
        .expect("channel open");

    // Adapter path (what run.rs will do): look up the envelope for labels,
    // queue the event, drain at the boundary.
    let mut mailbox = ClickMailbox::new();
    let now = Instant::now();
    let screen = canvas.screen_envelope(event.screen);
    mailbox.push_event(now, &event, screen.as_ref());
    assert_eq!(
        mailbox.take_submit(now + CLICK_COALESCE, false, false),
        Some("(画布选择) 已选 b · 顶部横向导航".to_string())
    );

    canvas.shutdown().await;
}
