//! Tests for ratatui frame rendering.

use super::*;
use crate::ui::app::{
    AgentStatus, App, BackgroundTaskEntry, Phase, ResultTier, SubAgentState, SubAgentStatus,
    TuiEvent,
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use phi_agent::{RuntimeEvent, SessionId};
use phi_kernel_tools::background_shell::BackgroundTaskStatus;
use phi_tui::lines::{LineKind, OutputLine};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn populated_app() -> App {
    let mut app = App::new();
    app.push_system("phimint — welcome");
    for i in 0..60 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            detail: None,
            text: format!("streamed line {i}"),
            kind: LineKind::Normal,
            tool_state: None,
        });
    }
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        detail: None,
        text: "* [sub/1] read_file {\"path\":\"src/lib.rs\"}".into(),
        kind: LineKind::Tool,
        tool_state: None,
    });
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        detail: None,
        text: "  ⛔ execute_command denied".into(),
        kind: LineKind::Error,
        tool_state: None,
    });
    app.composer.insert_str("hello\nworld");
    app.running = true;
    app.status = AgentStatus::Running {
        phase: Phase::ToolCall {
            tool: "build".into(),
        },
    };
    app
}

#[test]
fn draw_does_not_panic_on_populated_state() {
    let backend = TestBackend::new(100, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = populated_app();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
}

#[test]
fn draw_empty_and_scrolled_states() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // Fresh (empty) state.
    let mut app = App::new();
    terminal.draw(|f| draw(f, &mut app)).unwrap();

    // Scrolled up (not following bottom).
    let mut app = App::new();
    for i in 0..100 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            detail: None,
            text: format!("long line {i}"),
            kind: LineKind::Normal,
            tool_state: None,
        });
    }
    app.viewport.follow_bottom = false;
    app.viewport.scroll_offset = 50;
    terminal.draw(|f| draw(f, &mut app)).unwrap();
}

#[test]
fn draw_cursor_mid_multibyte_line() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.composer.insert_str("héllo");
    app.composer.move_home();
    app.composer.move_right(); // cursor after 'h' (byte 1), before the 2-byte 'é'
    terminal.draw(|f| draw(f, &mut app)).unwrap();
}

#[test]
fn draw_with_pending_approval_popup() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.approval_queue.push_back(crate::approval::ApprovalItem {
        request: phi_agent::ApprovalRequest {
            title: "write_file".into(),
            message: "Write file: src/lib.rs".into(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Destructive,
            raw: None,
            source: None,
        },
        decision_tx: tx,
    });
    terminal.draw(|f| draw(f, &mut app)).unwrap();
}

#[test]
fn snapshot_text_captures_layout_and_popup() {
    let mut app = App::new();
    app.push_system("hello");
    app.composer.insert_str("do a thing");
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.approval_queue.push_back(crate::approval::ApprovalItem {
        request: phi_agent::ApprovalRequest {
            title: "write_file".into(),
            message: "Write file: src/lib.rs".into(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Sensitive,
            raw: None,
            source: None,
        },
        decision_tx: tx,
    });

    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("approval"), "popup title missing:\n{text}");
    assert!(
        text.contains("do a thing"),
        "composer content missing:\n{text}"
    );
    assert!(
        text.contains('\n'),
        "snapshot should be multi-line:\n{text}"
    );
}

#[test]
fn approval_popup_shows_sub_agent_source() {
    let mut app = App::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.approval_queue.push_back(crate::approval::ApprovalItem {
        request: phi_agent::ApprovalRequest {
            title: "write_file".into(),
            message: "Write file: src/lib.rs".into(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Sensitive,
            raw: None,
            source: Some("root/coder-1".into()),
        },
        decision_tx: tx,
    });
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        text.contains("root/coder-1"),
        "popup must name the requesting sub-agent:\n{text}"
    );
    assert!(
        text.contains("sub-agent"),
        "popup must label the source as a sub-agent:\n{text}"
    );
}

#[test]
fn approval_popup_without_source_renders_unchanged() {
    let mut app = App::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.approval_queue.push_back(crate::approval::ApprovalItem {
        request: phi_agent::ApprovalRequest {
            title: "write_file".into(),
            message: "Write file: src/lib.rs".into(),
            action_key: None,
            risk_level: phi_agent::RiskLevel::Sensitive,
            raw: None,
            source: None,
        },
        decision_tx: tx,
    });
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        !text.contains("sub-agent"),
        "no source → no sub-agent label (主 agent 渲染与现状一致):\n{text}"
    );
    assert!(text.contains("Write file: src/lib.rs"));
}

#[test]
fn window_range_shifts_by_scroll_offset() {
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
    assert_eq!(app.viewport.window_range(100, 30), 70..100);
    app.viewport.follow_bottom = false;
    app.viewport.scroll_offset = 10;
    assert_eq!(app.viewport.window_range(100, 30), 60..90);
    // Past the top clamps to the first `height` lines.
    app.viewport.scroll_offset = 1000;
    assert_eq!(app.viewport.window_range(100, 30), 0..30);
}

#[test]
fn streaming_tail_renders_in_snapshot() {
    let mut app = App::new();
    app.running = true;
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: "partial answer".to_string(),
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("partial answer"), "tail missing:\n{text}");
}

#[test]
fn buffer_to_text_skips_wide_char_continuation() {
    let backend = TestBackend::new(12, 2);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            f.render_widget(Paragraph::new("你好"), f.area());
        })
        .unwrap();
    let text = buffer_to_text(terminal.backend().buffer());
    assert!(text.contains("你好"), "got: {text:?}");
    assert!(
        !text.contains("你 好"),
        "wide chars should be adjacent, got: {text:?}"
    );
}

#[test]
fn snapshot_shows_sub_agent_strip() {
    let mut app = App::new();
    app.sub_agents.insert(
        "root/a".to_string(),
        SubAgentState {
            name: "a".to_string(),
            status: SubAgentStatus::Running,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: None,
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
    app.sub_agents.insert(
        "root/b".to_string(),
        SubAgentState {
            name: "b".to_string(),
            status: SubAgentStatus::Done,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: Some(std::time::Instant::now()),
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("* a"), "running marker missing:\n{text}");
    assert!(text.contains("+ b"), "done marker missing:\n{text}");
}

#[test]
fn snapshot_omits_strip_when_no_sub_agents() {
    let mut app = App::new();
    let text = snapshot_text(&mut app, 80, 24);
    assert!(!text.contains("* ["), "strip should be absent:\n{text}");
}

#[test]
fn task_panel_lists_only_sub_agents() {
    // Panel discipline (2026-09-19): background shell tasks stay out of the
    // task panel — they live as transcript tool-call records + the status-bar
    // counter. A running bg task next to a sub-agent must not add a row, a
    // count, or width to the panel; and a bg-only app must show no panel.
    let mut app = App::new();
    app.sub_agents.insert(
        "root/a".to_string(),
        SubAgentState {
            name: "a".to_string(),
            status: SubAgentStatus::Running,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: None,
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
    app.background_tasks.insert(
        "bg_aaaa1111".to_string(),
        BackgroundTaskEntry {
            id: "bg_aaaa1111".to_string(),
            command: "cargo test".to_string(),
            timeout_ms: 120_000,
            status: BackgroundTaskStatus::Running,
            started_at: std::time::Instant::now(),
            finished_at: None,
            reported: false,
            output_tail: String::new(),
            consumed: false,
        },
    );
    assert!(app.should_show_task_panel(), "sub-agent opens the panel");
    let text = snapshot_text(&mut app, 100, 40);
    assert!(
        text.contains("Tasks (1)"),
        "panel counts sub-agents only:\n{text}"
    );
    assert!(
        !text.contains("bg_aaaa1111"),
        "bg task must not appear in the panel:\n{text}"
    );

    // Bg-only: no panel at all (transcript + status bar carry the facts).
    let mut app = App::new();
    app.background_tasks.insert(
        "bg_aaaa1111".to_string(),
        BackgroundTaskEntry {
            id: "bg_aaaa1111".to_string(),
            command: "cargo test".to_string(),
            timeout_ms: 120_000,
            status: BackgroundTaskStatus::Running,
            started_at: std::time::Instant::now(),
            finished_at: None,
            reported: false,
            output_tail: String::new(),
            consumed: false,
        },
    );
    assert!(
        !app.should_show_task_panel(),
        "bg task alone must not open the panel"
    );
}

#[test]
fn draw_with_selection_and_context_menu_does_not_panic() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    for i in 0..20 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            detail: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
            tool_state: None,
        });
    }
    app.output_area = Some((0, 0, 80, 20));
    app.handle_mouse(MouseEventKind::Down(MouseButton::Left), 0, 2, 80, 24);
    app.handle_mouse(MouseEventKind::Drag(MouseButton::Left), 0, 5, 80, 24);
    app.handle_mouse(MouseEventKind::Down(MouseButton::Right), 10, 5, 80, 24);
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    // The selection text round-trips even after rendering.
    assert_eq!(app.selection_text(), "line 2\nline 3\nline 4\nline 5");
}

#[test]
fn draw_and_snapshot_show_mention_popup() {
    let mut app = App::new();
    let root = std::env::temp_dir().join(format!("phimint-render-mention-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.rs"), "x").unwrap();
    app.set_workspace_root(root);

    // Open the picker and type a partial prefix.
    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('m'), KeyModifiers::NONE);

    // Offscreen draw must not panic, and the snapshot shows the popup.
    let text = snapshot_text(&mut app, 100, 40);
    // No framed box, no redundant `@prefix` header row (the composer shows it).
    assert!(
        !text.contains("mention"),
        "popup title should be gone:\n{text}"
    );
    // Rows are prefixed with the selection gutter; the synthetic row is selected.
    assert!(
        text.contains("▸ » m"),
        "selected synthetic row missing:\n{text}"
    );
    assert!(text.contains("📄 main.rs"), "file row missing:\n{text}");
    // The band keeps a bottom separator as the boundary against the transcript.
    assert!(text.contains('─'), "separator missing:\n{text}");
}

