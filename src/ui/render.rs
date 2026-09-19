//! ratatui frame rendering: transcript (output) / composer / status bar.

use phi_agent::RiskLevel;
use std::time::Instant;
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
    AgentStatus, App, CONTEXT_MENU_H, CONTEXT_MENU_W, FocusTarget, SubAgentStatus, context_menu_pos,
    is_writing_hint,
};
use phi_tui::lines::LineKind;
use phi_tui::markdown::{line_plain_text, render_markdown};
use phi_tui::wrap::wrap;

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
    let has_task_panel = app.should_show_task_panel();

    tracing::debug!(
        term_w,
        content_width,
        composer_inner_w,
        composer_vis,
        composer_height,
        has_task_panel,
        "draw layout"
    );

    let mut constraints = vec![
        Constraint::Min(3),                  // output (transcript)
    ];
    if has_task_panel {
        let task_count = app.sub_agents.len();
        constraints.push(Constraint::Length(task_count as u16 + 2)); // task panel
    }
    constraints.push(Constraint::Length(composer_height)); // composer
    constraints.push(Constraint::Length(1)); // status bar

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    // Record the output pane rect so mouse events can hit-test transcript rows.
    let out = chunks[0];
    app.output_area = Some((out.x, out.y, out.width, out.height));

    let mut idx = 0;
    render_output(f, app, chunks[idx]);
    idx += 1;
    if has_task_panel {
        render_task_panel(f, app, chunks[idx]);
        idx += 1;
    }
    render_composer(f, app, chunks[idx]);
    render_status(f, app, chunks[idx + 1]);

    if app.has_pending_approval() {
        render_approval_popup(f, app);
    }
    if app.context_menu().is_some() {
        render_context_menu(f, app);
    }
    if app.mention().is_some() {
        render_mention_popup(f, app, chunks[idx]);
    }
    if app.slash().is_some() {
        render_slash_popup(f, app, chunks[idx]);
    }
}

