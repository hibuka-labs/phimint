//! Tests for ratatui frame rendering.

use super::*;
use crate::ui::app::{AgentStatus, App, BackgroundTaskEntry, Phase, SubAgentState, SubAgentStatus, TuiEvent};
use phi_kernel_tools::background_shell::BackgroundTaskStatus;
use phi_tui::lines::{LineKind, OutputLine};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use phi_agent::{RuntimeEvent, SessionId};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn populated_app() -> App {
    let mut app = App::new();
    app.push_system("phimint — welcome");
    for i in 0..60 {
        app.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
            text: format!("streamed line {i}"),
            kind: LineKind::Normal,
        });
    }
    app.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
        text: "* [sub/1] read_file {\"path\":\"src/lib.rs\"}".into(),
        kind: LineKind::Tool,
    });
    app.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
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
                detail: None,
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
    assert!(text.contains("do a thing"), "composer content missing:\n{text}");
    assert!(text.contains('\n'), "snapshot should be multi-line:\n{text}");
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
        app.transcript.push(OutputLine { spans: None, original: None, text: format!("line {i}"), kind: LineKind::Normal, detail: None });
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
    app.sub_agents.insert("root/a".to_string(), SubAgentState {
        name: "a".to_string(),
        status: SubAgentStatus::Running,
        files: Vec::new(),
        started_at: std::time::Instant::now(),
        completed_at: None,
        last_tool_at: std::time::Instant::now(),
        events: Vec::new(),
    });
    app.sub_agents.insert("root/b".to_string(), SubAgentState {
        name: "b".to_string(),
        status: SubAgentStatus::Done,
        files: Vec::new(),
        started_at: std::time::Instant::now(),
        completed_at: Some(std::time::Instant::now()),
        last_tool_at: std::time::Instant::now(),
        events: Vec::new(),
    });
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
    app.sub_agents.insert("root/a".to_string(), SubAgentState {
        name: "a".to_string(),
        status: SubAgentStatus::Running,
        files: Vec::new(),
        started_at: std::time::Instant::now(),
        completed_at: None,
        last_tool_at: std::time::Instant::now(),
        events: Vec::new(),
    });
    app.background_tasks.insert("bg_aaaa1111".to_string(), BackgroundTaskEntry {
        id: "bg_aaaa1111".to_string(),
        command: "cargo test".to_string(),
        timeout_ms: 120_000,
        status: BackgroundTaskStatus::Running,
        started_at: std::time::Instant::now(),
        finished_at: None,
        reported: false,
    });
    assert!(app.should_show_task_panel(), "sub-agent opens the panel");
    let text = snapshot_text(&mut app, 100, 40);
    assert!(text.contains("Tasks (1)"), "panel counts sub-agents only:\n{text}");
    assert!(
        !text.contains("bg_aaaa1111"),
        "bg task must not appear in the panel:\n{text}"
    );

    // Bg-only: no panel at all (transcript + status bar carry the facts).
    let mut app = App::new();
    app.background_tasks.insert("bg_aaaa1111".to_string(), BackgroundTaskEntry {
        id: "bg_aaaa1111".to_string(),
        command: "cargo test".to_string(),
        timeout_ms: 120_000,
        status: BackgroundTaskStatus::Running,
        started_at: std::time::Instant::now(),
        finished_at: None,
        reported: false,
    });
    assert!(!app.should_show_task_panel(), "bg task alone must not open the panel");
}

