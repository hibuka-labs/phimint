//! Tests for App state machine and event handling.

use super::*;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use phi_agent::SessionId;
use phi_agent::{PlanItem, PlanStepStatus, UserEvent};
use phi_tui::lines::{DiffLineKind, LineDetail, MetaHead, ToolState};

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
    assert_eq!(
        app.status,
        AgentStatus::Running {
            phase: Phase::Thinking
        }
    );

    app.handle_event(TuiEvent::Runtime(text("answer")));
    assert_eq!(
        app.status,
        AgentStatus::Running {
            phase: Phase::Streaming
        }
    );

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
    assert_eq!(
        app.status,
        AgentStatus::Running {
            phase: Phase::Thinking
        }
    );

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
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(
        texts,
        vec![
            // No args to name → bare tool name; the `*` is the marker slot the
            // renderer fills from `tool_state`.
            "* read_file",
            // Adjacent to its own call → the tool name is already one line up.
            "  < done",
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
    // The root run is over, but `running` is cleared only by the turn
    // settling (TurnDone/TurnError) — never by a late RunFinished, which
    // would unlock the composer while the next turn is already queued.
    assert!(app.running, "only settle_after_turn clears the flag");
    assert_eq!(app.status, AgentStatus::Idle);
    app.handle_event(TuiEvent::TurnDone);
    assert!(!app.running, "settling the turn clears the flag");
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
        .transcript
        .output
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
        .transcript
        .output
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

    // Non-empty → Submit. The `running` flag is raised by the *send* side
    // (run.rs, right after `Cmd::Run` is queued), not by key handling — so
    // intercepted submits (/resume, /upgrade) can never leave the composer
    // locked with the flag up and no run in flight.
    let action = submit(&mut app, "do a thing");
    assert_eq!(action, Action::Submit("do a thing".to_string()));

    // Already running → Enter ignored (the send side raised the flag).
    app.running = true;
    app.composer.insert_str("second");
    assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
}

/// Regression (user report 2026-09-16, round 3): after resuming, the user
/// reads the history scrolled up; on submit the reply must stream into
/// view, not land below the fold ("no response after typing" was the reply being
/// rendered off-screen while the viewport stayed pinned at the head).
#[test]
fn submit_scrolls_back_to_bottom_so_reply_is_visible() {
    let mut app = App::new();
    for i in 0..100 {
        app.push_system(&format!("history line {i}"));
    }
    // Scroll up into the history (as after reading a resumed conversation).
    assert!(app.scroll_up());
    assert!(!app.viewport.follow_bottom, "precondition: scrolled up");

    let action = submit(&mut app, "好的");
    assert!(matches!(action, Action::Submit(_)));

    // Submit re-enters follow-bottom: the newest lines (the user echo now,
    // the reply as it streams) are what's on screen.
    assert!(app.viewport.follow_bottom, "submit must follow the bottom");
    assert!(!app.viewport.pin_top);
    let total = app.transcript.len();
    assert_eq!(
        app.viewport.window_range(total, 40),
        total.saturating_sub(40)..total,
        "the reply lands on screen, not below the fold"
    );
}

/// One PageUp/PageDown press moves half a screen, not one line: a resumed
/// conversation runs to hundreds of lines, and 1-line paging made the
/// history effectively unnavigable ("only the first screen shows").
#[test]
fn pagedown_pages_half_a_screen() {
    let mut app = App::new();
    for i in 0..200 {
        app.push_system(&format!("history line {i}"));
    }
    let total = app.transcript.len();
    assert!(
        total > 60,
        "precondition: enough content to page (got {total})"
    );
    // Emulate a rendered frame so the viewport knows its geometry.
    app.viewport.viewport_height = 40;
    app.viewport.rendered_total = total;
    assert_eq!(app.viewport.page_step(), 20, "half of the 40-row viewport");

    // Fresh boot follows the bottom: PgDn there is a no-op.
    assert!(!app.scroll_down(), "already at the bottom: no movement");

    // One PgUp press leaves follow-bottom and jumps a full half-screen.
    assert!(app.scroll_up());
    assert!(!app.viewport.follow_bottom);
    let range = app.viewport.window_range(total, 40);
    assert_eq!(range.start, total - 40 - 20, "one PgUp = half a screen");

    // PgDn walks back down and re-enters follow at the bottom.
    assert!(app.scroll_down());
    assert!(app.viewport.follow_bottom);
    assert_eq!(
        app.viewport.window_range(total, 40),
        total.saturating_sub(40)..total,
        "back at the newest lines"
    );
}

#[test]
fn submit_echoes_user_message() {
    let mut app = App::new();
    let action = submit(&mut app, "hello world");
    assert_eq!(action, Action::Submit("hello world".to_string()));
    let users: Vec<&str> = app
        .transcript
        .output
        .iter()
        .filter(|l| l.kind == LineKind::User)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(users, vec!["> hello world"]);
}

#[test]
fn progress_updates_and_clears_live_progress() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(tool_started("execute_command")));
    // The status line now carries a spinner frame + elapsed readout (the
    // liveness tick redraws it); assert the stable parts.
    let line = app.status_line();
    assert!(line.contains("🔧 execute_command"), "got: {line}");
    assert!(line.contains("(Ctrl+C cancel)"), "got: {line}");

    let prog = RuntimeEvent::UserEvent {
        session_id: SessionId::new(1),
        event: UserEvent::Progress {
            text: "Compiling phimint v0.1.0".to_string(),
        },
        agent_id: None,
        trace_id: None,
    };
    app.handle_event(TuiEvent::Runtime(prog));
    let line = app.status_line();
    assert!(
        line.contains("🔧 execute_command: Compiling phimint v0.1.0"),
        "got: {line}"
    );

    // A new tool call drops the previous tool's live progress.
    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    let line = app.status_line();
    assert!(line.contains("🔧 read_file"), "got: {line}");
    assert!(
        !line.contains("Compiling"),
        "stale progress must drop, got: {line}"
    );
}

#[test]
fn spawn_agent_invocation_line_is_compacted() {
    let mut app = App::new();
    let args = serde_json::json!({
        "task_name": "analyze-pi",
        "task": "分析 /Users/me/pi 工程的 agent 循环\n然后还要看工具系统\n报告带文件行号"
    })
    .to_string();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "spawn_agent".to_string(),
        args_json: args,
        agent_id: None,
        trace_id: None,
    }));
    let last = app.transcript.output.last().unwrap();
    assert!(
        last.text
            .starts_with("* spawn_agent analyze-pi - 分析 /Users/me/pi 工程的 agent 循环"),
        "got: {}",
        last.text
    );
    assert!(
        !last.text.contains("然后还要看"),
        "must not dump the full task"
    );
}

