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
use std::time::Instant;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::banner::{BannerStyle, ColorScheme, SpanSpec};
use crate::ui::app::{
    AgentStatus, App, CONTEXT_MENU_H, CONTEXT_MENU_W, FocusTarget, ResultTier, SubAgentStatus,
    context_menu_pos, is_writing_hint,
};
use crate::ui::task_panel::ThinkingPanelState;
use crate::ui::theme;
use phi_tui::completer::{MentionCompleter, SlashCompleter};
use phi_tui::layout::{Preview, folded_body, opens_block};
use phi_tui::lines::{LineKind, ToolState};
use phi_tui::markdown::{line_plain_text, render_markdown};
use phi_tui::popup_list::{GUTTER_W, PopupList, band_width};
use phi_tui::wrap::{Elide, elide, pad_cols, wrap, wrap_line};

/// Max composer rows shown (its box grows with the buffer up to this).
const MAX_COMPOSER_ROWS: usize = 8;

/// Thinking panel total height (border 2 + 6 content lines).
const THINKING_PANEL_H: u16 = 8;
/// History rows that must survive below the top of the output pane for the
/// panel to show — thinking must never occlude the whole transcript.
const MIN_PANEL_HISTORY_ROWS: u16 = 2;
/// Below this terminal height the thinking panel hides entirely. Belt and
/// suspenders: at current composer bounds the fit guard already hides it
/// (term < 14 ⇒ output < 10), but a future composer/layout change must not
/// resurrect the squeeze this guard exists for.
const MIN_PANEL_TERM_H: u16 = 12;

