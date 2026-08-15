//! Replay an `inline.raw` capture into a plain-text screen dump.
//!
//! The inline UI writes raw ANSI (cursor moves, clears, streaming text) to both
//! the terminal and `<session>/inline.raw`. This feeds that byte stream through
//! a small terminal emulator and prints the resulting screen as text, so a
//! session can be reviewed offline — the inline equivalent of reading the TUI's
//! `frames.txt`.
//!
//! Usage: `cargo run --bin replay -- <path-to-inline.raw> [--height N]`

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: replay <path-to-inline.raw> [--height N]");
        return ExitCode::FAILURE;
    };

    let mut height = 50usize;
    if let Some(i) = args.iter().position(|a| a == "--height") {
        if let Some(h) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
            height = h.max(1);
        }
    }

    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut term = Term::new(height);
    term.feed(&bytes);
    print!("{}", term.render());
    ExitCode::SUCCESS
}

/// A minimal terminal emulator — just enough to replay the sequences the inline
/// renderer emits (`\r`, `\n`, `\x1b[A`, `\x1b[C`, `\x1b[K`, `\x1b[J`, `\x1b[…m`).
struct Term {
    height: usize,
    /// Lines scrolled off the top (oldest first).
    scrollback: Vec<String>,
    /// The visible rows (always `height` of them).
    rows: VecDeque<Vec<Option<char>>>,
    row: usize,
    col: usize,
}

impl Term {
    fn new(height: usize) -> Self {
        let mut rows = VecDeque::with_capacity(height);
        for _ in 0..height {
            rows.push_back(Vec::new());
        }
        Self {
            height,
            scrollback: Vec::new(),
            rows,
            row: 0,
            col: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                0x1b => {
                    i += 1;
                    if i < bytes.len() && bytes[i] == b'[' {
                        i += 1;
                        let mut params: Vec<usize> = Vec::new();
                        let mut num = String::new();
                        let mut fin = 0u8;
                        while i < bytes.len() {
                            let c = bytes[i];
                            if c.is_ascii_digit() {
                                num.push(c as char);
                                i += 1;
                            } else if c == b';' {
                                params.push(parse(&num));
                                num.clear();
                                i += 1;
                            } else if matches!(c, b'?' | b'>' | b'!' | b'=') {
                                // Private/extension marker (e.g. `?2004h`).
                                i += 1;
                            } else {
                                fin = c;
                                i += 1;
                                break;
                            }
                        }
                        params.push(parse(&num));
                        self.csi(fin, &params);
                    }
                    // else: lone ESC or single-byte escape — skip it.
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                b'\n' => {
                    self.newline();
                    i += 1;
                }
                _ => {
                    let (ch, len) = decode(&bytes[i..]);
                    self.put(ch);
                    i += len;
                }
            }
        }
    }

    fn put(&mut self, ch: char) {
        let row = &mut self.rows[self.row];
        while row.len() <= self.col {
            row.push(None);
        }
        row[self.col] = Some(ch);
        self.col += 1;
    }

    fn newline(&mut self) {
        if self.row + 1 >= self.height {
            if let Some(off) = self.rows.pop_front() {
                self.scrollback.push(render_row(&off));
            }
            self.rows.push_back(Vec::new());
        } else {
            self.row += 1;
        }
    }

    fn csi(&mut self, fin: u8, params: &[usize]) {
        let n = params.first().copied().unwrap_or(1);
        match fin {
            b'A' => self.row = self.row.saturating_sub(n),
            b'C' => self.col += n,
            b'K' => {
                let row = &mut self.rows[self.row];
                if self.col < row.len() {
                    row.truncate(self.col);
                }
            }
            b'J' => {
                let row = &mut self.rows[self.row];
                if self.col < row.len() {
                    row.truncate(self.col);
                }
                for r in self.row + 1..self.height {
                    self.rows[r].clear();
                }
            }
            b'm' => {} // styles ignored for layout
            _ => {}
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for (n, line) in self.scrollback.iter().enumerate() {
            out.push_str(&format!("{:>4}│{}\n", n, line));
        }
        for (i, row) in self.rows.iter().enumerate() {
            let n = self.scrollback.len() + i;
            out.push_str(&format!("{:>4}│{}\n", n, render_row(row)));
        }
        out
    }
}

fn parse(s: &str) -> usize {
    if s.is_empty() {
        1
    } else {
        s.parse().unwrap_or(0)
    }
}

fn render_row(row: &[Option<char>]) -> String {
    let mut s: String = row.iter().map(|c| c.unwrap_or(' ')).collect();
    while s.ends_with(' ') {
        s.pop();
    }
    s
}

/// Decode one UTF-8 char from the front of `bytes` (the capture is valid UTF-8).
fn decode(bytes: &[u8]) -> (char, usize) {
    let b = bytes[0];
    let len = if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    };
    let s = std::str::from_utf8(&bytes[..len]).unwrap_or("?");
    (s.chars().next().unwrap_or('?'), len)
}
