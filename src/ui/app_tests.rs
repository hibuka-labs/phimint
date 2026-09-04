//! Tests for App state machine and event handling.

use super::*;
use agent_base::{PlanItem, PlanStepStatus, UserEvent};
use crossterm::event::{MouseButton, MouseEventKind};
use phi_agent::SessionId;

fn text(s: &str) -> RuntimeEvent {
    RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: s.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn thought(s: &str) -> RuntimeEvent {
    RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: s.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn tool_started(name: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        args_json: "{}".to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn tool_finished(name: &str, denied: bool) -> RuntimeEvent {
    RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        summary: "done".to_string(),
        agent_id: None,
        trace_id: None,
        denied,
        details: None,
    }
}

fn run_finished(agent_id: Option<&str>) -> RuntimeEvent {
    RuntimeEvent::RunFinished {
        session_id: SessionId::new(1),
        agent_id: agent_id.map(String::from),
        trace_id: None,
    }
}

fn child_text(agent: &str, s: &str) -> RuntimeEvent {
    RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: s.to_string(),
        agent_id: Some(agent.to_string()),
        trace_id: None,
    }
}

fn child_thought(agent: &str, s: &str) -> RuntimeEvent {
    RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: s.to_string(),
        agent_id: Some(agent.to_string()),
        trace_id: None,
    }
}

fn child_tool_started(agent: &str, name: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        args_json: "{}".to_string(),
        agent_id: Some(agent.to_string()),
        trace_id: None,
    }
}

fn child_tool_finished(agent: &str, name: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        summary: "done".to_string(),
        agent_id: Some(agent.to_string()),
        trace_id: None,
        denied: false,
        details: None,
    }
}

fn plan(objective: &str, steps: Vec<(&str, PlanStepStatus)>) -> RuntimeEvent {
    plan_ex(objective, None, steps)
}

fn plan_ex(
    objective: &str,
    explanation: Option<&str>,
    steps: Vec<(&str, PlanStepStatus)>,
) -> RuntimeEvent {
    RuntimeEvent::PlanUpdated {
        session_id: SessionId::new(1),
        objective: objective.to_string(),
        explanation: explanation.map(String::from),
        plan: steps
            .into_iter()
            .map(|(step, status)| PlanItem {
                step: step.to_string(),
                status,
            })
            .collect(),
        agent_id: None,
        trace_id: None,
    }
}

fn submit(app: &mut App, input: &str) -> Action {
    app.composer.clear();
    app.composer.insert_str(input);
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE).unwrap()
}

#[test]
fn status_tracks_state_machine() {
    let mut app = App::new();
    assert_eq!(app.status, AgentStatus::Idle);

    app.handle_event(TuiEvent::Runtime(thought("hmm")));
    assert_eq!(app.status, AgentStatus::Running { phase: Phase::Thinking });

    app.handle_event(TuiEvent::Runtime(text("answer")));
    assert_eq!(app.status, AgentStatus::Running { phase: Phase::Streaming });

    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    assert_eq!(
        app.status,
        AgentStatus::Running {
            phase: Phase::ToolCall {
                tool: "read_file".into()
            }
        }
    );

    app.handle_event(TuiEvent::Runtime(tool_finished("read_file", false)));
    assert_eq!(app.status, AgentStatus::Running { phase: Phase::Thinking });

    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert_eq!(app.status, AgentStatus::Idle);
    assert!(!app.running);
}

#[test]
fn tool_calls_render_inline_in_output() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    app.handle_event(TuiEvent::Runtime(tool_finished("read_file", false)));
    app.handle_event(TuiEvent::Runtime(tool_finished("execute_command", true)));

    // Invocation + result lines land inline in `output`, in event order.
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "⏺ read_file {}",
            "  ✓ read_file done",
            "  ⛔ execute_command denied",
        ]
    );
    assert_eq!(app.transcript.output[0].kind, LineKind::Tool);
    assert_eq!(app.transcript.output[1].kind, LineKind::ToolResult);
    assert_eq!(app.transcript.output[2].kind, LineKind::Error);
}

#[test]
fn run_finished_from_sub_agent_does_not_end_turn() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::Runtime(run_finished(Some("sub/1"))));
    // Sub-agent finish must not flip the top-level status to Idle.
    assert!(app.running);
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert!(!app.running);
    assert_eq!(app.status, AgentStatus::Idle);
}

#[test]
fn text_deltas_coalesce_into_fewer_lines() {
    let mut app = App::new();
    // Three fragments with no newline → one output line, not three.
    app.handle_event(TuiEvent::Runtime(text("hel")));
    app.handle_event(TuiEvent::Runtime(text("lo ")));
    app.handle_event(TuiEvent::Runtime(text("world")));
    app.handle_event(TuiEvent::Runtime(tool_started("verify"))); // flush
    let normals: Vec<_> = app
        .transcript.output
        .iter()
        .filter(|l| l.kind == LineKind::Normal)
        .collect();
    assert_eq!(normals.len(), 1);
    assert_eq!(normals[0].text, "hello world");
}

