use super::*;
use phi_agent::{Action, FinishReason, ParseOutcome, SessionId};

/// A PostLlmCtx holding `actions`, with `full_text` as the reply.
fn ctx(full_text: &str, actions: Vec<Action>) -> PostLlmCtx {
    let parse = if actions.is_empty() {
        ParseOutcome::Empty
    } else {
        ParseOutcome::Actions(actions)
    };
    PostLlmCtx {
        session_id: SessionId::new(1),
        full_text: full_text.to_string(),
        parse,
        available_tools: vec![],
        turn_count: 1,
        total_actions: 0,
        nudge_count: 0,
        turn_actions: 0,
        skip_push: false,
        follow_up_message: None,
        finish_reason: FinishReason::Stop,
    }
}

fn write(path: &str) -> Action {
    Action::from_tool_call(
        "c1",
        "write_file",
        format!(r#"{{"path":"{path}","content":"x"}}"#),
    )
}

fn cell() -> Action {
    Action::from_tool_call("c2", "code_cell", r#"{"code":"print(1)"}"#)
}

fn shell(cmd: &str) -> Action {
    Action::from_tool_call("c4", "execute_command", format!(r#"{{"command":"{cmd}"}}"#))
}

fn verify() -> Action {
    Action::from_tool_call("c3", "verify", r#"{}"#)
}

fn gate() -> (VerifyGateMiddleware, Arc<Mutex<EvidenceLedger>>) {
    let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
    ledger.lock().unwrap().set_compile_layer(true);
    (
        VerifyGateMiddleware::new(VerifyGateConfig::default(), Arc::clone(&ledger)),
        ledger,
    )
}

#[test]
fn edit_path_reads_the_path_argument() {
    assert_eq!(
        edit_path(r#"{"path":"src/a.rs","content":"x"}"#),
        Some("src/a.rs".to_string())
    );
    assert_eq!(edit_path(r#"{"code":"print(1)"}"#), None);
    assert_eq!(edit_path("not json"), None);
}

#[tokio::test]
async fn a_hand_back_with_no_edits_is_never_intercepted() {
    let (mw, _ledger) = gate();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_none(), "nothing was edited");
    assert!(!c.skip_push);
}

#[tokio::test]
async fn a_docs_only_edit_is_not_a_code_change() {
    // The user's hard rule: prose and loose config need no compile check.
    let (mw, _ledger) = gate();
    let mut c = ctx("", vec![write("README.md")]);
    mw.on_post_llm(&mut c).await.unwrap();

    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_none(), "README.md is not code");
}

#[tokio::test]
async fn a_code_edit_without_verification_is_intercepted_once() {
    let (mw, _ledger) = gate();

    // Turn 1: the edit (deferred — it lands next turn).
    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.follow_up_message.is_none(),
        "a write turn is not a hand-back"
    );

    // Turn 2: hand back. The deferred edit is now counted → intercept.
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.skip_push, "the premature done is suppressed");
    let msg = c.follow_up_message.expect("one nudge");
    assert!(msg.contains("[gate]"), "must be self-identifying: {msg}");
}

#[tokio::test]
async fn the_second_hand_back_degrades_to_a_marker_not_a_nudge() {
    // The user's requirement: intercept once is enough.
    let (mw, _ledger) = gate();

    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_some(), "first interception");

    let mut c = ctx("all done, really", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.follow_up_message.is_none(),
        "never nags twice about the same batch"
    );
    assert!(!c.skip_push, "the reply must reach the user");
    assert!(
        c.full_text.contains("⚠️"),
        "degraded to a visible marker: {}",
        c.full_text
    );
}

#[tokio::test]
async fn the_marker_distinguishes_laziness_from_failure() {
    let (mw, ledger) = gate();
    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();

    // A failed check after the edit: "tried and lost", not "lazy".
    ledger
        .lock()
        .unwrap()
        .record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(101),
            error_count: 1,
            warning_count: 0,
            summary: String::new(),
        });

    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.full_text.contains("Verification failed"),
        "should say checked-and-failed: {}",
        c.full_text
    );
    assert!(
        !c.full_text.contains("No verification since"),
        "{}",
        c.full_text
    );
}

#[tokio::test]
async fn the_nudge_names_the_verdict_not_just_verify() {
    // The first smoke run: the model ran `verify`, it failed, it said so —
    // and the gate still answered "please run verify", which cost a turn
    // and asked for something already done.
    let (mw, ledger) = gate();
    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();

    ledger
        .lock()
        .unwrap()
        .record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(101),
            error_count: 2,
            warning_count: 0,
            summary: String::new(),
        });

    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    let msg = c.follow_up_message.expect("one nudge");
    assert!(msg.contains("[gate]"), "must be self-identifying: {msg}");
    assert!(msg.contains("verified and failed"), "{msg}");
    assert!(msg.contains("exit 101"), "{msg}");
    assert!(
        !msg.contains("no green verification"),
        "it already ran verify — don't ask again: {msg}"
    );
}

