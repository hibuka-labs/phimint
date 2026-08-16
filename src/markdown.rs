//! Streaming markdown rendering for the inline UI.
//!
//! The assistant's `TextDelta` chunks arrive token-by-token, but markdown
//! structure (fenced code blocks, headings, bold, inline code) is line-oriented.
//! So we buffer into complete lines and render each line atomically as soon as
//! its `\n` arrives, keeping the trailing partial line pending. A fence split
//! across two tokens therefore still renders as a single code block.
//!
//! To keep the token-by-token streaming feel (a long single-line paragraph must
//! not appear "stuck"), a partial line that grows past [`PARTIAL_FLUSH`] chars is
//! flushed raw immediately and the rest of that physical line streams raw until
//! its `\n`. Markdown styling is cosmetic on a long prose run, so nothing is lost.

use std::io::Write;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Flush the pending partial line raw once it exceeds this many characters, so a
/// long paragraph streams instead of buffering until its newline.
const PARTIAL_FLUSH: usize = 200;

/// Incremental markdown renderer. Feed chunks; it writes styled lines to a sink.
#[derive(Default)]
pub struct Markdown {
    /// Trailing partial line (no terminating `\n` yet).
    pending: String,
    /// True while inside a fenced code block (``` … ```).
    in_fence: bool,
    /// True when the current line has been flushed raw (long line) and is still
    /// streaming — no `pending` buffer is used in this state.
    raw_open: bool,
}

impl Markdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset between turns, so an unbalanced fence in one answer can't leave the
    /// next answer dimmed (or stuck inside a code block).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.in_fence = false;
        self.raw_open = false;
    }

    /// Whether the cursor sits at column 0 of an empty line (i.e. the last thing
    /// emitted ended with a newline). Used by the renderer to track line state.
    pub fn at_line_start(&self) -> bool {
        self.pending.is_empty() && !self.raw_open
    }

    /// Feed a chunk of streamed text, writing styled output to `out`.
    pub fn feed(&mut self, chunk: &str, out: &mut impl Write) {
        if self.raw_open {
            self.feed_raw(chunk, out);
            return;
        }
        self.pending.push_str(chunk);
        self.process_pending(out);
    }

    /// Flush the trailing partial line (turn end / structural event), rendering it
    /// as a complete markdown line. Returns true when a line was emitted, so the
    /// caller knows the current line is now fresh.
    pub fn flush(&mut self, out: &mut impl Write) -> bool {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.render_line(&line, out);
            true
        } else {
            // In raw mode there is nothing buffered; the open raw line is left for
            // the caller to terminate with `\r\n`.
            false
        }
    }

    /// Emit complete lines from `pending`, leaving the trailing partial line.
    fn process_pending(&mut self, out: &mut impl Write) {
        while let Some(nl) = self.pending.find('\n') {
            let mut line: String = self.pending[..nl].to_string();
            self.pending.drain(..=nl);
            if line.ends_with('\r') {
                line.pop();
            }
            self.render_line(&line, out);
        }
        // A partial line that grows long is flushed raw so it streams.
        if self.pending.chars().count() > PARTIAL_FLUSH {
            let tail = std::mem::take(&mut self.pending);
            self.write_raw(&tail, out);
            self.raw_open = true;
        }
    }

    /// Stream a long line raw, up to the next newline, then resume normal
    /// line-buffered processing for whatever follows.
    fn feed_raw(&mut self, chunk: &str, out: &mut impl Write) {
        match chunk.find('\n') {
            Some(nl) => {
                let (mut line, after) = chunk.split_at(nl);
                if line.ends_with('\r') {
                    line = &line[..line.len() - 1];
                }
                self.write_raw(line, out);
                let _ = write!(out, "\r\n");
                self.raw_open = false;
                // The rest of this chunk resumes normal processing.
                self.pending.push_str(&after[1..]);
                self.process_pending(out);
            }
            None => {
                self.write_raw(chunk, out);
            }
        }
    }

    /// Render one complete line (without its `\n`).
    fn render_line(&mut self, line: &str, out: &mut impl Write) {
        if is_fence(line) {
            self.in_fence = !self.in_fence;
            return;
        }
        if self.in_fence {
            let _ = write!(out, "{DIM}  {line}{RESET}\r\n");
        } else {
            render_prose(line, out);
        }
    }

    /// Write a raw (already-buffered long line) fragment, styled for code blocks.
    fn write_raw(&self, text: &str, out: &mut impl Write) {
        if self.in_fence {
            let _ = write!(out, "{DIM}{text}{RESET}");
        } else {
            let _ = write!(out, "{text}");
        }
    }
}

/// A fenced code block delimiter: a line of 3+ backticks or tildes, optionally
/// followed by an info string (language), and nothing else.
fn is_fence(line: &str) -> bool {
    let s = line.trim_start();
    (s.starts_with("```") || s.starts_with("~~~")) && !s[3..].contains('`')
}

/// Render a prose line: a heading is de-hashed and bolded; otherwise inline
/// bold and inline code are styled.
fn render_prose(line: &str, out: &mut impl Write) {
    if let Some(rest) = heading_rest(line) {
        let _ = write!(out, "{BOLD}{rest}{RESET}\r\n");
    } else {
        let _ = write!(out, "{}\r\n", render_inline(line));
    }
}

