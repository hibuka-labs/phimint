//! Runtime state for external tools: the "patch panel" after merging the
//! five surfaces.
//!
//! Responsibilities (P3 product wiring layer):
//!
//! - **startup merge**: connections.json / tools.json / installed plugins →
//!   registered into the tool table, `${ENV:}` pre-resolved once at
//!   registration (a missing variable is named, a failed connection is never
//!   half-done);
//! - **command execution**: [`ExternalState::handle`] (the semantic body of
//!   [`super::commands`]);
//! - **switch ledger**: per-tool on/off takes effect immediately (unhook /
//!   re-hook in the table); tiers go through the [`LiveApprovalGate`]
//!   management surface;
//! - **human review before install**: plugin installs / agent-written tools
//!   all go through [`Confirm`], popping up the **full picture** (argv or wasm
//!   module + capabilities paths/hosts + timeout + env references); only a
//!   human nod persists it — **never allowed by default**.
//!
//! The main coupling is collected into two small interfaces, [`ToolHost`] /
//! [`Confirm`]: PhiAgent is the host, LiveApprovalGate is the confirmation
//! surface, tests use fakes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::plugin::loader::{PluginLoader, assign_mcp_tool_keys};
use phi_agent::plugin::manifest::{ResolvedCapability, ReviewDigest};
use phi_agent::plugin::{
    PluginMcpServer, PluginRegistry, PluginSource, commit_install, plan_install, remove_plugin,
};
use phi_agent::script_tool::{
    ScriptToolEntry, check_script_file_on_disk, script_file_index, script_file_is_program,
};
use phi_agent::{
    ApprovalDecision, ApprovalRequest, ApprovalTier, McpClient, McpServerConfig, McpToolAdapter,
    McpTransport, RiskLevel, Tool, ToolRegistry, resolve_env_map, system_env,
};
use tokio::sync::RwLock;

use super::connections::{self, ConnectionEntry};
use super::tools_json;
use super::{build_script_tool, connections_path, plugins_root, tools_path};
use crate::approval::LiveApprovalGate;

/// Tool-table host (PhiAgent's runtime face; tests use fakes).
#[async_trait]
pub trait ToolHost: Send + Sync {
    fn registry(&self) -> Arc<RwLock<ToolRegistry>>;
    /// Connect an MCP server and hook `mcp.{server}.*` into the table (the
    /// connection `/connect` owns).
    async fn attach_mcp(&self, config: McpServerConfig) -> Result<(), String>;
    /// Disconnect and unhook `mcp.{server}.*`.
    async fn detach_mcp(&self, name: &str);
}

/// Human-review-before-install surface (pops the full picture, waits for a
/// human nod). **Always asks** — regardless of the approval tier.
#[async_trait]
pub trait Confirm: Send + Sync {
    async fn confirm(&self, request: ApprovalRequest) -> ApprovalDecision;
}

/// The ledger gate is the confirmation surface: it goes through the same TUI
/// queue as tool approval (the same popup), but
/// [`LiveApprovalGate::confirm_always`] looks at neither tier nor mode —
/// installs always ask a human.
#[async_trait]
impl Confirm for LiveApprovalGate {
    async fn confirm(&self, request: ApprovalRequest) -> ApprovalDecision {
        self.confirm_always(request).await
    }
}

/// The source of a tool row (the `/tools` panel's "source/namespace" column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Built-in (bare builtin name; not in the ledger, goes through the
    /// three-tier approval).
    Builtin,
    /// MCP connection from `/connect` (`mcp.{server}.*`).
    Connection { name: String },
    /// Global script tool (`script.{name}`).
    Script { name: String },
    /// Plugin (`plugin.{plugin}.*`).
    Plugin { name: String },
}

impl Source {
    /// Determine the source from the identity key.
    pub fn of(identity: &str) -> Self {
        let mut segs = identity.split('.');
        match segs.next() {
            Some("mcp") => Self::Connection {
                name: segs.next().unwrap_or_default().to_string(),
            },
            Some("script") => Self::Script {
                name: segs.next().unwrap_or_default().to_string(),
            },
            Some("plugin") => Self::Plugin {
                name: segs.next().unwrap_or_default().to_string(),
            },
            _ => Self::Builtin,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Builtin => "builtin".into(),
            Self::Connection { name } => format!("mcp.{name}.*"),
            Self::Script { name } => format!("script.{name}"),
            Self::Plugin { name } => format!("plugin.{name}.*"),
        }
    }
}

/// Plugin runtime: load product + the owning MCP connection (the client must
/// stay alive for the tool rows to be callable).
struct PluginRuntime {
    /// The identity keys this plugin actually hooked into the table
    /// (including keys expanded from its owning MCP server).
    identity_keys: Vec<String>,
    /// The owning MCP server's client (drop = disconnect).
    #[allow(dead_code)] // held for RAII: dropped client kills the connection
    mcp_clients: Vec<Arc<McpClient>>,
}

/// The patch panel itself.
pub struct ExternalState {
    base_dir: PathBuf,
    workspace: PathBuf,
    /// In-memory copy of connections.json (written back via
    /// [`connections::save`]).
    connections: Vec<ConnectionEntry>,
    /// In-memory copy of tools.json.
    script_defs: Vec<ScriptToolEntry>,
    /// Tools that were switched off (the body is kept; switching back on
    /// re-hooks it — takes effect immediately).
    disabled: BTreeMap<String, Arc<dyn Tool>>,
    plugins: BTreeMap<String, PluginRuntime>,
    loader: PluginLoader,
    registry_file: PluginRegistry,
    /// Decent error notices from startup loading (corrupted registry, etc.),
    /// goes into the transcript, never silent.
    startup_notices: Vec<String>,
    /// The ledger management surface (tier query/set + `script.`/`plugin.`
    /// prefix cleanup).
    ledger: Arc<LiveApprovalGate>,
}

impl ExternalState {
    /// Read the two file surfaces (config/ledger/registry each have their own
    /// owner). No registration, no connection — that is [`Self::start`]'s job.
    pub fn load(base_dir: &Path, workspace: &Path, ledger: Arc<LiveApprovalGate>) -> Self {
        let surfaces = super::load_surfaces(base_dir);
        let plugins_root = plugins_root(base_dir);
        // Corrupted registry = report, never silently treat as empty (that is
        // the user's install ledger); but loading is fail-closed: with no
        // pinned integrity, not a single plugin is hooked up.
        let (registry_file, mut startup_notices) = match PluginRegistry::load(
            &PluginRegistry::path_in(&plugins_root),
        ) {
            Ok(registry) => (registry, Vec::new()),
            Err(e) => (
                PluginRegistry::new(),
                vec![format!(
                    "plugin registry unreadable: {e} (no plugins loaded; `plugin add` to re-pin)"
                )],
            ),
        };
        startup_notices.extend(surfaces.notices);
        let loader = match phi_agent::plugin::wasm::WasmRuntime::new() {
            Ok(runtime) => PluginLoader::with_wasm(Arc::new(runtime)),
            Err(_) => PluginLoader::new(),
        };
        Self {
            base_dir: base_dir.to_path_buf(),
            workspace: workspace.to_path_buf(),
            connections: surfaces.connections,
            script_defs: surfaces.script_tools,
            disabled: BTreeMap::new(),
            plugins: BTreeMap::new(),
            loader,
            registry_file,
            startup_notices,
            ledger,
        }
    }

    /// Handle to the ledger gate: the install confirmation surface is this
    /// very thing (the [`Confirm`] impl); the caller takes a copy of the
    /// `Arc` from here and then hands `&mut self` to [`Self::handle`], so the
    /// borrows don't fight.
    pub fn confirm_gate(&self) -> Arc<LiveApprovalGate> {
        Arc::clone(&self.ledger)
    }

