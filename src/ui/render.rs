//! ratatui frame rendering: transcript (output) / composer / status bar.

use phi_agent::RiskLevel;
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::banner::{BannerStyle, ColorScheme, SpanSpec};
use crate::ui::app::{
    AgentStatus, App, CONTEXT_MENU_H, CONTEXT_MENU_W, LineKind, SubAgentStatus, context_menu_pos,
    window_range, wrap,
};

/// Max composer rows shown (its box grows with the buffer up to this).
const MAX_COMPOSER_ROWS: usize = 8;

pub fn draw(f: &mut Frame, app: &mut App) {
    // Output area has no border — use the full terminal width for wrapping.
    let term_w = f.area().width;
    let content_width = term_w as usize;
    app.set_wrap_width(content_width);

    // Composer height: visual rows (accounting for soft-wrap) + 2 for border.
    // The composer's Block::borders(ALL) consumes 2 columns (left+right).
    let composer_inner_w = term_w.saturating_sub(2);
    let composer_vis = app.composer.visual_height(composer_inner_w).min(MAX_COMPOSER_ROWS);
    let composer_height = composer_vis as u16 + 2;
    let has_sub_agents = !app.sub_agents.is_empty();

    tracing::debug!(
        term_w,
        content_width,
        composer_inner_w,
        composer_vis,
        composer_height,
        "draw layout"
    );

    let mut constraints = vec![
        Constraint::Min(3),                  // output (transcript)
        Constraint::Length(composer_height), // composer
    ];
    if has_sub_agents {
        constraints.push(Constraint::Length(1)); // sub-agent strip
    }
    constraints.push(Constraint::Length(1)); // status bar

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    // Record the output pane rect so mouse events can hit-test transcript rows.
    let out = chunks[0];
    app.output_area = Some((out.x, out.y, out.width, out.height));

    render_output(f, app, chunks[0]);
    render_composer(f, app, chunks[1]);
    if has_sub_agents {
        render_sub_agents(f, app, chunks[2]);
        render_status(f, app, chunks[3]);
    } else {
        render_status(f, app, chunks[2]);
    }

    if app.has_pending_approval() {
        render_approval_popup(f, app);
    }
    if app.context_menu().is_some() {
        render_context_menu(f, app);
    }
    if app.mention().is_some() {
        render_mention_popup(f, app, chunks[1]);
    }
    if app.slash().is_some() {
        render_slash_popup(f, app, chunks[1]);
    }
}

fn render_output(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height as usize;

    tracing::debug!(
        out_area_w = area.width,
        out_area_x = area.x,
        out_area_y = area.y,
        wrap_width = app.current_wrap_width(),
        "render_output"
    );

    // Committed lines + the live streaming tail (uncommitted text renders
    // progressively, then flushes into `output` on the next structural event).
    // The tail is wrapped incrementally as deltas arrive (see `WrapCache`), so
    // this frame only styles the visible window — no re-wrap of the full tail.
    let (tail_lines, tail_kind) = match app.streaming_tail_lines() {
        Some((lines, kind)) => (lines, kind),
        None => (&[][..], LineKind::Normal),
    };

    let committed = app.output.len();
    let window = window_range(committed + tail_lines.len(), app, height);

    let mut lines: Vec<Line> = Vec::with_capacity(window.len());
    for i in window {
        let mut base = if i < committed {
            style_for(app.output[i].kind)
        } else {
            style_for(tail_kind)
        };
        // Highlight lines inside the active mouse selection — the background
        // overlays every segment below, including banner span colors.
        if i < committed && app.is_selected(i) {
            base = base.bg(Color::DarkGray);
        }
        let (text, spans): (&str, &[SpanSpec]) = if i < committed {
            let line = &app.output[i];
            (line.text.as_str(), line.spans.as_deref().unwrap_or(&[]))
        } else {
            (tail_lines[i - committed].as_str(), &[])
        };
        lines.push(span_line(text, spans, base, app.scheme()));
    }

    // No border/title — the transcript flows freely (Claude Code style); the
    // composer's own box is the visual boundary between output and input.
    f.render_widget(Paragraph::new(lines), area);
}