#[tokio::test]
async fn the_marker_is_not_appended_twice() {
    // The marker lives in the model's output stream, so the model can copy
    // it back — the first smoke run showed two footnotes for one problem.
    let (mw, _ledger) = gate();
    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done, really", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    let marker = c.full_text.trim().to_string();
    assert_eq!(marker.matches("⚠️").count(), 1, "{marker}");

    // Replay that marker as the model's own text on a fresh run.
    let (mw, _ledger) = gate();
    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_some(), "first interception");

    let mut c = ctx(&format!("done\n\n{marker}"), vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert_eq!(
        c.full_text.matches("⚠️").count(),
        1,
        "a copied marker must not be doubled: {}",
        c.full_text
    );
}

#[tokio::test]
async fn a_verify_in_the_same_turn_as_the_edit_does_not_cover_it() {
    // Rule 3: never a false green. The verify runs after this hook, so its
    // evidence must not be credited with a write it raced.
    let (mw, ledger) = gate();

    let mut c = ctx("", vec![write("src/a.rs"), verify()]);
    mw.on_post_llm(&mut c).await.unwrap();

    // The verify passes — but for code that predates the edit.
    ledger
        .lock()
        .unwrap()
        .record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(0),
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        });

    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.follow_up_message.is_some(),
        "the racing verify must not count as a green light"
    );
}