#[test]
fn draw_and_snapshot_show_slash_popup() {
    let mut app = App::new();
    app.set_skill_summaries(vec![
        ("review".into(), "Pre-landing PR review".into()),
        ("commit".into(), "Generate a commit message".into()),
        ("code-review".into(), "Review current changes".into()),
    ]);

    // Open the picker by typing `/` at empty composer.
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    // Type a partial prefix.
    app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);

    let text = snapshot_text(&mut app, 100, 40);
    assert!(
        !text.contains("skills"),
        "popup title should be gone:\n{text}"
    );
    // "review" matches the prefix and is the selected first row.
    assert!(
        text.contains("▸ review"),
        "selected skill row missing:\n{text}"
    );
    assert!(text.contains("Pre-landing"), "description missing:\n{text}");
}

#[test]
fn banner_spans_render_palette_on_top_of_system_style() {
    use crate::banner::build;
    use std::path::Path;

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.push_banner(build(
        Path::new("/tmp/ws"),
        Path::new("/tmp/ws/.phimint/s/1/session.log"),
        "0.1.0",
        120,
    ));
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let w = 120usize;
    let cell = |x: usize, y: usize| buf.content()[y * w + x].clone();

    // Wordmark row 0: face glyph 0 is Logo(0) = dark stop #FF6A3D; the shadow
    // stroke (col 6, '╗') is LogoShadow #4A3226; the connector/padding (col 8)
    // is Default → falls back to the System base (DarkGray).
    assert_eq!(cell(0, 0).style().fg, Some(Color::Rgb(0xff, 0x6a, 0x3d)));
    assert_eq!(cell(6, 0).style().fg, Some(Color::Rgb(0x4a, 0x32, 0x26)));
    assert_eq!(cell(8, 0).style().fg, Some(Color::DarkGray));

    // Slogan row (index 7): "Forged…" is bold Slogan #FFB066.
    let slogan = cell(0, 7).style();
    assert_eq!(slogan.fg, Some(Color::Rgb(0xff, 0xb0, 0x66)));
    assert!(slogan.add_modifier.contains(Modifier::BOLD));

    // Ad row (index 8): "Built on" label #F2A462, then "phi-agent" Ad #FFC66E (bold).
    assert_eq!(cell(0, 8).style().fg, Some(Color::Rgb(0xf2, 0xa4, 0x62)));
    let ad_x = "Built on   ".len();
    let ad = cell(ad_x, 8).style();
    assert_eq!(ad.fg, Some(Color::Rgb(0xff, 0xc6, 0x6e)));
    assert!(ad.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn snapshot_shows_diff_block() {
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

    let text = snapshot_text(&mut app, 100, 40);
    assert!(
        text.contains("┌─ src/main.rs"),
        "diff header missing:\n{text}"
    );
    assert!(text.contains("@@"), "hunk header missing:\n{text}");
    assert!(text.contains("println"), "diff content missing:\n{text}");
    // Verify line numbers are present (hunk header has "1,3")
    assert!(text.contains("-1,3"), "line numbers missing:\n{text}");
}

// ── block rhythm + preview ladder (design §1 / §4) ──────────────────────────
// The layout may change; the content and its order may not. These pin the two
// rules that keep that true: spacers are a *rendering* decision that never
// enters history, and the ladder only ever decides how much of an intact
// payload to draw.

#[test]
fn block_spacers_are_render_time_only() {
    // A blank row goes *between* blocks (design §1). It must be a draw-time
    // decision — history, copy and session replay never acquire blank lines.
    let mut app = App::new();
    app.push_system("one");
    app.push_system("two");
    let before = app.transcript.output.len();
    let text = snapshot_text(&mut app, 80, 12);
    assert_eq!(
        app.transcript.output.len(),
        before,
        "rendering appended to history:\n{text}"
    );
    assert!(
        app.transcript
            .output
            .iter()
            .all(|l| !l.text.trim().is_empty()),
        "blank line in history: {:?}",
        app.transcript
            .output
            .iter()
            .map(|l| &l.text)
            .collect::<Vec<_>>()
    );

    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.first().map(|r| r.trim()), Some("one"), "\n{text}");
    assert_eq!(
        rows.get(1).map(|r| r.trim()),
        Some(""),
        "missing spacer between blocks:\n{text}"
    );
    assert_eq!(rows.get(2).map(|r| r.trim()), Some("two"), "\n{text}");
}

#[test]
fn call_and_its_folded_result_are_one_block() {
    // A tool block is the call *and* its results (design §1) — the blank row
    // goes between blocks, never inside one. A result that folds its payload is
    // still that call's result; the fold must not tear the block apart.
    let mut app = app_with_multi_row_result();
    for tier in [
        ResultTier::Compact,
        ResultTier::Default,
        ResultTier::Expanded,
    ] {
        app.result_tier = tier;
        let text = snapshot_text(&mut app, 80, 20);
        let rows: Vec<&str> = text.lines().collect();
        let call = rows
            .iter()
            .position(|r| r.contains("search_files"))
            .unwrap_or_else(|| panic!("call row missing at {tier:?}:\n{text}"));
        let result = rows
            .iter()
            .position(|r| r.trim_start().starts_with("<"))
            .unwrap_or_else(|| panic!("result row missing at {tier:?}:\n{text}"));
        assert!(
            result > call && result == call + 1,
            "call and result split apart at {tier:?} (call {call}, result {result}):\n{text}"
        );
    }
}

fn app_with_multi_row_result() -> App {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "search_files".to_string(),
        args_json: serde_json::json!({"pattern": "TODO"}).to_string(),
        agent_id: None,
        trace_id: None,
    }));
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: "search_files".to_string(),
        summary: "head line\nalpha\nbeta\ngamma".to_string(),
        denied: false,
        details: None,
        agent_id: None,
        trace_id: None,
    }));
    app
}

#[test]
fn tool_result_rides_the_preview_ladder() {
    // The tool's answer is stored whole and drawn 0 / 2 / all rows. Only the
    // amount on screen changes — never the text itself, never its order.
    let mut app = app_with_multi_row_result();

    // Compact: nothing of the payload, just the count of what is held back.
    assert_eq!(app.result_tier, ResultTier::Compact);
    let text = snapshot_text(&mut app, 80, 20);
    assert!(text.contains("  < head line"), "meta row missing:\n{text}");
    assert!(text.contains("... +3 lines"), "fold hint missing:\n{text}");
    assert!(
        !text.contains("alpha") && !text.contains("gamma"),
        "payload leaked while compact:\n{text}"
    );

    // Default: two rows, the rest counted.
    app.result_tier = ResultTier::Default;
    let text = snapshot_text(&mut app, 80, 20);
    assert!(text.contains("alpha") && text.contains("beta"), "\n{text}");
    assert!(!text.contains("gamma"), "tail shown at default:\n{text}");
    assert!(text.contains("... +1 lines"), "fold hint missing:\n{text}");

    // Expanded: the whole answer, no hint.
    app.result_tier = ResultTier::Expanded;
    let text = snapshot_text(&mut app, 80, 20);
    for row in ["alpha", "beta", "gamma"] {
        assert!(text.contains(row), "{row} missing when expanded:\n{text}");
    }
    assert!(
        !text.contains("... +"),
        "fold hint left behind when expanded:\n{text}"
    );

    // The ladder is display-only: the payload is still whole in history.
    let result = app
        .transcript
        .output
        .iter()
        .find(|l| l.kind == LineKind::ToolResult)
        .expect("result line present");
    match &result.detail {
        Some(phi_tui::lines::LineDetail::Folded { raw, .. }) => {
            assert_eq!(raw, "head line\nalpha\nbeta\ngamma");
        }
        other => panic!("result text must be kept whole, got {other:?}"),
    }
}