#[test]
fn activity_clock_follows_root_status() {
    let mut app = App::new();
    assert!(!app.is_active());
    assert_eq!(app.spinner_char(), "⠋"); // idle: static first frame
    assert_eq!(app.elapsed_suffix(), "");

    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    assert!(app.is_active());
    assert!(app.activity_since.is_some());
    let line = app.status_line();
    assert!(
        line.contains(" - "),
        "running status carries elapsed, got: {line}"
    );

    // Turn settles with no children → Idle clears the clock.
    app.settle_after_turn(false);
    assert!(!app.is_active());
    assert!(app.activity_since.is_none());
    assert_eq!(app.spinner_char(), "⠋");
}

#[test]
fn submit_starts_activity_clock_and_keeps_origin() {
    let mut app = App::new();
    assert!(app.activity_since.is_none(), "idle: no stretch");

    let _ = submit(&mut app, "hello");
    // The stretch's clock starts at the keystroke, not at the engine's first
    // event: a model that thinks for half a minute before its first delta must
    // not render a frozen spinner with no elapsed readout (session
    // 20261006_9264ba3e).
    let origin = app.activity_since.expect("submit must start the clock");

    // A phase change inside the stretch keeps its origin — the elapsed readout
    // measures the whole stretch, not the time since the last event.
    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    assert_eq!(
        app.activity_since,
        Some(origin),
        "tool events must not restart the stretch clock"
    );
}

fn draft(index: usize, name: &str, args_len: usize, count: usize) -> RuntimeEvent {
    RuntimeEvent::ToolCallDraft {
        session_id: SessionId::new(1),
        index,
        name: name.to_string(),
        args_len,
        count,
        agent_id: None,
        trace_id: None,
    }
}

#[test]
fn tool_call_drafts_narrate_writing_and_clear_when_calls_materialize() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(draft(0, "spawn_agent", 881, 1)));
    assert!(app.is_active(), "a draft is a busy signal");
    let line = app.status_line();
    assert!(
        line.contains("writing spawn_agent (881 chars)..."),
        "got: {line}"
    );

    // A second parallel draft → plural form, summed chars.
    app.handle_event(TuiEvent::Runtime(draft(1, "spawn_agent", 400, 2)));
    let line = app.status_line();
    assert!(
        line.contains("writing 2 tool calls (1281 chars)..."),
        "got: {line}"
    );

    // The calls materializing (stream drained) ends the drafting window.
    app.handle_event(TuiEvent::Runtime(tool_started("spawn_agent")));
    assert!(
        app.tool_drafts.is_empty(),
        "materialized calls clear drafts"
    );
    let line = app.status_line();
    assert!(!line.contains("writing "), "got: {line}");
}

#[test]
fn tool_call_drafts_clear_on_turn_end() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(draft(0, "spawn_agent", 12, 1)));
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert!(
        app.tool_drafts.is_empty(),
        "a finished turn can draft no more"
    );
}

#[test]
fn child_tool_drafts_never_touch_root_state() {
    let mut app = App::new();
    // A child's draft belongs to the task panel's liveness, not the root
    // status strip — and must not flip the root to Running.
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallDraft {
        session_id: SessionId::new(1),
        index: 0,
        name: "read_file".to_string(),
        args_len: 64,
        count: 1,
        agent_id: Some("root/child".to_string()),
        trace_id: None,
    }));
    assert!(app.tool_drafts.is_empty());
    assert_eq!(app.status, AgentStatus::Idle);

    // A root draft survives a child's call materializing (the root's calls
    // materialize together only after the root's own stream drains).
    app.handle_event(TuiEvent::Runtime(draft(0, "spawn_agent", 32, 1)));
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "read_file".to_string(),
        args_json: "{}".to_string(),
        agent_id: Some("root/child".to_string()),
        trace_id: None,
    }));
    assert_eq!(
        app.tool_drafts.len(),
        1,
        "child materialize must not clear root drafts"
    );
}

#[test]
fn spawn_task_brief_rides_the_preview_ladder() {
    let mut app = App::new();
    // Short first line → the meta row shows it whole; the fold's body owns
    // rows[1..], and the full brief is whole on the ladder.
    let task = "看一眼 pi 的 agent 循环\n然后还要看工具系统\n报告带文件行号";
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "spawn_agent".to_string(),
        args_json: serde_json::json!({"task_name": "analyze-pi", "task": task}).to_string(),
        agent_id: None,
        trace_id: None,
    }));
    let last = app.transcript.output.last().unwrap();
    assert!(
        last.text.starts_with("* spawn_agent analyze-pi - "),
        "got: {}",
        last.text
    );
    match &last.detail {
        Some(LineDetail::Folded { raw, meta_head, .. }) => {
            assert_eq!(raw, task, "the full brief rides the ladder, whole");
            assert_eq!(*meta_head, MetaHead::Whole);
        }
        other => panic!("expected Folded brief, got {other:?}"),
    }
}

#[test]
fn spawn_task_head_is_abbreviated_when_the_first_line_is_long() {
    let mut app = App::new();
    // A first line past the gist budget is truncated in the meta row; the
    // ladder restores the full head at full expansion (Abbreviated), so the
    // truncation never loses text.
    let head = "分析".repeat(40); // 160 display cols > the 56-col gist
    let task = format!("{head}\n第二行");
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "spawn_agent".to_string(),
        args_json: serde_json::json!({"task_name": "analyze-pi", "task": task}).to_string(),
        agent_id: None,
        trace_id: None,
    }));
    let last = app.transcript.output.last().unwrap();
    assert!(
        !last.text.contains(&head),
        "meta row must not dump the head, got: {}",
        last.text
    );
    match &last.detail {
        Some(LineDetail::Folded { meta_head, .. }) => {
            assert_eq!(*meta_head, MetaHead::Abbreviated);
        }
        other => panic!("expected Folded brief, got {other:?}"),
    }
}

#[test]
fn spawn_result_is_humanized_with_raw_preserved() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "spawn_agent".to_string(),
        args_json: serde_json::json!({"task_name": "analyze-pi", "task": "看看 pi"}).to_string(),
        agent_id: None,
        trace_id: None,
    }));
    let summary = serde_json::json!({
        "agent_path": "root/analyze-pi",
        "message": "Agent spawned successfully (tools: read_only; registered: read_file, grep)"
    })
    .to_string();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: "spawn_agent".to_string(),
        summary: summary.clone(),
        agent_id: None,
        trace_id: None,
        denied: false,
        details: None,
    }));
    let last = app.transcript.output.last().unwrap();
    assert_eq!(last.text, "  < spawned root/analyze-pi (read_only)");
    match &last.detail {
        Some(LineDetail::Folded { raw, meta_head, .. }) => {
            assert_eq!(
                raw, &summary,
                "summary is display-only — raw rides the fold, never rewritten"
            );
            assert_eq!(
                *meta_head,
                MetaHead::None,
                "the label is a fact, not the payload's head"
            );
        }
        other => panic!("expected Folded result, got {other:?}"),
    }
}