fn render_output(f: &mut Frame, app: &mut App, area: Rect) {
    let height = area.height as usize;

    // Choose which transcript to display based on focus
    let focused_agent: Option<String> = match &app.task_panel.focus {
        FocusTarget::TaskList(index) => app.sub_agents.keys().nth(*index).cloned(),
        FocusTarget::Input => None,
    };
    let (transcript, is_sub_agent) = match &focused_agent {
        Some(id) => match app.sub_agent_transcripts.get(id) {
            Some(sub_transcript) => (sub_transcript.as_slice(), true),
            None => (app.transcript.output.as_slice(), false),
        },
        None => (app.transcript.output.as_slice(), false),
    };

    // Streaming tail: for Normal (AI prose), render the full pending_text
    // through markdown so the user sees styled output during streaming.
    // For other kinds (Thought), fall back to pre-wrapped tail_lines.
    // A focused child shows ITS live tail — the whole point of following a
    // sub-agent is watching it work (e.g. the long silent report-writing
    // stretch that has no tool-call boundary to flush at).
    let (tail_raw, tail_lines_fallback): (Option<(String, LineKind)>, Vec<String>) =
        if is_sub_agent {
            let id = focused_agent.as_deref().expect("is_sub_agent implies focus");
            (
                app.child_stream_tail_raw(id),
                app.child_stream_tail_lines(id)
                    .map(|(lines, _)| lines)
                    .unwrap_or_default(),
            )
        } else {
            (
                app.streaming_tail_raw()
                    .map(|(raw, kind)| (raw.to_string(), kind)),
                app.streaming_tail_lines()
                    .map(|(lines, _)| lines.to_vec())
                    .unwrap_or_default(),
            )
        };

    let committed = transcript.len();

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
        let line = &transcript[i];
        let kind = line.kind;
        let spans = line.spans.as_deref().unwrap_or(&[]);

        // Diff detail: render the invocation line, then expand the diff block.
        if let Some(ref detail) = line.detail {
            match detail {
                phi_tui::lines::ToolDetail::Diff { path, hunks } => {
                    // Invocation line (same as non-diff tool lines)
                    let base = style_for(kind);
                    let mut styled = span_line(&line.text, spans, base, app.scheme());
                    if app.is_selected(vis_idx) {
                        apply_bg(&mut styled, Color::DarkGray);
                    }
                    visual_lines_text.push(line_plain_text(&styled));
                    visual_to_output.push(i);
                    lines.push(styled);
                    vis_idx += 1;

                    // Diff header: "┌─ path"
                    let header_text = format!("┌─ {path}");
                    let mut header_line = Line::from(Span::styled(
                        header_text.clone(),
                        Style::default().fg(Color::DarkGray),
                    ));
                    if app.is_selected(vis_idx) {
                        apply_bg(&mut header_line, Color::DarkGray);
                    }
                    visual_lines_text.push(header_text);
                    visual_to_output.push(i);
                    lines.push(header_line);
                    vis_idx += 1;

                    // Hunk lines
                    let wrap_w = app.transcript.wrap_width().saturating_sub(8).max(20);
                    // Compute max line number width for alignment
                    let max_line = hunks.iter()
                        .flat_map(|h| h.lines.iter())
                        .flat_map(|l| [l.old_line, l.new_line])
                        .flatten()
                        .max()
                        .unwrap_or(1);
                    let num_w = format!("{max_line}").len();

                    for hunk in hunks {
                        // Hunk header: "@@ -a,b +c,d @@"
                        let hunk_text = format!("│ {:>num_w$} {}", "", hunk.header);
                        let mut hunk_line = Line::from(Span::styled(
                            hunk_text.clone(),
                            Style::default().fg(Color::Cyan),
                        ));
                        if app.is_selected(vis_idx) {
                            apply_bg(&mut hunk_line, Color::DarkGray);
                        }
                        visual_lines_text.push(hunk_text);
                        visual_to_output.push(i);
                        lines.push(hunk_line);
                        vis_idx += 1;

                        for dl in &hunk.lines {
                            let (sign, color) = match dl.kind {
                                phi_tui::lines::DiffLineKind::Add => ("+", Color::Green),
                                phi_tui::lines::DiffLineKind::Del => ("-", Color::Red),
                                phi_tui::lines::DiffLineKind::Context => (" ", Color::DarkGray),
                            };
                            // Line number: prefer old_line for context/del, new_line for add
                            let line_num = match dl.kind {
                                phi_tui::lines::DiffLineKind::Add => dl.new_line,
                                phi_tui::lines::DiffLineKind::Del => dl.old_line,
                                phi_tui::lines::DiffLineKind::Context => dl.old_line,
                            };
                            let num_str = match line_num {
                                Some(n) => format!("{n:>num_w$}"),
                                None => " ".repeat(num_w),
                            };
                            // Wrap long lines, indent continuations
                            let full = format!("│ {num_str} {sign} {}", dl.text);
                            let cont_prefix = format!("│ {} {sign} ", " ".repeat(num_w));
                            let wrapped = wrap(&full, wrap_w + 8);
                            for (wi, wline) in wrapped.iter().enumerate() {
                                let display = if wi == 0 {
                                    wline.clone()
                                } else {
                                    format!("{cont_prefix}{wline}")
                                };
                                let mut styled = Line::from(Span::styled(
                                    display.clone(),
                                    Style::default().fg(color),
                                ));
                                if app.is_selected(vis_idx) {
                                    apply_bg(&mut styled, Color::DarkGray);
                                }
                                visual_lines_text.push(display);
                                visual_to_output.push(i);
                                lines.push(styled);
                                vis_idx += 1;
                            }
                        }
                    }
                    continue; // skip the normal rendering path below
                }
            }
        }

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
    if let Some((raw, kind)) = &tail_raw {
        if *kind == LineKind::Normal {
            let md_lines = render_markdown(raw);
            for md_line in md_lines {
                visual_lines_text.push(line_plain_text(&md_line));
                visual_to_output.push(usize::MAX); // sentinel: no output index
                lines.push(md_line);
            }
        } else {
            // Thought / other kinds: use pre-wrapped lines.
            for text in tail_lines_fallback.iter() {
                let base = style_for(*kind);
                let styled = span_line(text, &[], base, app.scheme());
                visual_lines_text.push(text.clone());
                visual_to_output.push(usize::MAX);
                lines.push(styled);
            }
        }
    }

    // Visible window respecting scroll_offset and follow_bottom.
    let total = lines.len();
    app.visual_to_output = visual_to_output;
    app.visual_lines_text = visual_lines_text;
    app.viewport.set_visible(total, height);
    let window = app.viewport.window_range(total, height);
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
        AgentStatus::Waiting { .. } => Style::default().fg(Color::Cyan),
        AgentStatus::Running { .. } => Style::default().fg(Color::Yellow),
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(app.status_line(), style))), area);
}