#[test]
fn ctrl_e_cycles_the_result_tier() {
    let mut app = App::new();
    assert_eq!(app.result_tier, ResultTier::Compact);
    app.handle_key(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(app.result_tier, ResultTier::Default);
    app.handle_key(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(app.result_tier, ResultTier::Expanded);
    app.handle_key(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(app.result_tier, ResultTier::Compact);
}

#[test]
fn tool_state_glyph_reflects_call_state() {
    // The `*` in `text` is a marker slot, not chrome: the renderer substitutes
    // the state glyph, so a settled call is legible without opening anything.
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "search_files".to_string(),
        args_json: serde_json::json!({"pattern": "TODO"}).to_string(),
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 20);
    let row = text
        .lines()
        .find(|r| r.contains("search_files"))
        .unwrap_or_else(|| panic!("tool row missing:\n{text}"));
    assert!(
        !row.trim_start().starts_with('*'),
        "marker slot leaked into display: {row:?}"
    );

    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: "search_files".to_string(),
        summary: "1 hit".to_string(),
        denied: false,
        details: None,
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 20);
    let row = text
        .lines()
        .find(|r| r.contains("search_files"))
        .unwrap_or_else(|| panic!("tool row missing:\n{text}"));
    assert!(
        row.trim_start().starts_with('o'),
        "done glyph missing: {row:?}"
    );

    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "execute_command".to_string(),
        args_json: serde_json::json!({"command": "rm -rf /"}).to_string(),
        agent_id: None,
        trace_id: None,
    }));
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: "execute_command".to_string(),
        summary: String::new(),
        denied: true,
        details: None,
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 20);
    let row = text
        .lines()
        .find(|r| r.contains("execute_command") && r.trim_start().starts_with('x'))
        .unwrap_or_else(|| panic!("denied glyph missing:\n{text}"));
    assert!(row.trim_start().starts_with('x'));
}

#[test]
fn edit_diff_never_folds_at_any_tier() {
    // Red line from the design review: an edit *is* the evidence of what
    // changed, so no tier may drop a single line of it. The ladder is for
    // artifacts (a written file's content), never for a transformation.
    let mut app = App::new();
    let args = serde_json::json!({
        "path": "src/main.rs",
        "edits": [
            {
                "old_text": "fn main() {\n    println!(\"hello\");\n}",
                "new_text": "fn main() {\n    println!(\"world\");\n}"
            }
        ]
    });
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "edit_file".to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }));

    for tier in [
        ResultTier::Compact,
        ResultTier::Default,
        ResultTier::Expanded,
    ] {
        app.result_tier = tier;
        let text = snapshot_text(&mut app, 100, 30);
        assert!(text.contains("┌─ src/main.rs"), "{tier:?}:\n{text}");
        assert!(
            text.contains("println") && text.contains("-") && text.contains("+"),
            "diff content dropped at {tier:?}:\n{text}"
        );
        assert!(
            !text.contains("... +"),
            "a diff must never be folded: {tier:?}:\n{text}"
        );
    }
}

#[test]
fn write_create_rides_the_ladder_but_keeps_the_content() {
    // A created file is an artifact: its body folds onto the ladder. The
    // content itself is never rewritten or lost — expanding shows all of it.
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

    assert_eq!(app.result_tier, ResultTier::Compact);
    let text = snapshot_text(&mut app, 80, 20);
    assert!(
        text.contains("... +3 lines"),
        "written content should be folded when compact:\n{text}"
    );

    app.result_tier = ResultTier::Expanded;
    let text = snapshot_text(&mut app, 80, 20);
    for row in ["fn hello()", "println", "}"] {
        assert!(text.contains(row), "{row} missing when expanded:\n{text}");
    }
}

// ── CJK width-safety guard (session 20260908_a9f7a846) ──────────────────────
// CJK terminal fonts render symbols like ⏺ ✓ ● → ⚠ … — as double-width while
// the layout counts them single-width; the accumulated drift wrapped rows and
// pushed the composer off-screen. All chrome now uses ASCII; these tests keep
// it that way.

/// Symbols whose rendered width disagrees with `unicode-width` in CJK fonts.
/// Box drawing (─│╭) and block elements (█) are deliberately NOT listed:
/// CJK mono fonts keep those single-width (proven in the session above).
const CJK_WIDTH_UNSAFE: &[char] = &[
    '⏺', '⏸', '⏳', '✓', '✔', '✗', '✘', '●', '○', '→', '←', '⇒', '⇐', '⟳', '⚠', '❯', '×', '÷', '±',
    '≤', '≥', '≠', '≈', '≡', '…', '·', '—', '–', '•', '√', '§', '∑', '∏', '∫', '∂', '∇', '∞', '①',
    '▸',
];

#[test]
fn rendered_chrome_stays_cjk_width_safe() {
    let mut app = App::new();
    // Drive the real event pipeline so production format strings are exercised.
    let args = serde_json::json!({"path": "src/main.rs"});
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: "read_file".to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }));
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: "read_file".to_string(),
        summary: "10 lines".to_string(),
        denied: false,
        details: None,
        agent_id: None,
        trace_id: None,
    }));
    // Every status variant's chrome.
    let mut status_texts = Vec::new();
    status_texts.push(app.status_line());
    app.status = AgentStatus::Running {
        phase: Phase::Thinking,
    };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Running {
        phase: Phase::Streaming,
    };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Running {
        phase: Phase::ToolCall {
            tool: "edit_file".into(),
        },
    };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Running {
        phase: Phase::AwaitingApproval,
    };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Idle;
    status_texts.push(app.status_line());

    let text = snapshot_text(&mut app, 100, 30);
    let all = text + &status_texts.join("\n");
    let offenders: Vec<char> = all
        .chars()
        .filter(|c| CJK_WIDTH_UNSAFE.contains(c))
        .collect();
    assert!(
        offenders.is_empty(),
        "CJK-double-width glyphs in rendered chrome: {offenders:?}\n{all}"
    );
}

#[test]
fn chrome_sources_stay_cjk_width_safe() {
    // Scan the chrome-rendering sources (outside comments) for the symbols.
    const FILES: &[&str] = &[
        "src/ui/render.rs",
        "src/ui/app.rs",
        "src/ui/run.rs",
        "src/ui/task_panel.rs",
        "src/ui/child_results.rs",
        "src/ui/handlers/runtime.rs",
        "src/banner.rs",
        "src/ui/picker.rs",
    ];
    for file in FILES {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
        let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
        for (i, line) in source.lines().enumerate() {
            let code = line.split_once("//").map_or(line, |(code, _)| code);
            let offenders: Vec<char> = code
                .chars()
                .filter(|c| CJK_WIDTH_UNSAFE.contains(c))
                .collect();
            assert!(
                offenders.is_empty(),
                "{file}:{} renders CJK-double-width glyphs {offenders:?}: {line}",
                i + 1
            );
        }
    }
}

#[test]
fn thinking_panel_renders_title_and_stays_out_of_transcript() {
    let mut app = App::new();
    app.push_system("hello");
    let body = "analyze ".repeat(120); // 960 chars → ~320 tok
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: body,
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 24);
    // Anchor on the panel TITLE ("thinking - …"), not the status bar's
    // "thinking..." spinner line.
    assert!(text.contains("thinking -"), "panel title missing:\n{text}");
    assert!(text.contains("320 tok"), "token estimate missing:\n{text}");
    // The panel must NOT feed the transcript: committed lines unchanged.
    assert_eq!(app.transcript.len(), 1);
}

#[test]
fn thinking_panel_hidden_on_short_terminal() {
    let mut app = App::new();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(120),
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 10); // height < 12 → no panel
    assert!(!text.contains("thinking -"), "panel should hide:\n{text}");
}

#[test]
fn thinking_panel_follows_focused_child() {
    let mut app = App::new();
    app.sub_agents.insert(
        "root/searcher".to_string(),
        SubAgentState {
            name: "searcher".to_string(),
            status: SubAgentStatus::Running,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: None,
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "search ".repeat(100),
        agent_id: Some("root/searcher".to_string()),
        trace_id: None,
    }));
    app.task_panel.focus = FocusTarget::TaskList(0);
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        text.contains("thinking - root/searcher"),
        "agent name in panel title:\n{text}"
    );
    // Title owns attribution — the body must not repeat the id prefix.
    assert!(
        !text.contains("[root/searcher]"),
        "body must not double-attribute the agent:\n{text}"
    );
}