#[test]
fn spawn_result_label_keeps_recycled_and_degraded_facts() {
    let mut app = App::new();
    let spawn = |app: &mut App| {
        app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
            session_id: SessionId::new(1),
            tool_name: "spawn_agent".to_string(),
            args_json: serde_json::json!({"task_name": "t", "task": "x"}).to_string(),
            agent_id: None,
            trace_id: None,
        }));
    };
    let finish = |app: &mut App, message: &str| {
        app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallFinished {
            session_id: SessionId::new(1),
            tool_name: "spawn_agent".to_string(),
            summary: serde_json::json!({"agent_path": "root/t", "message": message}).to_string(),
            agent_id: None,
            trace_id: None,
            denied: false,
            details: None,
        }));
        app.transcript.output.last().unwrap().text.clone()
    };

    spawn(&mut app);
    let text = finish(
        &mut app,
        "Agent spawned successfully (recycled a finished agent with the same path)",
    );
    assert_eq!(text, "  < spawned root/t (recycled)", "got: {text}");

    spawn(&mut app);
    let text = finish(
        &mut app,
        "Agent spawned successfully (tools degraded to read-only: sandbox policy)",
    );
    assert_eq!(
        text, "  < spawned root/t (degraded to read-only)",
        "got: {text}"
    );
}

#[test]
fn write_file_content_head_is_never_restored() {
    let mut app = App::new();
    // A long first line wraps at display time; `MetaHead::None` is what keeps
    // the ladder from re-drawing that head on top of itself at full expansion
    // — the meta row is `* write_file <path>`, a label, never the content.
    let content = "fn main() { /* a deliberately long first line that wraps on any terminal and must not be drawn twice when the fold expands */ }\nsecond line\n";
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "write_file".to_string(),
        args_json: serde_json::json!({"path": "/tmp/x.rs", "content": content}).to_string(),
        agent_id: None,
        trace_id: None,
    }));
    let last = app.transcript.output.last().unwrap();
    match &last.detail {
        Some(LineDetail::Folded { raw, meta_head, .. }) => {
            assert_eq!(raw, content);
            assert_eq!(*meta_head, MetaHead::None);
        }
        other => panic!("expected Folded content, got {other:?}"),
    }
}

#[test]
fn elapsed_suffix_formats_minutes() {
    let mut app = App::new();
    app.activity_since = Some(Instant::now() - std::time::Duration::from_secs(91));
    assert_eq!(app.elapsed_suffix(), " - 1m32s");
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
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
            detail: None,
            tool_state: None,
        });
    }
    app.scroll_up();
    assert!(!app.viewport.follow_bottom);
    assert_eq!(app.viewport.scroll_offset, app.viewport.page_step());
}

#[test]
fn scroll_down_reenters_follow_bottom() {
    let mut app = App::new();
    for i in 0..100 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
            detail: None,
            tool_state: None,
        });
    }
    let step = app.viewport.page_step();
    app.scroll_up();
    app.scroll_up();
    assert_eq!(app.viewport.scroll_offset, 2 * step);
    app.scroll_down();
    assert_eq!(app.viewport.scroll_offset, step);
    app.viewport.scroll_offset = step;
    app.scroll_down();
    assert!(app.viewport.follow_bottom);
    assert_eq!(app.viewport.scroll_offset, 0);
}

#[test]
fn wheel_step_is_small_so_a_swipe_composes_smoothly() {
    let mut app = App::new();
    for i in 0..100 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
            detail: None,
            tool_state: None,
        });
    }
    // A few wheel ticks walk a few lines each — not half a screen per tick.
    app.scroll_wheel_up();
    app.scroll_wheel_up();
    assert!(!app.viewport.follow_bottom);
    assert_eq!(app.viewport.scroll_offset, 2 * WHEEL_STEP);
    // Wheeling back down re-enters follow-bottom exactly at the tail.
    for _ in 0..=WHEEL_STEP {
        app.scroll_wheel_down();
    }
    assert!(app.viewport.follow_bottom);
    assert_eq!(app.viewport.scroll_offset, 0);
}

#[test]
fn keyboard_page_step_stays_half_screen_despite_wheel_step() {
    let mut app = App::new();
    for i in 0..100 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
            detail: None,
            tool_state: None,
        });
    }
    app.viewport.set_visible(1000, 40); // a real render: 40-row pane
    app.scroll_up();
    assert_eq!(app.viewport.scroll_offset, app.viewport.page_step());
    assert!(app.viewport.page_step() > WHEEL_STEP);
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
            source: None,
        },
        decision_tx: tx,
    });
}

#[test]
fn approval_popup_routes_yan_and_swallows_others() {
    let mut app = App::new();
    pending_approval(&mut app);
    assert_eq!(
        app.current_approval().map(|r| r.title.as_str()),
        Some("write_file")
    );
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
            source: None,
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
            source: None,
        },
        agent_id: None,
        trace_id: None,
    }));
    assert_eq!(
        app.status,
        AgentStatus::Running {
            phase: Phase::AwaitingApproval
        }
    );
}

#[test]
fn sub_agent_text_is_labeled_and_lifecycle_tracked() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "found a thing")));
    // Flush to commit text
    app.flush_pending();
    assert_eq!(
        app.sub_agents.get("root/a").map(|s| &s.status),
        Some(&SubAgentStatus::Running)
    );
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
    assert_eq!(
        app.sub_agents.get("root/a").map(|s| &s.status),
        Some(&SubAgentStatus::Done)
    );
    // Main transcript should only have the "started" marker
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(texts, vec!["* [root/a] started",]);
    // Check sub-agent transcript
    let sub_texts: Vec<&str> = app
        .sub_agent_transcripts
        .get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    // Sub-agent transcript should have the text and done marker
    assert!(sub_texts.iter().any(|t| t.contains("found a thing")));
    assert!(sub_texts.iter().any(|t| t.contains("+ [root/a] done")));
}