    /// Startup merge: script tools → plugins → enabled connections, all
    /// registered/connected. Returns decent error notices (goes into the
    /// transcript; no panics, nothing left half-done).
    pub async fn start(&mut self, host: &dyn ToolHost) -> Vec<String> {
        let mut notices = std::mem::take(&mut self.startup_notices);
        // 1. Global script tools (`${ENV:}` pre-resolved at registration).
        for entry in self.script_defs.clone() {
            match build_script_tool(entry.clone(), &self.workspace, system_env) {
                Ok((_identity, tool)) => {
                    host.registry().write().await.register_arc(tool);
                }
                Err(e) => notices.push(e),
            }
        }
        // 2. Installed plugins (integrity re-verified during load: tampering →
        //    everything disabled + "add it again").
        let names: Vec<String> = self.registry_file.plugins.keys().cloned().collect();
        for name in names {
            match self.load_plugin(host, &name).await {
                Ok(mut msgs) => notices.append(&mut msgs),
                Err(e) => notices.push(e),
            }
        }
        // 3. Enabled connections (missing env → connection fails and says
        //    which one is missing). Startup merge and the `/connect` command
        //    path use the same criterion: connected but 0 tools is a ⚠, not
        //    silence — when the service can't be reached attach returns Err
        //    (the real cause is in e); 0 tools means it answered but came
        //    back empty-handed.
        for entry in self.connections.clone() {
            if !entry.enabled {
                continue;
            }
            match self.connect(host, &entry).await {
                Ok(0) => notices.push(connected_report(
                    &format!("`{}`", entry.name),
                    &entry.name,
                    0,
                )),
                Ok(_) => {}
                Err(e) => notices.push(e),
            }
        }
        notices
    }

    /// Establish one connection (`${ENV:}` pre-resolution → attach). Error
    /// copy is written for the user.
    async fn connect(&self, host: &dyn ToolHost, entry: &ConnectionEntry) -> Result<usize, String> {
        let config = entry
            .server_config(system_env)
            .map_err(|e| format!("connection `{}` not connected: {e}", entry.name))?;
        host.attach_mcp(config)
            .await
            .map_err(|e| format!("connection `{}` not connected: {e}", entry.name))?;
        let count = self
            .count_prefix(host, &format!("mcp.{}.", entry.name))
            .await;
        Ok(count)
    }

    async fn count_prefix(&self, host: &dyn ToolHost, prefix: &str) -> usize {
        let registry = host.registry();
        let guard = registry.read().await;
        guard
            .metadatas()
            .iter()
            .filter(|m| m.name.starts_with(prefix))
            .count()
    }

    /// Command semantic body. Returns the product copy for the transcript.
    pub async fn handle(
        &mut self,
        host: &dyn ToolHost,
        confirm: &dyn Confirm,
        cmd: super::commands::ExternalCommand,
    ) -> String {
        use super::commands::ExternalCommand as C;
        match cmd {
            C::ConnectHelp => super::catalog::render_catalog(),
            C::ConnectCatalog { name } => self.connect_catalog(host, &name).await,
            C::ConnectPaste { text } => self.connect_paste(host, &text).await,
            C::ToolsList => self.render_tools_panel(host).await,
            C::ToolToggle { key, on } => self.toggle_tool(host, &key, on).await,
            C::ToolRemove { name } => self.remove_script_tool(host, &name).await,
            C::PluginList => self.render_plugin_panel(host).await,
            C::PluginAdd { source } => self.plugin_add(host, confirm, &source).await,
            C::PluginRemove { name } => self.plugin_remove(host, &name).await,
        }
    }

    // ── /connect ─────────────────────────────────────────────────────────

    async fn connect_catalog(&mut self, host: &dyn ToolHost, name: &str) -> String {
        let Some(entry) = super::catalog::lookup(name) else {
            return format!(
                "no catalog entry named `{name}` — `/connect` lists the catalog; paste a config to add anything else"
            );
        };
        // env readiness criterion (UI: `[env ready]` / `[missing X] — export …`).
        let mut out = format!("● catalog hit: {} · {}", entry.title, entry.description);
        if !entry.required_env.is_empty() {
            out.push_str(&format!(" · needs {}", entry.required_env.join(", ")));
        }
        out.push('\n');
        if let Err(missing) = entry.check_env(system_env) {
            out.push_str(&missing_env_report(name, &missing));
            return out;
        }
        out.push_str("[env ready]\n");
        let conn = entry.to_connection();
        match self.connect(host, &conn).await {
            Ok(count) => {
                self.connections.retain(|c| c.name != conn.name);
                self.connections.push(conn.clone());
                if let Err(e) =
                    connections::save(&connections_path(&self.base_dir), &self.connections)
                {
                    out.push_str(&format!(
                        "⚠ connected but connections.json not saved: {e}\n"
                    ));
                }
                out.push_str(&connected_report(&entry.title, &conn.name, count));
            }
            Err(e) => out.push_str(&format!("✗ {e}")),
        }
        out
    }

    async fn connect_paste(&mut self, host: &dyn ToolHost, text: &str) -> String {
        let outcome = match super::wizard::parse_paste(text) {
            Ok(outcome) => outcome,
            Err(e) => return format!("✗ {e}"),
        };
        let entry = outcome.entry;
        let mut out = format!(
            "⚙ connection `{}` written to connections.json\n",
            entry.name
        );
        if !outcome.extracted.is_empty() {
            out.push_str("secret values extracted (never written, never echoed):\n");
            for line in &outcome.export_lines {
                out.push_str(&format!("  {line}\n"));
            }
        }
        for w in &outcome.warnings {
            out.push_str(&format!("({w})\n"));
        }
        // Literal secrets never hit disk — the entry only ever holds `${ENV:}`
        // references.
        self.connections.retain(|c| c.name != entry.name);
        self.connections.push(entry.clone());
        if let Err(e) = connections::save(&connections_path(&self.base_dir), &self.connections) {
            return format!("✗ connections.json not saved: {e}");
        }
        // If the env is ready, connect right away (one command does it);
        // otherwise tell the user the next step (≤2 commands in total).
        let refs = entry.env_refs();
        let missing: Vec<String> = refs
            .iter()
            .filter(|r| system_env(r).is_none())
            .cloned()
            .collect();
        if !missing.is_empty() {
            // The truth: a running phimint cannot see an `export` added
            // afterwards — env is read at startup. Fortunately the entry is
            // already saved, so the startup merge connects it after a restart
            // (still ≤2 commands in total).
            out.push_str(&format!(
                "next: export {} in the shell that launches phimint, then restart — this connection is saved and comes up on startup",
                missing.join(", "),
            ));
            return out;
        }
        match self.connect(host, &entry).await {
            Ok(count) => out.push_str(&connected_report(
                &format!("`{}`", entry.name),
                &entry.name,
                count,
            )),
            Err(e) => out.push_str(&format!("✗ {e}")),
        }
        out
    }

    // ── /tools ───────────────────────────────────────────────────────────

    /// Tier display for one tool row (builtins are not in the ledger and
    /// show `-`).
    fn key_tier(&self, key: &str) -> String {
        match Source::of(key) {
            Source::Builtin => "-".to_string(),
            _ => tier_label(self.ledger.tier_of(key)).to_string(),
        }
    }

    /// `/tools` panel: source / namespace / tier / switches (one screen).
    pub async fn render_tools_panel(&self, host: &dyn ToolHost) -> String {
        let registry = host.registry();
        let guard = registry.read().await;
        let mut names: Vec<String> = guard.metadatas().iter().map(|m| m.name.clone()).collect();
        names.sort();
        let mut disabled_keys: Vec<String> = self.disabled.keys().cloned().collect();
        disabled_keys.sort();

        let total = names.len() + disabled_keys.len();
        let mut out = format!(
            "tools — {total} total · {} on · {} off\n",
            names.len(),
            self.disabled.len()
        );
        // Group by source (namespace column + tier column).
        let mut groups: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
        for n in names {
            let source = Source::of(&n);
            groups.entry(source.label()).or_default().push((n, true));
        }
        for n in disabled_keys {
            let source = Source::of(&n);
            groups.entry(source.label()).or_default().push((n, false));
        }
        for (label, rows) in groups {
            let group_tier = match Source::of(&rows[0].0) {
                Source::Builtin => "builtin (3-tier approval)".to_string(),
                _ => tier_label(self.ledger.tier_of(&rows[0].0)).to_string(),
            };
            push_panel_group(&mut out, &label, &rows, &group_tier, &|key| {
                self.key_tier(key)
            });
        }
        out.push_str("toggle: /tools on|off <key> · remove script tool: /tools remove <key>");
        out
    }