#[test]
fn text_with_newline_splits_lines() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(text("a\nb")));
    app.handle_event(TuiEvent::Runtime(tool_started("verify")));
    let normals: Vec<_> = app
        .transcript.output
        .iter()
        .filter(|l| l.kind == LineKind::Normal)
        .collect();
    // The full raw text is stored as ONE OutputLine; the renderer handles
    // markdown parsing and wrapping at display time.
    assert_eq!(normals.len(), 1);
    assert_eq!(normals[0].text, "a\nb");
}

#[test]
fn thought_renders_before_text() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(thought("thinking")));
    app.handle_event(TuiEvent::Runtime(text("answer")));
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert_eq!(app.transcript.output[0].kind, LineKind::Thought);
    assert_eq!(app.transcript.output[0].text, "thinking");
    assert_eq!(app.transcript.output[1].kind, LineKind::Normal);
    assert_eq!(app.transcript.output[1].text, "answer");
}

#[test]
fn submit_requires_nonempty_and_not_running() {
    let mut app = App::new();
    // Empty composer → no submit.
    app.composer.clear();
    assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);

    // Non-empty → Submit + running.
    let action = submit(&mut app, "do a thing");
    assert_eq!(action, Action::Submit("do a thing".to_string()));
    assert!(app.running);

    // Already running → Enter ignored.
    app.composer.insert_str("second");
    assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
}

#[test]
fn submit_echoes_user_message() {
    let mut app = App::new();
    let action = submit(&mut app, "hello world");
    assert_eq!(action, Action::Submit("hello world".to_string()));
    let users: Vec<&str> = app
        .transcript.output
        .iter()
        .filter(|l| l.kind == LineKind::User)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(users, vec!["❯ hello world"]);
}

#[test]
fn progress_updates_and_clears_live_progress() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(tool_started("execute_command")));
    assert_eq!(app.status_line(), "🔧 execute_command (Ctrl+C cancel)");

    let prog = RuntimeEvent::UserEvent {
        session_id: SessionId::new(1),
        event: UserEvent::Progress {
            text: "Compiling phimint v0.1.0".to_string(),
        },
        agent_id: None,
        trace_id: None,
    };
    app.handle_event(TuiEvent::Runtime(prog));
    assert_eq!(
        app.status_line(),
        "🔧 execute_command: Compiling phimint v0.1.0"
    );

    // A new tool call drops the previous tool's live progress.
    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    assert_eq!(app.status_line(), "🔧 read_file (Ctrl+C cancel)");
}

#[test]
fn shift_enter_inserts_newline_not_submit() {
    let mut app = App::new();
    app.composer.insert_str("a");
    let r = app.handle_key(KeyCode::Enter, KeyModifiers::SHIFT);
    assert_eq!(r, None);
    assert_eq!(app.composer.text(), "a\n");
}

#[test]
fn ctrl_c_cancels_when_running() {
    let mut app = App::new();
    app.running = false;
    // First Ctrl+C when idle → show hint (not quit yet).
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        None
    );
    // Second Ctrl+C within timeout → quit.
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Quit)
    );
    // While running → cancel immediately (no hint).
    app.running = true;
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Cancel)
    );
}

#[test]
fn turn_error_appends_red_line_and_resets() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::TurnError("boom".to_string()));
    assert!(!app.running);
    assert_eq!(app.status, AgentStatus::Idle);
    assert_eq!(app.transcript.output.last().unwrap().kind, LineKind::Error);
    assert!(app.transcript.output.last().unwrap().text.contains("boom"));
}


#[test]
fn streaming_tail_lines_wraps_long_text() {
    let mut app = App::new();
    // 250 chars at DEFAULT_WRAP_WIDTH=100 → three wrapped lines.
    let long = "a".repeat(250);
    app.handle_event(TuiEvent::Runtime(text(&long)));
    let (lines, kind) = app.streaming_tail_lines().expect("tail present");
    assert_eq!(
        lines,
        ["a".repeat(100), "a".repeat(100), "a".repeat(50)].as_slice()
    );
    assert_eq!(kind, LineKind::Normal);
}

#[test]
fn streaming_tail_exposes_uncommitted_text() {
    let mut app = App::new();
    assert_eq!(app.streaming_tail_lines(), None);
    app.handle_event(TuiEvent::Runtime(text("hel")));
    app.handle_event(TuiEvent::Runtime(text("lo")));
    let (lines, kind) = app.streaming_tail_lines().expect("tail present");
    assert_eq!(lines, ["hello".to_string()].as_slice());
    assert_eq!(kind, LineKind::Normal);
    // A structural event flushes the tail into committed output (and, for a
    // tool call, also appends an inline invocation line).
    app.handle_event(TuiEvent::Runtime(tool_started("verify")));
    assert_eq!(app.streaming_tail_lines(), None);
    assert_eq!(app.transcript.output[0].text, "hello");
    assert_eq!(app.transcript.output[1].kind, LineKind::Tool);
}

#[test]
fn streaming_tail_flips_to_text_after_thought() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(thought("hmm")));
    let (lines, kind) = app.streaming_tail_lines().expect("thought tail present");
    assert_eq!(lines, ["hmm".to_string()].as_slice());
    assert_eq!(kind, LineKind::Thought);
    app.handle_event(TuiEvent::Runtime(text("answer")));
    let (lines, kind) = app.streaming_tail_lines().expect("text tail present");
    assert_eq!(lines, ["answer".to_string()].as_slice());
    assert_eq!(kind, LineKind::Normal);
}