#[test]
fn sub_agent_tool_calls_are_labeled() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_tool_started("root/a", "read_file")));
    app.handle_event(TuiEvent::Runtime(child_tool_finished(
        "root/a",
        "read_file",
    )));
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(texts, vec!["* [root/a] started",]);
    // Check sub-agent transcript
    let sub_texts: Vec<&str> = app
        .sub_agent_transcripts
        .get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        sub_texts,
        vec!["* [root/a] read_file", "  [root/a] < done",]
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
    assert_eq!(
        app.sub_agents.get("root/a").map(|s| &s.status),
        Some(&SubAgentStatus::Done)
    );
}

#[test]
fn agent_prefix_labels_sub_agents_only() {
    use crate::ui::handlers::runtime::agent_prefix;
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
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(texts, vec!["* [root/a] started"]);
    // Child content is preserved, committed ahead of the done marker.
    app.handle_event(TuiEvent::Runtime(run_finished(Some("root/a"))));
    let sub_texts: Vec<&str> = app
        .sub_agent_transcripts
        .get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    let done_idx = sub_texts.iter().position(|t| t.contains("done")).unwrap();
    assert!(
        sub_texts[..done_idx]
            .iter()
            .any(|t| t.contains("thinking hard")),
        "child thought must reach the child transcript: {sub_texts:?}"
    );
    assert!(
        sub_texts[..done_idx]
            .iter()
            .any(|t| t.contains("partial prose")),
        "child text must reach the child transcript: {sub_texts:?}"
    );
}

#[test]
fn child_stream_flushes_before_its_tool_line() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(child_text(
        "root/a",
        "read the config first",
    )));
    app.handle_event(TuiEvent::Runtime(child_tool_started("root/a", "read_file")));
    let sub_texts: Vec<&str> = app
        .sub_agent_transcripts
        .get("root/a")
        .map(|t| t.iter().map(|l| l.text.as_str()).collect())
        .unwrap_or_default();
    // Pending child text commits above the invocation line, in order.
    assert!(
        sub_texts.len() >= 2
            && sub_texts[0].contains("read the config first")
            && sub_texts[1].starts_with("* [root/a] read_file"),
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
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        texts.contains(&"hel"),
        "root text flushed intact: {texts:?}"
    );
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
    assert!(app.status_line().starts_with("Idle"));
    app.set_notice("📋 copied 5 chars");
    assert_eq!(app.status_line(), "📋 copied 5 chars");
    // Any keypress clears the notice before being handled.
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.status_line().starts_with("Idle"));
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
    let lines: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("📋 目标")),
        "got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("✅ 已完成")),
        "got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("🔄 进行中")),
        "got: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("- 待办")), "got: {lines:?}");
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
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
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
    assert!(
        app.transcript
            .output
            .iter()
            .any(|l| l.text.contains("↳ 检测到已有配置"))
    );
}

#[test]
fn update_plan_tool_result_line_is_suppressed() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(tool_started("update_plan")));
    app.handle_event(TuiEvent::Runtime(tool_finished("update_plan", false)));
    assert!(
        !app.transcript
            .output
            .iter()
            .any(|l| l.text.contains("update_plan"))
    );
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
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("📋 目标A")),
        "target A kept: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("📋 目标B")),
        "target B appended: {texts:?}"
    );
    assert_eq!(texts.iter().filter(|t| t.contains("📋")).count(), 2);
}

#[test]
fn mouse_drag_selects_line_range() {
    let mut app = App::new();
    for i in 0..10 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
            detail: None,
            tool_state: None,
        });
    }
    app.output_area = Some((0, 0, 100, 10));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2, 100, 10);
    app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 0, 5, 100, 10);
    assert_eq!(
        app.selection_state.selection,
        Some(Selection { anchor: 2, head: 5 })
    );
    assert_eq!(app.selection_text(), "line 2\nline 3\nline 4\nline 5");
}

#[test]
fn mouse_drag_up_normalizes_selection() {
    let mut app = App::new();
    for i in 0..10 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
            detail: None,
            tool_state: None,
        });
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
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        text: "x".into(),
        kind: LineKind::Normal,
        detail: None,
        tool_state: None,
    });
    app.output_area = Some((0, 0, 10, 5));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 10, 5);
    assert!(app.selection_state.selection.is_some());
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 50, 10, 5); // below pane
    assert!(app.selection_state.selection.is_none());
}

#[test]
fn selection_text_clamps_stale_indices() {
    let mut app = App::new();
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        text: "a".into(),
        kind: LineKind::Normal,
        detail: None,
        tool_state: None,
    });
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
    app.quit_hint_at = Some(Instant::now() - std::time::Duration::from_secs(5));
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
fn ctrl_c_force_quits_when_stuck_after_cancel() {
    let mut app = App::new();
    app.running = true;
    app.quit_hint_at = None;
    // First Ctrl+C while running -> cancel (and arms the force-quit window).
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Cancel)
    );
    assert!(app.notice.as_deref().unwrap_or("").contains("force quit"));
    // Second Ctrl+C within the window while still stuck -> force quit.
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Quit)
    );
    // After the window lapses (cancel landed but user pressed late), the
    // next press cancels again instead of quitting.
    app.running = true;
    app.quit_hint_at = Some(Instant::now() - std::time::Duration::from_secs(5));
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
        Some(Action::Cancel)
    );
}

#[test]
fn right_click_opens_menu_and_enter_copies() {
    let mut app = App::new();
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        text: "x".into(),
        kind: LineKind::Normal,
        detail: None,
        tool_state: None,
    });
    app.output_area = Some((0, 0, 10, 10));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 20, 20);
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 3, 4, 20, 20);
    assert_eq!(
        app.selection_state.context_menu,
        Some(ContextMenu {
            x: 3,
            y: 4,
            selected: 0
        })
    );
    assert_eq!(
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE),
        Some(Action::CopySelection)
    );
    assert_eq!(app.selection_state.context_menu, None);
}

#[test]
fn context_menu_arrows_move_highlight_and_esc_closes() {
    let mut app = App::new();
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        text: "x".into(),
        kind: LineKind::Normal,
        detail: None,
        tool_state: None,
    });
    app.output_area = Some((0, 0, 10, 10));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 0, 20, 20);
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
    assert_eq!(app.handle_key(KeyCode::Down, KeyModifiers::NONE), None);
    assert_eq!(
        app.selection_state.context_menu.as_ref().unwrap().selected,
        1
    );
    // Enter on the "cancel" item closes without copying.
    assert_eq!(app.handle_key(KeyCode::Enter, KeyModifiers::NONE), None);
    assert!(app.selection_state.context_menu.is_none());
}