    /// `/plugin list` (bare `/plugin` takes the same path): installed plugins
    /// overview — viewing / installing / uninstalling all close the loop in
    /// this namespace, no forcing the user to `/tools` just to look; per-tool
    /// toggles still belong to `/tools` (the footer points there — that panel
    /// manages tools from every source).
    pub async fn render_plugin_panel(&self, host: &dyn ToolHost) -> String {
        let installed: Vec<String> = self.registry_file.plugins.keys().cloned().collect();
        if installed.is_empty() {
            let usage = super::commands::usage_for("/plugin").unwrap_or_default();
            return format!(
                "no plugins installed yet — /plugin add <path|url> installs one\n\n{usage}"
            );
        }
        let registry = host.registry();
        let guard = registry.read().await;
        let live: Vec<String> = guard.metadatas().iter().map(|m| m.name.clone()).collect();
        drop(guard);

        let mut out = format!("plugins — {} installed\n", installed.len());
        for name in &installed {
            let prefix = format!("plugin.{name}.");
            let mut rows: Vec<(String, bool)> = live
                .iter()
                .filter(|k| k.starts_with(&prefix))
                .map(|k| (k.clone(), true))
                .collect();
            rows.extend(
                self.disabled
                    .keys()
                    .filter(|k| k.starts_with(&prefix))
                    .map(|k| (k.clone(), false)),
            );
            rows.sort();
            // Truth column: installed but not hooked into the table (load
            // failure / integrity disable) must be spoken out, not played dead.
            let group_tier = if rows.is_empty() {
                if self.plugins.contains_key(name.as_str()) {
                    "no live tools"
                } else {
                    "not loaded"
                }
                .to_string()
            } else {
                tier_label(self.ledger.tier_of(&rows[0].0)).to_string()
            };
            push_panel_group(
                &mut out,
                name, // group header = install name (the A the user sees; the A /plugin remove takes)
                &rows,
                &group_tier,
                &|key| self.key_tier(key),
            );
        }
        // The remove example follows the real name: name it outright with one
        // plugin, fall back to a placeholder only with several (the group
        // header is the name).
        let remove_example = if installed.len() == 1 {
            format!("/plugin remove {}", installed[0])
        } else {
            "/plugin remove <name>".to_string()
        };
        out.push_str(&format!(
            "add: /plugin add <path|url> · remove: {remove_example} · toggle: /tools on|off <key>"
        ));
        out
    }

    /// Per-tool toggle (takes effect immediately: off = unhook from the
    /// table, on = re-hook).
    async fn toggle_tool(&mut self, host: &dyn ToolHost, key: &str, on: bool) -> String {
        let registry = host.registry();
        if on {
            let Some(tool) = self.disabled.remove(key) else {
                return format!("`{key}` is not disabled");
            };
            registry.write().await.register_arc(tool);
            format!("✓ {key} on")
        } else {
            let mut guard = registry.write().await;
            let Some(tool) = guard.get(key) else {
                return format!("`{key}` not found in the tool table");
            };
            guard.remove(key);
            drop(guard);
            self.disabled.insert(key.to_string(), tool);
            format!("✓ {key} off (immediate — gone from the next turn)")
        }
    }

    /// `/tools remove <key>`: takes the form the panel shows
    /// (`script.deploy`; bare `deploy` resolves the same way — one entry, no
    /// ambiguity) → write back + unregister from the table + clean the ledger
    /// `script.{name}`.
    async fn remove_script_tool(&mut self, host: &dyn ToolHost, key: &str) -> String {
        let name = key.strip_prefix("script.").unwrap_or(key);
        if tools_json::remove_entry(&mut self.script_defs, name).is_err() {
            // Report truthfully: list the installed ones by the key form the
            // panel shows; no system-failure tone.
            let installed: Vec<String> = self
                .script_defs
                .iter()
                .map(|e| format!("script.{}", e.name))
                .collect();
            return if installed.is_empty() {
                format!("✗ `{key}` is not a global script tool — no script tools installed")
            } else {
                format!(
                    "✗ `{key}` is not a global script tool — installed: {}",
                    installed.join(", ")
                )
            };
        }
        if let Err(e) = tools_json::save(&tools_path(&self.base_dir), &self.script_defs) {
            return format!("✗ tools.json not saved: {e}");
        }
        let identity = format!("script.{name}");
        host.registry().write().await.remove(&identity);
        self.disabled.remove(&identity);
        self.ledger.remove_ledger_entry(&identity);
        format!("✓ tool `script.{name}` removed (tool table + tools.json + ledger)")
    }

    // ── agent-written tools (design: "human + agent-written (human-confirmed)") ──

    /// Agent-written tool: check the laws → **full confirmation** (always
    /// asks) → persist + register.
    ///
    /// Security baseline (SC7): before a human confirms the entry's full
    /// picture it is never executed and never allowed by default — the
    /// confirmation goes through [`Confirm`] ([`LiveApprovalGate::confirm_always`],
    /// ignores the tier), and the copy is [`render_tool_entry_review`]
    /// (name/description/full argv + absolute path/timeout/env references).
    ///
    /// Secret principle: `env` values accept only whole-value `${ENV:VAR}`
    /// references (`check_env_refs_only`); literals are refused.
    /// `registry` is the registry port (a Weak back-reference from the
    /// product tool `add_tool`; it does not go through [`ToolHost`] — this
    /// path never touches MCP).
    pub async fn add_tool_from_agent(
        &mut self,
        registry: &Arc<RwLock<ToolRegistry>>,
        confirm: &dyn Confirm,
        entry: ScriptToolEntry,
    ) -> String {
        // 1. Laws (name law + path law) — an invalid entry never reaches the
        //    confirmation dialog.
        if let Err(e) = entry.check_laws() {
            return format!("✗ tool `{}` rejected: {e}", entry.name);
        }
        // 1.5 Script file check (at install time; the same law as a missing
        //     plugin wasm file): hooking up a nonexistent script = broken the
        //     moment it's installed, and what the human sees in the
        //     confirmation dialog would be a false promise.
        if let Err(e) = (|| {
            let index = script_file_index(&entry.run)?;
            let require_executable = script_file_is_program(&entry.run)?;
            check_script_file_on_disk(Path::new(&entry.run[index]), require_executable)
        })() {
            return format!("✗ tool `{}` rejected: {e}", entry.name);
        }
        // 2. Secret principle (the agent side refuses any literal on sight;
        //    no guessing at which one is a token).
        if let Err(e) = check_env_refs_only(&entry.env) {
            return format!("✗ tool `{}` rejected: {e}", entry.name);
        }
        // 3. Duplicate name (same criterion as [`tools_json::insert`], cheap
        //    check first):
        //    - **same entry = idempotent retry**: the previous round may have
        //      been cancelled after persisting but before registering (tool
        //      timeout, Ctrl+C) — finish the persist/register and report
        //      success, without popping a second confirmation (the human
        //      already nodded at this full picture; asking again is noise);
        //    - **different entry = conflict**: spell out who holds the name
        //      and how to free it.
        if let Some(existing) = self.script_defs.iter().find(|e| e.name == entry.name) {
            if existing == &entry {
                let (identity, tool) =
                    match build_script_tool(entry.clone(), &self.workspace, system_env) {
                        Ok(built) => built,
                        Err(e) => return format!("✗ tool `{}` not added: {e}", entry.name),
                    };
                if let Err(e) = tools_json::save(&tools_path(&self.base_dir), &self.script_defs) {
                    return format!(
                        "✗ tools.json not saved: {e} (tool registered this session only)"
                    );
                }
                registry.write().await.register_arc(tool);
                return format!(
                    "✓ tool `{identity}` already mounted (identical entry — registered & callable now)"
                );
            }
            return format!(
                "✗ tool `{}` already exists as `script.{}` with a different entry — pick \
                 another name, or /tools remove `script.{}` first",
                entry.name, entry.name, entry.name
            );
        }
        // 4. Build before writing: `${ENV:}` pre-resolution failure (missing
        //    variable) → nothing persisted, nothing half-done.
        let (identity, tool) = match build_script_tool(entry.clone(), &self.workspace, system_env) {
            Ok(built) => built,
            Err(e) => return format!("✗ tool `{}` not added: {e}", entry.name),
        };
        // 5. Full-picture confirmation — only a human nod persists it (never
        //    allowed by default).
        let request = ApprovalRequest {
            title: format!("add tool `{identity}`"),
            message: render_tool_entry_review(&entry),
            action_key: None,
            risk_level: RiskLevel::Sensitive,
            raw: None,
            source: None,
        };
        match confirm.confirm(request).await {
            ApprovalDecision::Deny => {
                return format!(
                    "tool `{}` not added (declined — nothing written)",
                    entry.name
                );
            }
            ApprovalDecision::AllowOnce | ApprovalDecision::AllowAlways => {}
        }
        // 6. Persist + register (immediately callable).
        if let Err(e) = tools_json::insert(&mut self.script_defs, entry) {
            return format!("✗ {e}");
        }
        if let Err(e) = tools_json::save(&tools_path(&self.base_dir), &self.script_defs) {
            return format!("✗ tools.json not saved: {e} (tool registered this session only)");
        }
        registry.write().await.register_arc(tool);
        format!("✓ tool added as {identity} (tools.json + tool table — callable now)")
    }

