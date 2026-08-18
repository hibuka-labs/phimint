//! ratatui frame rendering: transcript (output) / composer / status bar.

use phi_agent::RiskLevel;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
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

/// Calculate the display width of a string considering CJK characters.
/// CJK characters take 2 columns, ASCII takes 1.
fn unicode_display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if c.is_ascii() {
                1
            } else {
                // CJK Unified Ideographs and common fullwidth characters
                let cp = c as u32;
                if (0x4E00..=0x9FFF).contains(&cp)   // CJK Unified Ideographs
                    || (0x3000..=0x303F).contains(&cp) // CJK Symbols and Punctuation
                    || (0xFF00..=0xFFEF).contains(&cp) // Halfwidth and Fullwidth Forms
                    || (0x3400..=0x4DBF).contains(&cp) // CJK Unified Ideographs Extension A
                    || (0x20000..=0x2A6DF).contains(&cp) // CJK Unified Ideographs Extension B
                    || (0xF900..=0xFAFF).contains(&cp)
                {
                    // CJK Compatibility Ideographs
                    2
                } else {
                    1
                }
            }
        })
        .sum()
}

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

fn render_output(f: &mut Frame, app: &mut App, area: Rect) {
    let height = area.height as usize;

    // Committed lines: always present in `app.output`.
    let committed = app.output.len();

    // Streaming tail: for Normal (AI prose), render the full pending_text
    // through markdown so the user sees styled output during streaming.
    // For other kinds (Thought), fall back to pre-wrapped tail_lines.
    let tail_raw = app.streaming_tail_raw();
    // For non-Normal streaming tail, fall back to pre-wrapped tail_lines.
    let tail_lines_fallback = match app.streaming_tail_lines() {
        Some((lines, _)) => lines,
        None => &[][..],
    };

    // Pre-render committed lines and streaming tail into a flat vec.
    let mut lines: Vec<Line> = Vec::with_capacity(committed + 32);
    // Build mapping from visual line index → originating output index.
    // Used by `line_index_at` so mouse selection works on markdown-expanded
    // content.  Tail lines (no output index) map to `usize::MAX`.
    let mut visual_to_output: Vec<usize> = Vec::with_capacity(committed + 32);
    // Plain text for each visual line (parallel to visual_to_output).
    // Used by `selection_text` so copy returns visually-selected lines.
    let mut visual_lines_text: Vec<String> = Vec::with_capacity(committed + 32);

    // --- committed output ---
    let mut vis_idx: usize = 0; // running visual line counter
    for i in 0..committed {
        let line = &app.output[i];
        let kind = line.kind;
        let spans = line.spans.as_deref().unwrap_or(&[]);

        if kind == LineKind::Normal && spans.is_empty() {
            let md_lines = render_markdown(&line.text);
            for mut md_line in md_lines {
                if app.is_selected(vis_idx) {
                    apply_bg(&mut md_line, Color::DarkGray);
                }
                visual_lines_text.push(line_plain_text(&md_line));
                visual_to_output.push(i);
                lines.push(md_line);
                vis_idx += 1;
            }
        } else {
            let base = style_for(kind);
            let mut styled = span_line(&line.text, spans, base, app.scheme());
            if app.is_selected(vis_idx) {
                apply_bg(&mut styled, Color::DarkGray);
            }
            visual_lines_text.push(line_plain_text(&styled));
            visual_to_output.push(i);
            lines.push(styled);
            vis_idx += 1;
        }
    }

    // --- streaming tail ---
    if let Some((raw, kind)) = tail_raw {
        if kind == LineKind::Normal {
            let md_lines = render_markdown(raw);
            for md_line in md_lines {
                visual_lines_text.push(line_plain_text(&md_line));
                visual_to_output.push(usize::MAX); // sentinel: no output index
                lines.push(md_line);
            }
        } else {
            // Thought / other kinds: use pre-wrapped lines.
            for text in tail_lines_fallback.iter() {
                let base = style_for(kind);
                let styled = span_line(text, &[], base, app.scheme());
                visual_lines_text.push(text.clone());
                visual_to_output.push(usize::MAX);
                lines.push(styled);
            }
        }
    }

    // Visible window respecting scroll_offset and follow_bottom.
    let total = lines.len();
    app.rendered_total = total;
    app.visual_to_output = visual_to_output;
    app.visual_lines_text = visual_lines_text;
    let window = window_range(total, app, height);
    let visible: Vec<Line> = lines[window].to_vec();

    // No border/title — the transcript flows freely (Claude Code style); the
    // composer's own box is the visual boundary between output and input.
    f.render_widget(Paragraph::new(visible), area);
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

/// Convert an borrowed `Line<'_>` (from tui-markdown) into an owned `Line<'static>`,
/// merging the line-level style into each span so ratatui renders it.
fn line_to_static(line: Line<'_>) -> Line<'static> {
    let line_style = line.style;
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| {
            let merged = s.style.patch(line_style);
            Span::styled(s.content.into_owned(), merged)
        })
        .collect();
    Line::from(spans)
}

/// Apply a background color to every span and the line-level style.
fn apply_bg(line: &mut Line<'_>, bg: Color) {
    line.style = line.style.bg(bg);
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
}

/// Extract plain text from a ratatui `Line` (concatenate all span contents).
fn line_plain_text(line: &Line<'_>) -> String {
    let mut s = String::new();
    for span in &line.spans {
        s.push_str(&span.content);
    }
    s
}

/// Convert common LaTeX commands to Unicode symbols for terminal display.
fn latex_to_unicode(latex: &str) -> String {
    let mut s = latex.to_string();
    // Common symbols (order matters: longer patterns first)
    s = s.replace("\\rightarrow", "→");
    s = s.replace("\\leftarrow", "←");
    s = s.replace("\\Rightarrow", "⇒");
    s = s.replace("\\Leftarrow", "⇐");
    s = s.replace("\\times", "×");
    s = s.replace("\\div", "÷");
    s = s.replace("\\pm", "±");
    s = s.replace("\\mp", "∓");
    s = s.replace("\\leq", "≤");
    s = s.replace("\\geq", "≥");
    s = s.replace("\\neq", "≠");
    s = s.replace("\\approx", "≈");
    s = s.replace("\\equiv", "≡");
    s = s.replace("\\cdot", "·");
    s = s.replace("\\ldots", "…");
    s = s.replace("\\cdots", "⋯");
    s = s.replace("\\infty", "∞");
    s = s.replace("\\partial", "∂");
    s = s.replace("\\nabla", "∇");
    s = s.replace("\\sum", "∑");
    s = s.replace("\\prod", "∏");
    s = s.replace("\\int", "∫");
    s = s.replace("\\alpha", "α");
    s = s.replace("\\beta", "β");
    s = s.replace("\\gamma", "γ");
    s = s.replace("\\delta", "δ");
    s = s.replace("\\epsilon", "ε");
    s = s.replace("\\theta", "θ");
    s = s.replace("\\lambda", "λ");
    s = s.replace("\\mu", "μ");
    s = s.replace("\\pi", "π");
    s = s.replace("\\sigma", "σ");
    s = s.replace("\\phi", "φ");
    s = s.replace("\\omega", "ω");
    s = s.replace("\\Delta", "Δ");
    s = s.replace("\\Sigma", "Σ");
    s = s.replace("\\Omega", "Ω");
    // Display commands: remove the command, keep the content
    s = s.replace("\\boxed{", "");
    s = s.replace("\\boxed(", "(");  // \boxed(...) → (...)
    // Size/style commands: just remove
    for cmd in &["\\large", "\\Large", "\\LARGE", "\\huge", "\\Huge",
                  "\\small", "\\normalsize", "\\bfseries", "\\itshape",
                  "\\textbf{", "\\textit{", "\\underline{"] {
        s = s.replace(*cmd, "");
    }
    // \sqrt{n} → √n
    while let Some(start) = s.find("\\sqrt{") {
        if let Some(end) = s[start + 6..].find('}') {
            let inner = &s[start + 6..start + 6 + end];
            let replacement = format!("√{}", inner);
            s = format!("{}{}{}", &s[..start], replacement, &s[start + 6 + end + 1..]);
        } else {
            break;
        }
    }
    // Remove remaining braces used for grouping: {x} → x
    // Only remove simple single-char braces to avoid breaking nested expressions
    s = s.replace("\\mathbf{", "");
    s = s.replace("\\mathrm{", "");
    s = s.replace("\\text{", "");
    // Clean up remaining single braces
    let mut result = String::new();
    let mut depth = 0i32;
    for ch in s.chars() {
        match ch {
            '{' => {
                if depth == 0 {
                    depth += 1;
                    continue; // skip opening brace
                }
                depth += 1;
                result.push(ch);
            }
            '}' => {
                depth -= 1;
                if depth < 0 {
                    depth = 0; // unmatched close brace
                    continue;
                }
                if depth == 0 {
                    continue; // skip closing brace
                }
                result.push(ch);
            }
            _ => result.push(ch),
        }
    }
    result
}

/// Render markdown text using pulldown-cmark with custom styling.
///
/// Unlike tui-markdown (which is a "source viewer" that keeps `#` markers),
/// this renderer produces rich output: headings are styled without markers,
/// horizontal rules become separator lines, and inline formatting is applied.
fn render_markdown(text: &str) -> Vec<Line<'static>> {
    let parser = Parser::new_ext(text, Options::all());
    let mut writer = MarkdownWriter::new();
    for event in parser {
        writer.process_event(event);
    }
    writer.finish()
}

