//! Tests for the canvas host policy (1C).
//!
//! Covers the automation rows this module owns: row 13 (status number = the
//! envelope's `options.len()`), row 14's policy half (the model can never pop
//! a browser — consent is raised once per URL and a deny never launches), and
//! row 15 (waiting-screen trigger). Plus the waiting summary's escaping and
//! the rebind reset. Browser spawning is injected, so no window ever opens.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use phi_canvas::tools::{BrowserLauncher, LaunchDecision};
use phi_canvas::{CloseReason, PushReceipt};
use tokio::sync::mpsc;

use super::*;
use crate::approval::ApprovalItem;

fn receipt(seq: u64, title: &str, options: usize, display_only: bool) -> PushReceipt {
    PushReceipt {
        screen: seq,
        url: "http://127.0.0.1:32123".to_string(),
        title: title.to_string(),
        option_count: options,
        display_only,
    }
}

fn face(options: usize, display_only: bool, waiting: bool) -> ScreenFace {
    ScreenFace {
        option_count: options,
        display_only,
        waiting,
    }
}

fn facts() -> AnswerFacts {
    AnswerFacts::default()
}

// ── status line (row 13) ────────────────────────────────────────────────────

#[test]
fn status_count_comes_from_the_envelope_options() {
    let line = status_line(&StatusInput {
        port: 32123,
        awaiting_consent: false,
        failed: false,
        closed: false,
        screen: ScreenStatus::Awaiting { options: 3 },
        queued: 0,
        queued_labels: Vec::new(),
    });
    assert_eq!(line.text, "canvas ● localhost:32123 · 3 个选项待选");
    assert!(!line.alert);
}

#[test]
fn status_states_match_the_spec_wording() {
    let base = StatusInput {
        port: 80,
        awaiting_consent: false,
        failed: false,
        closed: false,
        screen: ScreenStatus::Idle,
        queued: 0,
        queued_labels: Vec::new(),
    };
    let show = |input: &StatusInput| status_line(input).text;

    assert_eq!(show(&base), "canvas ● localhost:80 · 空闲");
    assert_eq!(
        show(&StatusInput {
            awaiting_consent: true,
            ..base.clone()
        }),
        "canvas ● localhost:80 · 待同意"
    );
    assert_eq!(
        show(&StatusInput {
            screen: ScreenStatus::DisplayOnly,
            ..base.clone()
        }),
        "canvas ● localhost:80 · 展示中"
    );
    assert_eq!(
        show(&StatusInput {
            screen: ScreenStatus::Waiting,
            ..base.clone()
        }),
        "canvas ● localhost:80 · 空闲"
    );
    assert_eq!(
        show(&StatusInput {
            closed: true,
            ..base.clone()
        }),
        "画布已关闭 · 可重开"
    );
    let failed = status_line(&StatusInput {
        failed: true,
        ..base
    });
    assert_eq!(failed.text, "canvas ● 已断开 (ctrl+b 重开)");
    assert!(failed.alert, "the crash face is the red one (row 16)");
}

#[test]
fn screen_status_tracks_the_mailbox_face() {
    // Nothing pushed yet.
    assert_eq!(screen_status(None, false, false), ScreenStatus::Idle);
    // Mockup: look-only.
    assert_eq!(
        screen_status(Some(&face(0, true, false)), false, false),
        ScreenStatus::DisplayOnly
    );
    // Awaiting a pick (nothing consumed).
    assert_eq!(
        screen_status(Some(&face(3, false, false)), false, false),
        ScreenStatus::Awaiting { options: 3 }
    );
    // Answer consumed → idle again (spec: 跳过 turns the bar to 空闲).
    assert_eq!(
        screen_status(Some(&face(3, false, false)), true, false),
        ScreenStatus::Idle
    );
    // Correction after submit: a new pick is owed.
    assert_eq!(
        screen_status(Some(&face(3, false, false)), true, true),
        ScreenStatus::Awaiting { options: 3 }
    );
    // The host's waiting screen is not a choice.
    assert_eq!(
        screen_status(Some(&face(0, true, true)), true, false),
        ScreenStatus::Waiting
    );
}