    // ── /plugin ──────────────────────────────────────────────────────────

    async fn plugin_add(
        &mut self,
        host: &dyn ToolHost,
        confirm: &dyn Confirm,
        source: &str,
    ) -> String {
        let source = match parse_plugin_source(source) {
            Ok(source) => source,
            Err(e) => return format!("✗ {e}"),
        };
        let plugins_root = plugins_root(&self.base_dir);
        let plan = match plan_install(&source, &plugins_root).await {
            Ok(plan) => plan,
            Err(e) => return format!("✗ plugin add failed: {e}"),
        };
        // Human review before install: full picture (capability details
        // paths/hosts) → only a human nod persists it.
        let digest = plan.digest.clone();
        let request = ApprovalRequest {
            title: format!("install plugin `{}`", digest.name),
            message: render_review(&digest),
            action_key: None,
            risk_level: RiskLevel::Sensitive,
            raw: None,
            source: None,
        };
        match confirm.confirm(request).await {
            ApprovalDecision::Deny => {
                plan.discard();
                return format!("plugin `{}` install cancelled", digest.name);
            }
            ApprovalDecision::AllowOnce | ApprovalDecision::AllowAlways => {}
        }
        let installed = match commit_install(&plan, &mut self.registry_file) {
            Ok(installed) => installed,
            Err(e) => {
                plan.discard();
                return format!("✗ plugin add failed: {e}");
            }
        };
        match self.load_plugin(host, &installed.pin.name).await {
            Ok(msgs) => {
                let name = &installed.pin.name;
                let prefix = format!("plugin.{name}.");
                // Closed loop: say "how to use it" right after install —
                // which keys are now in the tool table, what each one does
                // (descriptions from the manifest), and how to review/toggle/
                // uninstall.
                let registry = host.registry();
                let guard = registry.read().await;
                let tools: Vec<(String, String)> = guard
                    .metadatas()
                    .iter()
                    .filter(|m| m.name.starts_with(&prefix))
                    .map(|m| (m.name.clone(), m.description.clone()))
                    .collect();
                drop(guard);
                let mut out = format!("✓ {name} installed · {} tools ({prefix}*)\n", tools.len());
                for (key, desc) in &tools {
                    out.push_str(&format!("  {key:<32} {desc}\n"));
                }
                for m in msgs {
                    out.push_str(&format!("  {m}\n"));
                }
                out.push_str(&format!(
                    "→ these are model-callable tools: ask me to use them by name in chat · /plugin list to review · /tools on|off <key> to toggle · /plugin remove {name} to uninstall"
                ));
                out
            }
            Err(e) => format!("✓ {} installed, but load failed: {e}", installed.pin.name),
        }
    }

    async fn plugin_remove(&mut self, host: &dyn ToolHost, name: &str) -> String {
        // One thing, one name: only the install name shown in the /plugin
        // list group header is accepted. Wrong shape / not installed → report
        // truthfully (no "remove failed" system-failure tone), and lay out the
        // right name.
        if !self.registry_file.plugins.contains_key(name) {
            if let Some(mid) = name
                .strip_prefix("plugin.")
                .and_then(|rest| rest.split('.').next())
                .filter(|s| !s.is_empty())
                && self.registry_file.plugins.contains_key(mid)
            {
                // They copied a tool key / namespace shape: point out the
                // owning plugin name, but don't execute on their behalf (one
                // A, one spelling — show the way, don't type for them).
                return format!(
                    "✗ `{name}` is not a plugin name — it's `{mid}`: /plugin remove {mid}"
                );
            }
            let installed: Vec<String> = self.registry_file.plugins.keys().cloned().collect();
            return if installed.is_empty() {
                format!(
                    "✗ `{name}` is not installed — no plugins installed · add one: /plugin add <path|url>"
                )
            } else {
                format!(
                    "✗ `{name}` is not installed — installed: {}",
                    installed.join(", ")
                )
            };
        }
        let plugins_root = plugins_root(&self.base_dir);
        let report = match remove_plugin(&plugins_root, &mut self.registry_file, name) {
            Ok(report) => report,
            Err(e) => return format!("✗ plugin remove failed: {e}"),
        };
        // Clean up in all three places: tool table / ledger / registry (the
        // registry is already handled in remove_plugin).
        let prefix = format!("plugin.{name}.");
        let mut keys: Vec<String> = report.identity_keys.clone();
        if let Some(runtime) = self.plugins.remove(name) {
            keys.extend(runtime.identity_keys);
            // Dropping mcp_clients here disconnects the owning server.
        }
        {
            let registry = host.registry();
            let mut guard = registry.write().await;
            // Cover keys expanded from the owning MCP server (report only
            // has the label keys).
            let live: Vec<String> = guard
                .metadatas()
                .iter()
                .map(|m| m.name.clone())
                .filter(|n| n.starts_with(&prefix))
                .collect();
            keys.extend(live);
            keys.sort();
            keys.dedup();
            for key in &keys {
                guard.remove(key);
            }
        }
        for key in &keys {
            self.disabled.remove(key);
        }
        self.ledger.remove_ledger_prefix(&report.ledger_prefix);
        let mut out = format!(
            "✓ plugin `{name}` removed · {} tools unregistered · ledger swept\n",
            keys.len()
        );
        if !report.dir_removed {
            out.push_str("  (install directory could not be deleted — remove it manually)\n");
        }
        out
    }

    /// Load an installed plugin: integrity re-verified → build rows per form
    /// → hook up the owning MCP connection. Returns notices (decent error
    /// messages for corruption/tampering; never skips the whole package, never
    /// panics).
    async fn load_plugin(
        &mut self,
        host: &dyn ToolHost,
        name: &str,
    ) -> Result<Vec<String>, String> {
        let plugins_root = plugins_root(&self.base_dir);
        let loaded = self.loader.load(name, &self.registry_file, &plugins_root);
        let mut notices = Vec::new();
        if let Some(msg) = loaded.user_message() {
            notices.push(msg);
        }
        let mut identity_keys = loaded.identity_keys();
        {
            let registry = host.registry();
            let mut guard = registry.write().await;
            for tool in &loaded.tools {
                guard.register_arc(Arc::clone(tool));
            }
        }
        for d in &loaded.disabled {
            notices.push(format!("{} disabled — {}", d.identity, d.reason));
        }
        // Owning MCP server: keys are assigned as `plugin.{plugin}.{segment}`
        // (collisions get a suffix).
        let mut clients = Vec::new();
        for server in &loaded.mcp_servers {
            match self.connect_plugin_mcp(host, name, server).await {
                Ok((client, keys, rejected)) => {
                    notices.push(format!(
                        "plugin mcp `{}`: {} tools (plugin.{name}.*)",
                        server.label,
                        keys.len()
                    ));
                    for r in rejected {
                        notices.push(format!(
                            "plugin mcp `{}`: tool `{r}` rejected (bad name)",
                            server.label
                        ));
                    }
                    identity_keys.extend(keys);
                    clients.push(client);
                }
                Err(e) => notices.push(format!("plugin mcp `{}` not connected: {e}", server.label)),
            }
        }
        if self
            .plugins
            .insert(
                name.to_string(),
                PluginRuntime {
                    identity_keys,
                    mcp_clients: clients,
                },
            )
            .is_some()
        {
            notices.push(format!("plugin `{name}` reloaded"));
        }
        Ok(notices)
    }

