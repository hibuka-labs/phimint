use super::*;

pub(super) fn tier_label(tier: ApprovalTier) -> &'static str {
    match tier {
        ApprovalTier::Ask => "ask each time",
        ApprovalTier::AlwaysAllow => "always allow",
        ApprovalTier::Deny => "deny",
    }
}

/// Group header and tool rows of the panel (`/tools` and `/plugin list` share
/// one layout: source column 28, key column 32, state column on/off, tier
/// column).
pub(super) fn push_panel_group(
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

pub(super) fn render_capability(cap: &ResolvedCapability) -> String {
    match cap {
        ResolvedCapability::FsRead { paths } => format!("fs:read {}", paths.join(" ")),
        ResolvedCapability::NetHttp { hosts } => format!("net:http {}", hosts.join(" ")),
        // An empty program list means unrestricted (phi-cap's own meaning) —
        // say so out loud rather than rendering a bare `process:spawn `.
        ResolvedCapability::ProcessSpawn { programs } if programs.is_empty() => {
            "process:spawn (unrestricted)".to_string()
        }
        ResolvedCapability::ProcessSpawn { programs } => {
            format!("process:spawn {}", programs.join(" "))
        }
        ResolvedCapability::DeviceInput { targets } => {
            format!("device:input {}", targets.join(" "))
        }
        ResolvedCapability::DeviceCapture { targets } => {
            format!("device:capture {}", targets.join(" "))
        }
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
pub(super) fn check_env_refs_only(
    env: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
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
pub(super) fn connected_report(subject: &str, name: &str, count: usize) -> String {
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
pub(super) fn missing_env_report(name: &str, missing: &[String]) -> String {
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