#[test]
fn scroll_up_steps_from_bottom_not_noop() {
    let mut app = App::new();
    for i in 0..100 {
        app.transcript.push(OutputLine { spans: None, original: None, text: format!("line {i}"), kind: LineKind::Normal, detail: None });
    }
    app.scroll_up();
    assert!(!app.viewport.follow_bottom);
    assert_eq!(app.viewport.scroll_offset, SCROLL_STEP);
}

#[test]
fn scroll_down_reenters_follow_bottom() {
    let mut app = App::new();
    for i in 0..100 {
        app.transcript.push(OutputLine { spans: None, original: None, text: format!("line {i}"), kind: LineKind::Normal, detail: None });
    }
    app.scroll_up();
    app.scroll_up();
    assert_eq!(app.viewport.scroll_offset, 2 * SCROLL_STEP);
    app.scroll_down();
    assert_eq!(app.viewport.scroll_offset, SCROLL_STEP);
    app.viewport.scroll_offset = SCROLL_STEP;
    app.scroll_down();
    assert!(app.viewport.follow_bottom);
    assert_eq!(app.viewport.scroll_offset, 0);
}

fn pending_approval(app: &mut App) {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.approval_queue.push_back(ApprovalItem {
        request: ApprovalRequest {
            title: "write_file".to_string(),
            message: "Write file: src/lib.rs".to_string(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Sensitive,
            raw: None,
        },
        decision_tx: tx,
    });
}

#[test]
fn approval_popup_routes_yan_and_swallows_others() {
    let mut app = App::new();
    pending_approval(&mut app);
    assert_eq!(app.current_approval().map(|r| r.title.as_str()), Some("write_file"));
    assert!(app.has_pending_approval());

    // y/a/n route to the matching decision.
    assert_eq!(
        app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE),
        Some(Action::Approve(ApprovalDecision::AllowOnce))
    );
    assert_eq!(
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE),
        Some(Action::Approve(ApprovalDecision::AllowAlways))
    );
    assert_eq!(
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE),
        Some(Action::Approve(ApprovalDecision::Deny))
    );

    // Other keys (including Enter/submit) are swallowed while a popup is up.
    assert_eq!(app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE), None);
    assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);

    // Ctrl+C cancels even when a popup is up (and no turn is running).
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Cancel)
    );
}

#[test]
fn approve_front_pops_and_sends() {
    let mut app = App::new();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    app.approval_queue.push_back(ApprovalItem {
        request: ApprovalRequest {
            title: "t".to_string(),
            message: "m".to_string(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Safe,
            raw: None,
        },
        decision_tx: tx,
    });

    app.approve_front(ApprovalDecision::Deny);
    assert!(!app.has_pending_approval());
    assert_eq!(rx.try_recv().unwrap(), ApprovalDecision::Deny);
}

#[test]
fn awaiting_approval_sets_status() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::AwaitingApproval {
        session_id: SessionId::new(1),
        request: ApprovalRequest {
            title: "write_file".to_string(),
            message: "Write file: src/lib.rs".to_string(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Sensitive,
            raw: None,
        },
        agent_id: None,
        trace_id: None,
    }));
    assert_eq!(
        app.status,
        AgentStatus::Running { phase: Phase::AwaitingApproval }
    );
}

#[test]
fn sub_agent_text_is_labeled_and_lifecycle_tracked() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "found a thing")));
    // Flush to commit text
    app.flush_pending();
    assert_eq!(app.sub_agents.get("root/a").map(|s| &s.status), Some(&SubAgentStatus::Running));
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
    assert_eq!(app.sub_agents.get("root/a").map(|s| &s.status), Some(&SubAgentStatus::Done));
    // Main transcript should only have the "started" marker
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "⏺ [root/a] started",
        ]
    );
    // Check sub-agent transcript
    let sub_texts: Vec<&str> = app.sub_agent_transcripts.get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    // Sub-agent transcript should have the text and done marker
    assert!(sub_texts.iter().any(|t| t.contains("found a thing")));
    assert!(sub_texts.iter().any(|t| t.contains("✓ [root/a] done")));
}

#[test]
fn sub_agent_tool_calls_are_labeled() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_tool_started("root/a", "read_file")));
    app.handle_event(TuiEvent::Runtime(child_tool_finished("root/a", "read_file")));
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "⏺ [root/a] started",
        ]
    );
    // Check sub-agent transcript
    let sub_texts: Vec<&str> = app.sub_agent_transcripts.get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        sub_texts,
        vec![
            "⏺ [root/a] read_file {}",
            "  [root/a] ✓ read_file done",
        ]
    );
}

#[test]
fn sub_agents_cleared_on_root_finish() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "hi")));
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
    assert!(!app.sub_agents.is_empty());
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert!(app.sub_agents.is_empty());
}