pub fn draw(f: &mut Frame, app: &mut App) {
    // Output area has no border — use the full terminal width for wrapping.
    let term_w = f.area().width;
    let content_width = term_w as usize;
    app.set_wrap_width(content_width);

    // Composer height: visual rows (accounting for soft-wrap) + 2 for border.
    // The composer's Block::borders(ALL) consumes 2 columns (left+right).
    let composer_inner_w = term_w.saturating_sub(2);
    let composer_vis = app
        .composer
        .visual_height(composer_inner_w)
        .min(MAX_COMPOSER_ROWS);
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
        Constraint::Min(3), // output (transcript)
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

    // Streaming tail: only pending PROSE still renders inline (markdown, as
    // before). A pending THOUGHT lives in the thinking panel inline at the
    // flow tail (8 placeholder rows — see the panel block below) — the old
    // full-tail inline render was the wall of scrolling dim text that panel
    // replaces. A focused child shows ITS live tail — the whole point of
    // following a sub-agent is watching it work (e.g. the long silent
    // report-writing stretch with no tool-call boundary).
    let tail_raw: Option<(String, LineKind)> = if is_sub_agent {
        let id = focused_agent
            .as_deref()
            .expect("is_sub_agent implies focus");
        app.child_stream_tail_raw(id)
    } else {
        app.streaming_tail_raw()
            .map(|(raw, kind)| (raw.to_string(), kind))
    };

    let committed = transcript.len();

    // Pre-render committed lines and streaming tail into a flat vec.
    let mut lines: Vec<Line> = Vec::with_capacity(committed + 32);
    // Row bookkeeping (visual row -> output index + plain text for copy) in
    // one aligned map; tail/panel rows are unselectable. See `phi_tui::visual`.
    let mut visual_map = phi_tui::visual::VisualMap::with_capacity(committed + 32);

    // --- committed output ---
    // Theme for this frame: the scheme is resolved once at startup, and every
    // gray/truecolor site below reads its value from `theme` slots.
    let scheme = app.scheme();
    let sel_bg = theme::selection_bg(scheme);
    // Which lines open a block. Decided up front so the detail branches below
    // don't each have to carry rhythm state through their `continue`s.
    let opens: Vec<bool> = {
        let mut v = Vec::with_capacity(committed);
        let mut prev: Option<&phi_tui::lines::OutputLine<BannerStyle>> = None;
        for line in transcript.iter() {
            v.push(prev.is_none_or(|p| opens_block(p, line)));
            prev = Some(line);
        }
        v
    };
    let mut vis_idx: usize = 0; // running visual line counter
    for i in 0..committed {
        // Block spacer. Render-time only: it is a row on the screen and not a
        // row in `transcript.output`, so history, selection indices,
        // `follow_bottom`, export and resume digests never see it (the same
        // rule the thinking fold follows).
        if i > 0 && opens[i] {
            lines.push(Line::from(Span::raw("")));
            visual_map.push_unselectable(String::new());
            vis_idx += 1;
        }
        let line = &transcript[i];
        let kind = line.kind;
        let spans = line.spans.as_deref().unwrap_or(&[]);

        // Diff detail: render the invocation line, then expand the diff block.
        if let Some(ref detail) = line.detail {
            match detail {
                phi_tui::lines::LineDetail::Diff { path, hunks } => {
                    // Invocation line: same composed tool row as everywhere
                    // else (state glyph + name + grey args).
                    let mut styled = tool_invocation_line(
                        &line.text,
                        line.tool_state.unwrap_or(ToolState::Running),
                        app.spinner_char(),
                        scheme,
                    );
                    if app.is_selected(vis_idx) {
                        apply_bg(&mut styled, sel_bg);
                    }
                    visual_map.push_mapped(i, line_plain_text(&styled));
                    lines.push(styled);
                    vis_idx += 1;

                    // Diff header: "┌─ path"
                    let header_text = format!("┌─ {path}");
                    let mut header_line = Line::from(Span::styled(
                        header_text.clone(),
                        Style::default().fg(theme::faint(scheme)),
                    ));
                    if app.is_selected(vis_idx) {
                        apply_bg(&mut header_line, sel_bg);
                    }
                    visual_map.push_mapped(i, header_text);
                    lines.push(header_line);
                    vis_idx += 1;

                    // Hunk lines
                    let wrap_w = app.transcript.wrap_width().saturating_sub(8).max(20);
                    // Compute max line number width for alignment
                    let max_line = hunks
                        .iter()
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
                            apply_bg(&mut hunk_line, sel_bg);
                        }
                        visual_map.push_mapped(i, hunk_text);
                        lines.push(hunk_line);
                        vis_idx += 1;

                        for dl in &hunk.lines {
                            let (sign, color) = match dl.kind {
                                phi_tui::lines::DiffLineKind::Add => ("+", Color::Green),
                                phi_tui::lines::DiffLineKind::Del => ("-", Color::Red),
                                phi_tui::lines::DiffLineKind::Context => {
                                    (" ", theme::faint(scheme))
                                }
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
                                    apply_bg(&mut styled, sel_bg);
                                }
                                visual_map.push_mapped(i, display);
                                lines.push(styled);
                                vis_idx += 1;
                            }
                        }
                    }
                    continue; // skip the normal rendering path below
                }
                phi_tui::lines::LineDetail::Folded {
                    raw,
                    line_count,
                    char_count,
                    meta_head,
                } => {
                    let width = app.transcript.wrap_width();
                    // ── thinking ──────────────────────────────────────────
                    // Folded by default (one summary line); Ctrl+O expands to
                    // the full text re-wrapped at the current width. Folding
                    // is a render-time decision — the line always carries the
                    // full text in `detail.raw`.
                    if kind == LineKind::Thought {
                        if !app.show_thoughts {
                            // No leading `>`: that is the user line's marker
                            // (transcript.rs `push_user`) and a summary that
                            // starts with `>` reads as something you typed.
                            let summary = format!(
                                "thinking - {line_count} lines - ~{} tok",
                                fmt_k(*char_count / 3)
                            );
                            let mut styled = Line::from(Span::styled(
                                summary.clone(),
                                style_for(LineKind::Thought, scheme),
                            ));
                            if app.is_selected(vis_idx) {
                                apply_bg(&mut styled, sel_bg);
                            }
                            visual_map.push_mapped(i, summary);
                            lines.push(styled);
                            vis_idx += 1;
                            continue;
                        }
                        for wline in wrap(raw, width) {
                            let mut styled = Line::from(Span::styled(
                                wline.clone(),
                                style_for(LineKind::Thought, scheme),
                            ));
                            if app.is_selected(vis_idx) {
                                apply_bg(&mut styled, sel_bg);
                            }
                            visual_map.push_mapped(i, wline);
                            lines.push(styled);
                            vis_idx += 1;
                        }
                        continue;
                    }

                    // ── tool block: the preview ladder (design §4) ────────
                    // The meta row is already in `text`. Everything under it is
                    // the payload, shown 0 / 2 / all rows by `result_tier`.
                    // Only display changes: `raw` always holds the tool's
                    // answer whole, so the transcript, selection and copy are
                    // unaffected by which tier is on.
                    let meta = if kind == LineKind::Tool {
                        tool_invocation_line(
                            &line.text,
                            line.tool_state.unwrap_or(ToolState::Running),
                            app.spinner_char(),
                            scheme,
                        )
                    } else {
                        result_meta_line(&line.text, kind, scheme)
                    };
                    let mut styled = meta;
                    if app.is_selected(vis_idx) {
                        apply_bg(&mut styled, sel_bg);
                    }
                    visual_map.push_mapped(i, line_plain_text(&styled));
                    lines.push(styled);
                    vis_idx += 1;

                    // Which payload rows the tier owes: the meta row's
                    // relationship to `raw`'s head is recorded at commit time
                    // (`MetaHead`), so nothing is re-derived from the width
                    // here. Wrapping/indenting is ours; row selection is
                    // `phi_tui::layout`.
                    let preview = match app.result_tier {
                        ResultTier::Compact => Preview::Rows(0),
                        ResultTier::Default => Preview::Rows(2),
                        ResultTier::Expanded => Preview::All,
                    };
                    let fold = folded_body(raw, *meta_head, preview);
                    let inner = width.saturating_sub(FOLD_INDENT.len()).max(20);
                    let mut payload: Vec<String> = Vec::new();
                    // An abbreviated head comes back in full at Expanded —
                    // otherwise "expand" would still be hiding text. It is
                    // drawn *first*, so the expanded payload keeps the
                    // answer's own order (design: order may never change).
                    if let Some(head) = fold.restore_head {
                        payload.extend(wrap(head, inner));
                    }
                    for row in &fold.rows {
                        payload.extend(wrap(row, inner));
                    }
                    for wline in payload {
                        let display = format!("{FOLD_INDENT}{wline}");
                        let mut styled =
                            Line::from(Span::styled(display.clone(), style_for(kind, scheme)));
                        if app.is_selected(vis_idx) {
                            apply_bg(&mut styled, sel_bg);
                        }
                        visual_map.push_mapped(i, display);
                        lines.push(styled);
                        vis_idx += 1;
                    }
                    if fold.hidden > 0 {
                        // ASCII `...`: `…` is CJK-width-unsafe (render_tests.rs).
                        let hint = format!("{FOLD_INDENT}... +{} lines", fold.hidden);
                        let mut styled = Line::from(Span::styled(
                            hint.clone(),
                            Style::default().fg(theme::faint(scheme)),
                        ));
                        if app.is_selected(vis_idx) {
                            apply_bg(&mut styled, sel_bg);
                        }
                        visual_map.push_mapped(i, hint);
                        lines.push(styled);
                        vis_idx += 1;
                    }
                    continue;
                }
            }
        }

        if kind == LineKind::Normal && spans.is_empty() {
            // Markdown emits one styled Line per paragraph and the transcript
            // `Paragraph` does not soft-wrap — an over-wide line is clipped at
            // the pane edge, so the tail of a long answer disappears from the
            // screen. Wrap each markdown row into visual rows (span-level,
            // style-preserving) and map every row back to source line `i`.
            let width = app.transcript.wrap_width();
            let md_lines = render_markdown(&line.text);
            for md_line in md_lines {
                for mut wrapped in wrap_line(md_line, width) {
                    if app.is_selected(vis_idx) {
                        apply_bg(&mut wrapped, sel_bg);
                    }
                    visual_map.push_mapped(i, line_plain_text(&wrapped));
                    lines.push(wrapped);
                    vis_idx += 1;
                }
            }
        } else {
            // A tool invocation is composed (state glyph + name + grey args),
            // a result row splits the connector from the answer — only these
            // two need anything fancier than "paint the whole line".
            let mut styled = if kind == LineKind::Tool {
                tool_invocation_line(
                    &line.text,
                    line.tool_state.unwrap_or(ToolState::Running),
                    app.spinner_char(),
                    scheme,
                )
            } else if kind == LineKind::ToolResult {
                result_meta_line(&line.text, kind, scheme)
            } else {
                span_line(&line.text, spans, style_for(kind, scheme), scheme)
            };
            // Exempt `push_styled_line` rows (spans + no `original`) from the
            // wrap below. They are fixed-width chrome — banner wordmark art and
            // the full-width `─` rule — and phi-tui's `push_styled_line`
            // contract is "**no** soft-wrap (the wordmark must stay whole;
            // ratatui clips on narrow terminals), no re-wrap on width change
            // (`original: None`)". Wrapping breaks that in two ways: the rule
            // has no whitespace to break at, so it hard-splits into dash
            // fragments, and the 60-col wordmark splits at its letter gaps.
            // Emit as ONE visual row; ratatui clips at the pane edge.
            if line.spans.is_some() && line.original.is_none() {
                if app.is_selected(vis_idx) {
                    apply_bg(&mut styled, sel_bg);
                }
                visual_map.push_mapped(i, line_plain_text(&styled));
                lines.push(styled);
                vis_idx += 1;
            } else {
                // Same wrap contract as the markdown path: an over-wide tool or
                // attributed line is clipped by the pane otherwise (lines already
                // shortened with `elide`/`one_line` fit and take the fast path).
                let width = app.transcript.wrap_width();
                for mut wrapped in wrap_line(styled, width) {
                    if app.is_selected(vis_idx) {
                        apply_bg(&mut wrapped, sel_bg);
                    }
                    visual_map.push_mapped(i, line_plain_text(&wrapped));
                    lines.push(wrapped);
                    vis_idx += 1;
                }
            }
        }
    }

    // --- streaming tail (prose only) ---
    if let Some((raw, LineKind::Normal)) = &tail_raw {
        // Same wrap contract as the committed markdown path above: the live
        // tail is uncommitted prose, and clipping it hides the end of an
        // in-flight answer until it commits.
        let width = app.transcript.wrap_width();
        let md_lines = render_markdown(raw);
        for md_line in md_lines {
            for wrapped in wrap_line(md_line, width) {
                visual_map.push_unselectable(line_plain_text(&wrapped));
                lines.push(wrapped);
            }
        }
    }

    // In-flight thinking: two presentations behind Ctrl+O (`show_thoughts`),
    // one toggle flipping both live thinking AND history thoughts together.
    // Loose (on): stream every wrapped row of the thought straight into the
    // flow — print as it thinks, no box (the pre-panel behavior) — history
    // thoughts expand alongside. Children carry `[{agent}] ` on the first
    // row (loose has no panel title to name the author); the prefix is the
    // same one `flush_thought` bakes into `detail.raw`, so the commit seam
    // does not drop it. Boxed (off, default): fixed-height TailPanel on
    // placeholder rows at the flow tail (where thinking happens), so scroll
    // accounting treats it like content — reviewing history scrolls it away
    // with everything else and follow-bottom brings it back. The bordered
    // widget is overlaid only when the whole box fits inside the visible
    // window; a box cut by the window edge is left blank (it is one unit,
    // not clip-able rows). Terminal/fit guards are boxed-only — loose rows
    // are ordinary content and nothing squeezes. Panel body stays
    // UNPREFIXED (its title names the agent).
    let panel: Option<ThinkingPanelState> = if app.show_thoughts {
        if let Some(state) = app.thinking_panel_state() {
            let base = style_for(LineKind::Thought, scheme);
            let agent_prefix = state
                .agent
                .as_ref()
                .map(|a| format!("[{a}] "))
                .unwrap_or_default();
            for (n, row) in state.lines.iter().enumerate() {
                // Attribution rides the first row only (tool-line convention).
                // Re-wrap just that row so the prefix cannot overflow the pane.
                let rows: Vec<String> = if n == 0 && !agent_prefix.is_empty() {
                    wrap(&format!("{agent_prefix}{row}"), app.transcript.wrap_width())
                } else {
                    vec![row.clone()]
                };
                for r in rows {
                    visual_map.push_unselectable(r.clone());
                    lines.push(Line::from(Span::styled(r, base)));
                }
            }
        }
        None
    } else {
        app.thinking_panel_state().filter(|_| {
            f.area().height >= MIN_PANEL_TERM_H
                && height >= (THINKING_PANEL_H + MIN_PANEL_HISTORY_ROWS) as usize
        })
    };
    let panel_flow_start = lines.len();
    if panel.is_some() {
        for _ in 0..THINKING_PANEL_H {
            visual_map.push_unselectable(String::new());
            lines.push(Line::default());
        }
    }

    // Scroll anchor: the output line currently at the window top (previous
    // frame's map) plus how far the head sits INSIDE that line's visual block.
    // `phi_tui::visual` owns the arithmetic: expanded thinking wraps one
    // OutputLine into dozens of rows, and the head routinely lands mid-block,
    // so resolving to the block's first row would fling the view a whole
    // block upward and undo every one-row scroll-down (session 20260923:
    // "can't scroll to the bottom after Ctrl+O"). Unselectable tail rows and
    // the first frame fall back to index-holding inside `set_visible_anchored`.
    let anchor = {
        let prev_total = app.visual_map.len();
        let prev_height = app.viewport.viewport_height.max(1);
        let prev_window = app.viewport.window_range(prev_total, prev_height);
        app.visual_map.anchor_at(prev_window.start)
    };

    // Visible window respecting scroll_offset and follow_bottom.
    let total = lines.len();
    let preferred_head = anchor.and_then(|a| visual_map.resolve_anchor(a));
    app.visual_map = visual_map;
    app.viewport
        .set_visible_anchored(total, height, preferred_head);
    let window = app.viewport.window_range(total, height);

    // The panel's screen rect = where its placeholder rows land in the window.
    let panel_rect = if panel.is_some()
        && window.start <= panel_flow_start
        && panel_flow_start + THINKING_PANEL_H as usize <= window.end
    {
        Some(Rect {
            x: area.x,
            y: area.y + (panel_flow_start - window.start) as u16,
            width: area.width,
            height: THINKING_PANEL_H,
        })
    } else {
        None
    };

    let visible: Vec<Line> = lines[window].to_vec();

    // No border/title — the transcript flows freely (Claude Code style); the
    // composer's own box is the visual boundary between output and input.
    f.render_widget(Paragraph::new(visible), area);

    if let (Some(rect), Some(state)) = (panel_rect, &panel) {
        f.render_widget(Clear, rect);
        render_thinking_panel(
            f,
            app,
            rect,
            &state.lines,
            state.agent.as_deref(),
            state.chars,
        );
    }
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
            if let Some((vis_row, vis_col)) = cursor_pos
                && j == vis_row
            {
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
            items.push(Line::from(row.clone()));
        }
    }

    // Scroll to show the bottom of the buffer (cursor is usually at the end).
    let total = items.len();
    let scroll = total.saturating_sub(inner_height) as u16;

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded);
    f.render_widget(Paragraph::new(items).block(block).scroll((scroll, 0)), area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let style = match app.status {
        AgentStatus::Idle => Style::default().fg(Color::Green),
        AgentStatus::Waiting { .. } => Style::default().fg(Color::Cyan),
        AgentStatus::Running { .. } => Style::default().fg(Color::Yellow),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(app.status_line(), style))),
        area,
    );
}