    /// Owning MCP server connection + `plugin.{plugin}.{segment}` key
    /// registration.
    async fn connect_plugin_mcp(
        &self,
        host: &dyn ToolHost,
        plugin: &str,
        server: &PluginMcpServer,
    ) -> Result<(Arc<McpClient>, Vec<String>, Vec<String>), String> {
        let env = resolve_env_map(&server.env, system_env).map_err(|e| format!("env: {e}"))?;
        let transport = match (&server.command, &server.url) {
            (Some(command), _) => {
                let (program, args) = command
                    .split_first()
                    .ok_or_else(|| "empty command".to_string())?;
                let mut transport = McpTransport::stdio(program.clone(), args.to_vec());
                for (k, v) in &env {
                    transport = transport.with_env(k, v);
                }
                transport
            }
            (None, Some(url)) => {
                let headers = resolve_env_map(&server.headers, system_env)
                    .map_err(|e| format!("env: {e}"))?;
                let mut transport = McpTransport::http(url.clone());
                for (k, v) in &headers {
                    transport = transport.with_header(k, v);
                }
                transport
            }
            _ => return Err("plugin mcp server has neither command nor url".into()),
        };
        let client = Arc::new(
            McpClient::new(transport)
                .await
                .map_err(|e| format!("connect: {e}"))?,
        );
        let tools = client
            .list_tools()
            .await
            .map_err(|e| format!("list tools: {e}"))?;
        let raw_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        let assignment = assign_mcp_tool_keys(plugin, &raw_names);
        let mut keys = Vec::new();
        {
            let registry = host.registry();
            let mut guard = registry.write().await;
            for (raw, key) in &assignment.keys {
                if let Some(info) = tools.iter().find(|t| &t.name == raw) {
                    let adapter =
                        McpToolAdapter::with_identity_key(key, info.clone(), Arc::clone(&client));
                    guard.register_arc(Arc::new(adapter));
                    keys.push(key.clone());
                }
            }
        }
        Ok((client, keys, assignment.rejected))
    }
}

fn tier_label(tier: ApprovalTier) -> &'static str {
    match tier {
        ApprovalTier::Ask => "ask each time",
        ApprovalTier::AlwaysAllow => "always allow",
        ApprovalTier::Deny => "deny",
    }
}

/// Group header and tool rows of the panel (`/tools` and `/plugin list` share
/// one layout: source column 28, key column 32, state column on/off, tier
/// column).
fn push_panel_group(
    out: &mut String,
    label: &str,
    rows: &[(String, bool)],
    group_tier: &str,
    key_tier: &dyn Fn(&str) -> String,
) {
    out.push_str(&format!(
        "  ● {label:<28} {:>3} tools  {group_tier}\n",
        rows.len()
    ));
    for (key, on) in rows {
        let state = if *on { "on " } else { "off" };
        out.push_str(&format!("      {state}  {key:<32} {}\n", key_tier(key)));
    }
}

/// `plugin add` source parsing: tilde expansion + `url#rev` → git rev; the
/// law capping the channels (path / git / archive) lives in the framework's
/// [`PluginSource::parse`].
pub fn parse_plugin_source(spec: &str) -> Result<PluginSource, String> {
    let expanded = match spec.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home)
                .join(rest)
                .to_string_lossy()
                .into_owned(),
            None => spec.to_string(),
        },
        None => spec.to_string(),
    };
    // `#rev` is only meaningful for the git channel (the framework parse
    // does not carry rev).
    if let Some((head, rev)) = expanded.split_once('#')
        && !rev.is_empty()
        && (head.ends_with(".git") || head.starts_with("git@"))
    {
        return match PluginSource::parse(head) {
            Ok(PluginSource::Git { url, .. }) => Ok(PluginSource::Git {
                url,
                rev: Some(rev.to_string()),
            }),
            Ok(other) => Ok(other),
            Err(e) => Err(e.to_string()),
        };
    }
    PluginSource::parse(&expanded).map_err(|e| e.to_string())
}

/// Full picture for install human review: name/version + per-tool (form /
/// exec summary / timeout / capabilities). Design: "the confirmation dialog
/// for agent-written tools/plugins must render the full picture… a
/// name-only confirmation is not allowed".
pub fn render_review(digest: &ReviewDigest) -> String {
    let mut out = format!(
        "plugin `{}` v{} — {} tool(s)\nconfirm or abort with the keys below\n\n",
        digest.name,
        digest.version,
        digest.tools.len()
    );
    for tool in &digest.tools {
        out.push_str(&format!("  {}\n", tool.identity));
        out.push_str(&format!("    form: {}\n", tool.form));
        out.push_str(&format!("    exec: {}\n", tool.summary));
        if let Some(secs) = tool.timeout_secs {
            out.push_str(&format!("    timeout: {secs}s\n"));
        }
        for cap in &tool.capabilities {
            out.push_str(&format!("    capability: {}\n", render_capability(cap)));
        }
    }
    if !digest.skills.is_empty() {
        out.push_str(&format!("  skills: {}\n", digest.skills.join(", ")));
    }
    out
}

fn render_capability(cap: &ResolvedCapability) -> String {
    match cap {
        ResolvedCapability::FsRead { paths } => format!("fs:read {}", paths.join(" ")),
        ResolvedCapability::NetHttp { hosts } => format!("net:http {}", hosts.join(" ")),
    }
}

/// Full-picture confirmation copy for an agent-written tool entry (used by
/// the `add_tool` tool).
pub fn render_tool_entry_review(entry: &ScriptToolEntry) -> String {
    let mut out = format!(
        "add script tool `{}`\nconfirm or abort with the keys below\n\n  description: {}\n  argv: {:?}\n",
        entry.name, entry.description, entry.run
    );
    let timeout = entry.timeout_ms.map(|ms| ms / 1_000).unwrap_or(60);
    out.push_str(&format!("  timeout: {timeout}s\n"));
    if !entry.env.is_empty() {
        // Full picture = each env reference listed (not just the key names —
        // "a name-only confirmation is not allowed").
        out.push_str("  env:\n");
        for (key, value) in &entry.env {
            out.push_str(&format!("    {key} = {value}\n"));
        }
    }
    out.push_str("  (absolute path pinned at registration; secrets only as ${ENV:VAR})\n");
    out
}

