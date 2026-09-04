//! Mock tests for the multi-task panel feature.
//!
//! These tests simulate sub-agent lifecycles without requiring real LLM calls,
//! verifying the task panel state, transcript routing, and cleanup behavior.

use std::time::{Duration, Instant};

use super::*;
use crate::ui::app::{App, FocusTarget, LineKind, OutputLine, SubAgentState, SubAgentStatus, ToolEvent};
use phi_agent::{RuntimeEvent, SessionId};

// ── Mock Event Builders ──────────────────────────────────────────────────────

/// Create a TextDelta event for a sub-agent.
fn sub_text(agent_id: &str, text: &str) -> RuntimeEvent {
    RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: text.to_string(),
        agent_id: Some(agent_id.to_string()),
        trace_id: None,
    }
}

/// Create a ThoughtDelta event for a sub-agent.
fn sub_thought(agent_id: &str, text: &str) -> RuntimeEvent {
    RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: text.to_string(),
        agent_id: Some(agent_id.to_string()),
        trace_id: None,
    }
}

/// Create a ToolCallStarted event for a sub-agent.
fn sub_tool_started(agent_id: &str, tool_name: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: tool_name.to_string(),
        args_json: "{}".to_string(),
        agent_id: Some(agent_id.to_string()),
        trace_id: None,
    }
}

/// Create a ToolCallFinished event for a sub-agent.
fn sub_tool_finished(agent_id: &str, tool_name: &str, summary: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: tool_name.to_string(),
        summary: summary.to_string(),
        agent_id: Some(agent_id.to_string()),
        trace_id: None,
        denied: false,
        details: None,
    }
}

/// Create a RunFinished event for a sub-agent.
fn sub_run_finished(agent_id: &str) -> RuntimeEvent {
    RuntimeEvent::RunFinished {
        session_id: SessionId::new(1),
        agent_id: Some(agent_id.to_string()),
        trace_id: None,
    }
}

/// Create a RunCancelled event for a sub-agent.
fn sub_run_cancelled(agent_id: &str) -> RuntimeEvent {
    RuntimeEvent::RunCancelled {
        session_id: SessionId::new(1),
        agent_id: Some(agent_id.to_string()),
        trace_id: None,
    }
}

// ── Test Helpers ─────────────────────────────────────────────────────────────

/// Helper to create a SubAgentState with minimal fields.
fn mock_sub_agent(name: &str, status: SubAgentStatus) -> SubAgentState {
    SubAgentState {
        name: name.to_string(),
        status,
        files: vec![format!("src/{name}.rs")],
        task: format!("Implement {name}"),
        context: String::new(),
        started_at: Instant::now(),
        completed_at: None,
        events: Vec::new(),
    }
}

/// Helper to create a completed SubAgentState.
fn mock_completed_agent(name: &str, completed_secs_ago: u64) -> SubAgentState {
    SubAgentState {
        name: name.to_string(),
        status: SubAgentStatus::Done,
        files: vec![format!("src/{name}.rs")],
        task: format!("Implement {name}"),
        context: String::new(),
        started_at: Instant::now() - Duration::from_secs(completed_secs_ago + 5),
        completed_at: Some(Instant::now() - Duration::from_secs(completed_secs_ago)),
        events: Vec::new(),
    }
}

/// Insert a mock sub-agent into the app.
fn insert_mock_agent(app: &mut App, agent_id: &str, name: &str, status: SubAgentStatus) {
    app.sub_agents.insert(
        agent_id.to_string(),
        mock_sub_agent(name, status),
    );
}

/// Insert a completed agent with a specific completion time.
fn insert_completed_agent(app: &mut App, agent_id: &str, name: &str, completed_secs_ago: u64) {
    app.sub_agents.insert(
        agent_id.to_string(),
        mock_completed_agent(name, completed_secs_ago),
    );
}

// ── Task Panel State Tests ───────────────────────────────────────────────────