fn render_composer(f: &mut Frame, app: &App, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let inner_width = area.width.saturating_sub(2) as usize; // border left+right
    let prefix_w = 2usize; // "> " or "  "

    tracing::debug!(
        comp_area_w = area.width,
        inner_width,
        inner_height,
        "render_composer"
    );
    let lines = app.composer.lines();
    let cursor = app.composer.cursor();

    // Pre-wrap each logical line to `content_w` columns so each `Line` maps
    // to exactly one ratatui row. This avoids double-wrapping (our slice +
    // Paragraph::wrap) which caused misaligned display.
    let mut items: Vec<Line> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let prefix = if idx == 0 { "> " } else { "  " };
        let full = format!("{prefix}{line}");
        let wrapped = wrap(&full, inner_width);

        // Find which visual row the cursor falls on and its byte offset
        // within that wrapped row.
        let cursor_pos = if idx == cursor.0 {
            let cursor_byte = prefix_w + cursor.1;
            let mut col = 0usize;
            let mut cur_row = 0usize;
            let mut cur_col_in_row = 0usize;
            let mut byte_offset = 0usize;
            let mut found = None;
            for ch in full.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if col > 0 && col + cw > inner_width {
                    // Wrap: new row starts with this char.
                    col = 0;
                    cur_row += 1;
                    cur_col_in_row = 0;
                }
                if byte_offset >= cursor_byte {
                    // Cursor is at this column in this row.
                    found = Some((cur_row, cur_col_in_row));
                    break;
                }
                col += cw;
                cur_col_in_row += cw;
                byte_offset += ch.len_utf8();
            }
            // Cursor at end of text: place on last row at end.
            if found.is_none() {
                found = Some((cur_row, cur_col_in_row));
            }
            found
        } else {
            None
        };

        for (j, row) in wrapped.iter().enumerate() {
            if let Some((vis_row, vis_col)) = cursor_pos {
                if j == vis_row {
                    // Byte offset into the wrapped row for the cursor.
                    let mut b = 0usize;
                    let mut c = 0usize;
                    for ch in row.chars() {
                        if c >= vis_col {
                            break;
                        }
                        c += UnicodeWidthChar::width(ch).unwrap_or(0);
                        b += ch.len_utf8();
                    }
                    let before = &row[..b];
                    let at = row[b..].chars().next();
                    let after_start = b + at.map(|ch| ch.len_utf8()).unwrap_or(0);
                    let after = &row[after_start..];
                    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
                    let mut spans = vec![Span::raw(before.to_string())];
                    match at {
                        Some(ch) => spans.push(Span::styled(ch.to_string(), cursor_style)),
                        None => spans.push(Span::styled(" ".to_string(), cursor_style)),
                    }
                    spans.push(Span::raw(after.to_string()));
                    items.push(Line::from(spans));
                    continue;
                }
            }
            items.push(Line::from(row.clone()));
        }
    }

    // Scroll to show the bottom of the buffer (cursor is usually at the end).
    let total = items.len();
    let scroll = total.saturating_sub(inner_height) as u16;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);
    f.render_widget(
        Paragraph::new(items).block(block).scroll((scroll, 0)),
        area,
    );
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let style = match app.status {
        AgentStatus::Idle => Style::default().fg(Color::Green),
        AgentStatus::Running { .. } => Style::default().fg(Color::Yellow),
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(app.status_line(), style))), area);
}