/// The executable form of the secret principle (agent-written surface): an
/// `env` value must be a **whole-value reference** `${ENV:NAME}`. A machine
/// cannot tell which literal is a token, so the agent side refuses any
/// literal on sight — hand-written tools.json is unrestricted (that file
/// surface belongs to humans; the wizard does secret extraction).
fn check_env_refs_only(env: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    for (key, value) in env {
        let is_whole_ref = value
            .strip_prefix("${ENV:")
            .and_then(|rest| rest.strip_suffix('}'))
            .is_some_and(|name| {
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
        if !is_whole_ref {
            return Err(format!(
                "env `{key}` must be a whole-value `${{ENV:VAR}}` reference \
                 (secrets never land on disk as literals) — have the human `export` it"
            ));
        }
    }
    Ok(())
}

/// Report after connecting. **0 tools is not success** — the handshake went
/// through but the tool surface is empty (server wants auth / prerequisites
/// not ready / genuinely no tools), so surface it as a warning; a ✓ install
/// success is not allowed.
fn connected_report(subject: &str, name: &str, count: usize) -> String {
    if count == 0 {
        format!(
            "⚠ {subject} connected but exposed 0 tools (mcp.{name}.*) — the server answered but listed nothing; check its auth/session/docs"
        )
    } else {
        format!("✓ {subject} connected · {count} tools available (mcp.{name}.*)")
    }
}

/// Error copy for a missing env (used by `/connect <catalog>`).
///
/// Tell the truth: a running phimint **cannot see an `export` added
/// afterwards** — env is read at launch, so after exporting you must restart
/// and type `/connect` again. It's a pure function so the copy can be pinned
/// without eating the process environment.
fn missing_env_report(name: &str, missing: &[String]) -> String {
    let exports: Vec<String> = missing
        .iter()
        .map(|v| format!("export {v}=\"<paste value>\""))
        .collect();
    format!(
        "[missing {}] — export it in the shell that launches phimint:\n  {}\n\
         then restart phimint (env is read at launch) and `/connect {name}` again",
        missing.join(", "),
        exports.join("\n  ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::build_live_approval;

    /// Fake host: in-memory tool table; MCP only keeps books.
    #[allow(dead_code)] // fields are write-only in some tests
    struct FakeHost {
        registry: Arc<RwLock<ToolRegistry>>,
        attached: std::sync::Mutex<Vec<String>>,
        detached: std::sync::Mutex<Vec<String>>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                registry: Arc::new(RwLock::new(ToolRegistry::default())),
                attached: Default::default(),
                detached: Default::default(),
            }
        }
    }

    #[async_trait]
    impl ToolHost for FakeHost {
        fn registry(&self) -> Arc<RwLock<ToolRegistry>> {
            Arc::clone(&self.registry)
        }
        async fn attach_mcp(&self, config: McpServerConfig) -> Result<(), String> {
            self.attached.lock().unwrap().push(config.name.clone());
            Ok(())
        }
        async fn detach_mcp(&self, name: &str) {
            self.detached.lock().unwrap().push(name.to_string());
        }
    }

    struct NoConfirm;
    #[async_trait]
    impl Confirm for NoConfirm {
        async fn confirm(&self, _request: ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::AllowOnce
        }
    }

    struct DenyConfirm;

    #[async_trait]
    impl Confirm for DenyConfirm {
        async fn confirm(&self, _request: ApprovalRequest) -> ApprovalDecision {
            ApprovalDecision::Deny
        }
    }

    /// Records the confirmation copy + configurable allow (assertions about
    /// "invalid entries don't pop a dialog" use it to prove "it never asked").
    struct RecordingConfirm {
        seen: std::sync::Mutex<Vec<String>>,
        allow: bool,
    }

    impl RecordingConfirm {
        fn allow() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
                allow: true,
            }
        }
    }

    #[async_trait]
    impl Confirm for RecordingConfirm {
        async fn confirm(&self, request: ApprovalRequest) -> ApprovalDecision {
            self.seen.lock().unwrap().push(request.message);
            if self.allow {
                ApprovalDecision::AllowOnce
            } else {
                ApprovalDecision::Deny
            }
        }
    }

    fn test_state(dir: &Path) -> ExternalState {
        let ledger = build_live_approval("auto", None).0;
        ExternalState::load(dir, dir, ledger)
    }

    /// A real executable script (created once per process).
    ///
    /// The mount law checks the file exists and is executable — a fake path
    /// (the former `/bin/true`, absent on this machine) would be blocked by
    /// the new law; fixtures must be honest.
    fn real_script() -> PathBuf {
        use std::sync::OnceLock;
        static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
        SCRIPT
            .get_or_init(|| {
                let path = std::env::temp_dir()
                    .join(format!("phimint-test-script-{}.sh", std::process::id()));
                std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
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

    fn entry(name: &str) -> ScriptToolEntry {
        ScriptToolEntry::new(name, "d", vec![real_script().display().to_string()]).unwrap()
    }

    #[test]
    fn source_of_identity_keys() {
        assert_eq!(
            Source::of("mcp.figma.get_file"),
            Source::Connection {
                name: "figma".into()
            }
        );
        assert_eq!(
            Source::of("script.deploy"),
            Source::Script {
                name: "deploy".into()
            }
        );
        assert_eq!(
            Source::of("plugin.my-tools.wiki"),
            Source::Plugin {
                name: "my-tools".into()
            }
        );
        assert_eq!(Source::of("read_file"), Source::Builtin);
    }

    #[test]
    fn parse_plugin_source_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("my-tools");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            parse_plugin_source(&dir.to_string_lossy()),
            Ok(PluginSource::Path(_))
        ));
        assert!(matches!(
            parse_plugin_source("https://example.com/p.zip"),
            Ok(PluginSource::ArchiveUrl {
                format: phi_agent::plugin::ArchiveFormat::Zip,
                ..
            })
        ));
        assert!(matches!(
            parse_plugin_source("https://github.com/a/b.git"),
            Ok(PluginSource::Git { .. })
        ));
        // `#rev` only takes effect on the git channel.
        match parse_plugin_source("https://github.com/a/b.git#v1").unwrap() {
            PluginSource::Git { rev, .. } => assert_eq!(rev.as_deref(), Some("v1")),
            other => panic!("wrong source: {other:?}"),
        }
        // Neither a directory nor an archive → decent error message.
        assert!(parse_plugin_source("no-such-plugin-anywhere").is_err());
    }

    #[test]
    fn review_renders_full_facts_not_just_names() {
        let digest = ReviewDigest {
            name: "my-tools".into(),
            version: "0.1.0".into(),
            tools: vec![phi_agent::plugin::ReviewTool {
                identity: "plugin.my-tools.echo".into(),
                form: "wasm",
                summary: "/home/u/.phimint/plugins/my-tools/echo.wasm".into(),
                timeout_secs: Some(30),
                capabilities: vec![
                    ResolvedCapability::FsRead {
                        paths: vec!["/home/u/.phimint/plugins/my-tools/data".into()],
                    },
                    ResolvedCapability::NetHttp {
                        hosts: vec!["api.example.com".into()],
                    },
                ],
            }],
            skills: vec![],
        };
        let text = render_review(&digest);
        assert!(text.contains("echo.wasm"), "{text}");
        assert!(
            text.contains("fs:read /home/u/.phimint/plugins/my-tools/data"),
            "{text}"
        );
        assert!(text.contains("net:http api.example.com"), "{text}");
        assert!(text.contains("timeout: 30s"), "{text}");
    }

    /// Minimal script-form plugin package (`run[0]` is executed directly →
    /// the install law requires the executable bit).
    fn write_script_plugin(dir: &Path) {
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{
  "schemaVersion": 1,
  "name": "my-tools",
  "version": "0.1.0",
  "tools": [
    {
      "name": "deploy",
      "form": "script",
      "description": "deploy it",
      "run": ["scripts/deploy.sh", "--env", "${env}"],
      "parameters": { "env": { "type": "string" } }
    }
  ]
}"#,
        )
        .unwrap();
        std::fs::write(dir.join("scripts/deploy.sh"), "#!/bin/sh\necho deployed\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(dir.join("scripts/deploy.sh"))
                .unwrap()
                .permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(dir.join("scripts/deploy.sh"), perm).unwrap();
        }
    }

    /// The install report must close the loop: key names (the model calls by
    /// name) + description (what it does) + how to manage (/plugin list to
    /// review · /tools to toggle · /plugin remove to uninstall) — not just a
    /// bare "installed".
    #[tokio::test]
    async fn plugin_add_success_tells_how_to_use_and_manage() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let src = tmp.path().join("src-plugin");
        write_script_plugin(&src);

        let msg = state
            .plugin_add(&host, &NoConfirm, &src.to_string_lossy())
            .await;
        assert!(msg.contains("✓ my-tools installed"), "{msg}");
        // Loop one: the identity key — what the model sees and what the user
        // toggles against in /tools.
        assert!(msg.contains("plugin.my-tools.deploy"), "{msg}");
        // Loop two: the description — what this tool does.
        assert!(msg.contains("deploy it"), "{msg}");
        // Loop three: how to use (have the model call it by name) + how to
        // manage (review/toggle/uninstall).
        assert!(msg.contains("/plugin list"), "{msg}");
        assert!(msg.contains("/tools"), "{msg}");
        assert!(msg.contains("/plugin remove my-tools"), "{msg}");
        assert!(
            host.registry()
                .read()
                .await
                .get("plugin.my-tools.deploy")
                .is_some(),
            "the tool is really hooked into the table: {msg}"
        );
    }

    /// `/plugin list`: what is installed, each tool's state, how to manage —
    /// viewing too closes the loop in this namespace; no forcing the user to
    /// `/tools`.
    #[tokio::test]
    async fn plugin_list_panel_shows_tools_states_and_manage_footer() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let src = tmp.path().join("src-plugin");
        write_script_plugin(&src);
        state
            .plugin_add(&host, &NoConfirm, &src.to_string_lossy())
            .await;

        let panel = state.render_plugin_panel(&host).await;
        assert!(panel.contains("plugins — 1 installed"), "{panel}");
        // One thing, one name: the group header = the install name (the A
        // the user sees), and that is exactly what remove takes; the rows
        // keep the full key (the contract for toggle/call — copy and use).
        assert!(panel.contains("\n  ● my-tools "), "{panel}");
        assert!(panel.contains("on   plugin.my-tools.deploy"), "{panel}");
        // The tier is English (no Chinese on the product surface).
        assert!(panel.contains("ask each time"), "{panel}");
        assert!(!panel.contains('每'), "{panel}");
        assert!(panel.contains("remove: /plugin remove my-tools"), "{panel}");
        assert!(panel.contains("/tools on|off <key>"), "{panel}");

        // Switched-off tools are also visible in the list (off); the total
        // still counts as installed.
        state
            .toggle_tool(&host, "plugin.my-tools.deploy", false)
            .await;
        let panel = state.render_plugin_panel(&host).await;
        assert!(panel.contains("off  plugin.my-tools.deploy"), "{panel}");
        assert!(panel.contains("plugins — 1 installed"), "{panel}");
    }

    /// Installed but not hooked into the table must speak the truth: not
    /// loaded → "not loaded"; loaded but zero tools → "no live tools". Never
    /// play dead.
    #[tokio::test]
    async fn plugin_list_marks_installed_but_unusable_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let ghost = phi_agent::plugin::PluginPin {
            name: "ghost".into(),
            version: "0.0.0".into(),
            source: "test".into(),
            manifest_sha256: String::new(),
            tools: vec![],
        };
        state.registry_file.plugins.insert("ghost".into(), ghost);

        let panel = state.render_plugin_panel(&host).await;
        assert!(panel.contains("plugins — 1 installed"), "{panel}");
        assert!(panel.contains("not loaded"), "{panel}");

        // Has a load runtime but zero rows in the table (e.g. integrity
        // disable) → no live tools.
        state.plugins.insert(
            "ghost".into(),
            PluginRuntime {
                identity_keys: vec![],
                mcp_clients: vec![],
            },
        );
        let panel = state.render_plugin_panel(&host).await;
        assert!(panel.contains("no live tools"), "{panel}");
        assert!(!panel.contains("not loaded"), "{panel}");
    }

    /// None installed → teach the addition (usage hint), not a blank screen.
    #[tokio::test]
    async fn plugin_list_empty_state_shows_how_to_add() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path());
        let host = FakeHost::new();

        let panel = state.render_plugin_panel(&host).await;
        assert!(panel.contains("no plugins installed"), "{panel}");
        assert!(panel.contains("/plugin add <path|url>"), "{panel}");
    }

    /// One thing, one name: copying a tool key / namespace shape does not
    /// get execution on your behalf — point out the install name, but make
    /// the user type that one spelling; a wrong name lists the installed
    /// plugins (the A they see is right before them).
    #[tokio::test]
    async fn plugin_remove_refuses_wrong_spelling_and_teaches_the_real_name() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let src = tmp.path().join("src-plugin");
        write_script_plugin(&src);
        state
            .plugin_add(&host, &NoConfirm, &src.to_string_lossy())
            .await;

        // Tool key shape → show the way, don't delete on their behalf.
        let msg = state.plugin_remove(&host, "plugin.my-tools.deploy").await;
        assert!(msg.contains("is not a plugin name"), "{msg}");
        assert!(msg.contains("/plugin remove my-tools"), "{msg}");
        assert!(
            host.registry()
                .read()
                .await
                .get("plugin.my-tools.deploy")
                .is_some(),
            "showing the way must not really delete: {msg}"
        );

        // Installed but a wrong name → list what is installed.
        let msg = state.plugin_remove(&host, "nope").await;
        assert!(msg.contains("`nope` is not installed"), "{msg}");
        assert!(msg.contains("installed: my-tools"), "{msg}");

        // The correct A → delete clean.
        let msg = state.plugin_remove(&host, "my-tools").await;
        assert!(msg.contains("✓ plugin `my-tools` removed"), "{msg}");
        assert!(state.registry_file.plugins.is_empty());
    }

    /// Wrong name with zero installs → say truthfully there is none, no
    /// "remove failed" system-failure tone.
    #[tokio::test]
    async fn plugin_remove_with_none_installed_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();

        let msg = state.plugin_remove(&host, "nope").await;
        assert!(msg.contains("no plugins installed"), "{msg}");
        assert!(!msg.contains("remove failed"), "{msg}");
    }

    /// Tier labels are English (no Chinese on the product surface).
    #[test]
    fn tier_labels_are_english() {
        assert_eq!(tier_label(ApprovalTier::Ask), "ask each time");
        assert_eq!(tier_label(ApprovalTier::AlwaysAllow), "always allow");
        assert_eq!(tier_label(ApprovalTier::Deny), "deny");
    }

    #[tokio::test]
    async fn toggle_off_then_on_roundtrips_through_table() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let (identity, tool) = build_script_tool(entry("hello"), tmp.path(), |_| None).unwrap();
        host.registry().write().await.register_arc(tool);

        let msg = state.toggle_tool(&host, &identity, false).await;
        assert!(msg.contains("off"), "{msg}");
        assert!(host.registry().read().await.get(&identity).is_none());

        let msg = state.toggle_tool(&host, &identity, true).await;
        assert!(msg.contains("on"), "{msg}");
        assert!(host.registry().read().await.get(&identity).is_some());
        assert!(state.disabled.is_empty());
    }

    #[tokio::test]
    async fn tools_panel_shows_source_namespace_tier_and_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let (identity, tool) = build_script_tool(entry("deploy"), tmp.path(), |_| None).unwrap();
        host.registry().write().await.register_arc(tool);

        let panel = state.render_tools_panel(&host).await;
        assert!(panel.contains("script.deploy"), "{panel}");
        assert!(panel.contains("1 on · 0 off"), "{panel}");
        state.toggle_tool(&host, &identity, false).await;
        let panel = state.render_tools_panel(&host).await;
        assert!(panel.contains("0 on · 1 off"), "{panel}");
        assert!(panel.contains("off  script.deploy"), "{panel}");
    }

    #[tokio::test]
    async fn paste_extracts_secret_and_never_writes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let paste = r#"{"mcpServers": {"demo": {"command": "npx", "args": ["-y", "demo-mcp"],
            "env": {"DEMO_TOKEN": "ghp_secretvalue123456789012"}}}}"#;

        let report = state
            .handle(
                &host,
                &NoConfirm,
                super::super::commands::ExternalCommand::ConnectPaste {
                    text: paste.to_string(),
                },
            )
            .await;
        assert!(report.contains("DEMO_TOKEN"), "{report}");
        assert!(report.contains("export DEMO_TOKEN"), "{report}");
        assert!(!report.contains("ghp_secretvalue123456789012"), "{report}");
        assert!(report.contains("next: export DEMO_TOKEN"), "{report}");
        // The copy must tell the truth: a running phimint cannot see an
        // export added afterwards — it only takes effect after a restart; and
        // the entry is already saved, so the startup merge connects it
        // automatically ("just type /connect again" is not allowed).
        assert!(report.contains("restart"), "{report}");
        assert!(report.contains("on startup"), "{report}");
        // env not set → do not connect.
        assert!(host.attached.lock().unwrap().is_empty());

        let on_disk = std::fs::read_to_string(tmp.path().join("connections.json")).unwrap();
        assert!(
            !on_disk.contains("ghp_secretvalue123456789012"),
            "{on_disk}"
        );
        assert!(on_disk.contains("${ENV:DEMO_TOKEN}"), "{on_disk}");
    }

    /// The missing-env copy must tell the truth: an export added afterwards
    /// cannot reach a running phimint; a restart is needed after fixing it.
    #[test]
    fn missing_env_report_tells_the_restart_truth() {
        let report = missing_env_report("notion", &["NOTION_TOKEN".to_string()]);
        assert!(report.contains("missing NOTION_TOKEN"), "{report}");
        assert!(
            report.contains("export NOTION_TOKEN=\"<paste value>\""),
            "{report}"
        );
        assert!(report.contains("shell that launches phimint"), "{report}");
        assert!(report.contains("restart"), "{report}");
        assert!(report.contains("`/connect notion` again"), "{report}");
    }

    /// 0 tools must not show install success: a warning, not a ✓ (the kind
    /// of empty connection seen when the Taobao MCP session is missing).
    #[test]
    fn connected_report_zero_tools_is_a_warning_not_a_checkmark() {
        let zero = connected_report("`taobao`", "taobao", 0);
        assert!(zero.starts_with('⚠'), "{zero}");
        assert!(!zero.contains('✓'), "{zero}");
        assert!(zero.contains("0 tools"), "{zero}");
        let ok = connected_report("`taobao`", "taobao", 12);
        assert!(ok.starts_with('✓'), "{ok}");
        assert!(ok.contains("12 tools available (mcp.taobao.*)"), "{ok}");
    }

    /// The startup merge must not be silent: an enabled connection with 0
    /// tools → a transcript ⚠ (same criterion as `/connect`); a disabled
    /// connection is not disturbed and not attached.
    #[tokio::test]
    async fn startup_merge_surfaces_zero_tool_connection_as_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        // FakeHost attach is always Ok but drops no `mcp.*` tools = answered
        // but came back empty-handed.
        state.connections.push(ConnectionEntry {
            name: "taobao".into(),
            enabled: true,
            transport: connections::TransportSpec::Http {
                url: "http://localhost:3654/mcp".into(),
                headers: Default::default(),
            },
        });
        state.connections.push(ConnectionEntry {
            name: "off".into(),
            enabled: false,
            transport: connections::TransportSpec::Http {
                url: "http://localhost:1/mcp".into(),
                headers: Default::default(),
            },
        });
        let host = FakeHost::new();
        let notices = state.start(&host).await;
        let warn = notices
            .iter()
            .find(|n| n.contains("`taobao`"))
            .unwrap_or_else(|| panic!("no taobao notice in {notices:?}"));
        assert!(warn.starts_with('⚠'), "{warn}");
        assert!(warn.contains("0 tools"), "{warn}");
        assert!(
            !notices.iter().any(|n| n.contains("`off`")),
            "disabled connection must stay silent: {notices:?}"
        );
        let attached = host.attached.lock().unwrap().clone();
        assert_eq!(attached, vec!["taobao".to_string()]);
    }

    #[tokio::test]
    async fn remove_script_tool_clears_table_and_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let (identity, tool) = build_script_tool(entry("deploy"), tmp.path(), |_| None).unwrap();
        host.registry().write().await.register_arc(tool);
        state.script_defs.push(entry("deploy"));
        state
            .ledger
            .set_tier(&identity, phi_agent::ApprovalTier::AlwaysAllow);

        let report = state.remove_script_tool(&host, "deploy").await;
        assert!(report.contains("removed"), "{report}");
        assert!(host.registry().read().await.get(&identity).is_none());
        assert!(state.script_defs.is_empty());
        assert_eq!(
            state.ledger.tier_of(&identity),
            phi_agent::ApprovalTier::Ask
        );
    }

    /// Whatever key the panel shows, that is the key remove takes; the
    /// success receipt uses the same shape (one A — copy and use). A typo →
    /// list the installed ones truthfully, no system-failure tone.
    #[tokio::test]
    async fn remove_script_tool_takes_the_displayed_key() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let host = FakeHost::new();
        let (identity, tool) = build_script_tool(entry("deploy"), tmp.path(), |_| None).unwrap();
        host.registry().write().await.register_arc(tool);
        state.script_defs.push(entry("deploy"));

        let report = state.remove_script_tool(&host, "script.deploy").await;
        assert!(
            report.contains("✓ tool `script.deploy` removed"),
            "{report}"
        );
        assert!(host.registry().read().await.get(&identity).is_none());
        assert!(state.script_defs.is_empty());

        let report = state.remove_script_tool(&host, "script.deploy").await;
        assert!(
            report.contains("`script.deploy` is not a global script tool"),
            "{report}"
        );
        assert!(report.contains("no script tools installed"), "{report}");
    }

    // ── agent-written tools (add_tool) ───────────────────────────────

    #[tokio::test]
    async fn agent_add_tool_writes_registers_after_full_facts_allow() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let registry = Arc::new(RwLock::new(ToolRegistry::default()));
        let confirm = RecordingConfirm::allow();

        let msg = state
            .add_tool_from_agent(&registry, &confirm, entry("hello"))
            .await;
        assert!(msg.contains("script.hello"), "{msg}");
        assert!(registry.read().await.get("script.hello").is_some());
        let saved = std::fs::read_to_string(tools_path(tmp.path())).unwrap();
        assert!(saved.contains("\"hello\""), "{saved}");
        // Full-picture confirmation: complete argv (with absolute path) +
        // timeout, not just names.
        let asked = confirm.seen.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].contains(&real_script().display().to_string()),
            "{}",
            asked[0]
        );
        assert!(asked[0].contains("timeout"), "{}", asked[0]);
        assert!(asked[0].contains("argv"), "{}", asked[0]);
    }

    #[tokio::test]
    async fn agent_add_tool_missing_script_refused_before_confirm() {
        // Hooking up a nonexistent script = broken the moment it's
        // installed — refuse at install time, no confirmation dialog.
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let registry = Arc::new(RwLock::new(ToolRegistry::default()));
        let e = ScriptToolEntry::new(
            "hello",
            "d",
            vec![tmp.path().join("no-such-script.sh").display().to_string()],
        )
        .unwrap();
        let confirm = RecordingConfirm::allow();

        let msg = state.add_tool_from_agent(&registry, &confirm, e).await;
        assert!(msg.contains("rejected"), "{msg}");
        assert!(msg.contains("does not exist"), "{msg}");
        assert!(
            confirm.seen.lock().unwrap().is_empty(),
            "a missing file must not pop a confirmation"
        );
        assert!(!tools_path(tmp.path()).exists());
    }

    #[tokio::test]
    async fn agent_add_tool_deny_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let registry = Arc::new(RwLock::new(ToolRegistry::default()));

        let msg = state
            .add_tool_from_agent(&registry, &DenyConfirm, entry("hello"))
            .await;
        assert!(msg.contains("declined"), "{msg}");
        assert!(registry.read().await.get("script.hello").is_none());
        assert!(
            !tools_path(tmp.path()).exists(),
            "a decline = not a single byte written"
        );
    }

    #[tokio::test]
    async fn agent_add_tool_literal_env_secret_refused_before_confirm() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let registry = Arc::new(RwLock::new(ToolRegistry::default()));
        let mut e = entry("hello");
        e.env.insert("TOKEN".into(), "sk-live-literal".into());
        let confirm = RecordingConfirm::allow();

        let msg = state.add_tool_from_agent(&registry, &confirm, e).await;
        assert!(msg.contains("rejected"), "{msg}");
        assert!(msg.contains("${ENV:VAR}"), "{msg}");
        assert!(
            confirm.seen.lock().unwrap().is_empty(),
            "an invalid entry must not pop a confirmation"
        );
        assert!(!tools_path(tmp.path()).exists());
    }

    #[tokio::test]
    async fn agent_add_tool_env_refs_pass_secret_law_but_missing_var_fails_first() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let registry = Arc::new(RwLock::new(ToolRegistry::default()));
        let mut e = entry("hello");
        e.env
            .insert("TOKEN".into(), "${ENV:PHIMINT_TEST_NO_SUCH_VAR_000}".into());
        let confirm = RecordingConfirm::allow();

        // The reference shape is legal (passes the secret law), but the
        // registration-time pre-resolution finds a missing variable → name
        // it, no dialog, nothing persisted (no half-done state left behind).
        let msg = state.add_tool_from_agent(&registry, &confirm, e).await;
        assert!(msg.contains("PHIMINT_TEST_NO_SUCH_VAR_000"), "{msg}");
        assert!(
            confirm.seen.lock().unwrap().is_empty(),
            "a missing variable must not pop a confirmation"
        );
        assert!(!tools_path(tmp.path()).exists());
        assert!(registry.read().await.get("script.hello").is_none());
    }

    #[tokio::test]
    async fn agent_add_tool_identical_retry_is_idempotent() {
        // Half of BUG 5: after a spurious timeout the model retries with the
        // same entry. Same name, same full picture = idempotent completion
        // (persist + register), not `already exists`, and no second
        // confirmation.
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        state.script_defs.push(entry("hello"));
        let registry = Arc::new(RwLock::new(ToolRegistry::default()));
        let confirm = RecordingConfirm::allow();

        let msg = state
            .add_tool_from_agent(&registry, &confirm, entry("hello"))
            .await;
        assert!(msg.contains("script.hello"), "{msg}");
        assert!(msg.contains("already mounted"), "{msg}");
        assert!(
            confirm.seen.lock().unwrap().is_empty(),
            "the same entry must not pop a confirmation again"
        );
        assert!(
            registry.read().await.get("script.hello").is_some(),
            "registration must be completed"
        );
        let saved = std::fs::read_to_string(tools_path(tmp.path())).unwrap();
        assert!(
            saved.contains("\"hello\""),
            "persistence must be completed: {saved}"
        );
    }

    #[tokio::test]
    async fn agent_add_tool_duplicate_refused_before_confirm() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path());
        let mut taken = entry("hello");
        taken.description = "someone else's tool".into();
        state.script_defs.push(taken);
        let registry = Arc::new(RwLock::new(ToolRegistry::default()));
        let confirm = RecordingConfirm::allow();

        let msg = state
            .add_tool_from_agent(&registry, &confirm, entry("hello"))
            .await;
        assert!(msg.contains("already exists"), "{msg}");
        // The conflict copy must name the occupying form and the command to
        // free the name (in the BUG 5 incident there was only "already
        // exists", with no clue who held it).
        assert!(msg.contains("script.hello"), "{msg}");
        assert!(msg.contains("/tools remove"), "{msg}");
        assert!(confirm.seen.lock().unwrap().is_empty());
        assert!(
            !tools_path(tmp.path()).exists(),
            "a duplicate name must not touch the file"
        );
    }
}