#[test]
fn task_panel_focus_navigation() {
    let mut app = App::new();

    // Insert mock agents
    insert_mock_agent(&mut app, "root/cache", "cache", SubAgentStatus::Running);
    insert_mock_agent(&mut app, "root/auth", "auth", SubAgentStatus::Running);
    insert_mock_agent(&mut app, "root/db", "db", SubAgentStatus::Running);

    // Default focus is Input
    assert_eq!(app.task_panel.focus, FocusTarget::Input);

    // Up from Input moves to last task
    app.task_panel.focus = FocusTarget::Input;
    app.handle_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.task_panel.focus, FocusTarget::TaskList(2));

    // Up again moves to previous task
    app.handle_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.task_panel.focus, FocusTarget::TaskList(1));

    // Up again moves to first task
    app.handle_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.task_panel.focus, FocusTarget::TaskList(0));

    // Up at first task stays at first
    app.handle_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.task_panel.focus, FocusTarget::TaskList(0));

    // Down moves to next task
    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.task_panel.focus, FocusTarget::TaskList(1));

    // Down to last task
    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.task_panel.focus, FocusTarget::TaskList(2));

    // Down past last task moves to Input
    app.handle_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.task_panel.focus, FocusTarget::Input);
}

#[test]
fn task_panel_focus_resets_on_cleanup() {
    let mut app = App::new();

    // Insert agents, one already completed 4 seconds ago
    insert_mock_agent(&mut app, "root/cache", "cache", SubAgentStatus::Running);
    insert_completed_agent(&mut app, "root/auth", "auth", 4);

    // Focus on auth (index 1): the user is inspecting the list, so cleanup
    // must NOT reap under their eyes.
    app.task_panel.focus = FocusTarget::TaskList(1);
    assert!(!app.cleanup_completed_agents());
    assert_eq!(app.sub_agents.len(), 2);

    // Back to input: cleanup now removes auth...
    app.task_panel.focus = FocusTarget::Input;
    assert!(app.cleanup_completed_agents());

    // ...and focus would reset to the last valid index (still on Input here,
    // but the list shrank to cache only).
    assert_eq!(app.sub_agents.len(), 1);
}

#[test]
fn task_panel_focus_resets_to_input_when_all_removed() {
    let mut app = App::new();

    // Insert only completed agents
    insert_completed_agent(&mut app, "root/auth", "auth", 4);
    insert_completed_agent(&mut app, "root/cache", "cache", 5);

    // Focus on auth (index 0) blocks cleanup
    app.task_panel.focus = FocusTarget::TaskList(0);
    assert!(!app.cleanup_completed_agents());

    // Input focus lets it run: all removed
    app.task_panel.focus = FocusTarget::Input;
    assert!(app.cleanup_completed_agents());
    assert!(app.sub_agents.is_empty());
}

// ── Transcript Routing Tests ─────────────────────────────────────────────────

#[test]
fn sub_agent_text_routes_to_transcript() {
    let mut app = App::new();

    // Send text from sub-agent
    app.handle_event(TuiEvent::Runtime(sub_text("root/auth", "analyzing auth module")));

    // Main transcript should only have the "started" marker
    let main_texts: Vec<&str> = app.transcript.output.iter().map(|l| l.text.as_str()).collect();
    assert!(main_texts.iter().all(|t| !t.contains("analyzing auth module")));

    // Child deltas bypass the shared stream (whose pending tail is the main
    // view's live tail) — they commit on the child's lifecycle events.
    app.handle_event(TuiEvent::Runtime(sub_run_finished("root/auth")));

    // Should be in sub_agent_transcripts, ahead of the done marker
    let sub_lines = app.sub_agent_transcripts.get("root/auth").unwrap();
    assert_eq!(sub_lines.len(), 2);
    // Note: stream adds [agent_id] prefix to text
    assert!(sub_lines[0].text.contains("analyzing auth module"));
    assert_eq!(sub_lines[0].kind, LineKind::Normal);
    assert!(sub_lines[1].text.contains("done"));
}

