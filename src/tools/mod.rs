//! phiforge application tools: content search, repository map, and verify.
//!
//! `search_content` and `repo_map` are "pull-based" context providers (design
//! §3): rather than pushing the whole repository into the LLM, the agent asks
//! for a structural map or locates symbols by content on demand. `verify` is
//! the verification-loop tool (design §6): run a build/test command and get a
//! terse error summary back.

pub mod decompose;
pub mod diagnostics;
pub mod merge;
pub mod repomap;
pub mod ripgrep;
pub mod verify;
pub mod workspace;

use std::path::Path;

/// Validate a user-supplied path and return it trimmed.
///
/// There is no workspace sandbox: absolute paths and `..` traversal are allowed,
/// matching the framework file tools and Claude Code's model — safety comes from
/// the approval layer (`auto`/`ask`/`deny`), not path boundaries. Callers may
/// pass the result to `rg` (with `current_dir` set to the workspace root) or to
/// `root.join(..)`; both treat an absolute path as absolute and a `..` path as
/// escaping the root. Only the empty path is rejected.
pub fn validate_workspace_path(_root: &Path, user_path: &str) -> Result<String, String> {
    let trimmed = user_path.trim();
    if trimmed.is_empty() {
        return Err("empty path".to_string());
    }
    Ok(trimmed.to_string())
}

/// Apply the exclusions shared by both tools (build output, vendored deps).
///
/// These are explicit `--glob` negations rather than relying on `.gitignore`:
/// ripgrep only honors `.gitignore` inside a git repository, but a coding agent
/// often runs on a bare directory (like a temp smoke workspace), where
/// `target/` etc. must still be ignored.
pub(crate) fn apply_common_excludes(cmd: &mut std::process::Command) {
    for glob in ["!target/**", "!node_modules/**"] {
        cmd.arg("--glob").arg(glob);
    }
}