#[test]
fn sub_agent_run_finished_does_not_end_turn() {
    let mut app = App::new();
    app.running = true;
    // Root activity establishes the status...
    app.handle_event(TuiEvent::Runtime(text("working")));
    assert!(matches!(app.status, AgentStatus::Running { .. }));
    // ...child streaming and its RunFinished must leave it untouched.
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "hi")));
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
    assert!(app.running);
    assert!(matches!(app.status, AgentStatus::Running { .. }));
    assert_eq!(app.sub_agents.get("root/a").map(|s| &s.status), Some(&SubAgentStatus::Done));
}

#[test]
fn agent_prefix_labels_sub_agents_only() {
    assert_eq!(agent_prefix(None), "");
    assert_eq!(agent_prefix(Some("")), "");
    assert_eq!(agent_prefix(Some("root/a")), "[root/a] ");
}

// ── Child streams stay out of the main view (session 20260904_3eeb5610) ──
//
// A minutes-long child run streamed its TextDelta/ThoughtDelta into the
// shared stream buffer, whose pending tail IS the main view's live tail —
// the user watched it churn at ~10 events/sec. Child deltas must accumulate
// per-child (visible in the child's focus view), never in the main tail.

#[test]
fn child_deltas_bypass_main_stream_tail() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_thought("root/a", "thinking hard")));
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "partial prose")));
    // Main live tail: untouched by child deltas.
    assert!(app.streaming_tail_raw().is_none());
    assert!(!app.stream.has_pending_text());
    assert!(!app.stream.has_pending_thought());
    // Main transcript: only the started marker.
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, vec!["⏺ [root/a] started"]);
    // Child content is preserved, committed ahead of the done marker.
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
    let sub_texts: Vec<&str> = app.sub_agent_transcripts.get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    let done_idx = sub_texts.iter().position(|t| t.contains("done")).unwrap();
    assert!(
        sub_texts[..done_idx].iter().any(|t| t.contains("thinking hard")),
        "child thought must reach the child transcript: {sub_texts:?}"
    );
    assert!(
        sub_texts[..done_idx].iter().any(|t| t.contains("partial prose")),
        "child text must reach the child transcript: {sub_texts:?}"
    );
}

#[test]
fn child_stream_flushes_before_its_tool_line() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "read the config first")));
    app.handle_event(TuiEvent::Runtime(child_tool_started("root/a", "read_file")));
    let sub_texts: Vec<&str> = app.sub_agent_transcripts.get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    // Pending child text commits above the invocation line, in order.
    assert!(
        sub_texts.len() >= 2
            && sub_texts[0].contains("read the config first")
            && sub_texts[1].starts_with("⏺ [root/a] read_file"),
        "child text must precede its tool line: {sub_texts:?}"
    );
    assert!(app.streaming_tail_raw().is_none());
}

#[test]
fn root_tail_survives_child_interleaving() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(text("hel")));
    // First sight of the child flushes root pending text (started-marker
    // ordering); child deltas themselves must never join the root buffer.
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "child chunk")));
    app.handle_event(TuiEvent::Runtime(text("lo")));
    // Root tail: only root prose — no child content mixed in.
    let (raw, kind) = app.streaming_tail_raw().expect("root tail present");
    assert_eq!(raw, "lo");
    assert_eq!(kind, LineKind::Normal);
    // "hel" was committed whole by the started marker, unchopped.
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert!(texts.contains(&"hel"), "root text flushed intact: {texts:?}");
    assert!(
        texts.iter().all(|t| !t.contains("child chunk")),
        "child content must stay out of the main transcript: {texts:?}"
    );
}

#[test]
fn cleanup_completed_agents_drops_child_stream() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "some prose")));
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
    assert!(app.child_streams.contains_key("root/a"));
    // Backdate past the 3s reap delay, then reap from Idle.
    app.status = AgentStatus::Idle;
    app.sub_agents.get_mut("root/a").unwrap().completed_at =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(4));
    assert!(app.cleanup_completed_agents());
    assert!(!app.child_streams.contains_key("root/a"));
    assert!(!app.sub_agent_transcripts.contains_key("root/a"));
}

#[test]
fn last_reply_text_captures_reply_after_last_user() {
    let mut app = App::new();
    app.push_user("give me a url");
    app.handle_event(TuiEvent::Runtime(text("here: https://example.com/x")));
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert_eq!(app.last_reply_text(), "here: https://example.com/x");
}

#[test]
fn last_reply_text_skips_tools_and_stops_at_previous_user() {
    let mut app = App::new();
    app.push_user("turn one");
    app.handle_event(TuiEvent::Runtime(text("old answer")));
    app.handle_event(TuiEvent::Runtime(run_finished(None)));

    app.push_user("turn two");
    app.handle_event(TuiEvent::Runtime(text("part one")));
    app.handle_event(TuiEvent::Runtime(tool_started("verify")));
    app.handle_event(TuiEvent::Runtime(tool_finished("verify", false)));
    app.handle_event(TuiEvent::Runtime(text("part two")));
    app.handle_event(TuiEvent::Runtime(run_finished(None)));

    // Only turn two's prose; tool lines, results and the `✅ done` marker
    // are excluded, and the scan stops at the previous user turn.
    assert_eq!(app.last_reply_text(), "part one\npart two");
}