/// State machine for rendering pulldown-cmark events into ratatui Lines.
struct MarkdownWriter {
    lines: Vec<Line<'static>>,
    current_line: Line<'static>,
    inline_style: Style,
    in_code_block: bool,
    code_lang: String,
    needs_newline: bool,
    table_state: Option<TableState>,
}

/// State for accumulating table content.
struct TableState {
    alignments: Vec<pulldown_cmark::Alignment>,
    header: Vec<Vec<Vec<Span<'static>>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    current_row: Vec<Vec<Span<'static>>>,
    current_cell: Vec<Span<'static>>,
    in_header: bool,
    row_is_header: bool,
}

impl MarkdownWriter {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            current_line: Line::default(),
            inline_style: Style::default(),
            in_code_block: false,
            code_lang: String::new(),
            needs_newline: false,
            table_state: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_current_line();
        self.lines
    }

    fn flush_current_line(&mut self) {
        if !self.current_line.spans.is_empty() {
            self.lines.push(std::mem::take(&mut self.current_line));
        }
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.flush_current_line();
        self.lines.push(line);
    }

    fn push_blank_line(&mut self) {
        self.flush_current_line();
        self.lines.push(Line::default());
    }

    fn process_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.inline_code(&code),
            // Math events: render LaTeX as Unicode symbols (cyan, like inline code)
            Event::InlineMath(math) => {
                let span = Span::styled(
                    latex_to_unicode(&math),
                    Style::default().fg(Color::Cyan),
                );
                if let Some(ref mut ts) = self.table_state {
                    ts.current_cell.push(span);
                } else {
                    self.current_line.spans.push(span);
                }
            }
            Event::DisplayMath(math) => {
                // Display math: render on its own line(s)
                self.flush_current_line();
                let converted = latex_to_unicode(&math);
                for line in converted.lines() {
                    self.push_line(Line::from(Span::styled(
                        line.to_string(),
                        Style::default().fg(Color::Cyan),
                    )));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                self.flush_current_line();
            }
            Event::Rule => {
                self.flush_current_line();
                if !self.lines.is_empty() {
                    self.push_blank_line();
                }
                self.push_line(make_horizontal_rule());
                self.needs_newline = true;
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_current_line();
                if self.needs_newline {
                    self.push_blank_line();
                    self.needs_newline = false;
                }
                let style = heading_style(level);
                self.inline_style = style;
                // No # marker — just apply the style to content.
            }
            Tag::Paragraph => {
                if self.needs_newline {
                    self.push_blank_line();
                    self.needs_newline = false;
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush_current_line();
                if self.needs_newline {
                    self.push_blank_line();
                    self.needs_newline = false;
                }
                self.in_code_block = true;
                self.code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                    _ => String::new(),
                };
                // Code block header
                let header = if self.code_lang.is_empty() {
                    "  │".to_string()
                } else {
                    format!("  │ [{}]", self.code_lang)
                };
                self.push_line(Line::from(Span::styled(header, Style::default().fg(Color::DarkGray))));
            }
            Tag::List(_) => {
                self.flush_current_line();
                if self.needs_newline {
                    self.push_blank_line();
                    self.needs_newline = false;
                }
            }
            Tag::Item => {
                self.flush_current_line();
                self.current_line
                    .spans
                    .push(Span::styled("  • ", Style::default().fg(Color::DarkGray)));
            }
            Tag::BlockQuote(_) => {
                self.flush_current_line();
                self.current_line
                    .spans
                    .push(Span::styled("  │ ", Style::default().fg(Color::DarkGray)));
            }
            Tag::Table(alignments) => {
                self.flush_current_line();
                if self.needs_newline {
                    self.push_blank_line();
                    self.needs_newline = false;
                }
                self.table_state = Some(TableState {
                    alignments,
                    header: Vec::new(),
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    current_cell: Vec::new(),
                    in_header: false,
                    row_is_header: false,
                });
            }
            Tag::TableHead => {
                if let Some(ref mut ts) = self.table_state {
                    ts.in_header = true;
                    // pulldown-cmark 0.13: header cells are directly inside TableHead
                    // without a TableRow wrapper, so start collecting cells now
                    ts.current_row = Vec::new();
                }
            }
            Tag::TableRow => {
                if let Some(ref mut ts) = self.table_state {
                    ts.current_row = Vec::new();
                    // Capture header state at row start
                    ts.row_is_header = ts.in_header;
                }
            }
            Tag::TableCell => {
                if let Some(ref mut ts) = self.table_state {
                    ts.current_cell = Vec::new();
                }
            }
            Tag::Strong => {
                self.inline_style = self.inline_style.add_modifier(Modifier::BOLD);
            }
            Tag::Emphasis => {
                self.inline_style = self.inline_style.add_modifier(Modifier::ITALIC);
            }
            Tag::Strikethrough => {
                self.inline_style = self.inline_style.add_modifier(Modifier::CROSSED_OUT);
            }
            Tag::Link { dest_url, .. } => {
                // Links: cyan underline
                self.inline_style = self.inline_style.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED);
                // Store URL for potential tooltip/hover (future)
                let _ = dest_url;
            }
            Tag::Image { dest_url, .. } => {
                // Images: show as [Image: url]
                self.current_line.spans.push(Span::styled(
                    "[Image: ",
                    Style::default().fg(Color::DarkGray),
                ));
                let _ = dest_url;
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.flush_current_line();
                self.inline_style = Style::default();
                self.needs_newline = true;
            }
            TagEnd::Paragraph => {
                self.flush_current_line();
                self.needs_newline = true;
            }
            TagEnd::CodeBlock => {
                self.flush_current_line();
                self.in_code_block = false;
                self.code_lang.clear();
                self.needs_newline = true;
            }
            TagEnd::List(_) => {
                self.flush_current_line();
                self.needs_newline = true;
            }
            TagEnd::Item => {
                self.flush_current_line();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_current_line();
            }
            TagEnd::Table => {
                self.flush_current_line();
                if let Some(ts) = self.table_state.take() {
                    let table_lines = self.render_table(&ts);
                    for line in table_lines {
                        self.push_line(line);
                    }
                }
                self.needs_newline = true;
            }
            TagEnd::TableHead => {
                if let Some(ref mut ts) = self.table_state {
                    // Save header row collected directly from TableHead
                    let row = std::mem::take(&mut ts.current_row);
                    if !row.is_empty() {
                        ts.header.push(row);
                    }
                    ts.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(ref mut ts) = self.table_state {
                    let row = std::mem::take(&mut ts.current_row);
                    if ts.row_is_header {
                        ts.header.push(row);
                    } else {
                        ts.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(ref mut ts) = self.table_state {
                    let cell = std::mem::take(&mut ts.current_cell);
                    ts.current_row.push(cell);
                }
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                self.inline_style = Style::default();
            }
            TagEnd::Link => {
                self.inline_style = Style::default();
            }
            TagEnd::Image => {
                self.current_line.spans.push(Span::styled(
                    "]",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            // Code block content: monospace, dimmed
            for line in text.lines() {
                self.push_line(Line::from(Span::styled(
                    format!("  │ {line}"),
                    Style::default().fg(Color::Gray),
                )));
            }
        } else if let Some(ref mut ts) = self.table_state {
            // Table cell content
            let style = self.inline_style;
            ts.current_cell.push(Span::styled(text.to_string(), style));
        } else {
            let style = self.inline_style;
            self.current_line
                .spans
                .push(Span::styled(text.to_string(), style));
        }
    }

    fn inline_code(&mut self, code: &str) {
        // Inline code: cyan color (like Claude Code), no backticks, no background
        let span = Span::styled(
            code.to_string(),
            Style::default().fg(Color::Cyan),
        );
        if let Some(ref mut ts) = self.table_state {
            ts.current_cell.push(span);
        } else {
            self.current_line.spans.push(span);
        }
    }

    /// Render a complete table with box-drawing borders.
    /// Reference: codex-rs/tui/src/markdown_render.rs
    fn render_table(&self, ts: &TableState) -> Vec<Line<'static>> {
        // Calculate column widths from header + all rows
        let num_cols = ts.alignments.len();
        if num_cols == 0 {
            return Vec::new();
        }

        let mut col_widths = vec![0usize; num_cols];

        // Measure header cells
        for row in &ts.header {
            for (col, cell) in row.iter().enumerate() {
                if col < num_cols {
                    let w: usize = cell
                        .iter()
                        .map(|s| unicode_display_width(&s.content))
                        .sum();
                    col_widths[col] = col_widths[col].max(w);
                }
            }
        }

        // Measure body cells
        for row in &ts.rows {
            for (col, cell) in row.iter().enumerate() {
                if col < num_cols {
                    let w: usize = cell
                        .iter()
                        .map(|s| unicode_display_width(&s.content))
                        .sum();
                    col_widths[col] = col_widths[col].max(w);
                }
            }
        }

        // Ensure minimum width of 3
        for w in &mut col_widths {
            *w = (*w).max(3);
        }

        let mut result = Vec::new();

        // Build separator line: ├───┼───┤
        let build_separator = |col_widths: &[usize]| -> Line<'static> {
            let mut spans = Vec::new();
            spans.push(Span::styled("├", Style::default().fg(Color::DarkGray)));
            for (i, &w) in col_widths.iter().enumerate() {
                let dashes = "─".repeat(w + 2);
                spans.push(Span::styled(dashes, Style::default().fg(Color::DarkGray)));
                if i < col_widths.len() - 1 {
                    spans.push(Span::styled("┼", Style::default().fg(Color::DarkGray)));
                }
            }
            spans.push(Span::styled("┤", Style::default().fg(Color::DarkGray)));
            Line::from(spans)
        };

        // Build a data row with proper alignment and padding
        let build_row = |row: &[Vec<Span<'static>>],
                         col_widths: &[usize],
                         alignments: &[pulldown_cmark::Alignment]|
         -> Line<'static> {
            let mut spans = Vec::new();
            spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
            for (col, &width) in col_widths.iter().enumerate() {
                let cell = row.get(col);
                let cell_spans = cell.map(|c| c.as_slice()).unwrap_or(&[]);
                let cell_width: usize = cell_spans
                    .iter()
                    .map(|s| unicode_display_width(&s.content))
                    .sum();
                let pad = width.saturating_sub(cell_width);
                let alignment = alignments.get(col).copied().unwrap_or(pulldown_cmark::Alignment::None);

                let (left_pad, right_pad) = match alignment {
                    pulldown_cmark::Alignment::Center => (pad / 2, pad - pad / 2),
                    pulldown_cmark::Alignment::Right => (pad, 0),
                    _ => (0, pad),
                };

                spans.push(Span::styled(" ", Style::default()));
                if left_pad > 0 {
                    spans.push(Span::styled(" ".repeat(left_pad), Style::default()));
                }
                for s in cell_spans {
                    spans.push(s.clone());
                }
                if right_pad > 0 {
                    spans.push(Span::styled(" ".repeat(right_pad), Style::default()));
                }
                spans.push(Span::styled(" ", Style::default()));
                spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
            }
            Line::from(spans)
        };

        // Top border: ┌───┬───┐
        {
            let mut spans = Vec::new();
            spans.push(Span::styled("┌", Style::default().fg(Color::DarkGray)));
            for (i, &w) in col_widths.iter().enumerate() {
                let dashes = "─".repeat(w + 2);
                spans.push(Span::styled(dashes, Style::default().fg(Color::DarkGray)));
                if i < col_widths.len() - 1 {
                    spans.push(Span::styled("┬", Style::default().fg(Color::DarkGray)));
                }
            }
            spans.push(Span::styled("┐", Style::default().fg(Color::DarkGray)));
            result.push(Line::from(spans));
        }

        // Header rows
        for row in &ts.header {
            result.push(build_row(row, &col_widths, &ts.alignments));
        }

        // Separator after header
        result.push(build_separator(&col_widths));

        // Body rows
        for row in &ts.rows {
            result.push(build_row(row, &col_widths, &ts.alignments));
        }

        // Bottom border: └───┴───┘
        {
            let mut spans = Vec::new();
            spans.push(Span::styled("└", Style::default().fg(Color::DarkGray)));
            for (i, &w) in col_widths.iter().enumerate() {
                let dashes = "─".repeat(w + 2);
                spans.push(Span::styled(dashes, Style::default().fg(Color::DarkGray)));
                if i < col_widths.len() - 1 {
                    spans.push(Span::styled("┴", Style::default().fg(Color::DarkGray)));
                }
            }
            spans.push(Span::styled("┘", Style::default().fg(Color::DarkGray)));
            result.push(Line::from(spans));
        }

        result
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::ITALIC),
    }
}