/// A one-row "sub-agents" strip (Claude Code style): `● [p]` in cyan while
/// running, `✓ [p]` in green once done, joined by ` · `. Static markers (not an
/// animated spinner) keep the offscreen frame-capture dedup stable.
fn render_sub_agents(f: &mut Frame, app: &App, area: Rect) {
    let spans: Vec<Span> = app
        .sub_agents
        .iter()
        .enumerate()
        .flat_map(|(i, (path, status))| {
            let (marker, color) = match status {
                SubAgentStatus::Running => ("●", Color::Cyan),
                SubAgentStatus::Done => ("✓", Color::Green),
            };
            let mut items = vec![Span::styled(
                format!("{marker} [{path}]"),
                Style::default().fg(color),
            )];
            if i + 1 < app.sub_agents.len() {
                items.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            }
            items
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// A small context menu at the right-click cell, with a highlighted "copy" /
/// "cancel" choice (keyboard-driven: Up/Down/Enter/Esc).
fn render_context_menu(f: &mut Frame, app: &App) {
    let Some(menu) = app.context_menu() else {
        return;
    };

    let area = f.area();
    // Geometry shared with mouse hit-testing (App::handle_mouse): one source of
    // truth for where the popup actually lands on screen.
    let (x, y) = context_menu_pos(menu.x, menu.y, area.width, area.height);
    let rect = Rect::new(x, y, CONTEXT_MENU_W, CONTEXT_MENU_H);
    f.render_widget(Clear, rect);

    let items = [" 拷贝 ", " 取消 "];
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let style = if i == menu.selected {
                Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(label.to_string(), style))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title("copy");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// The `@` mention popup (Phase 10): anchored above the composer, listing the
/// current directory's entries with the highlighted row reverse-video'd. A
/// scrolling window keeps the selection visible in large directories.
fn render_mention_popup(f: &mut Frame, app: &App, composer: Rect) {
    let Some(m) = app.mention() else {
        return;
    };

    const WIDTH: u16 = 64;
    const LIST_HEIGHT: usize = 9;

    // Scroll the entry window so the highlighted row stays on screen.
    let total = m.entries.len();
    let start = if total <= LIST_HEIGHT {
        0
    } else if m.selected < LIST_HEIGHT / 2 {
        0
    } else {
        (m.selected - LIST_HEIGHT / 2).min(total - LIST_HEIGHT)
    };
    let end = (start + LIST_HEIGHT).min(total);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("@{}", m.prefix),
        Style::default().fg(Color::Cyan),
    )));
    lines.push(Line::from(Span::styled(
        "──────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    for (i, e) in m.entries[start..end].iter().enumerate() {
        let idx = start + i;
        let marker = if e.synthetic {
            "» "
        } else if e.is_dir {
            "📁 "
        } else {
            "📄 "
        };
        let style = if idx == m.selected {
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}{}", e.name),
            style,
        )));
    }

    let width = WIDTH.min(f.area().width.saturating_sub(2));
    let height = (lines.len() as u16 + 2).min(f.area().height.saturating_sub(2));
    let x = composer.x.min(f.area().width.saturating_sub(width));
    let y = composer.y.saturating_sub(height).max(1);
    let rect = Rect::new(x, y, width, height);
    f.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title("mention");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// The `/` skill picker popup: anchored above the composer, listing matching
/// skill names with the highlighted row reverse-video'd.
fn render_slash_popup(f: &mut Frame, app: &App, composer: Rect) {
    let Some(s) = app.slash() else {
        return;
    };

    const WIDTH: u16 = 72;
    const LIST_HEIGHT: usize = 12;

    let total = s.entries.len();
    let start = if total <= LIST_HEIGHT {
        0
    } else if s.selected < LIST_HEIGHT / 2 {
        0
    } else {
        (s.selected - LIST_HEIGHT / 2).min(total - LIST_HEIGHT)
    };
    let end = (start + LIST_HEIGHT).min(total);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("/{}", s.prefix),
        Style::default().fg(Color::Cyan),
    )));
    lines.push(Line::from(Span::styled(
        "────────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    if s.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching skills",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, (name, desc)) in s.entries[start..end].iter().enumerate() {
            let idx = start + i;
            let (name_style, desc_style) = if idx == s.selected {
                (
                    Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD),
                    Style::default().bg(Color::DarkGray).fg(Color::Gray),
                )
            } else {
                (
                    Style::default().fg(Color::White),
                    Style::default().fg(Color::DarkGray),
                )
            };
            // 截断描述，避免超出弹窗宽度
            let max_desc = 40usize;
            let short_desc = if desc.len() > max_desc {
                format!("{}…", &desc[..max_desc])
            } else {
                desc.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {name:<24}"), name_style),
                Span::styled(short_desc, desc_style),
            ]));
        }
    }

    let width = WIDTH.min(f.area().width.saturating_sub(2));
    let height = (lines.len() as u16 + 2).min(f.area().height.saturating_sub(2));
    let x = composer.x.min(f.area().width.saturating_sub(width));
    let y = composer.y.saturating_sub(height).max(1);
    let rect = Rect::new(x, y, width, height);
    f.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title("skills");
    f.render_widget(Paragraph::new(lines).block(block), rect);
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
        LineKind::Tool => Style::default().fg(Color::Cyan),
        LineKind::Done => Style::default().fg(Color::Green),
        LineKind::ToolResult => Style::default().fg(Color::Green),
        LineKind::Error => Style::default().fg(Color::Red),
        LineKind::System => Style::default().fg(Color::DarkGray),
        LineKind::Cancelled => Style::default().fg(Color::Yellow),
        LineKind::Approval => Style::default().fg(Color::Yellow),
        LineKind::User => Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    }
}