#[test]
fn last_reply_text_empty_without_reply() {
    let app = App::new();
    assert_eq!(app.last_reply_text(), "");
}

#[test]
fn ctrl_y_emits_copy_last_reply_and_plain_y_types() {
    let mut app = App::new();
    assert_eq!(
        app.handle_key(KeyCode::Char('y'), KeyModifiers::CONTROL),
        Some(Action::CopyLastReply)
    );
    // Plain 'y' (no Ctrl) still inserts a literal 'y'.
    assert_eq!(app.handle_key(KeyCode::Char('y'), KeyModifiers::NONE), None);
    assert_eq!(app.composer.text(), "y");
}

#[test]
fn notice_shows_in_status_line_and_clears_on_key() {
    let mut app = App::new();
    assert!(app.status_line().starts_with("⏸ Idle"));
    app.set_notice("📋 copied 5 chars");
    assert_eq!(app.status_line(), "📋 copied 5 chars");
    // Any keypress clears the notice before being handled.
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.status_line().starts_with("⏸ Idle"));
}

#[test]
fn plan_renders_status_markers() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(plan(
        "目标",
        vec![
            ("已完成", PlanStepStatus::Completed),
            ("进行中", PlanStepStatus::InProgress),
            ("待办", PlanStepStatus::Pending),
        ],
    )));
    let lines: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert!(lines.iter().any(|l| l.contains("📋 目标")), "got: {lines:?}");
    assert!(lines.iter().any(|l| l.contains("✅ 已完成")), "got: {lines:?}");
    assert!(lines.iter().any(|l| l.contains("🔄 进行中")), "got: {lines:?}");
    assert!(lines.iter().any(|l| l.contains("○ 待办")), "got: {lines:?}");
}

#[test]
fn plan_replaces_in_place_instead_of_appending() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(plan(
        "目标",
        vec![("步骤1", PlanStepStatus::Completed)],
    )));
    app.handle_event(TuiEvent::Runtime(plan(
        "目标",
        vec![
            ("步骤1", PlanStepStatus::Completed),
            ("步骤2", PlanStepStatus::Completed),
        ],
    )));
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts.iter().filter(|t| t.contains("📋 目标")).count(),
        1,
        "plan replaced, not duplicated: {texts:?}"
    );
    assert_eq!(texts.iter().filter(|t| t.contains("✅ 步骤1")).count(), 1);
    assert_eq!(texts.iter().filter(|t| t.contains("✅ 步骤2")).count(), 1);
}

#[test]
fn plan_renders_explanation() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(plan_ex(
        "目标",
        Some("检测到已有配置，直接复用"),
        vec![("步骤", PlanStepStatus::Pending)],
    )));
    assert!(app.transcript.output.iter().any(|l| l.text.contains("↳ 检测到已有配置")));
}

#[test]
fn update_plan_tool_result_line_is_suppressed() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(tool_started("update_plan")));
    app.handle_event(TuiEvent::Runtime(tool_finished("update_plan", false)));
    assert!(!app.transcript.output.iter().any(|l| l.text.contains("update_plan")));
}

#[test]
fn plan_resets_across_turns() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(plan(
        "目标A",
        vec![("步骤", PlanStepStatus::Pending)],
    )));
    // Starting a new turn (submit) clears the tracked plan range, so the next
    // plan appends instead of replacing the previous turn's plan.
    assert_eq!(submit(&mut app, "next"), Action::Submit("next".to_string()));
    app.handle_event(TuiEvent::Runtime(plan(
        "目标B",
        vec![("步骤B", PlanStepStatus::Pending)],
    )));
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("📋 目标A")), "target A kept: {texts:?}");
    assert!(texts.iter().any(|t| t.contains("📋 目标B")), "target B appended: {texts:?}");
    assert_eq!(texts.iter().filter(|t| t.contains("📋")).count(), 2);
}

#[test]
fn mouse_drag_selects_line_range() {
    let mut app = App::new();
    for i in 0..10 {
        app.transcript.push(OutputLine { spans: None, original: None, text: format!("line {i}"), kind: LineKind::Normal, detail: None });
    }
    app.output_area = Some((0, 0, 100, 10));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2, 100, 10);
    app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 0, 5, 100, 10);
    assert_eq!(app.selection_state.selection, Some(Selection { anchor: 2, head: 5 }));
    assert_eq!(app.selection_text(), "line 2\nline 3\nline 4\nline 5");
}

#[test]
fn mouse_drag_up_normalizes_selection() {
    let mut app = App::new();
    for i in 0..10 {
        app.transcript.push(OutputLine { spans: None, original: None, text: format!("line {i}"), kind: LineKind::Normal, detail: None });
    }
    app.output_area = Some((0, 0, 100, 10));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 5, 100, 10);
    app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 0, 2, 100, 10);
    assert!(app.is_selected(3));
    assert_eq!(app.selection_text(), "line 2\nline 3\nline 4\nline 5");
}