#[test]
fn clicking_menu_copy_item_copies_and_closes() {
    let mut app = App::new();
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        text: "x".into(),
        kind: LineKind::Normal,
        detail: None,
        tool_state: None,
    });
    app.output_area = Some((0, 0, 10, 10));
    app.selection_state.selection = Some(Selection { anchor: 0, head: 0 });
    // Open the menu at (0,0): a 12x4 box whose items sit at rows y+1 (copy)
    // and y+2 (dismiss).
    assert_eq!(
        app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20),
        None
    );
    assert!(app.selection_state.context_menu.is_some());
    // Left-click the copy row -> copy, menu closes, selection preserved.
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
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        text: "x".into(),
        kind: LineKind::Normal,
        detail: None,
        tool_state: None,
    });
    app.output_area = Some((0, 0, 10, 10));
    app.selection_state.selection = Some(Selection { anchor: 0, head: 0 });
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 0, 0, 20, 20);
    // The dismiss item is second, at row y+2. It closes the menu but keeps the
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
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        text: "x".into(),
        kind: LineKind::Normal,
        detail: None,
        tool_state: None,
    });
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
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER),
        None
    );
    // With a selection → copy.
    app.selection_state.selection = Some(Selection { anchor: 0, head: 0 });
    assert_eq!(
        app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER),
        Some(Action::CopySelection)
    );
}

fn mention_scratch(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("phimint-app-mention-{tag}-{}", std::process::id()));
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
    assert!(
        text.starts_with('/'),
        "outside path is absolute, got {text:?}"
    );
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
fn esc_with_typed_prefix_keeps_text_and_closes() {
    let mut app = App::new();
    let root = mention_scratch("esc-keep");
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    for c in "src".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mention().is_none());
    assert_eq!(
        app.composer.text(),
        "@src",
        "Esc closes the picker but keeps the typed text"
    );
}

#[test]
fn esc_after_dir_navigation_keeps_text() {
    let mut app = App::new();
    let root = mention_scratch("esc-dir");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/a.rs"), "x").unwrap();
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE); // descend into sub/
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE); // close the picker
    assert!(app.mention().is_none());
    assert_eq!(
        app.composer.text(),
        "@sub/",
        "Esc keeps the typed @path in the composer"
    );
}

#[test]
fn esc_on_bare_at_removes_trigger() {
    let mut app = App::new();
    let root = mention_scratch("esc-bare");
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.mention().is_none());
    assert_eq!(app.composer.text(), "");
}

#[test]
fn enter_on_directory_focuses_the_directory_row() {
    let mut app = App::new();
    let root = mention_scratch("dir-focus");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/a.rs"), "x").unwrap();
    std::fs::write(root.join("top.txt"), "x").unwrap();
    app.set_workspace_root(root.clone());

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    // Arrow onto `sub` — the first real entry, right after the synthetic row.
    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    let m = app.mention().expect("picker stays open on a directory");
    assert_eq!(m.prefix(), "sub/");
    assert_eq!(
        m.selected_index(),
        0,
        "focus is on the entered directory (synthetic row)"
    );
    assert!(m.entries()[0].synthetic);
    assert_eq!(app.composer.text(), "@sub/");
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
    // After sorting: commit(0), requesting-code-review(1), review(2)
    app.set_skill_summaries(vec![
        ("commit".into(), "Generate a commit message".into()),
        (
            "requesting-code-review".into(),
            "Request a code review".into(),
        ),
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
    assert!(
        app.slash().is_none(),
        "slash picker should not open mid-input"
    );
}

#[test]
fn slash_filters_by_prefix() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);

    let s = app.slash().unwrap();
    let names: Vec<&str> = s.entries().iter().map(|(n, _)| n.as_str()).collect();
    // "commit" starts with 'c' → kept
    assert!(names.contains(&"commit"), "commit should match prefix 'c'");
    // "requesting-code-review" has 'c' but does NOT start with 'c' → filtered out
    assert!(
        !names.contains(&"requesting-code-review"),
        "requesting-code-review should not match prefix 'c'"
    );
    // "review" does not start with 'c' → filtered out
    assert!(
        !names.contains(&"review"),
        "review should not match prefix 'c'"
    );
    // Only "commit" remains
    assert_eq!(names.len(), 1, "only 'commit' should match prefix 'c'");
}

#[test]
fn slash_filters_progressively() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    // All 3 visible initially
    assert_eq!(app.slash().unwrap().entries().len(), 3);

    // Type 'r' → "requesting-code-review" and "review" start with 'r'
    app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
    let names: Vec<&str> = app
        .slash()
        .unwrap()
        .entries()
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(names.len(), 2, "'r' should match 2 skills");
    assert!(names.contains(&"requesting-code-review"));
    assert!(names.contains(&"review"));

    // Type 'e' → "review" starts with "re", "requesting-code-review" starts with "re"
    app.handle_key(KeyCode::Char('e'), KeyModifiers::NONE);
    let names: Vec<&str> = app
        .slash()
        .unwrap()
        .entries()
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(names.len(), 2, "'re' should still match 2 skills");

    // Type 'v' → only "review" starts with "rev"
    app.handle_key(KeyCode::Char('v'), KeyModifiers::NONE);
    let names: Vec<&str> = app
        .slash()
        .unwrap()
        .entries()
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(names.len(), 1, "'rev' should match only 'review'");
    assert!(names.contains(&"review"));

    // Composer should show "/rev"
    assert_eq!(app.composer.text(), "/rev");
}

#[test]
fn slash_backspace_widens_filter() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('e'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('v'), KeyModifiers::NONE);
    assert_eq!(app.slash().unwrap().entries().len(), 1); // only "review"

    // Backspace → "re" → both "review" and "requesting-code-review" match again
    app.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
    let names: Vec<&str> = app
        .slash()
        .unwrap()
        .entries()
        .iter()
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(names.len(), 2, "backspace to 're' should widen filter");
    assert_eq!(app.composer.text(), "/re");
}

#[test]
fn slash_enter_confirms_selection() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    // The first entry is selected by default ("commit" after sorting)
    app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.slash().is_none(), "picker should close after Enter");
    assert_eq!(
        app.composer.text(),
        "/commit ",
        "should insert /name + space"
    );
}

#[test]
fn slash_esc_cancels() {
    let mut app = app_with_skills();
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.slash().is_none(), "picker should close on Esc");
    assert!(
        app.composer.is_empty(),
        "composer should be empty after cancel"
    );
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

    // Already at the end: Down does nothing
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
        Some(LineDetail::Diff { path, hunks }) => {
            assert_eq!(path, "src/main.rs");
            assert!(!hunks.is_empty(), "should have diff hunks");
            // Should have a Del line and an Add line
            let all_lines: Vec<_> = hunks.iter().flat_map(|h| h.lines.iter()).collect();
            assert!(all_lines.iter().any(|l| l.kind == DiffLineKind::Del));
            assert!(all_lines.iter().any(|l| l.kind == DiffLineKind::Add));
        }
        other => panic!("expected LineDetail::Diff, got {other:?}"),
    }
}

