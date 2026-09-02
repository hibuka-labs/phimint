//! Keyboard event handler for the TUI application.
//!
//! Handles keyboard input events, including:
//! - Global keys (Ctrl+C, Ctrl+D, etc.)
//! - Context-specific keys (approval, menu, etc.)
//! - Composer keys (typing, navigation, etc.)

use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};

use phi_agent::ApprovalDecision;
use crate::ui::app::{Action, App, AgentStatus, FocusTarget, Phase};

impl App {
    /// Handle a keyboard event and return an action if one was triggered.
    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        use KeyCode::*;

        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let shift = modifiers.contains(KeyModifiers::SHIFT);
        let super_key = modifiers.contains(KeyModifiers::SUPER);

        // Any key dismisses a transient notice (copy feedback) before handling.
        // Non-Ctrl+C keys also reset the quit-hint double-press window.
        self.notice = None;
        if !(ctrl && code == Char('c')) {
            self.quit_hint_at = None;
        }

        // Cmd+C (Super) is the macOS copy shortcut: copy the selection, and do
        // nothing when there's no selection — it never cancels/quits (that's
        // Ctrl+C's job).
        if super_key && code == Char('c') {
            if self.selection_state.selection.is_some() {
                self.selection_state.context_menu = None;
                return Some(Action::CopySelection);
            }
            return None;
        }

        // Ctrl+C: copy selection → cancel running → double-press to quit.
        //   1. If there's an active selection, copy it (caller clears selection).
        //   2. If the agent is running (or approval pending), cancel it.
        //   3. Otherwise, show a hint; a second Ctrl+C within timeout quits.
        if ctrl && code == Char('c') {
            if self.selection_state.selection.is_some() {
                tracing::info!("ctrl+c: has selection → copy");
                self.selection_state.context_menu = None;
                self.quit_hint_at = None;
                return Some(Action::CopySelection);
            }
            if self.running || !self.approval_queue.is_empty() {
                tracing::info!("ctrl+c: running/approval → cancel");
                self.quit_hint_at = None;
                return Some(Action::Cancel);
            }
            // Double-press to quit: first press shows hint, second press
            // within timeout actually quits.
            const QUIT_HINT_TIMEOUT: std::time::Duration =
                std::time::Duration::from_secs(2);
            if let Some(at) = self.quit_hint_at {
                if at.elapsed() < QUIT_HINT_TIMEOUT {
                    tracing::info!("ctrl+c: hint active → quit");
                    self.quit_hint_at = None;
                    return Some(Action::Quit);
                }
            }
            tracing::info!("ctrl+c: idle → show quit hint");
            self.quit_hint_at = Some(Instant::now());
            self.set_notice("Press Ctrl+C again to exit");
            return None;
        }

        // Right-click copy menu: Up/Down move the highlight, Enter copies (or
        // cancels), Esc closes; everything else is swallowed.
        if self.selection_state.menu_is_open() {
            let selected = self.selection_state.menu_selected();
            return match code {
                Up => {
                    self.selection_state.menu_move(-1);
                    None
                }
                Down => {
                    self.selection_state.menu_move(1);
                    None
                }
                Enter => {
                    self.selection_state.context_menu = None;
                    if selected == 0 {
                        Some(Action::CopySelection)
                    } else {
                        None
                    }
                }
                Esc => {
                    self.selection_state.context_menu = None;
                    None
                }
                _ => None,
            };
        }

        // While an approval popup is showing, route y/a/n and swallow the rest.
        if !self.approval_queue.is_empty() {
            return match code {
                Char('y') => Some(Action::Approve(ApprovalDecision::AllowOnce)),
                Char('a') => Some(Action::Approve(ApprovalDecision::AllowAlways)),
                Char('n') => Some(Action::Approve(ApprovalDecision::Deny)),
                _ => None,
            };
        }

        // While the @ mention picker is open, every key drives the picker — the
        // composer only receives the final path on Enter/Esc.
        if self.mention.is_some() {
            return self.handle_mention_key(code, modifiers);
        }

        // While the / skill picker is open, every key drives the picker.
        if self.slash.is_some() {
            return self.handle_slash_key(code, modifiers);
        }

        match code {
            Char('d') if ctrl => Some(Action::Quit),
            // Ctrl+Y copies the last reply (vim-yank convention); plain 'y'
            // still falls through to `Char(c)` and types a literal 'y'.
            Char('y') if ctrl => Some(Action::CopyLastReply),
            Esc => {
                // No menu is open here (handled above); clear a selection if
                // present, else clear the composer.
                if self.selection_state.selection.is_some() {
                    self.selection_state.selection = None;
                } else {
                    self.composer.clear();
                }
                None
            }
            Enter => {
                if shift {
                    self.composer.insert_char('\n');
                    None
                } else if !self.running && !self.composer.is_empty() {
                    let text = self.composer.text();
                    self.composer.clear();
                    // Echo the user's message into the transcript before the
                    // agent's reply, so the record keeps the human turn too.
                    self.push_user(&text);
                    self.transcript.clear_plan();
                    self.running = true;
                    self.viewport.follow_bottom = true;
                    self.viewport.scroll_offset = 0;
                    self.status = AgentStatus::Running {
                        phase: Phase::Thinking,
                    };
                    Some(Action::Submit(text))
                } else {
                    None
                }
            }
            Backspace => {
                self.composer.backspace();
                None
            }
            Delete => {
                self.composer.delete_forward();
                None
            }
            Left => {
                self.composer.move_left();
                None
            }
            Right => {
                self.composer.move_right();
                None
            }
            Up => {
                // Handle task panel navigation
                if self.should_show_task_panel() {
                    match &self.task_panel.focus {
                        FocusTarget::TaskList(index) => {
                            if *index > 0 {
                                self.task_panel.focus = FocusTarget::TaskList(index - 1);
                            }
                            return None;
                        }
                        FocusTarget::Input => {
                            // Move focus to task list (select last item)
                            if !self.sub_agents.is_empty() {
                                self.task_panel.focus = FocusTarget::TaskList(self.sub_agents.len() - 1);
                                return None;
                            }
                        }
                    }
                }
                self.composer.move_up();
                None
            }
            Down => {
                // Handle task panel navigation
                if self.should_show_task_panel() {
                    match &self.task_panel.focus {
                        FocusTarget::TaskList(index) => {
                            if *index + 1 < self.sub_agents.len() {
                                self.task_panel.focus = FocusTarget::TaskList(index + 1);
                            } else {
                                // Move focus to input
                                self.task_panel.focus = FocusTarget::Input;
                            }
                            return None;
                        }
                        FocusTarget::Input => {
                            // Already at input, do nothing
                            return None;
                        }
                    }
                }
                self.composer.move_down();
                None
            }
            Home => {
                self.composer.move_home();
                None
            }
            End => {
                self.composer.move_end();
                None
            }
            PageUp => {
                self.scroll_up();
                None
            }
            PageDown => {
                self.scroll_down();
                None
            }
            Char('/') if self.composer.is_empty() && !self.skill_summaries.is_empty() => {
                self.composer.insert_char('/');
                self.start_slash();
                None
            }
            Char('@') => {
                self.composer.insert_char('@');
                self.start_mention();
                None
            }
            Char(c) => {
                self.composer.insert_char(c);
                None
            }
            _ => None,
        }
    }
}