#[test]
fn click_outside_output_clears_selection() {
    let mut app = App::new();
    app.transcript.push(OutputLine { spans: None, original: None, text: "x".into(), kind: LineKind::Normal, detail: None });
    app.output_area = Some((0, 0, 10, 5));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 10, 5);
    assert!(app.selection_state.selection.is_some());
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 50, 10, 5); // below pane
    assert!(app.selection_state.selection.is_none());
}

#[test]
fn selection_text_clamps_stale_indices() {
    let mut app = App::new();
    app.transcript.push(OutputLine { spans: None, original: None, text: "a".into(), kind: LineKind::Normal, detail: None });
    app.selection_state.selection = Some(Selection { anchor: 0, head: 5 });
    assert_eq!(app.selection_text(), "a");
}

#[test]
fn ctrl_c_copies_selection_then_double_press_quits() {
    let mut app = App::new();
    // With selection → copy (caller clears selection after copy).
    app.selection_state.selection = Some(Selection { anchor: 0, head: 5 });
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::CopySelection)
    );
    // Caller clears selection after copy. First Ctrl+C → show hint.
    app.selection_state.selection = None;
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        None
    );
    assert!(app.notice.as_deref().unwrap_or("").contains("Ctrl+C"));
    // Second Ctrl+C within timeout → quit.
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Quit)
    );
    // After timeout, hint resets: first press → hint again.
    app.quit_hint_at =
        Some(Instant::now() - std::time::Duration::from_secs(5));
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        None
    );
    // While running → cancel (no hint).
    app.running = true;
    app.quit_hint_at = None;
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Cancel)
    );
}

#[test]
fn right_click_opens_menu_and_enter_copies() {
    let mut app = App::new();
    app.transcript.push(OutputLine { spans: None, original: None, text: "x".into(), kind: LineKind::Normal, detail: None });
    app.output_area = Some((0, 0, 10, 10));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 20, 20);
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 3, 4, 20, 20);
    assert_eq!(app.selection_state.context_menu, Some(ContextMenu { x: 3, y: 4, selected: 0 }));
    assert_eq!(
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(Action::CopySelection)
    );
    assert_eq!(app.selection_state.context_menu, None);
}

#[test]
fn context_menu_arrows_move_highlight_and_esc_closes() {
    let mut app = App::new();
    app.transcript.push(OutputLine { spans: None, original: None, text: "x".into(), kind: LineKind::Normal, detail: None });
    app.output_area = Some((0, 0, 10, 10));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 20, 20);
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
    assert_eq!(app.handle_key(KeyCode::Down, KeyModifiers::NONE), None);
    assert_eq!(app.selection_state.context_menu.as_ref().unwrap().selected, 1);
    // Enter on the "cancel" item closes without copying.
    assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert!(app.selection_state.context_menu.is_none());
}

#[test]
fn clicking_menu_copy_item_copies_and_closes() {
    let mut app = App::new();
    app.transcript.push(OutputLine { spans: None, original: None, text: "x".into(), kind: LineKind::Normal, detail: None });
    app.output_area = Some((0, 0, 10, 10));
    app.selection_state.selection = Some(Selection { anchor: 0, head: 0 });
    // Open the menu at (0,0): 12×4 box, items at rows y+1 ("拷贝") and y+2
    // ("取消").
    assert_eq!(
        app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20),
        None
    );
    assert!(app.selection_state.context_menu.is_some());
    // Left-click the "拷贝" row → copy, menu closes, selection preserved.
    assert_eq!(
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 1, 1, 20, 20),
        Some(Action::CopySelection)
    );
    assert!(app.selection_state.context_menu.is_none());
    assert!(app.selection_state.selection.is_some());
}

#[test]
fn clicking_menu_cancel_item_closes_without_copy() {
    let mut app = App::new();
    app.transcript.push(OutputLine { spans: None, original: None, text: "x".into(), kind: LineKind::Normal, detail: None });
    app.output_area = Some((0, 0, 10, 10));
    app.selection_state.selection = Some(Selection { anchor: 0, head: 0 });
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
    // "取消" is the second item, row y+2. It closes the menu but keeps the
    // selection.
    assert_eq!(
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 1, 2, 20, 20),
        None
    );
    assert!(app.selection_state.context_menu.is_none());
    assert!(app.selection_state.selection.is_some());
}

#[test]
fn clicking_outside_menu_closes_it_and_restarts_selection() {
    let mut app = App::new();
    app.transcript.push(OutputLine { spans: None, original: None, text: "x".into(), kind: LineKind::Normal, detail: None });
    app.output_area = Some((0, 0, 10, 10));
    app.selection_state.selection = Some(Selection { anchor: 0, head: 0 });
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
    assert!(app.selection_state.context_menu.is_some());
    // A click far outside the popup closes the menu (and starts a new
    // selection) exactly as before.
    assert_eq!(
        app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 15, 15, 20, 20),
        None
    );
    assert!(app.selection_state.context_menu.is_none());
}

#[test]
fn esc_clears_selection_before_composer() {
    let mut app = App::new();
    app.selection_state.selection = Some(Selection { anchor: 0, head: 2 });
    app.composer.insert_str("keep");
    assert_eq!(app.handle_key(KeyCode::Esc, KeyModifiers::NONE), None);
    assert!(app.selection_state.selection.is_none());
    assert_eq!(app.composer.text(), "keep");
}

