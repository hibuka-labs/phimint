//! Terminal integrity guard (self-healing after external termios changes).
//!
//! Failure shape (BUG 7): npm/node and friends in MCP child processes flip
//! termios back to cooked (`icanon`+`echo`). Two consequences, both invisible
//! to the app's own state:
//!
//! 1. Key presses go into the kernel's line buffer and crossterm receives no
//!    events at all — "keyboard dead";
//! 2. Noise drawn to the screen by child processes is never covered by
//!    ratatui's differential repaint — "garbled screen + stuck UI".
//!
//! This module only detects and repairs: periodically confirm the tty is
//! still raw, push it back to raw on drift, and let the caller force a full
//! screen repaint (which also covers the clobbered cells). Mechanism layer,
//! no copy or keybindings (phi-tui-cutting-principle).

use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

/// Check throttle: tcgetattr is one syscall; 1 Hz is fast enough to be
/// imperceptible to the user.
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// lflag criterion: any of `icanon`/`echo`/`isig` on means not raw.
/// Pure function, so the criterion itself can be pinned in unit tests.
pub(crate) fn lflags_look_raw(c_lflag: libc::tcflag_t) -> bool {
    c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG) == 0
}

/// Is the tty on the stdin side still raw? Non-tty / unreadable → `true`
/// (nothing to guard, don't fuss).
pub fn raw_mode_intact() -> bool {
    let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, term.as_mut_ptr()) } != 0 {
        return true;
    }
    let term = unsafe { term.assume_init() };
    lflags_look_raw(term.c_lflag)
}

/// Push raw back on. crossterm's `enable_raw_mode` is a no-op when it
/// believes raw is already on (it only looks at its own flag, not at an
/// external tcsetattr), so we must disable first and then enable to actually
/// write termios back.
pub fn reassert_raw_mode() {
    let _ = disable_raw_mode();
    let _ = enable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_when_all_off() {
        assert!(lflags_look_raw(0));
    }

    #[test]
    fn cooked_when_icanon_echo_or_isig() {
        assert!(!lflags_look_raw(libc::ICANON));
        assert!(!lflags_look_raw(libc::ECHO));
        assert!(!lflags_look_raw(libc::ISIG));
        assert!(!lflags_look_raw(libc::ICANON | libc::ECHO | libc::ISIG));
    }

    #[test]
    fn unrelated_lflags_do_not_count_as_cooked() {
        // Bits like IEXTEN / ECHONL are irrelevant to the criterion and must
        // not misclassify raw as cooked.
        assert!(lflags_look_raw(libc::IEXTEN | libc::ECHONL));
    }

    #[test]
    fn stdin_probe_never_panics() {
        // CI has no tty → tcgetattr fails → intact; with a tty, read the real
        // lflag.
        let _ = raw_mode_intact();
    }
}
