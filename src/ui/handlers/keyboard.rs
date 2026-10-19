//! Keyboard event handler for the TUI application.
//!
//! Handles keyboard input events, including:
//! - Global keys (Ctrl+C, Ctrl+D, etc.)
//! - Context-specific keys (approval, menu, etc.)
//! - Composer keys (typing, navigation, etc.)

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

use crate::ui::app::{Action, App, FocusTarget};
use phi_agent::ApprovalDecision;
use phi_kernel_tools::background_shell::BackgroundTaskStatus;

impl App {
    /// Handle a keyboard event and return an action if one was triggered.
    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        self.handle_key_event(code, modifiers, KeyEventKind::Press)
    }

    /// Full keyboard entry point: `kind` separates a real press from auto-Repeat,
    /// which must never decide an approval prompt (holding `a` must not grant
    /// standing approval to a whole queue).
    pub fn handle_key_event(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        kind: KeyEventKind,
    ) -> Option<Action> {
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
                if let Some(at) = self.quit_hint_at
                    && at.elapsed() < DOUBLE_PRESS_TIMEOUT
                {
                    tracing::info!("ctrl+c: stuck after cancel -> force quit");
                    self.quit_hint_at = None;
                    return Some(Action::Quit);
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
            if let Some(at) = self.quit_hint_at
                && at.elapsed() < DOUBLE_PRESS_TIMEOUT
            {
                tracing::info!("ctrl+c: hint active -> quit");
                self.quit_hint_at = None;
                return Some(Action::Quit);
            }
            tracing::info!("ctrl+c: idle -> show quit hint");
            self.quit_hint_at = Some(Instant::now());
            self.set_notice("Press Ctrl+C again to exit");
            return None;
        }

        // Shift+Tab: cycle the approval mode (auto ⇄ ask). Global — works even
        // while an approval popup is up (entering auto resolves pending
        // prompts; see App::cycle_approval_mode). `BackTab` is what crossterm
        // reports for Shift+Tab.
        if code == BackTab {
            let mode = self.cycle_approval_mode();
            tracing::info!(mode = ?mode, "shift+tab: approval mode cycled");
            return None;
        }

        // Ctrl+L: force a full screen repaint. Global — it is the user's
        // escape hatch for terminal-grid desync (clipped cells after a size
        // pulse, stale composite rows), so it must fire even while a popup
        // would otherwise swallow keys. Plain 'l' still types a literal 'l'.
        if ctrl && code == Char('l') {
            return Some(Action::Redraw);
        }

        // Ctrl+B: reopen the design canvas (1C row 16). Global for the same
        // reason as Ctrl+L — the crash face advertises "ctrl+b 重开", and an
        // approval popup (a canvas consent prompt is exactly the one that is
        // up when the server dies) must not swallow the recovery key it
        // advertises. A chord can never be an approval decision.
        if ctrl && code == Char('b') {
            return Some(Action::CanvasReopen);
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

        // While an approval popup is showing, letters only SELECT a choice and
        // bare Enter COMMITS it (Claude Code's modal pattern).
        // BUG 4 (final form): timing cannot tell a lone decision key from the
        // first letter of a typing burst ("anyday" — its `a` once granted
        // AllowAlways). So a decision needs a commit step typing never produces:
        //  - `y`/`a`/`n` (Press, no chord) only set `approval_selection`;
        //  - bare Enter submits the selection; with nothing selected it is a
        //    no-op;
        //  - everything else is swallowed — including Esc (an IME cancel sends
        //    Esc), chords (Ctrl+A must never mean AllowAlways) and auto-Repeat
        //    (holding `a` must not grant standing approval to a whole queue).
        if !self.approval_queue.is_empty() {
            let bare = !modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
            // Canvas-consent prompts (1C) offer no standing consent: `a` is not
            // in their legend, so it aliases to the one-shot grant rather than
            // AllowAlways (which nothing would honor as standing anyway).
            let consent = self
                .approval_queue
                .front()
                .is_some_and(|item| crate::ui::canvas_host::is_canvas_consent(&item.request));
            return match code {
                Char(c) if kind == KeyEventKind::Press && bare => {
                    let choice = match c.to_ascii_lowercase() {
                        'y' => Some(ApprovalDecision::AllowOnce),
                        'a' if consent => Some(ApprovalDecision::AllowOnce),
                        'a' => Some(ApprovalDecision::AllowAlways),
                        'n' => Some(ApprovalDecision::Deny),
                        _ => None,
                    };
                    if choice.is_some() {
                        self.approval_selection = choice;
                    }
                    None
                }
                Enter if kind == KeyEventKind::Press && bare => {
                    self.approval_selection.clone().map(Action::Approve)
                }
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
            // Ctrl+E cycles the tool-block preview ladder (compact → default →
            // expanded). Separate from Ctrl+O so "show me the thinking" keeps
            // its own key and its own meaning; this one is about how much of a
            // tool's payload is on screen. Also render-time only.
            Char('e') if ctrl => {
                self.result_tier = self.result_tier.next();
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
                    // Deliberately no turn state here. The echoed user message,
                    // the activity clock, the plan reset and the Thinking phase
                    // are all committed by `App::begin_user_turn` at the
                    // `Cmd::Run` send site in run.rs. `running` was already
                    // send-site-only; the rest must follow it, or a submit
                    // intercepted before the send (/resume, /upgrade) leaves
                    // the transcript echoing a message that was never sent and
                    // the status bar on a phantom "thinking" stretch (session
                    // 20261013_075bb995: a cancelled picker left total_turns
                    // at 0 while the spinner ran for minutes).
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