#[test]
fn cmd_c_copies_selection_but_never_quits() {
    let mut app = App::new();
    // No selection → Cmd+C does nothing (it must never quit).
    assert_eq!(app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER), None);
    // With a selection → copy.
    app.selection_state.selection = Some(Selection { anchor: 0, head: 0 });
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER),
        Some(Action::CopySelection)
    );
}

fn mention_scratch(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "phimint-app-mention-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn at_opens_mention_picker_listing_root() {
    let mut app = App::new();
    let root = mention_scratch("open");
    std::fs::write(root.join("a.txt"), "x").unwrap();
    app.set_workspace_root(root.clone());

    assert_eq!(app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE), None);
    let m = app.mention().expect("mention open");
    assert_eq!(m.prefix(), "");
    assert!(m.entries()[0].synthetic, "synthetic row is first");
    assert!(m.entries().iter().any(|e| e.name == "a.txt"));
    assert_eq!(app.composer.text(), "@");
}

#[test]
fn enter_on_empty_prefix_inserts_dot_and_closes() {
    let mut app = App::new();
    let root = mention_scratch("enter-empty");
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.mention().is_none());
    assert_eq!(app.composer.text(), ".");
}

#[test]
fn dotdot_prefix_inserts_absolute_parent_path() {
    let mut app = App::new();
    let root = mention_scratch("dotdot-app");
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    for c in "../".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    assert!(app.mention().is_some(), "picker stays open while typing");
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    let text = app.composer.text();
    assert!(!text.contains('@'), "composer holds the path, got {text:?}");
    assert!(text.starts_with('/'), "outside path is absolute, got {text:?}");
}

#[test]
fn typing_name_then_arrow_selects_file() {
    let mut app = App::new();
    let root = mention_scratch("select");
    std::fs::write(root.join("main.rs"), "x").unwrap();
    std::fs::write(root.join("other.rs"), "x").unwrap();
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    for c in "main".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    // Arrow down off the synthetic row onto the single real match, accept it.
    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.composer.text(), "main.rs");
}

#[test]
fn esc_cancels_mention_removing_at_and_prefix() {
    let mut app = App::new();
    let root = mention_scratch("esc");
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    for c in "src".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mention().is_none());
    assert_eq!(app.composer.text(), "");
}

#[test]
fn backspace_with_empty_prefix_cancels_mention() {
    let mut app = App::new();
    let root = mention_scratch("bsp");
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
    assert!(app.mention().is_none());
    assert_eq!(app.composer.text(), "");
}

#[test]
fn mention_inserts_after_existing_text() {
    let mut app = App::new();
    let root = mention_scratch("mid");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    app.set_workspace_root(root.clone());

    app.composer.insert_str("read ");
    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    for c in "sub".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.composer.text(), "read sub");
}

#[test]
fn paste_routes_to_mention_prefix_when_open() {
    let mut app = App::new();
    let root = mention_scratch("paste");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.paste("sub");
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.composer.text(), "sub");
}

// ── / skill picker tests ──

fn app_with_skills() -> App {
    let mut app = App::new();
    // 排序后: commit(0), requesting-code-review(1), review(2)
    app.set_skill_summaries(vec![
        ("commit".into(), "Generate a commit message".into()),
        ("requesting-code-review".into(), "Request a code review".into()),
        ("review".into(), "Pre-landing PR review".into()),
    ]);
    app
}

#[test]
fn slash_opens_picker_on_empty_composer() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(app.slash().is_some(), "slash picker should open");
    assert_eq!(app.composer.text(), "/");
}

#[test]
fn slash_does_not_open_when_composer_not_empty() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(app.slash().is_none(), "slash picker should not open mid-input");
}

#[test]
fn slash_filters_by_prefix() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);

    let s = app.slash().unwrap();
    let names: Vec<&str> = s.entries().iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"commit"));
    assert!(names.contains(&"requesting-code-review"));
    // "review" does not contain 'c' → filtered out
    assert!(!names.contains(&"review"));
}

#[test]
fn slash_enter_confirms_selection() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    // 默认选中第一个（排序后是 "commit"）
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.slash().is_none(), "picker should close after Enter");
    assert_eq!(app.composer.text(), "/commit ", "should insert /name + space");
}

#[test]
fn slash_esc_cancels() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.slash().is_none(), "picker should close on Esc");
    assert!(app.composer.is_empty(), "composer should be empty after cancel");
}

#[test]
fn slash_arrow_keys_navigate() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);

    assert_eq!(app.slash().unwrap().selected_index(), 0);

    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.slash().unwrap().selected_index(), 1);

    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.slash().unwrap().selected_index(), 2);

    // 已在末尾，Down 不动
    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.slash().unwrap().selected_index(), 2);

    app.handle_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.slash().unwrap().selected_index(), 1);
}

// ── Diff feature tests ──