#[test]
fn sub_agent_thought_routes_to_transcript() {
    let mut app = App::new();

    // Send thought from sub-agent
    app.handle_event(TuiEvent::Runtime(sub_thought("root/auth", "thinking about security")));

    // Child thought pends in the per-child stream — never the shared one.
    assert!(!app.stream.has_pending_thought());
    app.handle_event(TuiEvent::Runtime(sub_run_finished("root/auth")));

    let sub_lines = app.sub_agent_transcripts.get("root/auth").unwrap();
    assert!(sub_lines.len() >= 1);
    assert_eq!(sub_lines[0].kind, LineKind::Thought);
}

#[test]
fn sub_agent_tool_calls_route_to_transcript() {
    let mut app = App::new();

    // Simulate tool call lifecycle
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/auth", "read_file")));
    app.handle_event(TuiEvent::Runtime(sub_tool_finished("root/auth", "read_file", "120 lines")));

    // Should be in sub_agent_transcripts
    let sub_lines = app.sub_agent_transcripts.get("root/auth").unwrap();
    assert_eq!(sub_lines.len(), 2);
    assert!(sub_lines[0].text.contains("read_file"));
    assert!(sub_lines[1].text.contains("✓"));
}

#[test]
fn sub_agent_completion_routes_to_transcript() {
    let mut app = App::new();

    // Simulate complete lifecycle
    app.handle_event(TuiEvent::Runtime(sub_text("root/auth", "work done")));
    app.handle_event(TuiEvent::Runtime(sub_run_finished("root/auth")));

    // Check sub-agent state
    let state = app.sub_agents.get("root/auth").unwrap();
    assert_eq!(state.status, SubAgentStatus::Done);
    assert!(state.completed_at.is_some());

    // Check transcript
    let sub_lines = app.sub_agent_transcripts.get("root/auth").unwrap();
    assert!(sub_lines.last().unwrap().text.contains("done"));
}

// ── Auto-Cleanup Tests ───────────────────────────────────────────────────────

#[test]
fn cleanup_removes_old_completed_agents() {
    let mut app = App::new();

    // Insert agents with different completion times
    insert_mock_agent(&mut app, "root/running", "running", SubAgentStatus::Running);
    insert_completed_agent(&mut app, "root/recent", "recent", 1);  // 1 second ago
    insert_completed_agent(&mut app, "root/old", "old", 4);        // 4 seconds ago
    insert_completed_agent(&mut app, "root/older", "older", 10);   // 10 seconds ago

    // Cleanup
    let removed = app.cleanup_completed_agents();
    assert!(removed);

    // Running agent should remain
    assert!(app.sub_agents.contains_key("root/running"));

    // Recent (1s) should remain
    assert!(app.sub_agents.contains_key("root/recent"));

    // Old (4s) and older (10s) should be removed
    assert!(!app.sub_agents.contains_key("root/old"));
    assert!(!app.sub_agents.contains_key("root/older"));
}

#[test]
fn cleanup_removes_corresponding_transcripts() {
    let mut app = App::new();

    // Insert completed agent with transcript
    insert_completed_agent(&mut app, "root/auth", "auth", 4);
    app.sub_agent_transcripts.insert(
        "root/auth".to_string(),
        vec![OutputLine {
            text: "test".to_string(),
            kind: LineKind::Normal,
            spans: None,
            original: None,
            detail: None,
        }],
    );

    // Cleanup
    app.cleanup_completed_agents();

    // Transcript should be removed
    assert!(!app.sub_agent_transcripts.contains_key("root/auth"));
}

#[test]
fn cleanup_does_nothing_when_no_completed() {
    let mut app = App::new();

    // Insert only running agents
    insert_mock_agent(&mut app, "root/a", "a", SubAgentStatus::Running);
    insert_mock_agent(&mut app, "root/b", "b", SubAgentStatus::Running);

    // Cleanup should return false
    assert!(!app.cleanup_completed_agents());

    // All agents should remain
    assert_eq!(app.sub_agents.len(), 2);
}

// ── Panel Visibility Tests ───────────────────────────────────────────────────

#[test]
fn panel_visible_when_agents_exist() {
    let mut app = App::new();
    assert!(!app.should_show_task_panel());

    insert_mock_agent(&mut app, "root/auth", "auth", SubAgentStatus::Running);
    assert!(app.should_show_task_panel());
}

