//! Put the terminal back when phimint dies without its normal exit path.
//!
//! The TUI takes the terminal over: raw mode, the alternate screen, mouse
//! reporting, bracketed paste. `TerminalGuard` restores all of it when the UI
//! returns normally, and the panic hook restores it when the main thread
//! panics — but neither runs when the process is killed by a signal (SIGTERM
//! from a supervisor, SIGHUP when the terminal closes) or dies some other
//! abrupt way. The terminal is then left raw with the alternate screen up
//! and mouse reporting on: the shell stops echoing, the wheel stops
//! scrolling, and every mouse event piles up as text in the buffer.
//!
//! Two pieces close that gap:
//!
//! - [`install_signal_restore`] — a SIGTERM/SIGHUP/SIGINT handler that writes
//!   the restore sequences and the saved line discipline back to the tty,
//!   then exits.
//! - [`heal_terminal`] — repairs a terminal that an earlier run left broken,
//!   so the next start is not a shell that cannot echo and cannot scroll.

use std::sync::OnceLock;

/// The escape sequences that undo the TUI's takeover, in the order the
/// terminal wants them: mouse reporting, bracketed paste, the keyboard
/// enhancement flags, then the alternate screen.
pub const RESTORE_SEQUENCE: &[u8] =
    b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[<0u\x1b[?1049l";

/// The terminal phimint talks to.
const TTY_FD: libc::c_int = 1;

/// The line discipline as it was before the TUI touched it.
static SAVED_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// True when the line discipline is not canonical — the mark of a terminal
/// left in raw mode by a run that never restored it.
fn looks_raw(lflag: libc::tcflag_t) -> bool {
    lflag & libc::ICANON as libc::tcflag_t == 0
}

/// The flags a shell expects: canonical mode, echo, and the usual
/// input/output processing.
fn apply_sane(t: &mut libc::termios) {
    t.c_lflag |= libc::ICANON as libc::tcflag_t
        | libc::ECHO as libc::tcflag_t
        | libc::ECHOE as libc::tcflag_t
        | libc::ECHOK as libc::tcflag_t
        | libc::ISIG as libc::tcflag_t
        | libc::IEXTEN as libc::tcflag_t;
    t.c_lflag &= !(libc::NOFLSH as libc::tcflag_t | libc::TOSTOP as libc::tcflag_t);
    t.c_iflag |= libc::ICRNL as libc::tcflag_t | libc::IXON as libc::tcflag_t;
    t.c_oflag |= libc::OPOST as libc::tcflag_t | libc::ONLCR as libc::tcflag_t;
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;
}

/// Repair a terminal an earlier run left broken.
///
/// Only acts when the line discipline actually looks raw — a terminal that is
/// already fine is left exactly as it is. A no-op when stdout is not a
/// terminal.
pub fn heal_terminal() {
    // SAFETY: every libc call below is a plain termios/write call on the
    // terminal file descriptor; nothing here allocates or unwinds.
    unsafe {
        if libc::isatty(TTY_FD) != 1 {
            return;
        }
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(TTY_FD, &mut t) != 0 {
            return;
        }
        if !looks_raw(t.c_lflag) {
            return;
        }
        apply_sane(&mut t);
        libc::tcsetattr(TTY_FD, libc::TCSANOW, &t);
        // And drop whatever the dead run left switched on.
        libc::write(
            TTY_FD,
            RESTORE_SEQUENCE.as_ptr() as *const libc::c_void,
            RESTORE_SEQUENCE.len(),
        );
    }
}

/// Restore the terminal and exit, from a signal handler.
///
/// Async-signal-safe: `write(2)`, `tcsetattr(2)` and `_exit(2)` only — no
/// allocation, no locks beyond one atomic load, no unwinding.
extern "C" fn handle_signal(signo: libc::c_int) {
    // SAFETY: signal-handler context — only the calls listed above.
    unsafe {
        libc::write(
            TTY_FD,
            RESTORE_SEQUENCE.as_ptr() as *const libc::c_void,
            RESTORE_SEQUENCE.len(),
        );
        if let Some(saved) = SAVED_TERMIOS.get() {
            let t = *saved;
            libc::tcsetattr(TTY_FD, libc::TCSANOW, &t);
        }
        libc::_exit(128 + signo);
    }
}

/// Put the terminal back when the process is asked to stop.
///
/// Call it before the TUI takes the terminal over: the line discipline it
/// saves is the one the handler restores. A no-op when stdout is not a
/// terminal — there is nothing to restore, and writing escape sequences into
/// a redirected file would corrupt it.
pub fn install_signal_restore() {
    // SAFETY: `signal(2)` with a plain `extern "C"` handler, and one
    // `tcgetattr(2)` to snapshot the line discipline.
    unsafe {
        if libc::isatty(TTY_FD) != 1 {
            return;
        }
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(TTY_FD, &mut t) == 0 {
            let _ = SAVED_TERMIOS.set(t);
        }
        libc::signal(libc::SIGTERM, handler_arg());
        libc::signal(libc::SIGHUP, handler_arg());
        libc::signal(libc::SIGINT, handler_arg());
    }
}

/// The handler as `signal(2)` wants it.
fn handler_arg() -> libc::sighandler_t {
    handle_signal as *const () as usize as libc::sighandler_t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_sequence_turns_off_what_the_tui_turns_on() {
        for seq in [
            "\x1b[?1000l",
            "\x1b[?1002l",
            "\x1b[?1003l",
            "\x1b[?1006l",
            "\x1b[?2004l",
            "\x1b[?1049l",
        ] {
            assert!(
                RESTORE_SEQUENCE
                    .windows(seq.len())
                    .any(|w| w == seq.as_bytes()),
                "the restore sequence must contain {seq:?}"
            );
        }
    }

    #[test]
    fn the_alternate_screen_is_left_last() {
        // The other sequences are meaningless once the alternate screen is
        // gone, so it must be the final step.
        assert!(RESTORE_SEQUENCE.ends_with(b"\x1b[?1049l"));
    }

    #[test]
    fn a_canonical_line_discipline_is_not_raw() {
        let sane = libc::ICANON as libc::tcflag_t | libc::ECHO as libc::tcflag_t;
        assert!(!looks_raw(sane));
        assert!(looks_raw(libc::ECHO as libc::tcflag_t));
    }

    #[test]
    fn healing_a_terminal_that_is_fine_leaves_it_alone() {
        // Under `cargo test` stdout is usually not a tty: the heal must be a
        // no-op there, and must never panic when it is one.
        heal_terminal();
    }
}