#[test]
fn write_file_create_folds_the_written_content() {
    // A create is an artifact, not a transformation: the content rides the
    // preview ladder as a folded block. It is NOT an all-Add diff — that
    // rendering made a 10k-line write a 10k-line wall of green `+`.
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
        Some(LineDetail::Folded {
            raw, line_count, ..
        }) => {
            assert_eq!(raw, "fn hello() {\n    println!(\"hi\");\n}\n");
            assert_eq!(*line_count, 3);
        }
        other => panic!("expected LineDetail::Folded, got {other:?}"),
    }
}

#[test]
fn write_file_overwrite_becomes_a_real_diff() {
    // An overwrite *is* a transformation, so once the tool reports the old
    // content the staged written-content block is swapped for an old→new diff —
    // the same always-full evidence treatment an edit_file gets.
    let mut app = App::new();
    let args = serde_json::json!({
        "path": "src/new.rs",
        "content": "fn hello() {\n    println!(\"bye\");\n}\n",
        "overwrite": true
    });
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "write_file".to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }));
    // Pre-swap: folded content.
    assert!(matches!(
        app.transcript.output.last().unwrap().detail,
        Some(LineDetail::Folded { .. })
    ));

    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: "write_file".to_string(),
        summary: "Updated file: src/new.rs".to_string(),
        agent_id: None,
        trace_id: None,
        denied: false,
        details: Some(serde_json::json!({
            "write_mode": "overwrite",
            "old_content": "fn hello() {\n    println!(\"hi\");\n}\n",
        })),
    }));

    let tool_line = app
        .transcript
        .output
        .iter()
        .find(|l| l.kind == LineKind::Tool)
        .expect("tool line present");
    match &tool_line.detail {
        Some(LineDetail::Diff { hunks, .. }) => {
            let all: Vec<_> = hunks.iter().flat_map(|h| h.lines.iter()).collect();
            assert!(all.iter().any(|l| l.kind == DiffLineKind::Del));
            assert!(all.iter().any(|l| l.kind == DiffLineKind::Add));
        }
        other => panic!("expected LineDetail::Diff after overwrite, got {other:?}"),
    }
    // Settled in place — the marker slot is redrawn from this.
    assert_eq!(tool_line.tool_state, Some(ToolState::Done));
}

#[test]
fn non_file_tool_has_no_detail() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(tool_started("read_file")));
    let tool_line = app.transcript.output.last().expect("tool line present");
    assert!(
        tool_line.detail.is_none(),
        "non-file tools should have no detail"
    );
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
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
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
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 2, bg: 0 }
    ));

    app.handle_event(TuiEvent::TurnDone);
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("... waiting for sub-agents (2 running)"))
    );
    assert!(
        !texts.iter().any(|t| t.contains("✅ done")),
        "waiting is not done"
    );
    assert!(!app.running);
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 2, bg: 0 }
    ));
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
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
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
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 2, bg: 0 }
    ));

    // Watcher Progress events flip panel entries and refresh the count.
    app.mark_sub_agent_finished("root/a");
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 1, bg: 0 }
    ));
    assert_eq!(
        app.sub_agents.get("root/a").map(|s| &s.status),
        Some(&SubAgentStatus::Done)
    );

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
    assert!(
        app.sub_agents
            .values()
            .all(|s| s.status == SubAgentStatus::Done)
    );
    assert_eq!(app.status, AgentStatus::Idle);
}

#[test]
fn waiting_status_line_mentions_running_count() {
    let mut app = App::new();
    app.status = AgentStatus::Waiting { running: 3, bg: 0 };
    let line = app.status_line();
    assert!(line.contains('3'), "count missing: {line}");
    assert!(line.contains("waiting"), "waiting wording missing: {line}");
}

// ── Background-task waiting state (turn ends while bg tasks run) ────────────

fn running_bg_task(app: &mut App, id: &str, command: &str) {
    running_bg_task_with_timeout(app, id, command, 120_000);
}

/// Daemon-style entry (`timeout_ms == 0`): a server left running on purpose.
fn running_daemon_task(app: &mut App, id: &str, command: &str) {
    running_bg_task_with_timeout(app, id, command, 0);
}

fn running_bg_task_with_timeout(app: &mut App, id: &str, command: &str, timeout_ms: u64) {
    app.background_tasks.insert(
        id.to_string(),
        BackgroundTaskEntry {
            id: id.to_string(),
            command: command.to_string(),
            timeout_ms,
            status: BackgroundTaskStatus::Running,
            started_at: std::time::Instant::now(),
            finished_at: None,
            reported: false,
            output_tail: String::new(),
            consumed: false,
        },
    );
}

#[test]
fn turn_done_with_running_bg_tasks_enters_waiting_not_idle() {
    let mut app = App::new();
    app.running = true;
    running_bg_task(&mut app, "bg_aaaa1111", "cargo test");
    running_bg_task(&mut app, "bg_bbbb2222", "npm run build");

    // Root finishes its turn while the two background tasks are still out —
    // the status must NOT drop to Idle ("done"), it must wait on the bg
    // tasks the same way it waits on sub-agents.
    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 0, bg: 2 }
    ));

    app.handle_event(TuiEvent::TurnDone);
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("waiting for background tasks") && t.contains('2'))
    );
    assert!(
        !texts.iter().any(|t| t.contains("✅ done")),
        "waiting is not done"
    );
    assert!(!app.running);
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 0, bg: 2 }
    ));
    // Status bar: "waiting for background tasks", not "done".
    let line = app.status_line();
    assert!(
        line.contains("background tasks"),
        "bg wording missing: {line}"
    );
    assert!(line.contains('2'), "bg count missing: {line}");
}

#[test]
fn turn_done_with_children_and_bg_tasks_waits_for_both() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::Runtime(child_text("root/a", "working")));
    running_bg_task(&mut app, "bg_aaaa1111", "sleep 5");

    app.handle_event(TuiEvent::TurnDone);
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 1, bg: 1 }
    ));
    let line = app.status_line();
    assert!(
        line.contains("sub-agents") && line.contains("background tasks"),
        "both: {line}"
    );
}

