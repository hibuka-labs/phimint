//! Test helpers shared across the crate's modules.
//!
//! CI runs the suite on Linux, macOS and Windows (2026-11-22). A path that is
//! absolute on Unix is not absolute on Windows (`/bin/true` has no drive
//! prefix there), and the script path law demands an absolute program — so a
//! hardcoded Unix path is a test that only holds on one platform.

/// An absolute path to a program that exists on this platform, for tests that
/// need the path law to accept a `run` argv.
///
/// The file is never executed by these tests — only its absolute-ness matters.
pub fn abs_program() -> String {
    let p = if cfg!(windows) {
        std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe")
    } else {
        std::path::PathBuf::from("/bin/true")
    };
    p.to_string_lossy().into_owned()
}

/// The same path, JSON-escaped, for tests that write a raw JSON literal.
pub fn abs_program_json() -> String {
    abs_program().replace('\\', "\\\\")
}
