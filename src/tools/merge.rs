//! Merge tool (Phase 4): reconcile parallel sub-agent work and verify the whole.
//!
//! The "合" + "验" half of design §7's `decompose`/`merge`. After sub-agents have
//! written their slices into the shared workspace, `merge`:
//!
//! 1. diffs the workspace against the snapshot `decompose` recorded (problem #3's
//!    change attribution, without relying on git);
//! 2. flags conflicts — a changed file declared by 2+ slices (overlap) or changed
//!    outside any declared boundary (out-of-scope edit);
//! 3. runs `cargo check` (or the caller's command) and folds in the terse error
//!    summary (problem #4 "合并验证"), reusing `verify::run_and_summarize`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use super::verify::run_and_summarize;
use super::workspace::{Change, ChangeKind, Slice, WorkspaceTracker, normalize_path};

/// A conflict between declared slice boundaries and what actually changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub reason: String,
}

/// Detect conflicts over *changed* files: a file declared by 2+ slices, or a
/// change landing outside every slice's declared boundary.
///
/// Pure and testable. Overlap is a decomposition bug (two slices claimed the same
/// file); out-of-scope is a sub-agent wandering off its slice.
pub fn detect_conflicts(slices: &[Slice], changes: &[Change]) -> Vec<Conflict> {
    let mut declared: HashMap<String, Vec<String>> = HashMap::new();
    for s in slices {
        for f in &s.files {
            declared
                .entry(normalize_path(f))
                .or_default()
                .push(s.name.clone());
        }
    }

    let mut out: Vec<Conflict> = Vec::new();
    for c in changes {
        let key = normalize_path(&c.path);
        match declared.get(&key) {
            None => out.push(Conflict {
                path: c.path.clone(),
                reason: "changed but not declared by any slice (out-of-scope edit)".to_string(),
            }),
            Some(names) if names.len() > 1 => out.push(Conflict {
                path: c.path.clone(),
                reason: format!("declared by {} slices: {}", names.len(), names.join(", ")),
            }),
            _ => {}
        }
    }
    out
}

pub struct MergeTool {
    tracker: Arc<WorkspaceTracker>,
    root: PathBuf,
    timeout_ms: u64,
}

impl MergeTool {
    pub fn new(tracker: Arc<WorkspaceTracker>, root: PathBuf, timeout_ms: u64) -> Self {
        Self {
            tracker,
            root,
            timeout_ms,
        }
    }
}

/// Render a change list + conflicts + verify result as one compact report.
fn format_report(changes: &[Change], conflicts: &[Conflict], verify: &str) -> String {
    let mut out = String::new();

    if changes.is_empty() {
        out.push_str("No files changed since decompose.\n");
    } else {
        out.push_str(&format!("Changed files ({}):\n", changes.len()));
        for c in changes {
            let kind = match c.kind {
                ChangeKind::Added => "added",
                ChangeKind::Modified => "modified",
                ChangeKind::Removed => "removed",
            };
            out.push_str(&format!("  {kind:9} {}\n", c.path));
        }
    }

    out.push('\n');
    if conflicts.is_empty() {
        out.push_str("Conflicts: none.\n");
    } else {
        out.push_str(&format!("Conflicts ({}):\n", conflicts.len()));
        for c in conflicts {
            out.push_str(&format!("  ⚠️  {} — {}\n", c.path, c.reason));
        }
        out.push_str("  → resolve before proceeding; do not silently overwrite.\n");
    }

    out.push_str(&format!("\nVerification:\n{verify}"));
    out
}