#[tokio::test]
async fn a_passing_verify_does_not_rearm_the_nudge_budget() {
    // `max_nudges` is per user message (review finding p2/outside-voice#3):
    // a passing verify settles the batch, but it must not hand the gate a
    // second interception inside the same message.
    let (mw, ledger) = gate();

    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_some());

    // Verify passes for real.
    let mut c = ctx("", vec![verify()]);
    mw.on_post_llm(&mut c).await.unwrap();
    ledger
        .lock()
        .unwrap()
        .record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(0),
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        });

    // Hand back now: fresh green → no interception at all.
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_none(), "a green light settles it");

    // A NEW edit after the green light: the budget stays spent, so this
    // hand-back degrades to the marker instead of nagging again.
    let mut c = ctx("", vec![write("src/b.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.follow_up_message.is_none(),
        "one interception per user message, even across a passing verify"
    );
    assert!(
        c.full_text.contains("⚠️"),
        "degraded to the visible marker: {}",
        c.full_text
    );
}

#[tokio::test]
async fn a_turn_ending_in_a_verify_is_never_intercepted() {
    // The model is doing the right thing. Don't punish it mid-flight.
    let (mw, _ledger) = gate();
    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();

    let mut c = ctx("let me check it first", vec![verify()]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_none());
}

#[tokio::test]
async fn a_workspace_without_a_compile_layer_is_left_alone() {
    let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
    ledger.lock().unwrap().set_compile_layer(false);
    let mw = VerifyGateMiddleware::new(VerifyGateConfig::default(), Arc::clone(&ledger));

    let mut c = ctx("", vec![write("src/a.js")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();

    assert!(c.follow_up_message.is_none(), "nothing to verify against");
    assert!(
        !c.full_text.contains("⚠️"),
        "and no noise either: {}",
        c.full_text
    );
}

#[tokio::test]
async fn deny_mode_switches_the_gate_off() {
    let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
    ledger.lock().unwrap().set_compile_layer(true);
    let mw = VerifyGateMiddleware::new(
        VerifyGateConfig {
            writes_possible: Arc::new(|| false),
            ..VerifyGateConfig::default()
        },
        Arc::clone(&ledger),
    );

    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();

    assert!(c.follow_up_message.is_none());
    assert!(!c.full_text.contains("⚠️"));
}

#[tokio::test]
async fn the_writes_probe_is_re_read_every_turn() {
    // The approval mode is a live switch (Shift+Tab) — a construction-time
    // snapshot would go stale and gate a run that can no longer write.
    let ledger = Arc::new(Mutex::new(EvidenceLedger::new()));
    ledger.lock().unwrap().set_compile_layer(true);
    let can_write = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let probe = Arc::clone(&can_write);
    let mw = VerifyGateMiddleware::new(
        VerifyGateConfig {
            writes_possible: Arc::new(move || probe.load(std::sync::atomic::Ordering::SeqCst)),
            ..VerifyGateConfig::default()
        },
        Arc::clone(&ledger),
    );

    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();

    // Shift+Tab → deny mid-run: the gate must stop watching.
    can_write.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_none(), "reads the switch live");
}

#[tokio::test]
async fn a_new_user_message_resets_the_cycle() {
    let (mw, _ledger) = gate();
    let mut c = ctx("", vec![write("src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_some());

    // Spend the budget, then a new user message arrives.
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(c.follow_up_message.is_none());

    let mut uctx = UserMessageCtx {
        session_id: SessionId::new(1),
        user_input: "now do this instead".to_string(),
    };
    mw.on_user_message(&mut uctx).await.unwrap();

    let mut c = ctx("", vec![write("src/b.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.follow_up_message.is_some(),
        "a new task gets a fresh budget"
    );
}

#[tokio::test]
async fn an_unresolvable_edit_path_still_counts() {
    // A CodeAct cell carries no path. Conservative direction: treat it as
    // an edit rather than guessing it away (rule 3).
    let (mw, _ledger) = gate();
    let mut c = ctx("", vec![cell()]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.follow_up_message.is_some(),
        "an opaque code action is an edit until proven otherwise"
    );
}

#[tokio::test]
async fn execute_command_marks_the_ledger_dirty() {
    // A shell edit (sed -i, cat > file) is invisible at the tool boundary.
    // It must count as a possible edit so it can never ride on an older
    // verify — the false-pass direction rule 3 forbids.
    let (mw, _ledger) = gate();

    // Turn 1: a shell command, no verify.
    let mut c = ctx("", vec![shell("sed -i s/a/b/ src/a.rs")]);
    mw.on_post_llm(&mut c).await.unwrap();

    // Turn 2: hand back. The shell command counted as an edit → intercept.
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.skip_push,
        "a shell command may have edited code; the hand-back must not stand"
    );
    assert!(c.follow_up_message.is_some(), "one nudge naming the gap");
}

#[tokio::test]
async fn a_read_only_shell_command_does_not_dirty_the_gate() {
    // `ls` and `git status` leave the workspace untouched: a hand-back
    // after them must not be nagged (review finding p2/adversarial-native#1).
    let (mw, _ledger) = gate();
    let mut c = ctx("", vec![shell("ls -la src/")]);
    mw.on_post_llm(&mut c).await.unwrap();
    let mut c = ctx("", vec![shell("git status --short")]);
    mw.on_post_llm(&mut c).await.unwrap();

    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.follow_up_message.is_none(),
        "read-only shell is not an edit"
    );
    assert!(!c.full_text.contains("⚠️"), "{}", c.full_text);
}

#[test]
fn shell_may_mutate_separates_readers_from_writers() {
    for cmd in [
        "ls -la",
        "cat src/a.rs",
        "grep -rn foo src/",
        "rg --files",
        "git status",
        "git diff HEAD~1",
        "cat a.rs | grep fn | wc -l",
        "head -5 a.rs && tail -5 a.rs",
    ] {
        assert!(!shell_may_mutate(cmd), "{cmd} must not dirty the gate");
    }
    // Writers, unknown heads, and anything that runs something else —
    // the conservative direction must catch every one of them.
    for cmd in [
        "sed -i s/a/b/ src/a.rs",
        "cat > src/a.rs",
        "echo hi >> notes.txt",
        "ls | xargs rm",
        "python tools/gen.py",
        "make check",
        "git push",
        "git config user.name x",
        "find . -delete",
        "ls $(rm -rf /)",
        "touch src/a.rs",
    ] {
        assert!(shell_may_mutate(cmd), "{cmd} must dirty the gate");
    }
}

#[test]
fn shell_may_mutate_splits_on_newlines_and_ampersands() {
    // `sh -c` runs each of these as TWO commands; the classifier must see
    // the writer behind the reader's first word (review findings
    // v11/red-team#1, v11/adversarial-native#2).
    assert!(
        shell_may_mutate("ls\nrm -rf src/"),
        "a newline-separated writer must dirty"
    );
    assert!(
        shell_may_mutate("ls & rm -rf src/"),
        "an &-separated writer must dirty"
    );
    assert!(
        shell_may_mutate("ls\nls -la\nrm f"),
        "a writer at the tail of a newline chain must dirty"
    );
    assert!(
        !shell_may_mutate("ls\nls -la"),
        "newline-separated readers stay readers"
    );
}

#[tokio::test]
async fn deferred_edits_survive_a_user_message_boundary() {
    // A run can end between the deferral and the next on_post_llm: the user
    // speaks first. The deferred edit must fold into the ledger at that
    // boundary, not vanish with the reset — otherwise the earlier verify
    // stays "fresh" over a write it never covered.
    let (mw, ledger) = gate();
    ledger
        .lock()
        .unwrap()
        .record_verify(crate::verify_ledger::VerifyVerdict {
            kind: VerifyKind::Check,
            command: "cargo check".to_string(),
            exit_code: Some(0),
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        });

    // Turn 1: an edit racing a verify → deferred, not yet recorded.
    let mut c = ctx("", vec![write("src/a.rs"), verify()]);
    mw.on_post_llm(&mut c).await.unwrap();

    // The user speaks before the next LLM turn.
    let mut uctx = UserMessageCtx {
        session_id: SessionId::new(1),
        user_input: "carry on".to_string(),
    };
    mw.on_user_message(&mut uctx).await.unwrap();

    // Turn 2: hand back. The pre-edit green light must not survive the fold.
    let mut c = ctx("all done", vec![]);
    mw.on_post_llm(&mut c).await.unwrap();
    assert!(
        c.skip_push,
        "the old verify never covered the folded edit — no false green"
    );
}
