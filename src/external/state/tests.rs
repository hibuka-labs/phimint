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

    /// Register `count` stub `mcp.<name>.*` tools so the startup merge's
    /// `count_prefix` sees a non-empty exposed set (success-receipt path
    /// needs N>0; zero tools is the ⚠ branch).
    async fn expose_mcp_tools(&self, name: &str, count: usize) {
        let mut guard = self.registry.write().await;
        for i in 0..count {
            // `Tool::name` is `&'static str` — leak the dynamic name
            // (tests only).
            let leaked: &'static str = Box::leak(format!("mcp.{name}.tool-{i}").into_boxed_str());
            guard.register_arc(Arc::new(StubMcpTool(leaked)));
        }
    }
}

/// Registry-visible no-op behind [`FakeHost::expose_mcp_tools`].
struct StubMcpTool(&'static str);

#[async_trait]
impl phi_agent::Tool for StubMcpTool {
    fn name(&self) -> &'static str {
        self.0
    }
    fn description(&self) -> &'static str {
        "stub mcp tool"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn call(
        &self,
        _args: &serde_json::Value,
        _ctx: &phi_agent::ToolContext,
    ) -> phi_agent::AgentResult<Vec<phi_agent::Content>> {
        Ok(vec![])
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
            let path =
                std::env::temp_dir().join(format!("phimint-test-script-{}.sh", std::process::id()));
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
                ResolvedCapability::ProcessSpawn {
                    programs: vec!["python3".into()],
                },
                ResolvedCapability::DeviceInput {
                    targets: vec!["keyboard".into()],
                },
                ResolvedCapability::DeviceCapture {
                    targets: vec!["screen".into()],
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
    assert!(text.contains("process:spawn python3"), "{text}");
    assert!(text.contains("device:input keyboard"), "{text}");
    assert!(text.contains("device:capture screen"), "{text}");
    assert!(text.contains("timeout: 30s"), "{text}");
}

#[test]
fn empty_program_allow_list_is_rendered_as_unrestricted() {
    // A bare `process:spawn ` would read like a pinned (empty) allow-list;
    // the declaration means the opposite.
    assert_eq!(
        render_capability(&ResolvedCapability::ProcessSpawn { programs: vec![] }),
        "process:spawn (unrestricted)"
    );
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

/// The startup merge's success path is a receipt, not silence (P3
/// acceptance finding ①): a connection exposing N>0 tools must show the
/// same `✓ … connected` line the `/connect` command shows.
#[tokio::test]
async fn startup_merge_reports_successful_connect() {
    let tmp = tempfile::tempdir().unwrap();
    let mut state = test_state(tmp.path());
    state.connections.push(ConnectionEntry {
        name: "taobao".into(),
        enabled: true,
        transport: connections::TransportSpec::Http {
            url: "http://localhost:3654/mcp".into(),
            headers: Default::default(),
        },
    });
    let host = FakeHost::new();
    host.expose_mcp_tools("taobao", 2).await;
    let notices = state.start(&host).await;
    let receipt = notices
        .iter()
        .find(|n| n.contains("`taobao`"))
        .unwrap_or_else(|| panic!("no taobao notice in {notices:?}"));
    assert!(receipt.starts_with('✓'), "{receipt}");
    assert!(
        receipt.contains("2 tools available (mcp.taobao.*)"),
        "{receipt}"
    );
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