#[test]
fn committed_thought_folds_and_expands() {
    let mut app = App::new();
    let raw = "line one\nline two\nline three\nline four\nline five";
    app.transcript.push(OutputLine {
        spans: None,
        // Folded thoughts deliberately carry no `original` — `detail.raw` is
        // the single source of truth (`rewrap_output` would explode an
        // `original` on resize and drop `detail`). Construct the production
        // shape flush_thought emits.
        original: None,
        detail: Some(phi_tui::lines::LineDetail::Folded {
            raw: raw.to_string(),
            line_count: 5,
            char_count: raw.chars().count(),
            meta_head: phi_tui::lines::MetaHead::None,
        }),
        text: "line one".into(),
        kind: LineKind::Thought,
        tool_state: None,
    });
    let text = snapshot_text(&mut app, 80, 24);
    // Line count + estimate (spec: the summary carries a line count and an
    // estimated tok count). No leading `>` — that marker belongs to user
    // lines, and a summary starting with `>` reads as something you typed.
    assert!(
        text.contains("thinking - 5 lines - ~16 tok"),
        "summary missing:\n{text}"
    );
    assert!(
        !text.contains("line three"),
        "raw leaked while folded:\n{text}"
    );
    app.show_thoughts = true;
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        text.contains("line three"),
        "expanded text missing:\n{text}"
    );
    assert!(
        !text.contains("thinking - 5 lines"),
        "summary gone when expanded:\n{text}"
    );
}

#[test]
fn thinking_panel_scrolls_away_with_history() {
    // The panel is inline at the flow tail: reviewing history must scroll it
    // away with the content, and follow-bottom must bring it back.
    let mut app = populated_app();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(120),
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 24);
    // Anchor on the title's token estimate — unique to the panel in this
    // fixture (the status bar says "thinking...", fold summaries carry a
    // different count, and `thinking -` would false-match a future fold).
    assert!(text.contains("~320 tok"), "panel at flow tail:\n{text}");
    for _ in 0..80 {
        app.scroll_up();
    }
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        !text.contains("~320 tok"),
        "panel must scroll away with content:\n{text}"
    );
    app.scroll_to_bottom();
    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("~320 tok"), "panel back at tail:\n{text}");
}

#[test]
fn thinking_panel_hidden_when_focused_child_has_no_thought() {
    // Deviation ① focus-first lock: root is thinking, but the FOCUSED child
    // is idle → the panel must hide. (The draft root-first logic would show
    // the root panel here; that divergence is deliberate — the panel always
    // reports the stream the user is looking at.)
    let mut app = App::new();
    app.sub_agents.insert(
        "root/searcher".to_string(),
        SubAgentState {
            name: "searcher".to_string(),
            status: SubAgentStatus::Running,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: None,
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(120),
        agent_id: None,
        trace_id: None,
    }));
    app.task_panel.focus = FocusTarget::TaskList(0);
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        !text.contains("thinking -"),
        "root panel must not show while an idle child is focused:\n{text}"
    );
}

#[test]
fn thinking_panel_hidden_when_output_pane_too_short() {
    let shot = |w: u16, h: u16| {
        let mut app = App::new();
        app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
            session_id: SessionId::new(1),
            text: "analyze ".repeat(120),
            agent_id: None,
            trace_id: None,
        }));
        snapshot_text(&mut app, w, h)
    };
    // 80x12: output pane = 12 - composer(3) - status(1) = 8 rows — the box
    // alone would fill it; the fit guard hides the panel so history survives.
    let text = shot(80, 12);
    assert!(
        !text.contains("thinking -"),
        "panel must hide, not occlude:\n{text}"
    );
    // 80x14: output pane = 10 = box(8) + history(2) — exact boundary shows.
    let text = shot(80, 14);
    assert!(
        text.contains("thinking -"),
        "boundary (8+2) must show:\n{text}"
    );
    // 80x20: output pane = 16 ≥ box(8) + history(2) — panel shows.
    let text = shot(80, 20);
    assert!(
        text.contains("thinking -"),
        "panel should fit with history rows:\n{text}"
    );
}

#[test]
fn thinking_panel_blank_when_cut_by_window_edge() {
    // The panel is one unit: a box cut by the window edge is left blank —
    // never half-overlayed at the wrong height.
    let mut app = populated_app();
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(120),
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("thinking -"), "full panel at tail:\n{text}");
    assert!(text.contains("analyze"), "panel body at tail:\n{text}");
    app.scroll_wheel_up(); // 1-row cut → partial → blank
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        !text.contains("thinking -") && !text.contains("analyze"),
        "cut panel must be blank (no title, no body, no half box):\n{text}"
    );
}

#[test]
fn thinking_panel_anchors_row_position_below_history() {
    // The box sits exactly where its placeholder rows land: directly under the
    // committed history. Three separate `push_system` calls are three system
    // *blocks* (design §1 — a block is one message, not one kind), so they
    // interleave with render-time spacers and the title row lands at 5.
    let mut app = App::new();
    app.push_system("one");
    app.push_system("two");
    app.push_system("three");
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(120),
        agent_id: None,
        trace_id: None,
    }));
    let text = snapshot_text(&mut app, 80, 20);
    let rows: Vec<&str> = text.lines().collect();
    // Block rhythm: content, spacer, content, spacer, content, then the panel.
    assert_eq!(rows.first().map(|r| r.trim()), Some("one"), "\n{text}");
    assert_eq!(rows.get(1).map(|r| r.trim()), Some(""), "\n{text}");
    assert_eq!(rows.get(2).map(|r| r.trim()), Some("two"), "\n{text}");
    assert_eq!(rows.get(3).map(|r| r.trim()), Some(""), "\n{text}");
    assert_eq!(rows.get(4).map(|r| r.trim()), Some("three"), "\n{text}");

    let title = rows
        .iter()
        .position(|r| r.contains('╭') && r.contains("thinking -"))
        .unwrap_or_else(|| panic!("panel title row missing:\n{text}"));
    assert_eq!(
        title, 5,
        "panel title row must sit right below history, got row {title}:\n{text}"
    );
}

#[test]
fn history_position_holds_when_thought_opens() {
    // Scroll anchoring: opening the panel's 8 placeholder rows at the tail
    // must not yank a mid-read history view toward the tail.
    let mut app = populated_app();
    let _ = snapshot_text(&mut app, 80, 24); // establish viewport sizes
    for _ in 0..3 {
        app.scroll_up();
    }
    let before = snapshot_text(&mut app, 80, 24);
    let head = before.lines().next().unwrap_or_default().to_string();
    assert!(
        !head.is_empty(),
        "scrolled history should show content:\n{before}"
    );
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(120),
        agent_id: None,
        trace_id: None,
    }));
    let after = snapshot_text(&mut app, 80, 24);
    let head_after = after.lines().next().unwrap_or_default();
    assert_eq!(
        head, head_after,
        "history head must not jump when the thought panel opens:\n{after}"
    );
}

#[test]
fn expanded_thought_rewraps_at_current_width() {
    // Ctrl+O expansion re-wraps `detail.raw` at the live width — a 200-char
    // run must split into rows, not hard-clip or overflow one row.
    let mut app = App::new();
    let long = "x".repeat(200); // hard-wraps into 3 rows at width 80
    let raw = format!("line one\n{long}\nline three");
    let char_count = raw.chars().count();
    app.transcript.push(OutputLine {
        spans: None,
        // Production folded shape (see committed_thought_folds_and_expands).
        original: None,
        detail: Some(phi_tui::lines::LineDetail::Folded {
            raw: raw.clone(),
            line_count: 3,
            char_count,
            meta_head: phi_tui::lines::MetaHead::None,
        }),
        text: "line one".into(),
        kind: LineKind::Thought,
        tool_state: None,
    });
    app.show_thoughts = true;
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        text.contains(&"x".repeat(80)),
        "long line must hard-wrap:\n{text}"
    );
    assert!(
        !text.contains(&"x".repeat(200)),
        "un-wrapped 200-char run must not survive:\n{text}"
    );
    assert!(
        text.contains("line three"),
        "content after the long line:\n{text}"
    );
}

