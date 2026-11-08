//! JSON5 lexical scanning for the config surgery (see
//! [`crate::model::persist`]).
//!
//! The four primitives the splice code shares: skip whitespace and comments,
//! find the end of a quoted string, find the end of a value, find the end of
//! a balanced `{...}`/`[...]`. Nothing here parses or interprets comments —
//! they are opaque bytes to step over, never to rewrite.

/// Skip whitespace and JSON5 comments from `i`; the returned index may be
/// `bytes.len()`.
pub(super) fn skip_ws_comments(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        return i;
    }
}

/// End index (exclusive) of the quoted string starting at `i`, quotes
/// included. `None` if the literal never closes.
pub(super) fn scan_string(b: &[u8], i: usize) -> Option<usize> {
    let quote = *b.get(i)?;
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            c if c == quote => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// End index (exclusive) of the value starting at `i`: a quoted string, a
/// balanced `{...}`/`[...]`, or a bare scalar up to the next delimiter.
pub(super) fn scan_value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' | b'\'' => scan_string(b, i),
        b'{' | b'[' => scan_balanced(b, i),
        _ => {
            // A bare scalar (number, `true`/`false`/`null`, identifier) runs
            // to the first delimiter: a structural one (`,` `}` `]`), any
            // whitespace (a scalar cannot contain one), or a comment start
            // (`//` or `/*`). A comment after the value must never be
            // swallowed into the value span — the comment may itself carry
            // `}` or `,`, and a splice through it would corrupt the file.
            let mut j = i;
            while j < b.len() {
                let c = b[j];
                if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                    break;
                }
                if c == b'/' && j + 1 < b.len() && matches!(b[j + 1], b'/' | b'*') {
                    break;
                }
                j += 1;
            }
            Some(j)
        }
    }
}

/// End index (exclusive) of a balanced `{...}`/`[...]` starting at `i`,
/// strings and comments skipped while counting.
fn scan_balanced(b: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'/' if j + 1 < b.len() && b[j + 1] == b'/' => {
                while j < b.len() && b[j] != b'\n' {
                    j += 1;
                }
            }
            b'/' if j + 1 < b.len() && b[j + 1] == b'*' => {
                j += 2;
                while j + 1 < b.len() && !(b[j] == b'*' && b[j + 1] == b'/') {
                    j += 1;
                }
                j = (j + 2).min(b.len());
            }
            b'"' | b'\'' => j = scan_string(b, j)?,
            b'{' | b'[' => {
                depth += 1;
                j += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                j += 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// protects: the value span of a bare scalar ends before a comment, so a
    /// comment carrying `}` or `,` is never swallowed into the value — a
    /// splice through the comment would rewrite it and corrupt the file.
    /// fails_when: `scan_value_end` runs past a comment start or a line
    /// break after a bare scalar. no_claim: how comments survive the splice.
    #[test]
    fn a_comment_after_a_bare_scalar_is_outside_the_value() {
        let raw = b"model: m1 // fast, right? }\n";
        let end = scan_value_end(raw, 7).expect("value ends");
        assert_eq!(&raw[7..end], b"m1");

        let raw = b"model: 42 /* ,} */";
        let end = scan_value_end(raw, 7).expect("value ends");
        assert_eq!(&raw[7..end], b"42");

        // No comment at all: whitespace and structure still bound the value.
        let raw = b"model: true\nnext: 1";
        let end = scan_value_end(raw, 7).expect("value ends");
        assert_eq!(&raw[7..end], b"true");
    }

    /// protects: strings and balanced structures scan to their true end —
    /// delimiters inside them never cut the span. fails_when: a `}`/`,`
    /// inside a string or a nested object truncates the scan. no_claim:
    /// malformed-input recovery.
    #[test]
    fn strings_and_balanced_values_span_their_delimiters() {
        let raw = b"model: \"a, b}\" , next: 1";
        let end = scan_value_end(raw, 7).expect("value ends");
        assert_eq!(&raw[7..end], b"\"a, b}\"");

        let raw = b"model: { \"k\": [1, 2], \"j\": \"}\" } , x: 1";
        let end = scan_value_end(raw, 7).expect("value ends");
        assert_eq!(&raw[7..end], b"{ \"k\": [1, 2], \"j\": \"}\" }");
    }
}