#[test]
fn edit_file_produces_diff_detail() {
    let mut app = App::new();
    let args = serde_json::json!({
        "path": "src/main.rs",
        "edits": [
            { "old_text": "fn main() {\n    println!(\"hello\");\n}", "new_text": "fn main() {\n    println!(\"world\");\n}" }
        ]
    });
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "edit_file".to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }));

    // The tool line should have a Diff detail attached
    let tool_line = app.transcript.output.last().expect("tool line present");
    assert_eq!(tool_line.kind, LineKind::Tool);
    match &tool_line.detail {
        Some(ToolDetail::Diff { path, hunks }) => {
            assert_eq!(path, "src/main.rs");
            assert!(!hunks.is_empty(), "should have diff hunks");
            // Should have a Del line and an Add line
            let all_lines: Vec<_> = hunks.iter().flat_map(|h| h.lines.iter()).collect();
            assert!(all_lines.iter().any(|l| l.kind == DiffLineKind::Del));
            assert!(all_lines.iter().any(|l| l.kind == DiffLineKind::Add));
        }
        other => panic!("expected ToolDetail::Diff, got {other:?}"),
    }
}

#[test]
fn write_file_produces_all_add_diff() {
    let mut app = App::new();
    let args = serde_json::json!({
        "path": "src/new.rs",
        "content": "fn hello() {\n    println!(\"hi\");\n}\n"
    });
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "write_file".to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }));

    let tool_line = app.transcript.output.last().expect("tool line present");
    match &tool_line.detail {
        Some(ToolDetail::Diff { path, hunks }) => {
            assert_eq!(path, "src/new.rs");
            let all_lines: Vec<_> = hunks.iter().flat_map(|h| h.lines.iter()).collect();
            assert!(all_lines.iter().all(|l| l.kind == DiffLineKind::Add),
                "write_file should produce all-Add lines");
        }
        other => panic!("expected ToolDetail::Diff, got {other:?}"),
    }
}

#[test]
fn non_file_tool_has_no_detail() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    let tool_line = app.transcript.output.last().expect("tool line present");
    assert!(tool_line.detail.is_none(), "non-file tools should have no detail");
}

#[test]
fn edit_file_diff_lines_appear_in_transcript() {
    let mut app = App::new();
    let args = serde_json::json!({
        "path": "src/lib.rs",
        "edits": [
            { "old_text": "old_fn()", "new_text": "new_fn()" }
        ]
    });
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "edit_file".to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }));

    // The tool line itself should be present
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert!(texts[0].contains("edit_file"));
}

// ── Fan-in waiting state (turn ends while sub-agents run) ──────────────────

#[test]
fn turn_done_with_running_children_enters_waiting_state() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "working")));
    app.handle_event(TuiEvent::Runtime(child_text("root/b", "working")));
    // Root finishes its turn but children are still out — root RunFinished
    // must keep the panel alive and flip to Waiting.
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert_eq!(app.sub_agents.len(), 2);
    assert!(matches!(app.status, AgentStatus::Waiting { running: 2 }));

    app.handle_event(TuiEvent::TurnDone);
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("⏳ 等待子 agent 返回（2 个运行中）")));
    assert!(!texts.iter().any(|t| t.contains("✅ done")), "waiting is not done");
    assert!(!app.running);
    assert!(matches!(app.status, AgentStatus::Waiting { running: 2 }));
    // Panel + per-agent transcripts stay alive for inspection while waiting.
    assert_eq!(app.sub_agents.len(), 2);
    assert!(app.sub_agent_transcripts.contains_key("root/a"));
}

#[test]
fn turn_done_without_running_children_shows_done_and_clears_panel() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "hi")));
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a")))); // child done first
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    app.handle_event(TuiEvent::TurnDone);
    let texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("✅ done")));
    assert_eq!(app.status, AgentStatus::Idle);
    assert!(app.sub_agents.is_empty());
}

#[test]
fn finishing_children_updates_waiting_count_to_idle_at_zero() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "x")));
    app.handle_event(TuiEvent::Runtime(child_text("root/b", "y")));
    app.handle_event(TuiEvent::TurnDone);
    assert!(matches!(app.status, AgentStatus::Waiting { running: 2 }));

    // Watcher Progress events flip panel entries and refresh the count.
    app.mark_sub_agent_finished("root/a");
    assert!(matches!(app.status, AgentStatus::Waiting { running: 1 }));
    assert_eq!(app.sub_agents.get("root/a").map(|s| &s.status), Some(&SubAgentStatus::Done));

    app.mark_sub_agent_finished("root/b");
    assert_eq!(app.status, AgentStatus::Idle);
}

#[test]
fn batch_inject_marks_all_children_finished() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "x")));
    app.handle_event(TuiEvent::Runtime(child_text("root/b", "y")));
    app.handle_event(TuiEvent::TurnDone);
    app.mark_all_sub_agents_finished();
    assert!(app.sub_agents.values().all(|s| s.status == SubAgentStatus::Done));
    assert_eq!(app.status, AgentStatus::Idle);
}

#[test]
fn waiting_status_line_mentions_running_count() {
    let mut app = App::new();
    app.status = AgentStatus::Waiting { running: 3 };
    let line = app.status_line();
    assert!(line.contains('3'), "count missing: {line}");
    assert!(line.contains("等待"), "waiting wording missing: {line}");
}
