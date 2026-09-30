//! Keyboard event handler for the TUI application.
//!
//! Handles keyboard input events, including:
//! - Global keys (Ctrl+C, Ctrl+D, etc.)
//! - Context-specific keys (approval, menu, etc.)
//! - Composer keys (typing, navigation, etc.)

use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};

use crate::ui::app::{Action, AgentStatus, App, FocusTarget, Phase};
use phi_agent::ApprovalDecision;
use phi_kernel_tools::background_shell::BackgroundTaskStatus;

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
        //   2. If the agent is running (or approval pending), cancel it; a
        //      second press within the timeout force-quits (the cancel path
        //      can stall deep in the runtime — the escape hatch must not).
        //   3. If there are running background tasks, cancel them.
        //   4. Otherwise, show a hint; a second Ctrl+C within timeout quits.
        if ctrl && code == Char('c') {
            const DOUBLE_PRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
            if self.selection_state.selection.is_some() {
                tracing::info!("ctrl+c: has selection -> copy");
                self.selection_state.context_menu = None;
                self.quit_hint_at = None;
                return Some(Action::CopySelection);
            }
            if self.running || !self.approval_queue.is_empty() {
                // A cancel was already requested recently and the agent is
                // still stuck: force-quit instead of sending a no-op cancel.
                if let Some(at) = self.quit_hint_at {
                    if at.elapsed() < DOUBLE_PRESS_TIMEOUT {
                        tracing::info!("ctrl+c: stuck after cancel -> force quit");
                        self.quit_hint_at = None;
                        return Some(Action::Quit);
                    }
                }
                tracing::info!("ctrl+c: running/approval -> cancel");
                self.quit_hint_at = Some(Instant::now());
                self.set_notice("Cancelling... press Ctrl+C again to force quit");
                return Some(Action::Cancel);
            }
            // Cancel running background tasks
            let running_bg_tasks: Vec<String> = self
                .background_tasks
                .iter()
                .filter(|(_, t)| t.status == BackgroundTaskStatus::Running)
                .map(|(id, _)| id.clone())
                .collect();
            if !running_bg_tasks.is_empty() {
                tracing::info!(task_ids = ?running_bg_tasks, "ctrl+c: cancel background tasks");
                for task_id in &running_bg_tasks {
                    if let Some(registry) = &self.background_registry {
                        registry.cancel(task_id);
                    }
                }
                self.set_notice("Background tasks cancelled");
                return None;
            }
            // Double-press to quit: first press shows hint, second press
            // within timeout actually quits.
            if let Some(at) = self.quit_hint_at {
                if at.elapsed() < DOUBLE_PRESS_TIMEOUT {
                    tracing::info!("ctrl+c: hint active -> quit");
                    self.quit_hint_at = None;
                    return Some(Action::Quit);
                }
            }
            tracing::info!("ctrl+c: idle -> show quit hint");
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
            // Ctrl+O toggles committed thought blocks between the collapsed
            // one-line summary and the full dim text (global, render-time
            // decision). Key events already mark the loop dirty.
            Char('o') if ctrl => {
                self.show_thoughts = !self.show_thoughts;
                None
            }
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
                    // `running` is set at the Cmd::Run send site in run.rs —
                    // a submit intercepted before the send (/resume, /upgrade)
                    // must not leave the composer locked.
                    // Follow the conversation: the user just committed to a
                    // turn, so the reply must stream into view. If they were
                    // scrolled up reading history, this is the moment their
                    // reading position ends.
                    self.scroll_to_bottom();
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
                            // Move focus to task list (select last item). The
                            // panel lists sub-agents only.
                            let total = self.sub_agents.len();
                            if total > 0 {
                                self.task_panel.focus = FocusTarget::TaskList(total - 1);
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
                            // The panel lists sub-agents only.
                            let total = self.sub_agents.len();
                            if *index + 1 < total {
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