#[test]
fn history_head_survives_fold_reflow_above_viewport() {
    // Ctrl+O reflows the MIDDLE of the flow: a folded thought above the window
    // head grows from 1 summary row to 8. The reader's place must hold by
    // CONTENT — holding the raw head index would silently swap in content
    // from above (the tail-only anchoring gap).
    let mut app = App::new();
    let raw = (0..8)
        .map(|i| format!("thought line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.transcript.push(OutputLine {
        spans: None,
        // Production folded shape (see committed_thought_folds_and_expands).
        original: None,
        detail: Some(phi_tui::lines::LineDetail::Folded {
            raw: raw.clone(),
            line_count: 8,
            char_count: raw.chars().count(),
            meta_head: phi_tui::lines::MetaHead::None,
        }),
        text: "thought line 0".into(),
        kind: LineKind::Thought,
        tool_state: None,
    });
    for i in 0..60 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            detail: None,
            text: format!("filler row {i}"),
            kind: LineKind::Normal,
            tool_state: None,
        });
    }
    let _ = snapshot_text(&mut app, 80, 24); // establish viewport sizes
    assert!(app.scroll_up(), "fixture must be scrollable");
    let before = snapshot_text(&mut app, 80, 24);
    let head = before.lines().next().unwrap_or_default().to_string();
    assert!(
        head.contains("filler row"),
        "head must sit below the folded thought:\n{before}"
    );
    assert!(
        !before.contains("thought line 7"),
        "raw hidden while folded"
    );

    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL); // +7 rows above head

    let after = snapshot_text(&mut app, 80, 24);
    assert_eq!(
        head,
        after.lines().next().unwrap_or_default(),
        "mid-flow fold reflow must not move the reader:\n{after}"
    );
    // Positive control: the reflow really grew the flow — otherwise the
    // equality above would pass vacuously on a no-op toggle.
    app.viewport.scroll_to_top();
    let top = snapshot_text(&mut app, 80, 24);
    assert!(
        top.contains("thought line 7"),
        "expansion must be visible from the top:\n{top}"
    );
}

/// Fixture for the two expanded-thought scroll tests: one long thought (40
/// visual rows when expanded — taller than the ~20-row output pane) followed
/// by a short answer tail. Expanded thinking is ONE output row, so the window
/// head can sit anywhere inside that 40-row block.
fn expanded_thought_with_tail() -> App {
    let mut app = App::new();
    let raw = (0..40)
        .map(|i| format!("thought line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.transcript.push(OutputLine {
        spans: None,
        original: None,
        detail: Some(phi_tui::lines::LineDetail::Folded {
            raw: raw.clone(),
            line_count: 40,
            char_count: raw.chars().count(),
            meta_head: phi_tui::lines::MetaHead::None,
        }),
        text: "thought line 0".into(),
        kind: LineKind::Thought,
        tool_state: None,
    });
    for i in 0..6 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            detail: None,
            text: format!("filler row {i}"),
            kind: LineKind::Normal,
            tool_state: None,
        });
    }
    let _ = snapshot_text(&mut app, 80, 24); // establish viewport sizes
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL); // expand
    app
}

#[test]
fn one_row_scroll_up_off_bottom_stays_inside_expanded_thought() {
    // Under follow-bottom the window head lands mid-block (max_offset sits
    // inside the 40-row thought). A one-row scroll up must move exactly one
    // row — resolving the head to the block's first row flings the view a
    // whole thought upward (session 20260923: "can't scroll to the bottom").
    let mut app = expanded_thought_with_tail();

    let bottom = snapshot_text(&mut app, 80, 24);
    let head = bottom.lines().next().unwrap_or_default().to_string();
    let n: usize = head
        .trim()
        .strip_prefix("thought line ")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bottom head must sit mid-thought:\n{bottom}"));
    assert!(
        n >= 1,
        "bottom head must leave room for one row up:\n{bottom}"
    );
    assert!(
        bottom.contains("filler row 5"),
        "bottom must show the flow tail:\n{bottom}"
    );

    app.viewport.scroll_up(1);
    let after = snapshot_text(&mut app, 80, 24);
    assert_eq!(
        after.lines().next().unwrap_or_default().trim(),
        format!("thought line {}", n - 1),
        "one row up must move exactly one row (no block-top snap):\n{after}"
    );
}

#[test]
fn wheel_down_through_expanded_thought_reaches_tail() {
    // The user's literal gesture (session 20260923): wheel up off the bottom,
    // then wheel back down to the answer. The anchor re-resolves every frame,
    // so the test must render between ticks like the real event loop —
    // otherwise the scroll methods alone accumulate and mask the bug.
    let mut app = expanded_thought_with_tail();

    app.scroll_wheel_up();
    let _ = snapshot_text(&mut app, 80, 24);
    for _ in 0..80 {
        app.scroll_wheel_down();
        let _ = snapshot_text(&mut app, 80, 24);
    }

    let end = snapshot_text(&mut app, 80, 24);
    assert!(
        end.contains("filler row 5"),
        "wheel-down must reach the flow tail:\n{end}"
    );
}

#[test]
fn loose_mode_streams_thought_inline_without_box() {
    // Ctrl+O ON ("loose"): the in-flight thought prints into the flow like
    // any other content — no TailPanel box, no fold chrome. History thoughts
    // expand via the same toggle (committed_thought_folds_and_expands).
    let mut app = App::new();
    app.push_system("hello");
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(50),
        agent_id: None,
        trace_id: None,
    }));
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL); // loose
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        text.contains("analyze"),
        "thought streams into the flow:\n{text}"
    );
    // Panel absence is anchored on its title row (`thinking -`): the composer
    // is a rounded box too, so the bare `╭` glyph proves nothing here.
    assert!(
        !text.contains("thinking -"),
        "no panel box/title in loose mode:\n{text}"
    );
}

#[test]
fn loose_mode_child_thought_carries_agent_prefix() {
    // Boxed mode names the author in the panel title; loose mode has no
    // title, so attribution rides the first streamed row (tool-line style).
    let mut app = App::new();
    app.sub_agents.insert(
        "root/searcher".to_string(),
        SubAgentState {
            name: "searcher".to_string(),
            status: SubAgentStatus::Running,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: None,
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "search ".repeat(50),
        agent_id: Some("root/searcher".to_string()),
        trace_id: None,
    }));
    app.task_panel.focus = FocusTarget::TaskList(0);
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL); // loose
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        text.contains("[root/searcher]"),
        "author on the first row:\n{text}"
    );
    assert!(text.contains("search"), "thought body streams:\n{text}");
}

#[test]
fn ctrl_o_mid_thought_swaps_box_and_stream() {
    // One toggle, two coupled flips: the in-flight thought (box <-> inline
    // stream) and history thoughts (fold <-> expand) change together.
    let mut app = App::new();
    app.push_system("hello");
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "analyze ".repeat(50),
        agent_id: None,
        trace_id: None,
    }));
    let boxed = snapshot_text(&mut app, 80, 24);
    assert!(boxed.contains("thinking -"), "default is boxed:\n{boxed}");
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    let loose = snapshot_text(&mut app, 80, 24);
    assert!(
        loose.contains("analyze") && !loose.contains("thinking -"),
        "loose streams without the panel:\n{loose}"
    );
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    let boxed = snapshot_text(&mut app, 80, 24);
    assert!(
        boxed.contains("thinking -"),
        "box comes back around the same thought:\n{boxed}"
    );
}

#[test]
fn loose_commit_keeps_agent_prefix_after_flush() {
    // The flush seam: loose rows carry `[{agent}] ` and `detail.raw` is baked
    // with the same prefix, so when the segment commits and renders expanded
    // the author does not vanish mid-stream.
    let mut app = App::new();
    app.sub_agents.insert(
        "root/searcher".to_string(),
        SubAgentState {
            name: "searcher".to_string(),
            status: SubAgentStatus::Running,
            files: Vec::new(),
            started_at: std::time::Instant::now(),
            completed_at: None,
            last_tool_at: std::time::Instant::now(),
            events: Vec::new(),
        },
    );
    app.task_panel.focus = FocusTarget::TaskList(0);
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: "search ".repeat(50),
        agent_id: Some("root/searcher".to_string()),
        trace_id: None,
    }));
    app.handle_key(KeyCode::Char('o'), KeyModifiers::CONTROL); // loose BEFORE commit
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: "answer ".repeat(10),
        agent_id: Some("root/searcher".to_string()),
        trace_id: None,
    })); // kind flip flushes the thought
    let text = snapshot_text(&mut app, 80, 24);
    assert!(
        text.contains("[root/searcher]"),
        "expanded committed thought keeps the author at the seam:\n{text}"
    );
    assert!(
        text.contains("search"),
        "thought body still visible (expanded):\n{text}"
    );
}