/// Format files list for display (max 2 files, truncate with ...).
fn format_files(files: &[String], max_width: usize) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut result = String::new();
    let mut count = 0;
    for file in files {
        if count >= 2 {
            result.push_str("...");
            break;
        }
        if !result.is_empty() {
            result.push_str(", ");
        }
        // Show just the filename, not the full path
        let name = file.split('/').last().unwrap_or(file);
        result.push_str(name);
        count += 1;
    }
    // Truncate if too long
    if result.len() > max_width {
        format!("{}...", &result[..max_width.saturating_sub(3)])
    } else {
        result
    }
}

/// Format elapsed time as seconds.
fn format_time(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// Char-safe truncation to exactly `max` chars, ellipsis-terminated.
/// ASCII `...` instead of `…`: CJK fonts render `…` double-width while the
/// layout counts it single-width, and the accumulated drift pushes the
/// composer off-screen (session 20260908_a9f7a846).
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{head}...")
    }
}

/// Render the task panel showing sub-agents and their status.
fn render_task_panel(f: &mut Frame, app: &App, area: Rect) {
    let total = app.sub_agents.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!("Tasks ({})", total));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Column widths adapt to content: the longest name sets the name column
    // (capped — `analyze-deepseek-harness` must not shove later columns out
    // of alignment), and the activity column takes whatever width is left.
    let name_w = app
        .sub_agents
        .values()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 16);
    let files_w = 20usize;
    // marker+" ", icon+" ", `│ `+5 for the time column.
    let fixed_w = 2 + 2 + name_w + files_w + 2 + 5;
    let act_w = (inner.width as usize).saturating_sub(fixed_w).min(30);

    let mut lines: Vec<Line> = Vec::new();
    for (i, (_agent_id, state)) in app.sub_agents.iter().enumerate() {
        let is_selected = matches!(&app.task_panel.focus, FocusTarget::TaskList(idx) if *idx == i);
        let marker = if is_selected { ">" } else { " " };
        let (status_icon, status_color) = match state.status {
            SubAgentStatus::Running => ("*", Color::Cyan),
            SubAgentStatus::Done => ("+", Color::Green),
        };
        let files = format_files(&state.files, 20);
        // Done agents show their final runtime; only Running ones keep ticking.
        let time = format_time(match state.completed_at {
            Some(at) => at.duration_since(state.started_at),
            None => state.started_at.elapsed(),
        });
        // Current activity = the latest tool event (> in flight, + finished).
        // Kept to a tool name — the child's detail view has the full history.
        // A Running agent whose tool feed has gone quiet switches to the
        // `writing...` hint: it is generating prose/thought between tool
        // calls (the long silent report-writing stretch has no tool events).
        let activity = if act_w > 6 {
            if is_writing_hint(state, Instant::now()) {
                (ellipsize("writing...", act_w), Color::DarkGray)
            } else {
                state
                    .events
                    .last()
                    .map(|e| {
                        let (mark, color) = if e.is_finished {
                            ("+", Color::DarkGray)
                        } else {
                            (">", Color::Reset)
                        };
                        (ellipsize(&format!("{mark} {}", e.tool_name), act_w), color)
                    })
                    .unwrap_or((String::new(), Color::Reset))
            }
        } else {
            (String::new(), Color::Reset)
        };
        let bg_color = if is_selected {
            Color::DarkGray
        } else {
            Color::Reset
        };

        let spans = vec![
            Span::styled(format!("{marker} "), Style::default().bg(bg_color)),
            Span::styled(format!("{status_icon} "), Style::default().fg(status_color).bg(bg_color)),
            Span::styled(format!("{:<name_w$}", ellipsize(&state.name, name_w)), Style::default().bg(bg_color)),
            Span::styled(
                format!("{:<act_w$}", activity.0),
                Style::default().fg(activity.1).bg(bg_color),
            ),
            Span::styled(format!("{:<files_w$}", files), Style::default().fg(Color::DarkGray).bg(bg_color)),
            Span::styled(format!("│ {:>5}", time), Style::default().fg(Color::DarkGray).bg(bg_color)),
        ];
        lines.push(Line::from(spans));
    }

    // Background shell tasks are NOT panel rows (2026-09-19): they render as
    // transcript tool-call records (launch line carries `background: true`,
    // completion via the bg-wake turn) plus the status-bar counter. The
    // panel's only switchable views were ever the sub-agents.

    f.render_widget(Paragraph::new(lines), inner);
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
            "!! Approval required",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    // D4：来源标识——子 agent 发起时标注（独立行），主 agent 保持现状。
    if let Some(source) = &request.source {
        lines.push(Line::from(Span::styled(
            format!("requested by sub-agent [{source}]"),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        request.title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("Risk: {risk}"),
        Style::default().fg(risk_color),
    )));
    lines.push(Line::from(""));
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
    let total = m.entries().len();
    let start = if total <= LIST_HEIGHT {
        0
    } else if m.selected_index() < LIST_HEIGHT / 2 {
        0
    } else {
        (m.selected_index() - LIST_HEIGHT / 2).min(total - LIST_HEIGHT)
    };
    let end = (start + LIST_HEIGHT).min(total);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("@{}", m.prefix()),
        Style::default().fg(Color::Cyan),
    )));
    lines.push(Line::from(Span::styled(
        "──────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    for (i, e) in m.entries()[start..end].iter().enumerate() {
        let idx = start + i;
        let marker = if e.synthetic {
            "» "
        } else if e.is_dir {
            "📁 "
        } else {
            "📄 "
        };
        let style = if idx == m.selected_index() {
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

    let total = s.entries().len();
    let start = if total <= LIST_HEIGHT {
        0
    } else if s.selected_index() < LIST_HEIGHT / 2 {
        0
    } else {
        (s.selected_index() - LIST_HEIGHT / 2).min(total - LIST_HEIGHT)
    };
    let end = (start + LIST_HEIGHT).min(total);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("/{}", s.prefix()),
        Style::default().fg(Color::Cyan),
    )));
    lines.push(Line::from(Span::styled(
        "────────────────────────────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));

    if s.entries().is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching skills",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (i, (name, desc)) in s.entries()[start..end].iter().enumerate() {
            let idx = start + i;
            let (name_style, desc_style) = if idx == s.selected_index() {
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
            // 截断描述，避免超出弹窗宽度。按字符而非字节（byte 切片会在
            // CJK 中间断开直接 panic）；ASCII "..." 而非 "…"（CJK 字体双宽）。
            let max_desc = 40usize;
            let short_desc = if desc.chars().count() > max_desc {
                let head: String = desc.chars().take(max_desc - 3).collect();
                format!("{head}...")
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

/// Apply a background color to every span and the line-level style.
fn apply_bg(line: &mut Line<'_>, bg: Color) {
    line.style = line.style.bg(bg);
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
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
#[path = "render_tests.rs"]
mod render_tests;
