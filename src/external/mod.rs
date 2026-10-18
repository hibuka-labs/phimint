//! Parse-and-merge of the five config surfaces (P3 product layer).
//!
//! The design's "config file model" has six lines / five surfaces; the
//! product layer reads each at startup/command time and merges them into one
//! runtime view:
//!
//! | File | Surface | This module |
//! |---|---|---|
//! | `~/.phimint/config.json` | global settings | [`crate::config`] (existing) |
//! | `~/.phimint/connections.json` | MCP connections | [`connections`] |
//! | `~/.phimint/tools.json` | global script tools | [`tools_json`] |
//! | `~/.phimint/approvals.json` | approval ledger | [`crate::approval`] (existing) |
//! | `~/.phimint/plugins/registry.json` | plugin integrity pinning | `phi_agent::plugin` |
//!
//! Two merge rules:
//!
//! - **`${ENV:}` pre-resolution** happens once at connection time / tool
//!   registration ([`connections::ConnectionEntry::server_config`],
//!   [`build_script_tool`]); a missing variable is **named on the spot**, not
//!   deferred to call time;
//! - **secret principle**: tokens appear only as `${ENV:VAR}` references. The
//!   paste surface is handled by [`wizard`] extraction; the agent-written
//!   surface is gated by the confirmation dialog (full rendering, only a
//!   human nod persists it).

pub mod catalog;
pub mod commands;
pub mod connections;
pub mod state;
pub mod tools_json;
pub mod wizard;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use phi_agent::Tool;
use phi_agent::script_tool::{ScriptTool, ScriptToolEntry};

/// Location of `~/.phimint/connections.json`.
pub fn connections_path(base_dir: &Path) -> PathBuf {
    base_dir.join("connections.json")
}

/// Location of `~/.phimint/tools.json`.
pub fn tools_path(base_dir: &Path) -> PathBuf {
    base_dir.join("tools.json")
}

/// `~/.phimint/plugins/` (plugin directory + `registry.json` live together).
pub fn plugins_root(base_dir: &Path) -> PathBuf {
    base_dir.join("plugins")
}

/// Startup load report: each surface's valid content + decent error notices
/// (goes into the transcript).
#[derive(Debug, Default)]
pub struct Surfaces {
    pub connections: Vec<connections::ConnectionEntry>,
    pub script_tools: Vec<ScriptToolEntry>,
    pub notices: Vec<String>,
}

/// The read half of the five-surface merge: connections.json + tools.json
/// (config/ledger/registry each have their own owner; this does not overstep).
/// Missing files are the normal state (first use).
pub fn load_surfaces(base_dir: &Path) -> Surfaces {
    let mut surfaces = Surfaces::default();
    match connections::load(&connections_path(base_dir)) {
        Ok(entries) => surfaces.connections = entries,
        Err(e) => surfaces.notices.push(e.to_string()),
    }
    let report = tools_json::load(&tools_path(base_dir));
    surfaces.script_tools = report.entries;
    surfaces.notices.extend(report.notices);
    surfaces
}

/// `${ENV:}` pre-resolution at script-tool registration + the identity key
/// (`script.{name}`).
///
/// Missing variable → `Err(decent error message)` (the tool does not enter
/// the table, and the error names which variable); success → a line that can
/// go straight into `ToolRegistry::register_arc`. `cwd` is pinned to the
/// current project root (design: "working directory = current project root;
/// pinned at product-layer registration"). `lookup` is the value port
/// (product uses [`phi_agent::system_env`]; tests inject a dict).
pub fn build_script_tool<F>(
    entry: ScriptToolEntry,
    workspace: &Path,
    lookup: F,
) -> Result<(String, Arc<dyn Tool>), String>
where
    F: Fn(&str) -> Option<String>,
{
    let identity = format!("script.{}", entry.name);
    let mut entry = entry;
    entry.cwd = Some(workspace.to_path_buf());
    // `${ENV:}` references in env values are resolved right now (once, at
    // tool registration).
    if !entry.env.is_empty() {
        let resolved = phi_agent::resolve_env_map(&entry.env, &lookup)
            .map_err(|e| format!("tool `{}`: {e}", entry.name))?;
        entry.env = resolved;
    }
    let tool = ScriptTool::with_key(&identity, entry);
    Ok((identity, Arc::new(tool)))
}

/// Live implementation of [`state::ToolHost`]: attached to a real
/// [`phi_agent::PhiAgent`] (tests use the fakes in state). The tool table =
/// `runtime().tools_mut()`; connections go through PhiAgent's MCP factory
/// (attach registers `mcp.{server}.*`, detach removes it).
pub struct PhiToolHost {
    pub agent: Arc<phi_agent::PhiAgent>,
}

#[async_trait::async_trait]
impl state::ToolHost for PhiToolHost {
    fn registry(&self) -> Arc<tokio::sync::RwLock<phi_agent::ToolRegistry>> {
        self.agent.runtime().tools_mut()
    }

    async fn attach_mcp(&self, config: phi_agent::McpServerConfig) -> Result<(), String> {
        self.agent
            .attach_mcp(config)
            .await
            .map_err(|e| e.to_string())
    }

    async fn detach_mcp(&self, name: &str) {
        self.agent.detach_mcp(name).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_surfaces_reads_both_files_and_reports_notices() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("connections.json"),
            r#"[{"name":"notion","enabled":true,"transport":{"type":"stdio","command":"npx","args":[],"env":{"NOTION_TOKEN":"${ENV:NOTION_TOKEN}"}}}]"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("tools.json"),
            r#"{"tools":[{"name":"deploy","description":"d","run":["/bin/true"]}]}"#,
        )
        .unwrap();
        let surfaces = load_surfaces(tmp.path());
        assert_eq!(surfaces.connections.len(), 1);
        assert_eq!(surfaces.script_tools.len(), 1);
        assert!(surfaces.notices.is_empty(), "{:?}", surfaces.notices);
    }

    #[test]
    fn load_surfaces_missing_files_is_clean_slate() {
        let tmp = tempfile::tempdir().unwrap();
        let surfaces = load_surfaces(tmp.path());
        assert!(surfaces.connections.is_empty());
        assert!(surfaces.script_tools.is_empty());
        assert!(surfaces.notices.is_empty());
    }

    #[test]
    fn build_script_tool_pins_workspace_and_resolves_env() {
        let entry = ScriptToolEntry {
            name: "deploy".into(),
            description: "d".into(),
            parameters: Default::default(),
            run: vec!["/bin/true".into()],
            timeout_ms: None,
            env: std::collections::BTreeMap::from([(
                "TARGET".to_string(),
                "${ENV:PHIMINT_TEST_ENV}".to_string(),
            )]),
            cwd: None,
            exposure: Default::default(),
        };
        // Missing variable → report its name.
        let err = build_script_tool(entry.clone(), Path::new("/ws"), |_| None)
            .err()
            .expect("missing env must fail");
        assert!(err.contains("PHIMINT_TEST_ENV"), "{err}");

        // Variable set → pre-resolved to a literal; cwd pinned to the project
        // root.
        let (identity, _tool) = build_script_tool(entry, Path::new("/ws"), |n| {
            (n == "PHIMINT_TEST_ENV").then(|| "staging".to_string())
        })
        .unwrap();
        assert_eq!(identity, "script.deploy");
    }
}
