use super::super::*;
use super::*;

// ── the consent gate (row 14) ───────────────────────────────────────────────

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

// ── the launch target (what y actually opens) ───────────────────────────────

#[test]
fn the_consent_prompt_names_the_design_and_the_terminal_key() {
    let (host, mut rx, _urls) = host_recording_urls();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));
    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    let item = rx.try_recv().expect("consent prompt queued");
    let message = &item.request.message;
    assert!(
        message.contains("opens the design"),
        "the promise names the design: {message}"
    );
    assert!(
        message.contains("terminal"),
        "and where the key lives: {message}"
    );
    // The key is in the terminal *and not on the page* — without that clause
    // the promise reads as "something on the page opens it", which is how a
    // model came to hunt for a consent button there (see the tool-text tests).
    assert!(
        message.contains("not anything on the page"),
        "the answer's place is named and the page is ruled out: {message}"
    );
}

#[test]
fn y_opens_the_design_screen_not_the_canvas_root() {
    // The promise is "come look at the design" — the design screen, not
    // whatever screen happens to be current when the user finally answers.
    let (host, mut rx, urls) = host_recording_urls();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));

    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    answer_yes(&mut rx);
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::Launched))
    );
    assert_eq!(
        launched_urls(&urls),
        vec!["http://127.0.0.1:4242/screens/1".to_string()]
    );
}

#[test]
fn the_pause_screen_is_not_the_launch_target() {
    // Turn end replaces the design with the pause before the user looks —
    // that replacement must not become what y opens.
    let (host, mut rx, urls) = host_recording_urls();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));
    host.on_push(&pause(2, "Continue in the terminal…"));
    host.on_waiting_push(2);

    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    answer_yes(&mut rx);
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::Launched))
    );
    assert_eq!(
        launched_urls(&urls),
        vec!["http://127.0.0.1:4242/screens/1".to_string()]
    );
}

#[test]
fn before_any_design_the_root_is_what_opens() {
    // Nothing pushed yet: the root page is the promise ("the design will
    // appear here") — keep opening it.
    let (host, mut rx, urls) = host_recording_urls();
    host.note_canvas("http://127.0.0.1:4242", 4242);

    assert_eq!(
        host.request_model_launch("http://127.0.0.1:4242"),
        LaunchDecision::AwaitingConsent
    );
    answer_yes(&mut rx);
    assert_eq!(
        host.poll_consent(),
        Some(ConsentOutcome::Decision(LaunchDecision::Launched))
    );
    assert_eq!(
        launched_urls(&urls),
        vec!["http://127.0.0.1:4242".to_string()]
    );
}

#[test]
fn the_user_key_opens_the_design_too() {
    // Ctrl+B is the same promise with the keypress as the consent.
    let (host, _rx, urls) = host_recording_urls();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));

    host.request_user_launch("http://127.0.0.1:4242");
    assert_eq!(
        launched_urls(&urls),
        vec!["http://127.0.0.1:4242/screens/1".to_string()]
    );
}

#[test]
fn a_replayed_history_restores_the_launch_target_after_a_rebind() {
    // The yes-after-death recovery rebinds a new port (history cleared) and
    // the run loop replays the store's receipts — the target must survive.
    let (host, _rx, urls) = host_recording_urls();
    host.note_canvas("http://127.0.0.1:4242", 4242);
    host.on_push(&receipt(1, "布局选择", 3, false));
    host.on_push(&pause(2, "Continue in the terminal…"));

    host.note_canvas("http://127.0.0.1:4243", 4243);
    for replayed in [
        receipt(1, "布局选择", 3, false),
        pause(2, "Continue in the terminal…"),
    ] {
        host.on_push(&replayed);
    }

    host.request_user_launch("http://127.0.0.1:4243");
    assert_eq!(
        launched_urls(&urls),
        vec!["http://127.0.0.1:4243/screens/1".to_string()]
    );
}

#[tokio::test]
async fn replaying_a_real_canvas_history_restores_the_launch_target() {
    // What `reopen_canvas` must do after noting the new URL: refill the
    // bookkeeping from the store — the receipts carry which pushes are pauses.
    let tmp = tempfile::tempdir().unwrap();
    let config = phi_canvas::CanvasConfig::new(tmp.path().join("canvas"), "sess-replay");
    let canvas = phi_canvas::Canvas::open(config)
        .await
        .expect("canvas opens");

    let design = phi_canvas::ScreenEnvelope {
        title: "布局选择".to_string(),
        subtitle: None,
        options: vec![],
        body_html: String::new(),
        screen: None,
        multiselect: false,
        reference: None,
        escape: None,
        interstitial: false,
    };
    canvas.push(design).await.expect("pushes");
    canvas
        .push(waiting_envelope(&[], &[]))
        .await
        .expect("pushes");

    let (host, _rx, urls) = host_recording_urls();
    host.note_canvas(&canvas.url(), canvas.port());
    host.replay_pushes(&canvas);
    host.request_user_launch(&canvas.url());
    assert_eq!(
        launched_urls(&urls),
        vec![format!("{}/screens/1", canvas.url())]
    );

    canvas.shutdown().await;
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
        "the tool keeps reporting the unanswered wait — nothing launched"
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