#[test]
fn queued_pick_wins_the_face_so_a_busy_click_stays_visible() {
    // Walkthrough 烦②: a click during a long turn looked like nothing
    // happened. The pick was only queued (M2 never injects mid-turn), and the
    // old face kept saying `3 个选项待选` — silent. Now the debt is spelled out
    // and *named* — the count alone only raises "which one?".
    let base = StatusInput {
        port: 80,
        awaiting_consent: false,
        failed: false,
        closed: false,
        screen: ScreenStatus::Awaiting { options: 3 },
        queued: 1,
        queued_labels: vec!["浅色 SaaS".to_string()],
    };
    assert_eq!(
        status_line(&base).text,
        "canvas ● localhost:80 · 已选待送：浅色 SaaS",
        "a frozen pick outranks the screen face and names itself"
    );
    assert_eq!(
        status_line(&StatusInput {
            queued: 2,
            queued_labels: vec!["浅色 SaaS".to_string(), "深色系".to_string()],
            ..base.clone()
        })
        .text,
        "canvas ● localhost:80 · 2 个已选待送：浅色 SaaS、深色系",
        "one line per screen: the count is the queued screens, the labels name the picks"
    );
    assert_eq!(
        status_line(&StatusInput {
            queued_labels: Vec::new(),
            ..base.clone()
        })
        .text,
        "canvas ● localhost:80 · 1 个已选待送",
        "the bare count face survives when nothing is named"
    );
    // A long CJK label is cut to the bar's budget, never shoved off it.
    let long = status_line(&StatusInput {
        queued_labels: vec!["一个非常非常长的候选方案名称会超过状态栏预算".to_string()],
        ..base.clone()
    });
    assert!(
        long.text.contains("已选待送：") && long.text.ends_with('…'),
        "truncated, not overflowed: {}",
        long.text
    );
    // Crash and consent still outrank the ack.
    assert_eq!(
        status_line(&StatusInput {
            failed: true,
            ..base.clone()
        })
        .text,
        "canvas ● 已断开 (ctrl+b 重开)"
    );
    assert_eq!(
        status_line(&StatusInput {
            awaiting_consent: true,
            queued: 0,
            ..base
        })
        .text,
        "canvas ● localhost:80 · 待同意"
    );
}

// ── waiting trigger (row 15) ────────────────────────────────────────────────

fn waiting_input<'a>(current: Option<&'a ScreenFace>) -> WaitingInput<'a> {
    WaitingInput {
        current,
        current_seq: Some(7),
        push_turn: 1,
        turn: 2,
        waiting_seq: None,
        answered: true,
        draft: false,
        drain_started_turn: false,
    }
}

#[test]
fn waiting_is_pushed_once_a_choice_is_consumed() {
    let f = face(3, false, false);
    assert!(should_push_waiting(&waiting_input(Some(&f))));
}

#[test]
fn waiting_never_covers_a_just_picked_screen() {
    // The boundary drain consumed a click and started the user turn that the
    // agent is about to answer — covering the screen now is exactly the
    // "抹掉刚选的屏" the spec forbids.
    let f = face(3, false, false);
    let input = WaitingInput {
        drain_started_turn: true,
        ..waiting_input(Some(&f))
    };
    assert!(!should_push_waiting(&input));
}

#[test]
fn waiting_never_wipes_a_screen_that_landed_this_turn() {
    // Freshness guard: the agent pushed it during the turn that just ended
    // (a fresh mockup must survive its own turn).
    let f = face(0, true, false);
    let input = WaitingInput {
        push_turn: 2,
        ..waiting_input(Some(&f))
    };
    assert!(!should_push_waiting(&input));
}

#[test]
fn waiting_skips_an_unanswered_choice_screen() {
    // The canvas still wants a pick — don't bury it under "continue in terminal".
    let f = face(3, false, false);
    let input = WaitingInput {
        answered: false,
        ..waiting_input(Some(&f))
    };
    assert!(!should_push_waiting(&input));
}