#[async_trait]
impl Tool for MergeTool {
    fn name(&self) -> &'static str {
        "merge"
    }

    fn description(&self) -> &'static str {
        "Reconcile changes after a `decompose`: diff the workspace against the pre-decompose snapshot, report changed files and conflicts (overlapping or out-of-scope edits), and run `cargo check` to verify the whole still compiles. Optional — `verify` alone is enough for the common single-writer flow."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Verification command (default `cargo check`)."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Merge sub-agent work and verify the workspace.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("cargo check")
            .to_string();

        tracing::info!(command = %command, "merge start");

        let changes = match self.tracker.changed_files(&self.root) {
            Some(c) => c,
            None => {
                return Ok(vec![Content::text(
                    "[Error]: no snapshot recorded — call `decompose` first so `merge` knows the baseline.",
                )]);
            }
        };

        let slices = self.tracker.slices();
        let conflicts = detect_conflicts(&slices, &changes);
        let verify = run_and_summarize(&command, self.timeout_ms).await;

        tracing::info!(
            changes = changes.len(),
            conflicts = conflicts.len(),
            "merge done"
        );

        Ok(vec![Content::text(format_report(
            &changes,
            &conflicts,
            &verify,
        ))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(name: &str, files: &[&str]) -> Slice {
        Slice {
            name: name.to_string(),
            files: files.iter().map(|s| s.to_string()).collect(),
            context: String::new(),
            task: String::new(),
        }
    }

    fn change(path: &str, kind: ChangeKind) -> Change {
        Change {
            path: path.to_string(),
            kind,
        }
    }

    #[test]
    fn detect_no_conflict_when_disjoint() {
        let slices = vec![slice("cache", &["src/cache.rs"]), slice("logging", &["src/logging.rs"])];
        let changes = vec![
            change("src/cache.rs", ChangeKind::Added),
            change("src/logging.rs", ChangeKind::Added),
        ];
        assert!(detect_conflicts(&slices, &changes).is_empty());
    }

    #[test]
    fn detect_out_of_scope_edit() {
        let slices = vec![slice("cache", &["src/cache.rs"])];
        let changes = vec![change("src/main.rs", ChangeKind::Modified)];
        let conflicts = detect_conflicts(&slices, &changes);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "src/main.rs");
        assert!(conflicts[0].reason.contains("out-of-scope"), "{}", conflicts[0].reason);
    }

    #[test]
    fn detect_overlap_when_two_slices_claim_a_file() {
        let slices = vec![
            slice("a", &["src/lib.rs"]),
            slice("b", &["src/lib.rs", "src/b.rs"]),
        ];
        let changes = vec![change("src/lib.rs", ChangeKind::Modified)];
        let conflicts = detect_conflicts(&slices, &changes);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "src/lib.rs");
        assert!(conflicts[0].reason.contains("2 slices"), "{}", conflicts[0].reason);
    }

    #[test]
    fn overlap_only_matters_on_changed_files() {
        // Both slices claim lib.rs but only b.rs changed → no conflict.
        let slices = vec![slice("a", &["src/lib.rs"]), slice("b", &["src/lib.rs", "src/b.rs"])];
        let changes = vec![change("src/b.rs", ChangeKind::Modified)];
        assert!(detect_conflicts(&slices, &changes).is_empty());
    }

    #[test]
    fn normalize_path_applied_to_slice_declarations() {
        // "./src/cache.rs" and "src/cache.rs" must be treated as the same path.
        let slices = vec![slice("cache", &["./src/cache.rs"])];
        let changes = vec![change("src/cache.rs", ChangeKind::Added)];
        assert!(detect_conflicts(&slices, &changes).is_empty());
    }

    #[test]
    fn format_report_lists_changes_conflicts_and_verify() {
        let changes = vec![change("src/cache.rs", ChangeKind::Added)];
        let conflicts = vec![Conflict {
            path: "src/main.rs".into(),
            reason: "out-of-scope".into(),
        }];
        let report = format_report(&changes, &conflicts, "✓ passed");
        assert!(report.contains("src/cache.rs"), "{report}");
        assert!(report.contains("added"), "{report}");
        assert!(report.contains("src/main.rs"), "{report}");
        assert!(report.contains("✓ passed"), "{report}");
    }

    #[test]
    fn format_report_empty_changes() {
        let report = format_report(&[], &[], "✓ passed");
        assert!(report.contains("No files changed"), "{report}");
        assert!(report.contains("Conflicts: none"), "{report}");
    }

    #[test]
    fn metadata_and_schema() {
        let t = MergeTool::new(Arc::new(WorkspaceTracker::new()), PathBuf::from("."), 30_000);
        assert_eq!(t.name(), "merge");
        let schema = t.schema();
        assert_eq!(schema["type"], "object");
        // `command` is optional — defaults to `cargo check`.
        assert!(schema["required"].as_array().map(|a| a.is_empty()).unwrap_or(false));
    }
}