#[test]
fn bg_task_finishing_while_waiting_updates_count() {
    let mut app = App::new();
    app.status = AgentStatus::Waiting { running: 0, bg: 1 };

    // Registry flip → reconcile → refresh (the production trigger path).
    let registry = BackgroundTaskRegistry::new(4);
    app.set_background_registry(registry.clone());
    let token = tokio_util::sync::CancellationToken::new();
    let id = registry
        .register("cargo test", None, token, None, 120_000)
        .unwrap();
    assert!(app.reconcile_background_tasks(), "new task is a change");
    assert!(matches!(
        app.status,
        AgentStatus::Waiting { running: 0, bg: 1 }
    ));

    registry.finish(&id, Some(0));
    assert!(app.reconcile_background_tasks(), "status flip is a change");
    assert_eq!(app.status, AgentStatus::Idle, "last bg task done → idle");
}

#[test]
fn daemon_only_settle_is_done_plus_service_note_not_waiting() {
    // Session 20260922_6d262d0f: the turn ended with a `mvn spring-boot:run`
    // daemon (`timeout_ms: 0`) up, and the TUI promised an automatic report
    // for a process that never completes. A daemon is steady state: settle
    // as done and note the service, never "waiting".
    let mut app = App::new();
    app.running = true;
    running_daemon_task(&mut app, "bg_daemon01", "mvn spring-boot:run");

    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    app.handle_event(TuiEvent::TurnDone);

    assert_eq!(
        app.status,
        AgentStatus::Idle,
        "daemon alone = the agent can rest"
    );
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert!(texts.contains(&"✅ done"), "work is done: {texts:?}");
    assert!(
        texts
            .iter()
            .any(|t| t.contains("daemon") && t.contains('1')),
        "service noted: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("waiting")),
        "nothing to wait for: {texts:?}"
    );
    let line = app.status_line();
    assert!(
        line.contains("daemon"),
        "status bar keeps the service visible: {line}"
    );
    assert!(
        !line.contains("waiting"),
        "status bar must not wait: {line}"
    );
}

#[test]
fn mixed_bounded_and_daemon_settle_waits_only_for_the_bounded() {
    let mut app = App::new();
    app.running = true;
    running_bg_task(&mut app, "bg_aaaa1111", "cargo test");
    running_daemon_task(&mut app, "bg_daemon01", "mvn spring-boot:run");

    app.handle_event(TuiEvent::Runtime(run_finished(None)));
    app.handle_event(TuiEvent::TurnDone);

    assert!(
        matches!(app.status, AgentStatus::Waiting { running: 0, bg: 2 }),
        "counter still covers everything running"
    );
    let texts: Vec<&str> = app
        .transcript
        .output
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    let wait = texts
        .iter()
        .find(|t| t.contains("waiting for background tasks"))
        .expect("waiting line");
    assert!(wait.contains("1 running"), "count = bounded only: {wait}");
    assert!(wait.contains("daemon"), "daemon still listed: {wait}");
    let line = app.status_line();
    assert!(
        line.contains("waiting for background tasks") && line.contains("daemon"),
        "{line}"
    );
}

#[test]
fn idle_status_line_lists_daemons_without_waiting() {
    let mut app = App::new();
    running_daemon_task(&mut app, "bg_daemon01", "mvn spring-boot:run");
    app.status = AgentStatus::Idle;
    let line = app.status_line();
    assert!(line.starts_with("Idle"), "{line}");
    assert!(line.contains("1 daemon(s)"), "{line}");
}

#[test]
fn thought_delta_opens_thinking_segment() {
    let mut app = App::new();
    assert!(!app.thinking_since.contains_key(""));
    app.handle_event(TuiEvent::Runtime(thought("hmm ")));
    assert!(
        app.thinking_since.contains_key(""),
        "first delta opens a segment"
    );
}

#[test]
fn flush_pending_clears_thinking_segment() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(thought(&"x ".repeat(200))));
    assert!(app.thinking_since.contains_key(""));
    app.flush_pending();
    assert!(!app.thinking_since.contains_key(""));
}

fn thought_as(s: &str, agent: &str) -> RuntimeEvent {
    RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: s.to_string(),
        agent_id: Some(agent.to_string()),
        trace_id: None,
    }
}

#[test]
fn second_thought_delta_keeps_segment_start() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(thought("a ")));
    let t0 = app.thinking_since.get("").copied();
    app.handle_event(TuiEvent::Runtime(thought("b ")));
    assert_eq!(
        app.thinking_since.get("").copied(),
        t0,
        "same segment keeps its start"
    );
}

#[test]
fn text_delta_retires_thought_timer() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(thought(&"x ".repeat(200))));
    assert!(app.thinking_since.contains_key(""));
    app.handle_event(TuiEvent::Runtime(text("answer ")));
    assert!(
        !app.thinking_since.contains_key(""),
        "implicit flush retires the timer"
    );
}

#[test]
fn child_flush_keeps_other_children_timers() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(thought_as("A thinking ", "root/a")));
    app.handle_event(TuiEvent::Runtime(thought_as("B thinking ", "root/b")));
    assert!(app.thinking_since.contains_key("root/a"));
    assert!(app.thinking_since.contains_key("root/b"));
    app.flush_child_stream("root/a");
    assert!(!app.thinking_since.contains_key("root/a"));
    assert!(
        app.thinking_since.contains_key("root/b"),
        "B's timer survives A's flush"
    );
}

#[test]
fn ctrl_o_toggles_show_thoughts() {
    let mut app = App::new();
    assert!(!app.show_thoughts);
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(app.show_thoughts);
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(!app.show_thoughts);
    // Plain 'o' still types into the composer.
    app.handle_key(KeyCode::Char('o'), KeyModifiers::NONE);
    assert!(!app.show_thoughts);
    assert_eq!(app.composer.text(), "o");
}

#[test]
fn thought_agent_change_restarts_timer() {
    // `push_thought` implicitly flushes the previous segment on a kind/agent
    // flip. The new segment must re-arm the panel timer (`!was` alone keeps
    // the old `Instant` and overstates elapsed).
    let mut app = App::new();
    let thought = |agent: Option<&str>| {
        TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
            session_id: SessionId::new(1),
            text: "think ".repeat(50),
            agent_id: agent.map(String::from),
            trace_id: None,
        })
    };
    app.handle_event(thought(Some(""))); // routes to the root stream, tag Some("")
    let first = *app.thinking_since.get("").expect("timer armed");
    let n = app.transcript.len();
    app.handle_event(thought(None)); // same stream, different tag → implicit flush
    assert!(
        app.transcript.len() > n,
        "agent-change flush must commit the old segment"
    );
    assert!(
        app.transcript
            .output
            .iter()
            .any(|l| l.kind == LineKind::Thought),
        "old segment commits as a thought line"
    );
    let second = *app.thinking_since.get("").expect("timer re-armed");
    assert!(
        second > first,
        "re-arm must install a FRESH Instant — a stale one overstates elapsed"
    );
}