#[test]
fn draw_with_selection_and_context_menu_does_not_panic() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    for i in 0..20 {
        app.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
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
    assert!(text.contains("┌─ src/main.rs"), "diff header missing:\n{text}");
    assert!(text.contains("@@"), "hunk header missing:\n{text}");
    assert!(text.contains("println"), "diff content missing:\n{text}");
    // Verify line numbers are present (hunk header has "1,3")
    assert!(text.contains("-1,3"), "line numbers missing:\n{text}");
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
    '⏺', '⏸', '⏳', '✓', '✔', '✗', '✘', '●', '○', '→', '←', '⇒', '⇐',
    '⟳', '⚠', '❯', '×', '÷', '±', '≤', '≥', '≠', '≈', '≡', '…', '·',
    '—', '–', '•', '√', '§', '∑', '∏', '∫', '∂', '∇', '∞', '①', '▸',
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
    app.status = AgentStatus::Running { phase: Phase::Thinking };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Running { phase: Phase::Streaming };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Running { phase: Phase::ToolCall { tool: "edit_file".into() } };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Running { phase: Phase::AwaitingApproval };
    status_texts.push(app.status_line());
    app.status = AgentStatus::Idle;
    status_texts.push(app.status_line());

    let text = snapshot_text(&mut app, 100, 30);
    let all = text + &status_texts.join("\n");
    let offenders: Vec<char> = all.chars().filter(|c| CJK_WIDTH_UNSAFE.contains(c)).collect();
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
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {file}: {e}"));
        for (i, line) in source.lines().enumerate() {
            let code = line.split_once("//").map_or(line, |(code, _)| code);
            let offenders: Vec<char> =
                code.chars().filter(|c| CJK_WIDTH_UNSAFE.contains(c)).collect();
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
        detail: Some(phi_tui::lines::LineDetail::Thought {
            raw: raw.to_string(),
            line_count: 5,
            char_count: raw.chars().count(),
        }),
        text: "line one".into(),
        kind: LineKind::Thought,
    });
    let text = snapshot_text(&mut app, 80, 24);
    // Full summary: marker + line count + estimate (spec: 摘要文案含行数与估算 tok).
    assert!(
        text.contains("> thinking - 5 行 - ~16 tok"),
        "summary missing:\n{text}"
    );
    assert!(!text.contains("line three"), "raw leaked while folded:\n{text}");
    app.show_thoughts = true;
    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains("line three"), "expanded text missing:\n{text}");
    assert!(!text.contains("> thinking"), "summary gone when expanded:\n{text}");
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
    assert!(!text.contains("thinking -"), "panel must hide, not occlude:\n{text}");
    // 80x14: output pane = 10 = box(8) + history(2) — exact boundary shows.
    let text = shot(80, 14);
    assert!(text.contains("thinking -"), "boundary (8+2) must show:\n{text}");
    // 80x20: output pane = 16 ≥ box(8) + history(2) — panel shows.
    let text = shot(80, 20);
    assert!(text.contains("thinking -"), "panel should fit with history rows:\n{text}");
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
    // The box sits exactly where its placeholder rows land: 3 committed rows
    // above → the `╭` title row is screen row 3.
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
    let row = text.lines().nth(3).unwrap_or_default();
    assert!(
        row.contains("thinking -") && row.contains('╭'),
        "panel title row must sit at flow row 3, got: {row:?}\n{text}"
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
    assert!(!head.is_empty(), "scrolled history should show content:\n{before}");
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
        detail: Some(phi_tui::lines::LineDetail::Thought {
            raw: raw.clone(),
            line_count: 3,
            char_count,
        }),
        text: "line one".into(),
        kind: LineKind::Thought,
    });
    app.show_thoughts = true;
    let text = snapshot_text(&mut app, 80, 24);
    assert!(text.contains(&"x".repeat(80)), "long line must hard-wrap:\n{text}");
    assert!(
        !text.contains(&"x".repeat(200)),
        "un-wrapped 200-char run must not survive:\n{text}"
    );
    assert!(text.contains("line three"), "content after the long line:\n{text}");
}

#[test]
fn history_head_survives_fold_reflow_above_viewport() {
    // Ctrl+O reflows the MIDDLE of the flow: a folded thought above the window
    // head grows from 1 summary row to 8. The reader's place must hold by
    // CONTENT — holding the raw head index would silently swap in content
    // from above (the tail-only anchoring gap).
    let mut app = App::new();
    let raw = (0..8).map(|i| format!("thought line {i}")).collect::<Vec<_>>().join("\n");
    app.transcript.push(OutputLine {
        spans: None,
        // Production folded shape (see committed_thought_folds_and_expands).
        original: None,
        detail: Some(phi_tui::lines::LineDetail::Thought {
            raw: raw.clone(),
            line_count: 8,
            char_count: raw.chars().count(),
        }),
        text: "thought line 0".into(),
        kind: LineKind::Thought,
    });
    for i in 0..60 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            detail: None,
            text: format!("filler row {i}"),
            kind: LineKind::Normal,
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
    assert!(!before.contains("thought line 7"), "raw hidden while folded");

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
        detail: Some(phi_tui::lines::LineDetail::Thought {
            raw: raw.clone(),
            line_count: 40,
            char_count: raw.chars().count(),
        }),
        text: "thought line 0".into(),
        kind: LineKind::Thought,
    });
    for i in 0..6 {
        app.transcript.push(OutputLine {
            spans: None,
            original: None,
            detail: None,
            text: format!("filler row {i}"),
            kind: LineKind::Normal,
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
    assert!(n >= 1, "bottom head must leave room for one row up:\n{bottom}");
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
    assert!(text.contains("analyze"), "thought streams into the flow:\n{text}");
    // Panel absence is anchored on its title row (`thinking -`): the composer
    // is a rounded box too, so the bare `╭` glyph proves nothing here.
    assert!(!text.contains("thinking -"), "no panel box/title in loose mode:\n{text}");
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
    assert!(text.contains("[root/searcher]"), "author on the first row:\n{text}");
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