#[test]
fn mention_lines_mark_kinds_and_elide_long_names() {
    let mut app = App::new();
    let root = std::env::temp_dir().join(format!("phimint-mention-lines-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("deep/nested/dir")).unwrap();
    std::fs::write(
        root.join("deep/nested/dir/a_very_long_test_file_name.rs"),
        "x",
    )
    .unwrap();
    app.set_workspace_root(root);

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    for c in "deep/nested/dir/a_very".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
    }

    let m = app.mention().expect("picker open");
    let rows = mention_lines(m, 30);
    let row_text =
        |i: usize| -> String { rows[i].spans.iter().map(|s| s.content.as_ref()).collect() };

    // Row 0 is the synthetic "use what I typed" row: the typed prefix itself
    // (21 columns) still fits the 26-column budget, so it is left whole.
    assert_eq!(row_text(0), "» deep/nested/dir/a_very");

    // Row 1 is the matching file: its 29-column name is elided into the
    // 25-column budget (30 − gutter 2 − marker 3).
    let file = row_text(1);
    assert!(file.starts_with("📄 "), "kind marker missing: {file:?}");
    assert!(file.ends_with("_name.rs"), "filename lost: {file:?}");
    assert!(file.contains("..."), "expected an elision: {file:?}");
    assert!(unicode_width::UnicodeWidthStr::width(file.as_str()) <= 28);
}

#[test]
fn slash_lines_elide_descriptions_to_the_band_width() {
    let mut app = App::new();
    app.set_skill_summaries(vec![(
        "review".into(),
        "Pre-landing PR review with a deliberately long description".into(),
    )]);
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);

    let s = app.slash().expect("picker open");
    let rows = slash_lines(s, 60, ColorScheme::Dark);
    let text: String = rows[0].spans.iter().map(|sp| sp.content.as_ref()).collect();
    assert!(text.starts_with("review"), "name column missing: {text:?}");
    assert!(text.ends_with("..."), "description not elided: {text:?}");
    assert!(unicode_width::UnicodeWidthStr::width(text.as_str()) <= 58);
}

#[test]
fn slash_lines_pads_the_name_column_in_display_columns() {
    let mut app = App::new();
    // Skill names come from free-form frontmatter `name:`, not a validated
    // ASCII slug, so a multi-byte name must not overshoot the name column.
    app.set_skill_summaries(vec![("代码审查工具".into(), "desc".into())]);
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);

    let s = app.slash().expect("picker open");
    let rows = slash_lines(s, 60, ColorScheme::Dark);
    // The name span alone must be exactly NAME_W (24) display columns. Padding
    // by char count would give 24 chars = 30 columns here.
    let name_span = &rows[0].spans[0];
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(name_span.content.as_ref()),
        24,
        "name column drifted: {:?}",
        name_span.content.as_ref()
    );
    // And the whole row still fits the band.
    let text: String = rows[0].spans.iter().map(|sp| sp.content.as_ref()).collect();
    assert!(
        unicode_width::UnicodeWidthStr::width(text.as_str()) <= 58,
        "row overflow: {text:?}"
    );
}

#[test]
fn slash_lines_fit_a_band_narrower_than_the_name_column() {
    let mut app = App::new();
    app.set_skill_summaries(vec![("review".into(), "a description".into())]);
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);

    let s = app.slash().expect("picker open");
    // A 20-column band is narrower than GUTTER_W + NAME_W + GAP_W (28). The
    // fixed 24-col name column built a 28-col row that `Paragraph` then
    // hard-clipped. Reachable via `ui.popup.width: 20`.
    let rows = slash_lines(s, 20, ColorScheme::Dark);
    let text: String = rows[0].spans.iter().map(|sp| sp.content.as_ref()).collect();
    assert!(
        unicode_width::UnicodeWidthStr::width(text.as_str()) <= 18,
        "row overflows a 20-col band: {text:?}"
    );
    // And a degenerate 3-column band must not go negative either.
    let tiny = slash_lines(s, 3, ColorScheme::Dark);
    let tiny_text: String = tiny[0].spans.iter().map(|sp| sp.content.as_ref()).collect();
    assert!(
        unicode_width::UnicodeWidthStr::width(tiny_text.as_str()) <= 1,
        "row overflows a 3-col band: {tiny_text:?}"
    );
}

#[test]
fn slash_lines_keeps_the_selected_row_two_tone() {
    let mut app = App::new();
    app.set_skill_summaries(vec![
        ("review".into(), "first".into()),
        ("qa".into(), "second".into()),
    ]);
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);

    let s = app.slash().expect("picker open");
    assert_eq!(s.selected_index(), 0, "expected the first row selected");
    let rows = slash_lines(s, 60, ColorScheme::Dark);
    // spans: [name, gap, desc]
    let fg = |row: usize, span: usize| rows[row].spans[span].style.fg;

    assert_eq!(fg(0, 0), Some(Color::White), "selected name lost its fg");
    // The description stays dim on the selected row, but lifts `faint` →
    // `lifted` so it survives the widget's selection background.
    assert_eq!(
        fg(0, 2),
        Some(Color::Gray),
        "selected description must not match the highlight background"
    );
    // Unselected row keeps the plain dim description.
    assert_eq!(
        fg(1, 2),
        Some(Color::DarkGray),
        "unselected description drifted"
    );
    assert_eq!(fg(1, 0), Some(Color::White), "unselected name drifted");
}

#[test]
fn light_scheme_flips_the_gray_and_truecolor_slots() {
    // The Dark palette is pinned by the tests above (`App::new()` is Dark);
    // this guards the other half: a light terminal must not wear dark chrome.
    assert_eq!(
        style_for(LineKind::System, ColorScheme::Light).fg,
        Some(theme::faint(ColorScheme::Light))
    );
    assert_eq!(
        style_for(LineKind::ToolResult, ColorScheme::Light).fg,
        Some(theme::muted(ColorScheme::Light))
    );
    assert_eq!(
        style_for(LineKind::Thought, ColorScheme::Light).fg,
        Some(theme::thought(ColorScheme::Light))
    );
    assert_eq!(
        style_for(LineKind::User, ColorScheme::Light).fg,
        Some(theme::user(ColorScheme::Light))
    );
    // And the split is real: Light does not fall back to the Dark values.
    for kind in [
        LineKind::System,
        LineKind::ToolResult,
        LineKind::Thought,
        LineKind::User,
    ] {
        assert_ne!(
            style_for(kind, ColorScheme::Light).fg,
            style_for(kind, ColorScheme::Dark).fg,
            "{kind:?} is identical in Dark and Light"
        );
    }
}

#[test]
fn popup_highlight_follows_the_scheme() {
    let mut app = App::new();
    // `App::new()` is Dark: the highlight is the dark selection chrome.
    assert_eq!(
        app.popup_style().highlight.bg,
        Some(theme::selection_bg(ColorScheme::Dark))
    );
    app.set_scheme(ColorScheme::Light);
    assert_eq!(
        app.popup_style().highlight.bg,
        Some(theme::selection_bg(ColorScheme::Light))
    );
    // Foreground stays the row's own — a forced fg would flatten the `/`
    // popup's two-tone name/description rows.
    assert_eq!(app.popup_style().highlight.fg, None);
}

/// Draw and return each row as `(symbol, fg, bg)` cells, so tests can pin
/// selection styling and not just the flattened text.
fn snapshot_cells(app: &mut App, width: u16, height: u16) -> Vec<Vec<(String, Color, Color)>> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("offscreen terminal");
    terminal.draw(|f| draw(f, app)).expect("offscreen draw");
    let buf = terminal.backend().buffer();
    let area = buf.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| {
                    let c = &buf[(x, y)];
                    (c.symbol().to_string(), c.fg, c.bg)
                })
                .collect()
        })
        .collect()
}

#[test]
fn empty_slash_shows_an_unselected_placeholder() {
    let mut app = App::new();
    app.set_skill_summaries(vec![
        ("review".into(), "first".into()),
        ("qa".into(), "second".into()),
    ]);
    app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
    // A prefix that matches nothing leaves the picker open with zero rows.
    for c in "zzz".chars() {
        app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    assert!(
        app.slash().is_some(),
        "picker must stay open on a dead prefix"
    );

    let rows = snapshot_cells(&mut app, 100, 40);
    let text_of =
        |r: &[(String, Color, Color)]| -> String { r.iter().map(|(s, _, _)| s.as_str()).collect() };
    let idx = rows
        .iter()
        .position(|r| text_of(r).contains("no matching skills"))
        .expect("placeholder row missing");
    let text = text_of(&rows[idx]);
    // The widget supplies the 2-column gutter, so the row pads itself by none —
    // with the old `"  no matching skills"` it rendered double-indented.
    assert!(
        text.starts_with("  no matching skills"),
        "placeholder gutter wrong: {text:?}"
    );
    // It is a notice, not a choice: no glyph of it may be highlighted.
    let highlighted: Vec<_> = rows[idx]
        .iter()
        .filter(|(sym, _, bg)| *bg == Color::DarkGray && !sym.trim().is_empty())
        .collect();
    assert!(
        highlighted.is_empty(),
        "placeholder highlighted as a selectable row: {highlighted:?}"
    );
}

#[test]
fn framed_style_draws_a_bordered_band() {
    use phi_tui::popup_list::PopupStyle;
    let mut app = App::new();
    let root = std::env::temp_dir().join(format!("phimint-render-framed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.rs"), "x").unwrap();
    app.set_workspace_root(root);
    app.set_popup_style(PopupStyle::framed(30, 9));

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('m'), KeyModifiers::NONE);

    let text = snapshot_text(&mut app, 100, 40);
    assert!(
        text.contains('╭') && text.contains('╰'),
        "border missing:\n{text}"
    );
    assert!(text.contains("📄 main.rs"), "file row missing:\n{text}");
    // `WidthSpec::Fixed(30)` must actually narrow the band: the top border row
    // is 30 columns (old popup width 64 — this is the assertion that fails
    // before the migration, so the test really pins the injection path).
    let border = text.lines().find(|l| l.contains('╭')).expect("border row");
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(border.trim_end()),
        30,
        "band width not applied:\n{text}"
    );
}