#[test]
fn panel_hidden_after_all_agents_removed() {
    let mut app = App::new();

    // Insert and then remove agent
    insert_mock_agent(&mut app, "root/auth", "auth", SubAgentStatus::Running);
    assert!(app.should_show_task_panel());

    app.sub_agents.clear();
    assert!(!app.should_show_task_panel());
}

// ── Multiple Sub-Agents Tests ────────────────────────────────────────────────

#[test]
fn multiple_sub_agents_isolated_transcripts() {
    let mut app = App::new();

    // Simulate two sub-agents working in parallel
    // Need to flush between agents to ensure proper routing
    app.handle_event(TuiEvent::Runtime(sub_text("root/auth", "auth work")));
    app.flush_pending();
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/auth", "read_file")));

    app.handle_event(TuiEvent::Runtime(sub_text("root/cache", "cache work")));
    app.flush_pending();
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/cache", "write_file")));

    app.handle_event(TuiEvent::Runtime(sub_tool_finished("root/auth", "read_file", "done")));
    app.handle_event(TuiEvent::Runtime(sub_tool_finished("root/cache", "write_file", "done")));
    app.handle_event(TuiEvent::Runtime(sub_run_finished("root/auth")));
    app.handle_event(TuiEvent::Runtime(sub_run_finished("root/cache")));

    // Check isolated transcripts
    let auth_lines = app.sub_agent_transcripts.get("root/auth").unwrap();
    let cache_lines = app.sub_agent_transcripts.get("root/cache").unwrap();

    // Auth transcript should only contain auth-related content
    assert!(auth_lines.iter().all(|l| !l.text.contains("cache work")));
    assert!(auth_lines.iter().any(|l| l.text.contains("auth work")));
    assert!(auth_lines.iter().any(|l| l.text.contains("read_file")));

    // Cache transcript should only contain cache-related content
    assert!(cache_lines.iter().all(|l| !l.text.contains("auth work")));
    assert!(cache_lines.iter().any(|l| l.text.contains("cache work")));
    assert!(cache_lines.iter().any(|l| l.text.contains("write_file")));
}

#[test]
fn multiple_sub_agents_events_tracked() {
    let mut app = App::new();

    // Simulate tool calls for multiple agents
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/auth", "read_file")));
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/cache", "write_file")));
    app.handle_event(TuiEvent::Runtime(sub_tool_finished("root/auth", "read_file", "done")));
    app.handle_event(TuiEvent::Runtime(sub_tool_finished("root/cache", "write_file", "done")));

    // Check events in SubAgentState
    let auth_state = app.sub_agents.get("root/auth").unwrap();
    let cache_state = app.sub_agents.get("root/cache").unwrap();

    assert_eq!(auth_state.events.len(), 1);
    assert_eq!(auth_state.events[0].tool_name, "read_file");
    assert!(auth_state.events[0].is_finished);

    assert_eq!(cache_state.events.len(), 1);
    assert_eq!(cache_state.events[0].tool_name, "write_file");
    assert!(cache_state.events[0].is_finished);
}

// ── Integration Test: Full Lifecycle ─────────────────────────────────────────

#[test]
fn full_sub_agent_lifecycle() {
    let mut app = App::new();

    // 1. Sub-agent starts with text
    app.handle_event(TuiEvent::Runtime(sub_text("root/auth", "starting analysis")));
    assert!(app.should_show_task_panel());
    assert_eq!(app.sub_agents.get("root/auth").unwrap().status, SubAgentStatus::Running);

    // 2. Sub-agent does tool calls
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/auth", "read_file")));
    app.handle_event(TuiEvent::Runtime(sub_tool_finished("root/auth", "read_file", "120 lines")));

    // 3. Sub-agent finishes
    app.handle_event(TuiEvent::Runtime(sub_run_finished("root/auth")));
    assert_eq!(app.sub_agents.get("root/auth").unwrap().status, SubAgentStatus::Done);

    // 4. Panel still visible (agent not yet cleaned up)
    assert!(app.should_show_task_panel());

    // 5. Simulate time passing (manually set completed_at to 4 seconds ago)
    app.sub_agents.get_mut("root/auth").unwrap().completed_at =
        Some(Instant::now() - Duration::from_secs(4));

    // 6. Cleanup removes the agent
    assert!(app.cleanup_completed_agents());
    assert!(!app.should_show_task_panel());
    assert!(app.sub_agents.is_empty());
    assert!(app.sub_agent_transcripts.is_empty());
}

