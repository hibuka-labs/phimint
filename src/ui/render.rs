//! ratatui frame rendering: output / tool log / composer / status bar.

use std::ops::Range;

use phi_agent::RiskLevel;
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::ui::app::{AgentStatus, App, LineKind, ToolCallEntry, WRAP_WIDTH, wrap};

/// Max composer rows shown (its box grows with the buffer up to this).
const MAX_COMPOSER_ROWS: usize = 8;

pub fn draw(f: &mut Frame, app: &App) {
    let composer_height = app.composer.height().min(MAX_COMPOSER_ROWS) as u16 + 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),                 // output
            Constraint::Length(6),              // tool log
            Constraint::Length(composer_height), // composer
            Constraint::Length(1),              // status bar
        ])
        .split(f.area());

    render_output(f, app, chunks[0]);
    render_tool_log(f, app, chunks[1]);
    render_composer(f, app, chunks[2]);
    render_status(f, app, chunks[3]);

    if app.has_pending_approval() {
        render_approval_popup(f, app);
    }
}

fn render_output(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;

    // Committed lines + the live streaming tail (uncommitted text renders
    // progressively, then flushes into `output` on the next structural event).
    // Only the visible window is materialized: cloning every historical line on
    // each frame would make scrolling sluggish as the buffer grows.
    let tail: Vec<Line> = app
        .streaming_tail()
        .map(|(tail, kind)| {
            wrap(tail, WRAP_WIDTH)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style_for(kind))))
                .collect()
        })
        .unwrap_or_default();

    let committed = app.output.len();
    let window = window_range(committed + tail.len(), app, height);

    let mut lines: Vec<Line> = Vec::with_capacity(window.len());
    for i in window {
        if i < committed {
            let l = &app.output[i];
            lines.push(Line::from(Span::styled(l.text.clone(), style_for(l.kind))));
        } else {
            lines.push(tail[i - committed].clone());
        }
    }

    let block = Block::default().borders(Borders::ALL).title("output");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_tool_log(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize;
    let items: Vec<ListItem> = app
        .tool_log
        .iter()
        .rev()
        .take(height)
        .map(tool_item)
        .collect();

    let block = Block::default().borders(Borders::ALL).title("tools");
    f.render_widget(List::new(items).block(block), area);
}