#[test]
fn framed_band_elides_rows_to_the_content_width() {
    use phi_tui::popup_list::PopupStyle;
    let mut app = App::new();
    let root = std::env::temp_dir().join(format!("phimint-framed-elide-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a_very_long_test_file_name.rs"), "x").unwrap();
    app.set_workspace_root(root);
    // A 30-column framed band has only 28 columns of content — the border eats
    // two. Rows budgeted against the outer band would be clipped by `Paragraph`
    // at the right edge, losing the filename tail instead of marking the cut.
    app.set_popup_style(PopupStyle::framed(30, 9));

    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);

    let text = snapshot_text(&mut app, 100, 40);
    let row = text.lines().find(|l| l.contains('📄')).expect("file row");
    // Strip the band's border cells so the assertions look at content only.
    let inner = row.trim().trim_matches('│');
    // Budgeted at 28 content columns the row is ≤28 wide and the elision keeps
    // the tail whole. Budgeted at 30 it would render 30 columns into a 28-wide
    // area and come out as `...name.` — the `.rs` would be clipped off.
    assert!(
        inner.ends_with(".rs"),
        "filename tail clipped instead of elided: {inner:?}\n{text}"
    );
    assert!(
        inner.contains("..."),
        "expected an elision: {inner:?}\n{text}"
    );
    assert!(
        unicode_width::UnicodeWidthStr::width(inner) <= 28,
        "row wider than the 28-col content area: {inner:?}\n{text}"
    );
}

// ── Task panel column accounting ─────────────────────────────────────────────
// The name / activity / files cells must be laid out in terminal columns. They
// used to mix three accountings — `chars().count()`, `format!("{:<w$}")` (chars)
// and `result.len()` (bytes) — and the bytes one slices mid-char.

#[test]
fn format_files_truncates_cjk_names_without_panicking() {
    // A basename over 20 bytes hit `&result[..max_width - 3]`, which is a byte
    // slice and panics mid-char ("byte index N is not a char boundary; it is
    // inside <a multi-byte char>"). That panic ran inside `render`, so it took
    // the whole TUI down.
    let files = vec!["测试文件名称很长的文件名.rs".to_string()];
    let out = format_files(&files, 20);
    assert!(out.ends_with("..."), "expected an elision: {out:?}");
    assert!(
        unicode_width::UnicodeWidthStr::width(out.as_str()) <= 20,
        "output wider than its column: {out:?}"
    );
}

#[test]
fn format_files_lists_up_to_two_basenames() {
    // Basenames only, joined by ", ", with "..." marking a longer list.
    let two = format_files(&["src/a.rs".into(), "src/b.rs".into()], 20);
    assert_eq!(two, "a.rs, b.rs");
    let three = format_files(&["a.rs".into(), "b.rs".into(), "c.rs".into()], 40);
    assert_eq!(three, "a.rs, b.rs...");
    // Short names pass through untouched.
    assert_eq!(format_files(&[], 20), "");
}

// ---------------------------------------------------------------------------
// README demo reel generator
// ---------------------------------------------------------------------------
//
// Writes ANSI frames (real colours, real rendering) for a scripted session so
// the README's GIF/screenshots are produced from the actual UI rather than a
// mockup. Not part of the test suite: run it explicitly with
//
//     PHIMINT_REEL_OUT=docs/assets/frames cargo test frame_reel -- --ignored
//
// The emitted `*.ans` files are then painted to PNG and assembled by the
// companion script (see the project notes). Ignored by default so `cargo test`
// never writes files.

/// SGR sequence for one cell's style, or an empty string when it matches
/// default terminal styling.
fn cell_sgr(cell: &ratatui::buffer::Cell) -> String {
    use ratatui::style::{Color, Modifier};

    fn color(c: Color, fg: bool) -> String {
        let chan = if fg { 38 } else { 48 };
        match c {
            Color::Reset => String::new(),
            Color::Black => format!("{}", if fg { 30 } else { 40 }),
            Color::Red => format!("{}", if fg { 31 } else { 41 }),
            Color::Green => format!("{}", if fg { 32 } else { 42 }),
            Color::Yellow => format!("{}", if fg { 33 } else { 43 }),
            Color::Blue => format!("{}", if fg { 34 } else { 44 }),
            Color::Magenta => format!("{}", if fg { 35 } else { 45 }),
            Color::Cyan => format!("{}", if fg { 36 } else { 46 }),
            Color::Gray => format!("{}", if fg { 37 } else { 47 }),
            Color::DarkGray => format!("{}", if fg { 90 } else { 100 }),
            Color::LightRed => format!("{}", if fg { 91 } else { 101 }),
            Color::LightGreen => format!("{}", if fg { 92 } else { 102 }),
            Color::LightYellow => format!("{}", if fg { 93 } else { 103 }),
            Color::LightBlue => format!("{}", if fg { 94 } else { 104 }),
            Color::LightMagenta => format!("{}", if fg { 95 } else { 105 }),
            Color::LightCyan => format!("{}", if fg { 96 } else { 106 }),
            Color::White => format!("{}", if fg { 97 } else { 107 }),
            Color::Rgb(r, g, b) => format!("{chan};2;{r};{g};{b}"),
            Color::Indexed(i) => format!("{chan};5;{i}"),
        }
    }

    let mut parts: Vec<String> = Vec::new();
    let m = cell.modifier;
    if m.contains(Modifier::BOLD) {
        parts.push("1".into());
    }
    if m.contains(Modifier::DIM) {
        parts.push("2".into());
    }
    if m.contains(Modifier::ITALIC) {
        parts.push("3".into());
    }
    if m.contains(Modifier::UNDERLINED) {
        parts.push("4".into());
    }
    if m.contains(Modifier::REVERSED) {
        parts.push("7".into());
    }
    let fg = color(cell.fg, true);
    if !fg.is_empty() {
        parts.push(fg);
    }
    let bg = color(cell.bg, false);
    if !bg.is_empty() {
        parts.push(bg);
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", parts.join(";"))
    }
}

/// Render the app to a grid of styled text — same wide-glyph handling as
/// [`buffer_to_text`], but keeping each run's colours.
fn snapshot_ansi(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("offscreen terminal");
    terminal.draw(|f| draw(f, app)).expect("offscreen draw");
    let buf = terminal.backend().buffer();
    let area = buf.area;
    let cells = buf.content();
    let w = area.width as usize;

    let mut out = String::from("\x1b[0m");
    let mut cur = String::new();
    for y in 0..area.height {
        if y > 0 {
            out.push_str("\x1b[0m\n");
            cur.clear();
        }
        let mut x = 0usize;
        while x < w {
            let cell = &cells[y as usize * w + x];
            let sgr = cell_sgr(cell);
            if sgr != cur {
                out.push_str("\x1b[0m");
                out.push_str(&sgr);
                cur = sgr;
            }
            let sym = cell.symbol();
            out.push_str(sym);
            x += sym.width().max(1);
        }
    }
    out.push_str("\x1b[0m");
    out
}