#[test]
fn waiting_skips_while_a_correction_is_owed() {
    let f = face(3, false, false);
    let input = WaitingInput {
        draft: true,
        ..waiting_input(Some(&f))
    };
    assert!(!should_push_waiting(&input));
}

#[test]
fn waiting_does_not_repaint_itself() {
    let f = face(0, true, true);
    let input = WaitingInput {
        waiting_seq: Some(7),
        ..waiting_input(Some(&f))
    };
    assert!(!should_push_waiting(&input));
}

#[test]
fn waiting_covers_a_done_display_only_screen() {
    let f = face(0, true, false);
    assert!(should_push_waiting(&waiting_input(Some(&f))));
}

#[test]
fn waiting_needs_a_screen_to_replace() {
    assert!(!should_push_waiting(&waiting_input(None)));
}

// ── waiting content ─────────────────────────────────────────────────────────

#[test]
fn waiting_envelope_is_the_fixed_line_plus_summary() {
    let env = waiting_envelope(
        &["已选 B · 顶部横向导航".to_string()],
        &["配色 · 3 个选项".to_string()],
    );
    assert_eq!(env.title, "继续在终端讨论…");
    assert!(env.is_display_only(), "the waiting screen offers no choice");
    assert!(env.body_html.contains("已定"));
    assert!(env.body_html.contains("已选 B · 顶部横向导航"));
    assert!(env.body_html.contains("待定"));
    assert!(env.body_html.contains("配色 · 3 个选项"));
}

#[test]
fn waiting_envelope_is_interstitial_not_a_version() {
    // The canvas version rail is the design history. If the waiting screen
    // counted as a version, every pause in the discussion would sit in it.
    let env = waiting_envelope(&[], &[]);
    assert!(env.interstitial);
}

#[test]
fn waiting_envelope_escapes_model_supplied_labels() {
    let env = waiting_envelope(&["已选 <script>alert(1)</script>".to_string()], &[]);
    assert!(
        !env.body_html.contains("<script>"),
        "labels are model output — the summary must not inject HTML: {}",
        env.body_html
    );
    assert!(env.body_html.contains("&lt;script&gt;"));
}

#[test]
fn answer_lines_are_picked_out_of_a_merged_submit() {
    let merged = "看一下这两个方向\n\n(画布选择) 已选 B · 顶部横向导航\n(画布选择) 跳过 · 不做选择";
    assert_eq!(
        canvas_answer_lines(merged),
        vec![
            "(画布选择) 已选 B · 顶部横向导航".to_string(),
            "(画布选择) 跳过 · 不做选择".to_string(),
        ]
    );
    assert!(canvas_answer_lines("just typed text").is_empty());
}

#[test]
fn consent_prompts_are_tagged_for_the_renderer() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let host = CanvasHost::with_launch_fn(tx, |_| LaunchOutcome::Spawned);
    host.note_canvas("http://127.0.0.1:1", 1);
    // The tool-side seam is the BrowserLauncher impl — the one model-facing
    // port every open goes through.
    let launcher = ConsentLauncher(Arc::clone(&host));
    assert_eq!(
        launcher.launch("http://127.0.0.1:1"),
        LaunchDecision::AwaitingConsent
    );
    let item = rx.try_recv().expect("consent prompt queued");
    assert!(is_canvas_consent(&item.request));
    assert_eq!(item.request.risk_level, RiskLevel::Safe);
}

// ── the consent gate (row 14) ───────────────────────────────────────────────

type LaunchCounter = Arc<AtomicUsize>;

fn host_with_counter() -> (
    Arc<CanvasHost>,
    mpsc::UnboundedReceiver<ApprovalItem>,
    LaunchCounter,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let counter: LaunchCounter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    let host = CanvasHost::with_launch_fn(tx, move |_| {
        counter_clone.fetch_add(1, Ordering::SeqCst);
        LaunchOutcome::Spawned
    });
    (host, rx, counter)
}