/// If `line` is an ATX heading (`# …` … `###### …`), return the text after the
/// `#` marker(s) and the space, trimmed.
fn heading_rest(line: &str) -> Option<&str> {
    let s = line.trim_start();
    let n = s.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&n) && s.chars().nth(n) == Some(' ') {
        Some(s[n + 1..].trim())
    } else {
        None
    }
}

/// Style inline `**bold**` and `` `code` `` spans; other text is passed through.
fn render_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut rest = line;
    loop {
        match next_inline(rest) {
            None => {
                out.push_str(rest);
                break;
            }
            Some((idx, kind)) => {
                out.push_str(&rest[..idx]);
                match kind {
                    Inline::Bold => {
                        if let Some(close) = rest[idx + 2..].find("**") {
                            out.push_str(BOLD);
                            out.push_str(&rest[idx + 2..idx + 2 + close]);
                            out.push_str(RESET);
                            rest = &rest[idx + 2 + close + 2..];
                        } else {
                            out.push_str("**");
                            rest = &rest[idx + 2..];
                        }
                    }
                    Inline::Code => {
                        if let Some(close) = rest[idx + 1..].find('`') {
                            out.push_str(DIM);
                            out.push_str(&rest[idx + 1..idx + 1 + close]);
                            out.push_str(RESET);
                            rest = &rest[idx + 1 + close + 1..];
                        } else {
                            out.push('`');
                            rest = &rest[idx + 1..];
                        }
                    }
                }
            }
        }
    }
    out
}

enum Inline {
    Bold,
    Code,
}

/// The earliest inline marker (`**` or backtick) in `s`, if any.
fn next_inline(s: &str) -> Option<(usize, Inline)> {
    let bold = s.find("**");
    let code = s.find('`');
    match (bold, code) {
        (Some(b), Some(c)) => Some(if b < c { (b, Inline::Bold) } else { (c, Inline::Code) }),
        (Some(b), None) => Some((b, Inline::Bold)),
        (None, Some(c)) => Some((c, Inline::Code)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(chunks: &[&str]) -> String {
        let mut md = Markdown::new();
        let mut out = Vec::new();
        for c in chunks {
            md.feed(c, &mut out);
        }
        md.flush(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn code_fence_renders_lines_dimmed_and_indented() {
        let out = render(&["```rust\nlet x = 1;\nprintln!(\"hi\");\n```\n"]);
        assert!(out.contains("\x1b[2m  let x = 1;\x1b[0m\r\n"), "got: {out:?}");
        assert!(out.contains("\x1b[2m  println!(\"hi\");\x1b[0m\r\n"), "got: {out:?}");
        assert!(!out.contains("```"), "fence markers must be stripped, got: {out:?}");
    }

    #[test]
    fn fence_split_across_tokens_still_one_block() {
        // "```" and "rust\n" arrive separately; must still open one code block.
        let out = render(&["```", "rust\nlet x = 1;\n```\n"]);
        assert!(out.contains("\x1b[2m  let x = 1;\x1b[0m\r\n"), "got: {out:?}");
        assert!(!out.contains("```"), "got: {out:?}");
    }

    #[test]
    fn bold_and_inline_code_are_styled() {
        let out = render(&["use **bold** and `code` here\n"]);
        assert!(out.contains("\x1b[1mbold\x1b[0m"), "got: {out:?}");
        assert!(out.contains("\x1b[2mcode\x1b[0m"), "got: {out:?}");
    }

    #[test]
    fn heading_is_dehashed_and_bolded() {
        let out = render(&["## Section Title\n"]);
        assert!(out.contains("\x1b[1mSection Title\x1b[0m\r\n"), "got: {out:?}");
        assert!(!out.contains("##"), "got: {out:?}");
    }

    #[test]
    fn plain_prose_passes_through() {
        let out = render(&["hello world\n"]);
        assert!(out.contains("hello world\r\n"), "got: {out:?}");
    }

    #[test]
    fn partial_line_is_buffered_until_newline() {
        let mut md = Markdown::new();
        let mut out = Vec::new();
        md.feed("hel", &mut out);
        assert!(out.is_empty(), "no newline yet, should buffer: {out:?}");
        md.feed("lo\n", &mut out);
        assert!(String::from_utf8(out).unwrap().contains("hello\r\n"));
    }

    #[test]
    fn long_line_flushes_raw_to_stream() {
        let mut md = Markdown::new();
        let mut out = Vec::new();
        let long = "x".repeat(PARTIAL_FLUSH + 1);
        md.feed(&long, &mut out);
        // Past the threshold the partial line is flushed raw (streams), not held.
        assert!(out.len() >= PARTIAL_FLUSH, "long line should stream raw");
        assert!(md.raw_open, "raw mode should be engaged");
        // The remainder of the line streams raw, then a newline ends it.
        md.feed("tail\n", &mut out);
        assert!(!md.raw_open);
        assert!(md.at_line_start());
    }

    #[test]
    fn reset_clears_unclosed_fence() {
        let mut md = Markdown::new();
        let mut out = Vec::new();
        md.feed("```rust\nlet x = 1;\n", &mut out); // unclosed fence
        md.reset();
        md.feed("next answer\n", &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("\x1b[2mnext answer"), "got: {s:?}");
    }

    #[test]
    fn no_bare_newlines_anywhere() {
        let out = render(&[
            "# h\n",
            "text with **bold**\n",
            "```js\nconst a = 1;\n```\n",
            "tail",
        ]);
        let bytes = out.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                assert!(i > 0 && bytes[i - 1] == b'\r', "bare \\n at byte {i}: {out:?}");
            }
        }
    }
}