// ── Edge Cases ───────────────────────────────────────────────────────────────

#[test]
fn empty_agent_id_ignored() {
    let mut app = App::new();

    // Send text with empty agent_id (should be treated as root)
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: "root text".to_string(),
        agent_id: Some("".to_string()),
        trace_id: None,
    }));

    // Should be in main transcript (since empty agent_id is treated as root)
    // Note: track_agent ignores empty agent_id, so text goes to main transcript
    // but might be buffered in stream
    if app.stream.has_pending_text() {
        // Text is in stream buffer, which is expected
        // It will be flushed on the next structural event
    } else {
        // Text was flushed to main transcript
        assert!(app.transcript.output.iter().any(|l| l.text.contains("root text")));
    }
    assert!(app.sub_agent_transcripts.is_empty());
}

#[test]
fn duplicate_agent_id_merges() {
    let mut app = App::new();

    // Send multiple events from same agent, with the child's tool call as
    // the flush point between them (child deltas commit on child lifecycle
    // events, not on the root's flush_pending).
    app.handle_event(TuiEvent::Runtime(sub_text("root/auth", "first")));
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/auth", "read_file")));
    app.handle_event(TuiEvent::Runtime(sub_text("root/auth", "second")));
    app.handle_event(TuiEvent::Runtime(sub_run_finished("root/auth")));

    // All in the same transcript, in order
    let lines = app.sub_agent_transcripts.get("root/auth").unwrap();
    assert_eq!(lines.len(), 4);
    // Note: stream adds [agent_id] prefix to text
    assert!(lines[0].text.contains("first"));
    assert!(lines[1].text.contains("read_file"));
    assert!(lines[2].text.contains("second"));
    assert!(lines[3].text.contains("done"));
}

#[test]
fn cleanup_with_no_completed_at() {
    let mut app = App::new();

    // Insert agent with Done status but no completed_at (edge case)
    let mut state = mock_sub_agent("auth", SubAgentStatus::Done);
    state.completed_at = None;
    app.sub_agents.insert("root/auth".to_string(), state);

    // Cleanup should not remove it
    assert!(!app.cleanup_completed_agents());
    assert!(app.sub_agents.contains_key("root/auth"));
}

// ── Panel Rendering (activity column + timer freeze, 20260904 UX batch) ─────

/// Insert a SubAgentState with a preset tool event.
fn insert_agent_with_event(app: &mut App, agent_id: &str, name: &str, tool: &str, finished: bool) {
    let mut state = mock_sub_agent(name, SubAgentStatus::Running);
    state.events.push(ToolEvent {
        tool_name: tool.to_string(),
        summary: String::new(),
        is_finished: finished,
    });
    app.sub_agents.insert(agent_id.to_string(), state);
}

#[test]
fn panel_shows_frozen_time_for_done_agents() {
    let mut app = App::new();
    let mut state = mock_completed_agent("auth", 10);
    // Started 65 s ago, finished 10 s ago → final runtime 55 s, frozen.
    state.started_at = Instant::now() - Duration::from_secs(65);
    state.completed_at = Some(Instant::now() - Duration::from_secs(10));
    app.sub_agents.insert("root/auth".to_string(), state);

    let snap = crate::ui::render::snapshot_text(&mut app, 100, 30);
    assert!(
        snap.contains("│   55s"),
        "done agent's runtime must freeze at completed-started, got:\n{snap}"
    );
}

#[test]
fn panel_activity_column_shows_latest_tool() {
    let mut app = App::new();
    insert_agent_with_event(&mut app, "root/auth", "auth", "read_file", false);

    let snap = crate::ui::render::snapshot_text(&mut app, 100, 30);
    assert!(
        snap.contains("→ read_file"),
        "panel must show the in-flight tool, got:\n{snap}"
    );

    // Finished → the checkmark form.
    app.handle_event(TuiEvent::Runtime(
        sub_tool_finished("root/auth", "read_file", "10 lines"),
    ));
    let snap = crate::ui::render::snapshot_text(&mut app, 100, 30);
    assert!(
        snap.contains("✓ read_file"),
        "finished tool must show as ✓, got:\n{snap}"
    );
}