fn tool_item(entry: &ToolCallEntry) -> ListItem<'_> {
    let marker = if entry.denied {
        "⛔"
    } else if entry.done {
        "✓"
    } else {
        "…"
    };
    let agent = entry
        .agent_id
        .as_deref()
        .map(|a| format!("[{a}] "))
        .unwrap_or_default();

    let mut spans = vec![
        Span::styled(
            marker,
            Style::default().fg(if entry.denied { Color::Red } else { Color::Green }),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{agent}{}", entry.tool_name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if !entry.args.is_empty() {
        spans.push(Span::styled(
            format!(" {}", entry.args),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(summary) = &entry.summary {
        spans.push(Span::styled(
            format!(" ↳ {summary}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn render_composer(f: &mut Frame, app: &App, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let lines = app.composer.lines();
    // Show the tail of the buffer so the cursor (usually at the end) is visible.
    let start = lines.len().saturating_sub(inner_height);
    let cursor = app.composer.cursor();

    let items: Vec<Line> = lines[start..]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let idx = start + i;
            // A `>` prompt marks the first line; continuation lines indent to
            // the same column (Claude Code style).
            let prefix = if idx == 0 { "> " } else { "  " };
            let full = format!("{prefix}{line}");
            if idx == cursor.0 {
                line_with_cursor(&full, prefix.len() + cursor.1)
            } else {
                Line::from(full)
            }
        })
        .collect();

    let title = if app.running {
        "input — agent is running"
    } else {
        "input"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title);
    f.render_widget(Paragraph::new(items).block(block), area);
}

/// Render one composer line with a reversed-video cursor at `byte`.
fn line_with_cursor(line: &str, byte: usize) -> Line<'static> {
    let before = &line[..byte];
    let at = line[byte..].chars().next();
    let after_start = byte + at.map(|c| c.len_utf8()).unwrap_or(0);
    let after = &line[after_start..];

    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    let mut spans = vec![Span::raw(before.to_string())];
    match at {
        Some(c) => spans.push(Span::styled(c.to_string(), cursor_style)),
        None => spans.push(Span::styled(" ".to_string(), cursor_style)),
    }
    spans.push(Span::raw(after.to_string()));
    Line::from(spans)
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let style = match app.status {
        AgentStatus::Idle => Style::default().fg(Color::Green),
        AgentStatus::Running { .. } => Style::default().fg(Color::Yellow),
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(app.status_line(), style))), area);
}

/// A centered modal popup showing the current approval request + y/a/n hint.
fn render_approval_popup(f: &mut Frame, app: &App) {
    let Some(request) = app.current_approval() else {
        return;
    };

    let area = centered_rect(70, 50, f.area());
    f.render_widget(Clear, area);

    let (risk, risk_color) = risk_label(&request.risk_level);
    let mut lines = vec![
        Line::from(Span::styled(
            "⚠️ Approval required",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            request.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("Risk: {risk}"),
            Style::default().fg(risk_color),
        )),
        Line::from(""),
    ];
    for raw in request.message.split('\n') {
        lines.push(Line::from(raw.to_string()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[y] allow once   [a] allow always   [n] deny",
        Style::default().fg(Color::Cyan),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title("approval");
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}

/// A centered rectangle occupying `percent_x`/`percent_y` of `area`.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn risk_label(level: &RiskLevel) -> (&'static str, Color) {
    match level {
        RiskLevel::Safe => ("Safe", Color::Green),
        RiskLevel::Sensitive => ("Sensitive", Color::Yellow),
        RiskLevel::Destructive => ("Destructive", Color::Red),
    }
}

fn style_for(kind: LineKind) -> Style {
    match kind {
        LineKind::Normal => Style::default(),
        LineKind::Thought => Style::default()
            .add_modifier(Modifier::DIM)
            .add_modifier(Modifier::ITALIC),
        LineKind::Plan => Style::default().fg(Color::Cyan),
        LineKind::Done => Style::default().fg(Color::Green),
        LineKind::Error => Style::default().fg(Color::Red),
        LineKind::System => Style::default().fg(Color::DarkGray),
        LineKind::Cancelled => Style::default().fg(Color::Yellow),
        LineKind::Approval => Style::default().fg(Color::Yellow),
    }
}

/// The `[start, end)` range of `total` lines to show in a window of `height` rows.
fn window_range(total: usize, app: &App, height: usize) -> Range<usize> {
    if total <= height {
        return 0..total;
    }
    if app.follow_bottom {
        return total - height..total;
    }
    let start = total.saturating_sub(height).saturating_sub(app.scroll_offset);
    start..(start + height).min(total)
}

/// Serialize a rendered buffer to a plain-text grid (one row per line).
///
/// Box-drawing borders and emoji markers survive as UTF-8; styles (color, bold)
/// are dropped. Used by the frame-capture log so a TUI session can be reviewed
/// afterward as a flipbook of text screens.
pub fn buffer_to_text(buf: &Buffer) -> String {
    let area = buf.area;
    let cells = buf.content();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            line.push_str(cells[(y as usize) * (area.width as usize) + (x as usize)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Render `app` offscreen at `width`×`height` and return it as text.
///
/// Mirrors the on-screen `draw` (same layout, status bar, and popups), so the
/// captured frames match what a live terminal shows.
pub fn snapshot_text(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("offscreen terminal");
    terminal.draw(|f| draw(f, app)).expect("offscreen draw");
    buffer_to_text(terminal.backend().buffer())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::{AgentStatus, App, LineKind, OutputLine, Phase, ToolCallEntry, TuiEvent};
    use phi_agent::{RuntimeEvent, SessionId};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn populated_app() -> App {
        let mut app = App::new();
        app.push_system("phiforge — welcome");
        for i in 0..60 {
            app.output.push(OutputLine {
                text: format!("streamed line {i}"),
                kind: LineKind::Normal,
            });
        }
        app.tool_log.push(ToolCallEntry {
            agent_id: Some("sub/1".into()),
            tool_name: "read_file".into(),
            args: "{\"path\":\"src/lib.rs\"}".into(),
            summary: Some("123 lines".into()),
            denied: false,
            done: true,
        });
        app.tool_log.push(ToolCallEntry {
            agent_id: None,
            tool_name: "execute_command".into(),
            args: "cargo test".into(),
            summary: None,
            denied: true,
            done: true,
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
        let app = populated_app();
        terminal.draw(|f| draw(f, &app)).unwrap();
    }

    #[test]
    fn draw_empty_and_scrolled_states() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        // Fresh (empty) state.
        let app = App::new();
        terminal.draw(|f| draw(f, &app)).unwrap();

        // Scrolled up (not following bottom).
        let mut app = App::new();
        for i in 0..100 {
            app.output.push(OutputLine {
                text: format!("long line {i}"),
                kind: LineKind::Normal,
            });
        }
        app.follow_bottom = false;
        app.scroll_offset = 50;
        terminal.draw(|f| draw(f, &app)).unwrap();
    }

    #[test]
    fn draw_cursor_mid_multibyte_line() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.composer.insert_str("héllo");
        app.composer.move_home();
        app.composer.move_right(); // cursor after 'h' (byte 1), before the 2-byte 'é'
        terminal.draw(|f| draw(f, &app)).unwrap();
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
        terminal.draw(|f| draw(f, &app)).unwrap();
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

        let text = snapshot_text(&app, 80, 24);
        assert!(text.contains("approval"), "popup title missing:\n{text}");
        assert!(text.contains("output"), "output pane title missing:\n{text}");
        assert!(text.contains("do a thing"), "composer content missing:\n{text}");
        assert!(text.contains('\n'), "snapshot should be multi-line:\n{text}");
    }

    #[test]
    fn window_range_shifts_by_scroll_offset() {
        let mut app = App::new();
        for i in 0..100 {
            app.output.push(OutputLine { text: format!("line {i}"), kind: LineKind::Normal });
        }
        assert_eq!(window_range(100, &app, 30), 70..100);
        app.follow_bottom = false;
        app.scroll_offset = 10;
        assert_eq!(window_range(100, &app, 30), 60..90);
        // Past the top clamps to the first `height` lines.
        app.scroll_offset = 1000;
        assert_eq!(window_range(100, &app, 30), 0..30);
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
        let text = snapshot_text(&app, 80, 24);
        assert!(text.contains("partial answer"), "tail missing:\n{text}");
    }
}