/// A banner run's style for the given scheme: 24-bit fg + bold for the brand.
/// `BannerStyle::Default` never reaches here (spans exclude it).
fn banner_style(style: BannerStyle, scheme: ColorScheme) -> Style {
    let (r, g, b) = style.rgb(scheme);
    let mut s = Style::default().fg(Color::Rgb(r, g, b));
    if style.is_bold() {
        s = s.add_modifier(Modifier::BOLD);
    }
    s
}

/// Build the styled line for `text`, with styled byte-runs overriding the base
/// style. Segments outside the runs keep `base`; each run takes `base` patched
/// with its banner palette color (so a selection background survives).
fn span_line(text: &str, spans: &[SpanSpec], base: Style, scheme: ColorScheme) -> Line<'static> {
    if spans.is_empty() {
        return Line::from(Span::styled(text.to_string(), base));
    }
    let mut out = Vec::with_capacity(spans.len() * 2 + 1);
    let mut cur = 0usize;
    for SpanSpec { start, len, style } in spans {
        let start = *start;
        let end = start + *len;
        if start > cur {
            out.push(Span::styled(text[cur..start].to_string(), base));
        }
        out.push(Span::styled(
            text[start..end].to_string(),
            base.patch(banner_style(*style, scheme)),
        ));
        cur = end;
    }
    if cur < text.len() {
        out.push(Span::styled(text[cur..].to_string(), base));
    }
    Line::from(out)
}

/// Serialize a rendered buffer to a plain-text grid (one row per line).
///
/// Box-drawing borders and emoji markers survive as UTF-8; styles (color, bold)
/// are dropped. Used by the frame-capture log so a TUI session can be reviewed
/// afterward as a flipbook of text screens.
pub fn buffer_to_text(buf: &Buffer) -> String {
    let area = buf.area;
    let cells = buf.content();
    let width = area.width as usize;
    let mut out = String::with_capacity((width + 1) * area.height as usize);
    for y in 0..area.height {
        let mut line = String::new();
        let mut x = 0usize;
        while x < width {
            let sym = cells[y as usize * width + x].symbol();
            line.push_str(sym);
            // A wide glyph (CJK, etc.) is stored in one cell with the following
            // cell as a reset continuation; step over it so we don't emit a
            // stray space between wide chars.
            x += sym.width().max(1);
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
pub fn snapshot_text(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("offscreen terminal");
    terminal.draw(|f| draw(f, app)).expect("offscreen draw");
    buffer_to_text(terminal.backend().buffer())
}

#[cfg(test)]
mod tests {
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
            app.output.push(OutputLine { spans: None, original: None,
                text: format!("streamed line {i}"),
                kind: LineKind::Normal,
            });
        }
        app.output.push(OutputLine { spans: None, original: None,
            text: "⏺ [sub/1] read_file {\"path\":\"src/lib.rs\"}".into(),
            kind: LineKind::Tool,
        });
        app.output.push(OutputLine { spans: None, original: None,
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
            app.output.push(OutputLine { spans: None, original: None,
                text: format!("long line {i}"),
                kind: LineKind::Normal,
            });
        }
        app.follow_bottom = false;
        app.scroll_offset = 50;
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
            app.output.push(OutputLine { spans: None, original: None, text: format!("line {i}"), kind: LineKind::Normal });
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
            app.output.push(OutputLine { spans: None, original: None,
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
}
