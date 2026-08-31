//! Tests for ratatui frame rendering.

use super::*;
use crate::ui::app::{AgentStatus, App, LineKind, OutputLine, Phase, SubAgentStatus, TuiEvent};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use phi_agent::{RuntimeEvent, SessionId};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn populated_app() -> App {
    let mut app = App::new();
    app.push_system("phimint — welcome");
    for i in 0..60 {
        app.transcript.push(OutputLine { spans: None, original: None,
            text: format!("streamed line {i}"),
            kind: LineKind::Normal,
        });
    }
    app.transcript.push(OutputLine { spans: None, original: None,
        text: "⏺ [sub/1] read_file {\"path\":\"src/lib.rs\"}".into(),
        kind: LineKind::Tool,
    });
    app.transcript.push(OutputLine { spans: None, original: None,
        text: "  ⛔ execute_command denied".into(),
        kind: LineKind::Error,
    });
    app.composer.insert_str("hello\nworld");
    app.running = true;
    app.status = AgentStatus::Running {
        phase: Phase::ToolCall {
            tool: "verify".into(),
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
        app.transcript.push(OutputLine { spans: None, original: None,
            text: format!("long line {i}"),
            kind: LineKind::Normal,
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
        },
        decision_tx: tx,
    });

    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("approval"), "popup title missing:\n{text}");
    assert!(text.contains("do a thing"), "composer content missing:\n{text}");
    assert!(text.contains('\n'), "snapshot should be multi-line:\n{text}");
}

#[test]
fn window_range_shifts_by_scroll_offset() {
    let mut app = App::new();
    for i in 0..100 {
        app.transcript.push(OutputLine { spans: None, original: None, text: format!("line {i}"), kind: LineKind::Normal });
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
    assert!(!text.contains("你 好"), "wide chars should be adjacent, got: {text:?}");
}

#[test]
fn snapshot_shows_sub_agent_strip() {
    let mut app = App::new();
    app.sub_agents.insert("root/a".to_string(), SubAgentStatus::Running);
    app.sub_agents.insert("root/b".to_string(), SubAgentStatus::Done);
    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("● [root/a]"), "running marker missing:\n{text}");
    assert!(text.contains("✓ [root/b]"), "done marker missing:\n{text}");
}

#[test]
fn snapshot_omits_strip_when_no_sub_agents() {
    let mut app = App::new();
    let text = snapshot_text(&mut app, 80, 24);
    assert!(!text.contains("● ["), "strip should be absent:\n{text}");
}

#[test]
fn draw_with_selection_and_context_menu_does_not_panic() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    for i in 0..20 {
        app.transcript.push(OutputLine { spans: None, original: None,
            text: format!("line {i}"),
            kind: LineKind::Normal,
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
    let root = std::env::temp_dir().join(format!(
        "phimint-render-mention-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("main.rs"), "x").unwrap();
    app.set_workspace_root(root);

    // Open the picker and type a partial prefix.
    app.handle_key(KeyCode::Char('@'), KeyModifiers::NONE);
    app.handle_key(KeyCode::Char('m'), KeyModifiers::NONE);

    // Offscreen draw must not panic, and the snapshot shows the popup.
    let text = snapshot_text(&mut app, 100, 40);
    assert!(text.contains("mention"), "popup title missing:\n{text}");
    assert!(text.contains("@m"), "prefix header missing:\n{text}");
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
    assert!(text.contains("skills"), "popup title missing:\n{text}");
    assert!(text.contains("/r"), "prefix header missing:\n{text}");
    // "review" contains "r" → should appear
    assert!(text.contains("review"), "matching skill missing:\n{text}");
    // description should render too
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
    ));
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let w = 120usize;
    let cell = |x: usize, y: usize| buf.content()[y * w + x].clone();

    // Wordmark row 0: glyph 0 ('p') is LogoA dark fg; connector (col 8)
    // is Default → falls back to the System base (DarkGray).
    assert_eq!(cell(0, 0).style().fg, Some(Color::Rgb(0xff, 0x78, 0x47)));
    assert_eq!(cell(8, 0).style().fg, Some(Color::DarkGray));

    // Tagline row (index 6): "Phimint" is bold Brand.
    let brand = cell(0, 6).style();
    assert_eq!(brand.fg, Some(Color::Rgb(0xff, 0xb0, 0x66)));
    assert!(brand.add_modifier.contains(Modifier::BOLD));
}