/// Render a horizontal rule as a styled separator line.
fn make_horizontal_rule() -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(60),
        Style::default().fg(Color::DarkGray),
    ))
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

    #[test]
    fn custom_markdown_heading_and_hr() {
        // Test our custom pulldown-cmark renderer.
        let cases: Vec<(&str, &str)> = vec![
            ("h1", "# 标题"),
            ("h2", "## 标题"),
            ("h3", "### 🔑 核心设计亮点"),
            ("hr ---", "---"),
            ("hr ***", "***"),
            ("bold", "**粗体** 和 `code`"),
            ("setext", "标题\n---"),
        ];

        for (label, input) in &cases {
            let lines = render_markdown(input);
            println!("\n=== {label} === input: {input:?}");
            for (i, line) in lines.iter().enumerate() {
                let parts: Vec<String> = line
                    .spans
                    .iter()
                    .map(|s| {
                        let mods = format!("{:?}", s.style.add_modifier);
                        format!("[{:?} {}]", s.content, mods)
                    })
                    .collect();
                println!("  line[{i}]: {}", parts.join(" "));
            }
        }

        // Verify specific behaviors:
        // Heading: no # prefix
        let lines = render_markdown("### 🔑 核心设计亮点");
        let text = line_plain_text(&lines[0]);
        assert!(!text.starts_with("###"), "heading should not have # prefix: {text}");
        assert!(text.contains("🔑 核心设计亮点"));

        // Horizontal rule
        let lines = render_markdown("---");
        let text = line_plain_text(&lines[0]);
        assert!(text.chars().all(|c| c == '─'), "hr should be ───: {text:?}");

        // Bold
        let lines = render_markdown("**粗体**");
        let text = line_plain_text(&lines[0]);
        assert!(text.contains("粗体"));
        let has_bold = lines[0].spans.iter().any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "bold text should have BOLD modifier");

        // Table with header
        let table_md = "| Header1 | Header2 |\n|---------|--------|\n| Cell1   | Cell2  |";
        let lines = render_markdown(table_md);
        println!("\n=== Table rendering ===");
        for (i, line) in lines.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("  line[{i}]: {text:?}");
        }
        // Should have top border, header row, separator, body row, bottom border = 5 lines
        assert!(lines.len() >= 5, "table should have at least 5 lines, got {}", lines.len());
        // Header row should not be empty
        let header_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("Header1"), "header should contain 'Header1', got: {header_text:?}");
    }

    #[test]
    fn latex_to_unicode_conversion() {
        assert_eq!(latex_to_unicode("\\sqrt{9}"), "√9");
        assert_eq!(latex_to_unicode("\\sqrt{4}"), "√4");
        assert_eq!(latex_to_unicode("2 \\times 3"), "2 × 3");
        assert_eq!(latex_to_unicode("6 \\div 2"), "6 ÷ 2");
        assert_eq!(latex_to_unicode("\\pi r^2"), "π r^2");
        assert_eq!(latex_to_unicode("a \\neq b"), "a ≠ b");
        assert_eq!(latex_to_unicode("\\alpha + \\beta"), "α + β");
        assert_eq!(latex_to_unicode("\\sqrt{4} \\times 9"), "√4 × 9");
        assert_eq!(latex_to_unicode("$(\\sqrt{4} \\times 9 - 10) \\times 3$"), "$(√4 × 9 - 10) × 3$");
    }

    #[test]
    fn math_latex_rendering() {
        // Inline math should render with Unicode symbols
        let lines = render_markdown("计算步骤：$\\sqrt{4}=2$，$2\\times9=18$");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        // Should have Unicode symbols, not raw LaTeX
        assert!(text.contains("√4=2"), "should have √ symbol: {text}");
        assert!(text.contains("2×9=18"), "should have × symbol: {text}");
        // Math spans should be cyan
        let has_cyan = lines[0].spans.iter().any(|s| s.style.fg == Some(Color::Cyan));
        assert!(has_cyan, "math should be cyan colored");

        // Table with math cells
        let table_md = "| 步骤 | 运算 | 结果 |\n|------|------|------|\n| ① | $\\sqrt{4}$ | $2$ |";
        let lines = render_markdown(table_md);
        let body_line = &lines[3]; // 0: top, 1: header, 2: separator, 3: body, 4: bottom
        let body_text: String = body_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(body_text.contains("√4"), "table should have √ symbol: {body_text}");
    }
}