#[test]
fn model_launch_asks_once_and_never_spawns_on_its_own() {
    let (host, mut rx, launches) = host_with_counter();
    host.note_canvas("http://127.0.0.1:4242", 4242);

    // First model ask (canvas_open or an implicit push) raises the prompt.
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0, "nothing pops before y");
    let item = rx.try_recv().expect("consent prompt queued");
    assert!(is_canvas_consent(&item.request));
    assert!(item.request.message.contains("http://127.0.0.1:4242"));
    assert!(
        item.request.message.contains('y'),
        "the prompt teaches the key"
    );

    // Second ask (another push, say) must not stack prompts.
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    assert!(
        rx.try_recv().is_err(),
        "one prompt per canvas URL — no nagging"
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);
}

#[test]
fn a_moved_address_is_announced() {
    // A yes that lands after the canvas died rebinds a fresh port — but the
    // prompt the user answered still named the old URL. First address is the
    // tool's to print (its return text has it); the move is ours to say.
    let (host, _rx, _launches) = host_with_counter();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    assert!(
        host.take_note().is_none(),
        "the first address comes with the tool's answer"
    );

    host.note_canvas("http://127.0.0.1:4243", 4243);
    let note = host.take_note().expect("a moved address is announced");
    assert!(note.contains("reopened"), "{note}");
    assert!(note.contains("4243"), "{note}");

    // Same URL again (Ctrl+B on a live canvas) is not a move — quiet.
    host.note_canvas("http://127.0.0.1:4243", 4243);
    assert!(host.take_note().is_none(), "no move, no note");
}

#[test]
fn note_canvas_keeps_a_prompt_the_tool_already_raised() {
    // The tool's `canvas_open` consults the launcher *before* the run loop's
    // tick can note the bind. The first `note_canvas` for that URL must not
    // reset the gate — that would ask twice (and orphan the first oneshot).
    let (host, mut rx, launches) = host_with_counter();
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    let item = rx.try_recv().expect("consent prompt queued");

    host.note_canvas("http://127.0.0.1:4242", 4242);
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    assert!(
        rx.try_recv().is_err(),
        "still one prompt — the note kept it"
    );

    // The original prompt still resolves into a launch.
    item.decision_tx.send(ApprovalDecision::AllowOnce).unwrap();
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::Launched))
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[test]
fn note_canvas_drops_a_prompt_from_a_previous_url() {
    // A rebind mid-prompt makes the pending question stale: its URL is gone.
    // Dropping the receiver answers it as "nothing was opened" later, and the
    // new URL asks fresh.
    let (host, mut rx, launches) = host_with_counter();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.request_model_launch("http://127.0.0.1:4242");
    let stale = rx.try_recv().unwrap();

    host.note_canvas("http://127.0.0.1:4243", 4243);
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4243"),
        LaunchDecision::AwaitingConsent
    );
    let fresh = rx.try_recv().expect("new URL asks again");
    assert!(fresh.request.message.contains("4243"));

    // Answering the stale prompt no longer launches anything (receiver gone).
    assert!(
        stale.decision_tx.send(ApprovalDecision::AllowOnce).is_err(),
        "the stale prompt's receiver was dropped with the old URL"
    );
    fresh.decision_tx.send(ApprovalDecision::AllowOnce).unwrap();
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::Launched))
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1, "the fresh yes, once");
}

