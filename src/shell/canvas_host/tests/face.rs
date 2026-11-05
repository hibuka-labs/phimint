use super::super::*;
use super::*;

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
    assert_eq!(line.text, "canvas ● localhost:32123 · 3 options to pick");
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

    assert_eq!(show(&base), "canvas ● localhost:80 · idle");
    assert_eq!(
        show(&StatusInput {
            awaiting_consent: true,
            ..base.clone()
        }),
        "canvas ● localhost:80 · awaiting consent"
    );
    assert_eq!(
        show(&StatusInput {
            screen: ScreenStatus::DisplayOnly,
            ..base.clone()
        }),
        "canvas ● localhost:80 · display only"
    );
    assert_eq!(
        show(&StatusInput {
            screen: ScreenStatus::Waiting,
            ..base.clone()
        }),
        "canvas ● localhost:80 · idle"
    );
    assert_eq!(
        show(&StatusInput {
            closed: true,
            ..base.clone()
        }),
        "canvas closed · can reopen"
    );
    let failed = status_line(&StatusInput {
        failed: true,
        ..base
    });
    assert_eq!(failed.text, "canvas ● disconnected (ctrl+b reopen)");
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
    // old face kept saying `3 options to pick` — silent. Now the debt is spelled out
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
        "canvas ● localhost:80 · picked, queued: 浅色 SaaS",
        "a frozen pick outranks the screen face and names itself"
    );
    assert_eq!(
        status_line(&StatusInput {
            queued: 2,
            queued_labels: vec!["浅色 SaaS".to_string(), "深色系".to_string()],
            ..base.clone()
        })
        .text,
        "canvas ● localhost:80 · 2 picked, queued: 浅色 SaaS, 深色系",
        "one line per screen: the count is the queued screens, the labels name the picks"
    );
    assert_eq!(
        status_line(&StatusInput {
            queued_labels: Vec::new(),
            ..base.clone()
        })
        .text,
        "canvas ● localhost:80 · 1 picked, queued",
        "the bare count face survives when nothing is named"
    );
    // A long CJK label is cut to the bar's budget, never shoved off it.
    let long = status_line(&StatusInput {
        queued_labels: vec!["一个非常非常长的候选方案名称会超过状态栏预算".to_string()],
        ..base.clone()
    });
    assert!(
        long.text.contains("picked, queued: ") && long.text.ends_with('…'),
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
        "canvas ● disconnected (ctrl+b reopen)"
    );
    assert_eq!(
        status_line(&StatusInput {
            awaiting_consent: true,
            queued: 0,
            ..base
        })
        .text,
        "canvas ● localhost:80 · awaiting consent"
    );
}

// ── waiting trigger (row 15) ────────────────────────────────────────────────

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
        &["picked B · 顶部横向导航".to_string()],
        &["配色 · 3 options".to_string()],
    );
    assert_eq!(env.title, "Continue in the terminal…");
    assert!(env.is_display_only(), "the waiting screen offers no choice");
    assert!(env.body_html.contains("Decided"));
    assert!(env.body_html.contains("picked B · 顶部横向导航"));
    assert!(env.body_html.contains("Pending"));
    assert!(env.body_html.contains("配色 · 3 options"));
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
    let env = waiting_envelope(&["picked <script>alert(1)</script>".to_string()], &[]);
    assert!(
        !env.body_html.contains("<script>"),
        "labels are model output — the summary must not inject HTML: {}",
        env.body_html
    );
    assert!(env.body_html.contains("&lt;script&gt;"));
}

#[test]
fn answer_lines_are_picked_out_of_a_merged_submit() {
    let merged = "看一下这两个方向\n\n(canvas pick) picked B · 顶部横向导航\n(canvas pick) skipped · no choice";
    assert_eq!(
        canvas_answer_lines(merged),
        vec![
            "(canvas pick) picked B · 顶部横向导航".to_string(),
            "(canvas pick) skipped · no choice".to_string(),
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