fn reel_text(text: &str) -> RuntimeEvent {
    RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: text.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn reel_thought(text: &str) -> RuntimeEvent {
    RuntimeEvent::ThoughtDelta {
        session_id: SessionId::new(1),
        text: text.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn reel_tool_started(name: &str, args: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallStarted {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        args_json: args.to_string(),
        agent_id: None,
        trace_id: None,
    }
}

fn reel_tool_finished(name: &str, summary: &str) -> RuntimeEvent {
    RuntimeEvent::ToolCallFinished {
        session_id: SessionId::new(1),
        tool_name: name.to_string(),
        summary: summary.to_string(),
        agent_id: None,
        trace_id: None,
        denied: false,
        details: None,
    }
}

/// The README demo reel: a short, believable coding session.
#[test]
#[ignore = "writes README demo assets; run explicitly with PHIMINT_REEL_OUT"]
fn frame_reel() {
    use phi_agent::{PlanItem, PlanStepStatus};
    use std::io::Write as _;

    const W: u16 = 100;
    const H: u16 = 32;

    let out_dir = std::path::PathBuf::from(
        std::env::var("PHIMINT_REEL_OUT").unwrap_or_else(|_| "target/reel".to_string()),
    );
    std::fs::create_dir_all(&out_dir).expect("create reel dir");

    let mut app = App::new();
    app.push_banner(crate::banner::build(
        std::path::Path::new("/Users/you/src/your-project"),
        std::path::Path::new("/home/you/.phimint/sessions/20260930_demo01"),
        // No leading `v`: the banner's tagline row prepends it.
        "0.1.0",
        W as usize,
    ));
    app.set_notice(
        "Tip: trackpad scrolling needs View > Allow Mouse Reporting; fn+Up/fn+Down always work",
    );

    let mut frames: Vec<String> = Vec::new();
    let mut snap = |app: &mut App| frames.push(snapshot_ansi(app, W, H));
    snap(&mut app);

    // --- turn 1: orient in an unfamiliar repo -----------------------------
    app.push_user("Map this repo and tell me where the background-task wake policy lives.");
    app.running = true;
    snap(&mut app);

    for chunk in [
        "Let me get the lay of the land first, ",
        "then search for the wake policy directly.",
    ] {
        app.handle_event(TuiEvent::Runtime(reel_thought(chunk)));
        snap(&mut app);
    }
    app.handle_event(TuiEvent::Runtime(reel_tool_started("repo_map", "{}")));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "repo_map",
        "Repository layout (6 modules, 48 files)",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "search_content",
        r#"{"query":"bg_wake|BG_WAKE_QUIET"}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "search_content",
        "12 matches in 3 files",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(RuntimeEvent::PlanUpdated {
        session_id: SessionId::new(1),
        objective: "Locate and summarize the background-task wake policy".to_string(),
        explanation: None,
        plan: vec![
            PlanItem {
                step: "Map the repo and locate the wake module".into(),
                status: PlanStepStatus::Completed,
            },
            PlanItem {
                step: "Trace who calls the wake and when".into(),
                status: PlanStepStatus::InProgress,
            },
            PlanItem {
                step: "Summarize the delivery policy for the user".into(),
                status: PlanStepStatus::Pending,
            },
        ],
        agent_id: None,
        trace_id: None,
    }));
    snap(&mut app);

    for chunk in [
        "The policy lives in ",
        "`src/ui/bg_wake.rs`, and `run.rs` only executes the side effects. ",
        "It batches completions behind a 15s quiet window so a burst of ",
        "finished tasks costs one notification turn, not ten.",
    ] {
        app.handle_event(TuiEvent::Runtime(reel_text(chunk)));
        snap(&mut app);
    }
    app.handle_event(TuiEvent::TurnDone);
    snap(&mut app);

    // --- turn 2: a tool-heavy change --------------------------------------
    app.push_user("Add a regression test for the quiet-window batching.");
    app.running = true;
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "read_file",
        r#"{"path":"src/ui/bg_wake.rs"}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "read_file",
        "Read 292 lines",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "edit_file",
        r#"{"path":"src/ui/bg_wake_tests.rs"}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "edit_file",
        "Applied edit",
    )));
    snap(&mut app);

    app.handle_event(TuiEvent::Runtime(reel_tool_started(
        "execute_command",
        r#"{"command":"cargo test bg_wake","background":false}"#,
    )));
    snap(&mut app);
    app.handle_event(TuiEvent::Runtime(reel_tool_finished(
        "execute_command",
        "test result: ok. 14 passed; 0 failed",
    )));
    snap(&mut app);

    for chunk in [
        "Added `wake_batches_completions_inside_the_quiet_window`. ",
        "It pins the 15s window with an injected clock so the test never sleeps, ",
        "and asserts that two tasks finishing 3s apart produce one wake.",
    ] {
        app.handle_event(TuiEvent::Runtime(reel_text(chunk)));
        snap(&mut app);
    }
    app.handle_event(TuiEvent::TurnDone);
    snap(&mut app);

    // --- write the frames --------------------------------------------------
    for (i, frame) in frames.iter().enumerate() {
        let path = out_dir.join(format!("frame_{i:04}.ans"));
        let mut f = std::fs::File::create(&path).expect("write frame");
        f.write_all(frame.as_bytes()).expect("write frame bytes");
    }
    println!(
        "wrote {} frames ({}x{}) to {}",
        frames.len(),
        W,
        H,
        out_dir.display()
    );
}

#[test]
fn prose_longer_than_width_must_wrap_not_clip() {
    let mut app = App::new();
    // Assistant prose that clearly overflows 80 columns.
    let long = "The policy lives in the wake module and batches completions behind a quiet window so a burst of finished tasks costs one notification turn, not ten of them.";
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: long.to_string(),
        agent_id: None,
        trace_id: None,
    }));
    app.handle_event(TuiEvent::TurnDone);
    let snap = snapshot_text(&mut app, 80, 24);
    // Wrapped, not clipped: the tail is on screen and every word survives.
    let has_head = snap.lines().any(|l| l.contains("The policy lives in"));
    let has_tail = snap.lines().any(|l| l.contains("them."));
    assert!(has_head, "head clipped:\n{snap}");
    assert!(has_tail, "TAIL LOST (clipped at width):\n{snap}");
    for w in long.split_whitespace() {
        assert!(snap.contains(w), "word {w:?} lost (clipped):\n{snap}");
    }
    // Real wrap: the sentence spans several visual rows, not one hard-cut row.
    let prose_rows = snap
        .lines()
        .filter(|l| l.contains("policy") || l.contains("window so") || l.contains("them."))
        .count();
    assert!(
        prose_rows >= 2,
        "prose must wrap into multiple rows:\n{snap}"
    );
}

/// The same clip bug on the uncommitted streaming tail: prose mid-stream must
/// wrap, not vanish at the pane edge until it commits.
#[test]
fn streaming_tail_longer_than_width_must_wrap_not_clip() {
    let mut app = App::new();
    let long = "Streaming answers stay uncommitted until the turn ends, so the live tail is the only place a reader sees the sentence, and its ending must remain on screen the whole time.";
    app.handle_event(TuiEvent::Runtime(RuntimeEvent::TextDelta {
        session_id: SessionId::new(1),
        text: long.to_string(),
        agent_id: None,
        trace_id: None,
    }));
    // No TurnDone: the text is still the streaming tail.
    let snap = snapshot_text(&mut app, 80, 24);
    let has_head = snap
        .lines()
        .any(|l| l.contains("Streaming answers stay uncommitted"));
    let has_tail = snap.lines().any(|l| l.contains("whole time."));
    assert!(has_head, "head clipped:\n{snap}");
    assert!(has_tail, "TAIL LOST (clipped at width):\n{snap}");
    for w in long.split_whitespace() {
        assert!(snap.contains(w), "word {w:?} lost (clipped):\n{snap}");
    }
}

/// The opposite contract on `push_styled_line` rows: they must NOT soft-wrap.
/// phi-tui documents "**no** soft-wrap (the wordmark must stay whole; ratatui
/// clips on narrow terminals), no re-wrap on width change (`original: None`)".
/// Banner art and the full-width `─` rule are fixed-width chrome — wrapping the
/// rule (no break points) hard-splits it into dash fragments and the 60-col
/// wordmark splits at its letter gaps. Render a banner wider than the pane and
/// assert the rule stays ONE visual row (clipped), not a stack of fragments.
#[test]
fn styled_banner_rows_stay_whole_and_do_not_soft_wrap() {
    let mut app = App::new();
    // 100-col rule on a 40-col pane: wider than the backend, no break points.
    app.push_banner(crate::banner::build(
        std::path::Path::new("/w"),
        std::path::Path::new("/l"),
        "0.1.0",
        100,
    ));
    let snap = snapshot_text(&mut app, 40, 30);
    let dash_rows: Vec<&str> = snap
        .lines()
        .filter(|l| !l.trim().is_empty() && l.trim().chars().all(|c| c == '─'))
        .collect();
    assert_eq!(
        dash_rows.len(),
        1,
        "rule must stay one clipped visual row, not wrap into fragments:\n{snap}"
    );
    // And the wordmark keeps one visual row per glyph row (6), not 12 halves.
    // (Row 5 is pure shadow `╚═╝`, so count the full face+shadow charset.)
    let wordmark_rows = snap
        .lines()
        .filter(|l| l.chars().any(|c| "█╔═╗║╚╝".contains(c)))
        .count();
    assert_eq!(
        wordmark_rows, 6,
        "wordmark must stay whole (6 rows), not split at letter gaps:\n{snap}"
    );
}