#[test]
fn consent_yes_launches_exactly_once() {
    let (host, mut rx, launches) = host_with_counter();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.request_model_launch("http://127.0.0.1:4242");
    let item = rx.try_recv().unwrap();
    item.decision_tx.send(ApprovalDecision::AllowOnce).unwrap();

    // The tool layer caches what the gate decided (M3 text honesty).
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::Launched))
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    // A later model ask reports `browser launched` and does not re-pop.
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::Launched
    );
    assert_eq!(host.poll_consent(), None, "nothing left to resolve");
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[test]
fn consent_grant_on_a_dead_canvas_recovers_by_reopening() {
    // Row 16: the server can die while the consent prompt is still up. A yes
    // is a yes to *seeing the design*, not to one port — so the gate routes it
    // to Ctrl+B's recovery (rebind + open the new URL) instead of opening a
    // page that cannot load or shrugging at the user.
    for close in [CanvasHost::note_failed, CanvasHost::note_closed] {
        let (host, mut rx, launches) = host_with_counter();
        host.note_canvas("http://127.0.0.1:4242", 4242);
        host.request_model_launch("http://127.0.0.1:4242");
        let item = rx.try_recv().unwrap();

        close(&host); // crash or graceful close — the recovery is the same
        item.decision_tx.send(ApprovalDecision::AllowOnce).unwrap();
        assert_eq!(
            host.poll_consent(),
            Some(ConsentOutcome::ReopenAndLaunch),
            "the yes routes to a rebind, never straight to a dead URL"
        );
        assert_eq!(
            launches.load(Ordering::SeqCst),
            0,
            "the gate itself opens nothing — the rebind's launch does"
        );

        // The gate has settled: no re-nag, and no `browser launched` claim
        // until the reopen's own launch is noted.
        assert_eq!(
            host.request_model_launch("http://127.0.0.1:4242"),
            LaunchDecision::AwaitingConsent,
            "a settled gate never nags again"
        );
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn allow_always_still_means_launch_once() {
    // Standing consent would let a later implicit push pop a browser with no
    // y in between (negative list #1) — so "always" is mapped to "once".
    let (host, mut rx, launches) = host_with_counter();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.request_model_launch("http://127.0.0.1:4242");
    let item = rx.try_recv().unwrap();
    item.decision_tx
        .send(ApprovalDecision::AllowAlways)
        .unwrap();
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::Launched))
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    // A rebind (new URL) asks again — the yes was for that one URL.
    host.note_canvas("http://127.0.0.1:4243", 4243);
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4243"),
        LaunchDecision::AwaitingConsent
    );
    let item = rx.try_recv().unwrap();
    assert!(is_canvas_consent(&item.request));
}

#[test]
fn consent_no_shows_the_url_instead_of_a_browser() {
    let (host, mut rx, launches) = host_with_counter();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.request_model_launch("http://127.0.0.1:4242");
    let item = rx.try_recv().unwrap();
    item.decision_tx.send(ApprovalDecision::Deny).unwrap();
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::AwaitingConsent)),
        "the tool keeps reporting `awaiting consent` — nothing launched"
    );

    assert_eq!(launches.load(Ordering::SeqCst), 0, "n means no browser");
    let note = host.take_note().expect("the URL is printed instead");
    assert!(note.contains("http://127.0.0.1:4242"));
    // Declined is sticky: later pushes never re-ask.
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    assert!(rx.try_recv().is_err(), "a deny must not be re-litigated");
}

#[test]
fn ctrl_b_is_its_own_consent_and_never_prompts() {
    let (host, mut rx, launches) = host_with_counter();
    host.note_canvas("http://127.0.0.1:4242", 4242);

    // Even after a deny, the user's own key opens the browser.
    host.request_model_launch("http://127.0.0.1:4242");
    let item = rx.try_recv().unwrap();
    item.decision_tx.send(ApprovalDecision::Deny).unwrap();
    let _ = host.poll_consent();

    host.request_user_launch("http://127.0.0.1:4242");
    assert_eq!(launches.load(Ordering::SeqCst), 1, "ctrl+b opens directly");
    assert!(rx.try_recv().is_err(), "no prompt on the user's own key");
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::Launched
    );
}

#[test]
fn headless_launch_prints_the_url_and_carries_on() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let host = CanvasHost::with_launch_fn(tx, |_| LaunchOutcome::ShowUrl);
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.request_user_launch("http://127.0.0.1:4242");
    let note = host.take_note().expect("headless prints the URL");
    assert!(note.contains("http://127.0.0.1:4242"));
}

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