#[test]
fn child_thought_text_thought_rearms_timer() {
    // Kind-flip through prose: thought → text → thought on one child stream.
    // The implicit flush retires the timer; the new segment must re-arm it.
    let mut app = App::new();
    let thought = || {
        TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
            session_id: SessionId::new(1),
            text: "think ".repeat(50),
            agent_id: Some("root/a".to_string()),
            trace_id: None,
        })
    };
    app.handle_event(thought());
    let first = *app.thinking_since.get("root/a").expect("child timer armed");
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: "prose ".repeat(20),
        agent_id: Some("root/a".to_string()),
        trace_id: None,
    }));
    assert!(
        !app.thinking_since.contains_key("root/a"),
        "prose retires the child's thought timer"
    );
    app.handle_event(thought());
    let second = *app
        .thinking_since
        .get("root/a")
        .expect("child timer re-armed");
    assert!(second > first, "new segment must get a fresh timer");
}

#[test]
fn popup_style_defaults_to_frameless_and_is_settable() {
    use phi_tui::popup_list::{PopupStyle, WidthSpec};
    let mut app = App::new();
    assert!(
        !app.popup_style().frame,
        "default popup style must be frameless"
    );
    assert_eq!(app.popup_style().width, WidthSpec::Fill);

    app.set_popup_style(PopupStyle::framed(64, 9));
    assert!(app.popup_style().frame);
    assert_eq!(app.popup_style().width, WidthSpec::Fixed(64));
}

// ── Notice rendering (Batch E, PR4) ─────────────────────────────────────────

fn notice(kind: phi_agent::NoticeKind, source: &str, text: &str) -> RuntimeEvent {
    RuntimeEvent::UserEvent {
        session_id: SessionId::new(1),
        event: UserEvent::Notice {
            kind,
            source: source.to_string(),
            text: text.to_string(),
        },
        agent_id: None,
        trace_id: None,
    }
}

#[test]
fn notice_warning_renders_persistent_error_line() {
    let mut app = App::new();
    // Mid-turn precondition: only then is "does not settle the turn"
    // observable — Idle→Idle proves nothing.
    app.handle_event(TuiEvent::Runtime(tool_started("execute_command")));
    assert!(
        matches!(app.status, AgentStatus::Running { .. }),
        "precondition: mid-turn"
    );
    let lines_before = app.transcript.output.len();

    app.handle_event(TuiEvent::Runtime(notice(
        phi_agent::NoticeKind::Warning,
        "guard",
        "guard judge unparsed — treating as complete",
    )));

    // Warning lands as a red (Error-kind) transcript line — persistent.
    assert_eq!(
        app.transcript.output.len(),
        lines_before + 1,
        "exactly one line emitted"
    );
    let line = app.transcript.output.last().expect("one line");
    assert_eq!(line.kind, LineKind::Error, "warning renders as red line");
    // User-facing copy: the mechanism fact ("guard judge …") never reaches
    // the user — nobody outside the engine knows what a "judge" is.
    assert!(
        line.text.contains("Answer accepted without verification."),
        "line carries the user-facing copy, got: {}",
        line.text
    );
    assert!(
        !line.text.contains("judge") && !line.text.contains("guard"),
        "mechanism jargon must not leak to the user, got: {}",
        line.text
    );
    // No turn settlement: the notice must not call settle_after_turn —
    // status stays Running.
    assert!(
        matches!(app.status, AgentStatus::Running { .. }),
        "warning must not settle the turn, got: {:?}",
        app.status
    );
}

/// The three judge fail-open causes (unparsed / call failed / unavailable)
/// are indistinguishable to a user and none of them is actionable — product
/// layer collapses them to one copy (design §8: mechanism layer keeps the
/// cause-specific English facts for logs and tests).
#[test]
fn notice_guard_judge_fail_open_causes_collapse_to_one_user_copy() {
    let user_copy = "Answer accepted without verification.";
    for mechanism in [
        "guard judge unparsed — treating as complete",
        "guard judge call failed — treating as complete",
        "guard judge unavailable — treating as complete",
    ] {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(notice(
            phi_agent::NoticeKind::Warning,
            "guard",
            mechanism,
        )));
        let line = app.transcript.output.last().expect("one line");
        assert!(
            line.text.contains(user_copy),
            "mechanism `{mechanism}` must render as the user copy, got: {}",
            line.text
        );
    }

    // Guard notices that are NOT judge fail-open keep their own text.
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(notice(
        phi_agent::NoticeKind::Warning,
        "guard",
        "guard reasoning strikes exhausted",
    )));
    let line = app.transcript.output.last().expect("one line");
    assert!(
        line.text.contains("guard reasoning strikes exhausted"),
        "non-judge guard notices pass through, got: {}",
        line.text
    );
}

#[test]
fn notice_progress_updates_live_progress() {
    let mut app = App::new();
    // live_progress renders only in the Running/ToolCall phase — same as
    // tool Progress (existing semantics, see progress_updates_and_clears…).
    app.handle_event(TuiEvent::Runtime(tool_started("execute_command")));
    app.handle_event(TuiEvent::Runtime(notice(
        phi_agent::NoticeKind::Progress,
        "compactor",
        "compacting context…",
    )));

    // Progress is transient status-bar text — same channel as tool progress.
    let line = app.status_line();
    assert!(
        line.contains("compacting context…"),
        "progress notice surfaces in the status bar, got: {line}"
    );
    let lines_after_progress = app.transcript.output.len();
    // Any further notices/transcript churn must not come from this event:
    // the notice only feeds the status bar.
    app.handle_event(TuiEvent::Runtime(notice(
        phi_agent::NoticeKind::Progress,
        "compactor",
        "still compacting…",
    )));
    assert_eq!(
        app.transcript.output.len(),
        lines_after_progress,
        "progress must not pollute the transcript"
    );
}

#[test]
fn notice_info_renders_plain_line() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(notice(
        phi_agent::NoticeKind::Info,
        "reaper",
        "reaped 2 background tasks",
    )));

    assert_eq!(app.transcript.output.len(), 1, "one line emitted");
    let line = &app.transcript.output[0];
    assert_eq!(
        line.kind,
        LineKind::System,
        "info renders as gray system line"
    );
    assert!(
        line.text.contains("reaped 2 background tasks"),
        "got: {}",
        line.text
    );
}
