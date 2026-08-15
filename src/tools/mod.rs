//! phiforge application tools: content search, repository map, and verify.
//!
//! `search_content` and `repo_map` are "pull-based" context providers (design
//! §3): rather than pushing the whole repository into the LLM, the agent asks
//! for a structural map or locates symbols by content on demand. `verify` is
//! the verification-loop tool (design §6): run a build/test command and get a
//! terse error summary back.

pub mod decompose;
pub mod merge;
pub mod repomap;
pub mod ripgrep;
pub mod verify;
pub mod workspace;

use std::path::{Component, Path};

/// Validate a user-supplied workspace-relative path and return it as-is.
///
/// Rejects absolute paths and `..` traversal that would escape the workspace
/// root. Mirrors `phi-kernel-tools`' private `resolve_path` (file/mod.rs),
/// reimplemented here because that helper isn't public. Returns the trimmed
/// relative path on success — callers run `rg` with `current_dir` set to the
/// workspace root, so a relative path stays in-scope.
pub fn validate_workspace_path(root: &Path, user_path: &str) -> Result<String, String> {
    let trimmed = user_path.trim();
    if trimmed.is_empty() {
        return Err("empty path".to_string());
    }

    let mut depth: i32 = 0;
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!(
                        "path traversal rejected: '{trimmed}' escapes the workspace root"
                    ));
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("absolute paths not allowed: '{trimmed}'"));
            }
            Component::Normal(_) => depth += 1,
        }
    }

    // If the target exists, canonicalize and confirm it stays inside the root
    // (guards against symlink escapes).
    let resolved = root.join(trimmed);
    if resolved.exists() {
        let canonical = resolved
            .canonicalize()
            .map_err(|e| format!("failed to resolve '{trimmed}': {e}"))?;
        let root_canonical = root
            .canonicalize()
            .map_err(|e| format!("failed to resolve workspace root: {e}"))?;
        if !canonical.starts_with(&root_canonical) {
            return Err(format!("path '{trimmed}' resolves outside the workspace root"));
        }
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
