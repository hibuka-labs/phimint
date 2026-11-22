//! Test helpers shared across the crate's modules.
//!
//! CI runs the suite on Linux, macOS and Windows (2026-11-22). A path that is
//! absolute on Unix is not absolute on Windows (`/bin/true` has no drive
//! prefix there), and the script path law demands an absolute program — so a
//! hardcoded Unix path is a test that only holds on one platform.

use std::path::PathBuf;
use std::sync::OnceLock;

/// A real, absolute path to a program that exists on this platform.
///
/// The program is a plain file with **no extension** and a stem that is not
/// an interpreter name. Both matter:
///
/// * The script path law (`agent_works::script_tool::law`) allows `run[0]`
///   to be a relative path only when it names a whitelisted interpreter
///   (`powershell` / `pwsh` / `cmd`) — and then it demands that
///   interpreter's script-file argument. So a fixture that points at
///   `cmd.exe` is rejected with `InterpreterMissingScriptArg`: the law
///   treats it as "you meant to run a script", not "this is the program".
/// * The file must exist. The mount law checks the script file on disk, so
///   a path that is not there is blocked later — and a fixture that only
///   passes by accident is a fixture that stops passing when the law
///   changes.
///
/// The file is never executed by these tests; only its path shape matters.
pub fn abs_program() -> String {
    program_path().display().to_string()
}

/// The same path, JSON-escaped, for tests that write a raw JSON literal.
pub fn abs_program_json() -> String {
    abs_program().replace('\\', "\\\\")
}

/// The path of the one program file these tests share, created on first use.
fn program_path() -> PathBuf {
    static PROGRAM: OnceLock<PathBuf> = OnceLock::new();
    PROGRAM
        .get_or_init(|| {
            let path =
                std::env::temp_dir().join(format!("phimint-test-program-{}", std::process::id()));
            std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perm = std::fs::metadata(&path).unwrap().permissions();
                perm.set_mode(0o755);
                std::fs::set_permissions(&path, perm).unwrap();
            }
            path
        })
        .clone()
}