/// Format files list for display (max 2 files, truncate with ...).
fn format_files(files: &[String], max_width: usize) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut result = String::new();
    for file in files.iter().take(2) {
        if !result.is_empty() {
            result.push_str(", ");
        }
        // Show just the filename, not the full path
        let name = file.split('/').next_back().unwrap_or(file);
        result.push_str(name);
    }
    if files.len() > 2 {
        result.push_str("...");
    }
    // Truncate to the column budget. `&result[..n]` is a BYTE slice and panics
    // mid-char on a multi-byte basename ("end byte index N is not a char
    // boundary; it is inside <a multi-byte char>") -- and since this runs
    // inside `render`, that panic took the whole TUI down. `elide` budgets by
    // display columns, is char-boundary safe, and is a no-op when the text
    // already fits.
    elide(&result, max_width, Elide::Head)
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

/// Render the task panel showing sub-agents and their status.
fn render_task_panel(f: &mut Frame, app: &App, area: Rect) {
    let scheme = app.scheme();
    let total = app.sub_agents.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!("Tasks ({})", total));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Column widths adapt to content in DISPLAY COLUMNS: the longest name sets
    // the name column (capped — `analyze-deepseek-harness` must not shove later
    // columns out of alignment), and the activity column takes whatever width
    // is left. Measuring and padding by char count instead pushes every later
    // column right for a CJK name (1 char = 2 columns) and breaks alignment.
    let name_w = app
        .sub_agents
        .values()
        .map(|s| UnicodeWidthStr::width(s.name.as_str()))
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
                (
                    elide("writing...", act_w, Elide::Head),
                    theme::faint(scheme),
                )
            } else {
                state
                    .events
                    .last()
                    .map(|e| {
                        let (mark, color) = if e.is_finished {
                            ("+", theme::faint(scheme))
                        } else {
                            (">", Color::Reset)
                        };
                        (
                            elide(&format!("{mark} {}", e.tool_name), act_w, Elide::Head),
                            color,
                        )
                    })
                    .unwrap_or((String::new(), Color::Reset))
            }
        } else {
            (String::new(), Color::Reset)
        };
        let bg_color = if is_selected {
            theme::selection_bg(scheme)
        } else {
            Color::Reset
        };

        // Every cell is elided to its column then padded OUT to it, in display
        // columns: `format!("{:<w$}")` pads by chars and would overshoot.
        let spans = vec![
            Span::styled(format!("{marker} "), Style::default().bg(bg_color)),
            Span::styled(
                format!("{status_icon} "),
                Style::default().fg(status_color).bg(bg_color),
            ),
            Span::styled(
                pad_cols(&elide(&state.name, name_w, Elide::Head), name_w),
                Style::default().bg(bg_color),
            ),
            Span::styled(
                pad_cols(&activity.0, act_w),
                Style::default().fg(activity.1).bg(bg_color),
            ),
            Span::styled(
                pad_cols(&files, files_w),
                Style::default().fg(theme::faint(scheme)).bg(bg_color),
            ),
            Span::styled(
                format!("│ {:>5}", time),
                Style::default().fg(theme::faint(scheme)).bg(bg_color),
            ),
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
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    // D4 source marker: a sub-agent's request is tagged on its own line; the main agent is unchanged.
    if let Some(source) = &request.source {
        lines.push(Line::from(Span::styled(
            format!("requested by sub-agent [{source}]"),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
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
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
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

    let items = [" Copy ", " Cancel "];
    let sel_bg = theme::selection_bg(app.scheme());
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let style = if i == menu.selected {
                Style::default().bg(sel_bg).add_modifier(Modifier::BOLD)
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

/// Rows for the `@` popup: kind marker + name, elided to the band width.
/// `width` is the row-content column count
/// (`PopupStyle::content_width(band_width(…))`), not the outer band.
fn mention_lines(m: &MentionCompleter, width: usize) -> Vec<Line<'static>> {
    m.entries()
        .iter()
        .map(|e| {
            let marker = if e.synthetic {
                "» "
            } else if e.is_dir {
                "📁 "
            } else {
                "📄 "
            };
            let budget = width.saturating_sub(GUTTER_W + UnicodeWidthStr::width(marker));
            let text = elide(&e.name, budget, Elide::Tail { sep: Some('/') });
            Line::from(Span::raw(format!("{marker}{text}")))
        })
        .collect()
}

/// Rows for the `/` popup: name column + dim description, elided to the band
/// width (`Head` for the description: it reads front-to-back).
///
/// The row is deliberately two-tone on the selected line as well: the widget's
/// `highlight` carries only the background + emphasis, so both foregrounds stay
/// the product's call. The description steps `faint` → `lifted` when selected
/// so it survives the highlight's background instead of vanishing into it.
fn slash_lines(s: &SlashCompleter, width: usize, scheme: ColorScheme) -> Vec<Line<'static>> {
    const NAME_W: usize = 24;
    const GAP_W: usize = 2;
    let selected = s.selected_index();
    s.entries()
        .iter()
        .enumerate()
        .map(|(i, (name, desc))| {
            // Fit the fixed columns inside the band. A band narrower than
            // GUTTER_W + NAME_W + GAP_W would otherwise build a row wider than
            // the band and have `Paragraph` hard-clip the tail — reachable now
            // that `ui.popup.width` accepts any column count. Shrinking the
            // name column first keeps the row exactly `width` wide at any size.
            let avail = width.saturating_sub(GUTTER_W);
            let name_w = NAME_W.min(avail);
            let gap_w = GAP_W.min(avail.saturating_sub(name_w));
            let budget = avail.saturating_sub(name_w + gap_w);

            let name = elide(name, name_w, Elide::Tail { sep: None });
            let desc_fg = if i == selected {
                theme::lifted(scheme)
            } else {
                theme::faint(scheme)
            };
            Line::from(vec![
                Span::styled(
                    pad_cols(&name, name_w),
                    Style::default().fg(theme::strong(scheme)),
                ),
                Span::raw(" ".repeat(gap_w)),
                Span::styled(
                    elide(desc, budget, Elide::Head),
                    Style::default().fg(desc_fg),
                ),
            ])
        })
        .collect()
}

/// The `@` mention popup: the product's rows rendered by the popup-list widget.
fn render_mention_popup(f: &mut Frame, app: &App, composer: Rect) {
    let Some(m) = app.mention() else {
        return;
    };
    let style = app.popup_style();
    // Budget the rows against the content width, not the band: a framed band
    // spends 2 columns on its border, and `Paragraph` would hard-clip those.
    let width = style.content_width(band_width(composer, f.area(), &style.width)) as usize;
    PopupList {
        rows: mention_lines(m, width),
        selected: m.selected_index(),
        style,
        title: None,
    }
    .render_at(f, composer);
}

/// The `/` skill popup: the product's rows rendered by the popup-list widget.
fn render_slash_popup(f: &mut Frame, app: &App, composer: Rect) {
    let Some(s) = app.slash() else {
        return;
    };
    let style = app.popup_style();
    // Budget the rows against the content width, not the band: a framed band
    // spends 2 columns on its border, and `Paragraph` would hard-clip those.
    let width = style.content_width(band_width(composer, f.area(), &style.width)) as usize;
    let mut rows = slash_lines(s, width, app.scheme());
    // An empty result is not a choice: show it as plain dim text that is never
    // highlighted. The widget already supplies the 2-column gutter, so the row
    // carries no padding of its own. `usize::MAX` never equals `window.start +
    // n`, so no row can be marked selected.
    let selected = if rows.is_empty() {
        rows.push(Line::from(Span::styled(
            "no matching skills",
            Style::default().fg(theme::faint(app.scheme())),
        )));
        usize::MAX
    } else {
        s.selected_index()
    };
    PopupList {
        rows,
        selected,
        style,
        title: None,
    }
    .render_at(f, composer);
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

/// The live thinking panel: bordered, internally scrolling (last lines of the
/// pending thought), title carries spinner / agent / elapsed / token estimate.
fn render_thinking_panel(
    f: &mut Frame,
    app: &App,
    area: Rect,
    lines: &[String],
    agent: Option<&str>,
    chars: usize,
) {
    let dim = Style::default().fg(theme::faint(app.scheme()));
    let spinner = app.spinner_char();
    // Per-stream timer (`""` = root, matching the agent_id routing key).
    let key = agent.unwrap_or("");
    let elapsed = app
        .thinking_since
        .get(key)
        .map_or(0, |t| t.elapsed().as_secs() + 1);
    let mut title = vec![Span::styled(format!("{spinner} thinking"), dim)];
    if let Some(id) = agent {
        title.push(Span::styled(format!(" - {id}"), dim));
    }
    title.push(Span::styled(
        format!(
            " - {} - ~{} tok",
            App::fmt_elapsed(elapsed),
            fmt_k(chars / 3)
        ),
        dim,
    ));
    f.render_widget(
        phi_tui::tail_panel::TailPanel {
            title: Line::from(title),
            lines,
            line_style: style_for(LineKind::Thought, app.scheme()),
            border_style: dim,
        },
        area,
    );
}

/// `950` → "950"; `12000` → "12.0k". Cosmetic counts in titles/summaries.
fn fmt_k(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

fn style_for(kind: LineKind, scheme: ColorScheme) -> Style {
    // Hue is punctuation, grey is area (design §5). Only three things should
    // jump out of a screen: your input (blue), what went wrong (red), what is
    // running right now (cyan). Tools and results are the densest elements in
    // the transcript — tint those as a block and the pane turns into noise.
    // Grays and absolute tones come from `theme` so the same roles survive a
    // light background; the hues above are the terminal's to paint.
    match kind {
        LineKind::Normal => Style::default(),
        // Process text: dimmer than results, still legible. Shape still
        // separates it from results (summary line / un-indented body vs
        // `  < ` + indented payload).
        LineKind::Thought => Style::default().fg(theme::thought(scheme)),
        LineKind::Plan => Style::default().fg(Color::Cyan),
        LineKind::Tool => Style::default().fg(Color::Cyan),
        LineKind::Done => Style::default().fg(Color::Green),
        // Result text is the tool's answer, not a verdict — grey, so a red or
        // green actually means something.
        LineKind::ToolResult => Style::default().fg(theme::muted(scheme)),
        LineKind::Error => Style::default().fg(Color::Red),
        LineKind::System => Style::default().fg(theme::faint(scheme)),
        LineKind::Cancelled => Style::default().fg(Color::Yellow),
        LineKind::Approval => Style::default().fg(Color::Yellow),
        LineKind::User => Style::default()
            .fg(theme::user(scheme))
            .add_modifier(Modifier::BOLD),
    }
}

/// Split a tool invocation body into `(who, args)`.
///
/// `runtime::tool_invocation_text` joins the tool name (plus any sub-agent
/// prefix) to its readable arguments with **two** spaces, and emits the name
/// alone when there is nothing worth naming. Two spaces is the contract — a
/// tool name never contains a double space, so the split is unambiguous.
fn split_tool_body(body: &str) -> (&str, &str) {
    body.split_once("  ").unwrap_or((body, ""))
}

/// Style for a tool state's leading glyph — the one row element whose meaning
/// is "how is this call doing", so it gets the state colour.
fn tool_state_style(state: ToolState, scheme: ColorScheme) -> Style {
    match state {
        ToolState::Running => Style::default().fg(Color::Cyan),
        ToolState::Done => Style::default().fg(Color::LightGreen),
        ToolState::Failed | ToolState::Denied => Style::default().fg(Color::Red),
        ToolState::Queued => Style::default().fg(theme::faint(scheme)),
    }
}

/// A tool invocation line, composed rather than uniformly tinted.
///
/// The `*` in `text` is a marker slot, not a glyph: it is replaced here by the
/// call's state glyph (the braille frame while running, `o`/`x` once settled),
/// so the row answers "did this finish?" without opening anything. Arguments
/// drop to grey — the tool name is the identity, the values are detail.
fn tool_invocation_line(
    text: &str,
    state: ToolState,
    spinner: &str,
    scheme: ColorScheme,
) -> Line<'static> {
    let body = text.strip_prefix("* ").unwrap_or(text);
    let (who, args) = split_tool_body(body);
    let glyph = match state {
        ToolState::Running => spinner,
        ToolState::Done => "o",
        ToolState::Queued => "o",
        ToolState::Failed | ToolState::Denied => "x",
    };
    let mut spans = vec![
        Span::styled(glyph.to_string(), tool_state_style(state, scheme)),
        Span::styled(format!(" {who}"), style_for(LineKind::Tool, scheme)),
    ];
    if !args.is_empty() {
        spans.push(Span::styled(
            format!("  {args}"),
            Style::default().fg(theme::muted(scheme)),
        ));
    }
    Line::from(spans)
}

/// Row indent for a folded block's payload.
///
/// Four columns, which is exactly where a result row's answer starts (`  < `
/// is the connector) — so the extra rows read as more of the same answer
/// rather than as a quote. Under a call row the same indent still nests
/// cleanly, and one constant keeps the ladder's shape identical everywhere.
const FOLD_INDENT: &str = "    ";

/// The meta row for a tool result: the connector dimmer than the answer.
///
/// `  < ` is the result's attachment marker (the glyph column plus one). It is
/// punctuation, so it sits in the darkest grey; the tool's words are the
/// content and take the result style.
fn result_meta_line(text: &str, kind: LineKind, scheme: ColorScheme) -> Line<'static> {
    let connector = Style::default().fg(theme::faint(scheme));
    if let Some(rest) = text.strip_prefix("  < ") {
        Line::from(vec![
            Span::styled("  < ".to_string(), connector),
            Span::styled(rest.to_string(), style_for(kind, scheme)),
        ])
    } else if text.trim_end() == "  <" {
        Line::from(Span::styled(text.to_string(), connector))
    } else {
        // Denied / error rows keep their own wording and colour.
        Line::from(Span::styled(text.to_string(), style_for(kind, scheme)))
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