#[test]
fn panel_columns_align_with_long_names() {
    let mut app = App::new();
    insert_agent_with_event(
        &mut app,
        "root/analyze-deepseek-harness",
        "analyze-deepseek-harness",
        "read_file",
        false,
    );
    insert_agent_with_event(&mut app, "root/pi", "pi", "read_file", false);

    let snap = crate::ui::render::snapshot_text(&mut app, 100, 30);
    // Name column capped at 16 → the long name is ellipsized instead of
    // shoving the later columns out of alignment.
    assert!(snap.contains("analyze-deepsee…"), "got:\n{snap}");
    // Both rows' time column (│) must sit at the same character position.
    let cols: Vec<usize> = snap
        .lines()
        .filter(|l| l.contains("analyze-deepsee…") || l.contains(" pi "))
        .filter_map(|l| l.find('│'))
        .collect();
    assert_eq!(cols.len(), 2, "both panel rows expected, got:\n{snap}");
    assert_eq!(cols[0], cols[1], "time column must align, got:\n{snap}");
}

// ── Visibility Guarantees (session 20260903 fan-in) ──────────────────────────
// The root turn ends while children still run (the fan-in norm), so between
// turns child events keep flowing through the persistent bus subscription.
// They must not clobber the root's Waiting status, and finished children's
// transcripts must survive until the root is Idle and the user is not reading.

#[test]
fn cleanup_skipped_while_root_waiting() {
    let mut app = App::new();

    // A child finished 10s ago but the root is Waiting for its siblings.
    insert_completed_agent(&mut app, "root/auth", "auth", 10);
    app.sub_agent_transcripts.insert(
        "root/auth".to_string(),
        vec![OutputLine {
            text: "partial progress".to_string(),
            kind: LineKind::Normal,
            spans: None,
            original: None,
            detail: None,
        }],
    );
    app.status = AgentStatus::Waiting { running: 2 };

    // Cleanup must not reap: the transcript is the only record of what the
    // sub-agent did, and the turn may resume at any moment.
    assert!(!app.cleanup_completed_agents());
    assert!(app.sub_agents.contains_key("root/auth"));
    assert!(app.sub_agent_transcripts.contains_key("root/auth"));
}

#[test]
fn child_streaming_does_not_clobber_waiting_status() {
    let mut app = App::new();

    // Root turn ended, two children still running.
    app.status = AgentStatus::Waiting { running: 2 };
    insert_mock_agent(&mut app, "root/auth", "auth", SubAgentStatus::Running);

    // Child text/thought/tool events arrive via the persistent subscription.
    app.handle_event(TuiEvent::Runtime(sub_text("root/auth", "reading files")));
    app.handle_event(TuiEvent::Runtime(sub_tool_started("root/auth", "read_file")));
    app.handle_event(TuiEvent::Runtime(sub_tool_finished("root/auth", "read_file", "10 lines")));

    // Root status must stay Waiting; only root events drive it.
    assert_eq!(
        app.status,
        AgentStatus::Waiting { running: 2 },
        "child events must not flip the root back to Running"
    );

    // ...but the child's activity still lands in its transcript.
    let sub_lines = app.sub_agent_transcripts.get("root/auth").unwrap();
    assert!(sub_lines.iter().any(|l| l.text.contains("read_file")));
}

// ── Phase 5: lifecycle snapshot reconciliation ───────────────────────────────

use agent_works::multi_agent::registry::{AgentSnapshot, RegistrySnapshot};

/// A snapshot with a single agent in the given derived status.
fn snap(path: &str, status: &str) -> std::sync::Arc<RegistrySnapshot> {
    std::sync::Arc::new(RegistrySnapshot {
        agents: vec![AgentSnapshot {
            path: path.to_string(),
            status: status.to_string(),
            running_secs: None,
            last_activity_secs: None,
            tool_calls: 0,
            task: None,
            pending_results: 0,
        }],
    })
}

#[test]
fn snapshot_creates_entry_at_spawn_before_first_tool_event() {
    // Phase 5's whole point: the event-driven path only discovered a child
    // at its first ToolCallStarted; the snapshot makes the panel entry
    // appear the moment the agent registers.
    let mut app = App::new();
    app.handle_event(TuiEvent::Lifecycle(snap("root/worker", "running")));

    let state = app.sub_agents.get("root/worker").expect("entry exists");
    assert_eq!(state.status, SubAgentStatus::Running);
    assert!(app.transcript.output.iter().any(|l| l.text.contains("[root/worker] started")));
}

#[test]
fn snapshot_running_to_done_sets_completed_at() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Lifecycle(snap("root/worker", "running")));
    app.handle_event(TuiEvent::Lifecycle(snap("root/worker", "done")));

    let state = app.sub_agents.get("root/worker").unwrap();
    assert_eq!(state.status, SubAgentStatus::Done);
    assert!(state.completed_at.is_some());
}

#[test]
fn snapshot_done_to_running_reopens_retasked_entry() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Lifecycle(snap("root/worker", "running")));
    app.handle_event(TuiEvent::Lifecycle(snap("root/worker", "done")));
    // send_message(trigger=true) re-tasks the agent → fact flips back.
    app.handle_event(TuiEvent::Lifecycle(snap("root/worker", "running")));

    let state = app.sub_agents.get("root/worker").unwrap();
    assert_eq!(state.status, SubAgentStatus::Running);
    assert!(state.completed_at.is_none(), "reopened entry must not be reaped");
}

#[test]
fn snapshot_unknown_done_agent_is_not_inserted() {
    // A freshly registered agent reads `done` until its first send_task —
    // inserting it would show a phantom done entry.
    let mut app = App::new();
    app.handle_event(TuiEvent::Lifecycle(snap("root/fresh", "done")));

    assert!(app.sub_agents.is_empty());
}

#[test]
fn snapshot_disappearance_marks_done_but_keeps_entry() {
    // Unregistration = normal exit after posting, or close — the panel must
    // keep the review window; the 3s reaper owns removal.
    let mut app = App::new();
    app.handle_event(TuiEvent::Lifecycle(snap("root/worker", "running")));
    app.handle_event(TuiEvent::Lifecycle(std::sync::Arc::new(RegistrySnapshot { agents: vec![] })));

    let state = app.sub_agents.get("root/worker").expect("entry kept");
    assert_eq!(state.status, SubAgentStatus::Done);
    assert!(state.completed_at.is_some());
}

#[test]
fn snapshot_keeps_waiting_count_in_sync() {
    // Snapshots are full views of the registry — both agents present until
    // one is done (a real snapshot would keep showing it until
    // unregister; here "done" rows are simply dropped from the view to
    // also exercise the disappearance backstop).
    let two = |a: &str, b: &str| {
        std::sync::Arc::new(RegistrySnapshot {
            agents: vec![
                AgentSnapshot {
                    path: "root/a".into(),
                    status: a.into(),
                    running_secs: None,
                    last_activity_secs: None,
                    tool_calls: 0,
                    task: None,
                    pending_results: 0,
                },
                AgentSnapshot {
                    path: "root/b".into(),
                    status: b.into(),
                    running_secs: None,
                    last_activity_secs: None,
                    tool_calls: 0,
                    task: None,
                    pending_results: 0,
                },
            ],
        })
    };

    let mut app = App::new();
    app.status = AgentStatus::Waiting { running: 2 };
    app.handle_event(TuiEvent::Lifecycle(two("running", "running")));
    assert!(matches!(app.status, AgentStatus::Waiting { running: 2 }));

    app.handle_event(TuiEvent::Lifecycle(two("done", "running")));
    assert!(matches!(app.status, AgentStatus::Waiting { running: 1 }));

    app.handle_event(TuiEvent::Lifecycle(two("done", "done")));
    assert!(matches!(app.status, AgentStatus::Idle));
}
