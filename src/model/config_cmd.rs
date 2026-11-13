//! The `phimint init` and `phimint config show|set` commands: where they act
//! and what they say (notes/install-onboarding-design.md, decisions 2/3/6).
//!
//! The installer calls `phimint init --if-missing`; a user who prefers the
//! command line to an editor fills the key with `phimint config set api_key -`
//! (the secret comes from stdin — an argv value lands in shell history and
//! process listings). All three commands share one target rule
//! ([`command_target`]) and one write channel (`model::config_write`, the
//! member surgery) — a user's config is never regenerated under them, and the
//! key is never echoed raw.

use std::path::{Path, PathBuf};

use crate::model::bootstrap;
use crate::model::config::{ConfigError, ModelConfig, TierConfig};
use crate::model::config_write;
use crate::model::persist::PersistError;

/// The argv-exposure note for a secret that rode in on the command line.
/// Shell history and process listings both see it there; the stdin form of
/// the same command does not.
const ARGV_SECRET_NOTE: &str = "\n  note: an api_key on the command line lands in shell history \
                               and process listings; `phimint config set api_key -` reads it \
                               from stdin instead";

/// The top-level members `config set` may write. Everything else — `model`
/// (the /model pointer's slot), tier overrides, UI settings — is edited in
/// the file itself, where the comments can explain the layout.
pub const WRITABLE_MEMBERS: [&str; 4] = ["api_key", "base_url", "protocol", "main"];

/// Where the config commands read and write: the `--config` flag when given,
/// else the file the loader would read ([`ModelConfig::resolve_path`]: an
/// existing `LLM_CONFIG` or default location), else [`command_target_from`]'s
/// skeleton fallback. One rule for all three commands, so `config set` can
/// never create a second config beside an existing one.
pub fn command_target(cli_config: Option<&str>) -> PathBuf {
    command_target_from(
        cli_config.map(PathBuf::from),
        ModelConfig::resolve_path(),
        std::env::var("LLM_CONFIG").ok().as_deref(),
        dirs::home_dir(),
    )
}

/// [`command_target`] with the environment read outside — the pure half.
/// `LLM_CONFIG` is the config location even before the file exists (that is
/// what [`bootstrap::skeleton_target_from`] decides).
pub fn command_target_from(
    cli_config: Option<PathBuf>,
    existing: Option<PathBuf>,
    llm_config: Option<&str>,
    home: Option<PathBuf>,
) -> PathBuf {
    cli_config
        .or(existing)
        .unwrap_or_else(|| bootstrap::skeleton_target_from(llm_config, home))
}

/// `phimint init`: ensure the skeleton exists and say what to do next.
/// `if_missing` (the installer's mode) goes quiet when a config is already
/// there; the bare command reports the no-op. Never touches existing bytes.
pub fn init(target: &Path, if_missing: bool) -> Result<String, String> {
    if target.exists() {
        return Ok(if if_missing {
            String::new()
        } else {
            init_existing_message(target)
        });
    }
    let created = bootstrap::ensure_skeleton(target)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    if created {
        Ok(init_created_message(target))
    } else if if_missing {
        Ok(String::new())
    } else {
        Ok(init_existing_message(target))
    }
}

/// `phimint config show`: the resolved config with a masked key. A missing
/// config names `phimint init` as the fix; an unparseable one offers the
/// delete-to-regenerate hatch instead of a dead end.
pub fn show(target: &Path, env_key: Option<&str>) -> Result<String, String> {
    if !target.exists() {
        return Err(format!(
            "no config file found: {}\n  Run `phimint init` to create one.",
            target.display()
        ));
    }
    let cfg = ModelConfig::from_file(&target.to_path_buf())
        .map_err(|e| config_error_message(target, e))?;
    Ok(bootstrap::show_report(target, &cfg, env_key))
}

/// `phimint config set`: write one writable member through the member
/// surgery. A missing config is created first (fill-in-one-command path),
/// the confirmation masks an `api_key`, and the two ways a fresh edit can be
/// silently shadowed are named outright: `PHIMINT_API_KEY` overriding the
/// file, and a Full `main` tier whose own member wins at runtime. A plain
/// name over a Full `main` tier is refused outright — splicing it would drop
/// the tier's own members from the file while the command prints success.
/// `value_on_argv` marks a value that rode in on the command line — for an
/// `api_key` that is a leak worth pointing out.
pub fn set(
    target: &Path,
    key: &str,
    value: &str,
    env_key: Option<&str>,
    value_on_argv: bool,
) -> Result<String, String> {
    if !WRITABLE_MEMBERS.contains(&key) {
        return Err(format!(
            "cannot set `{key}` here: writable members are {}\n  (`model` is the /model picker's selection - edit the file for advanced layouts)",
            WRITABLE_MEMBERS.join(", ")
        ));
    }
    let mut created = false;
    if !target.exists() {
        created = bootstrap::ensure_skeleton(target)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    if let Some(refusal) = full_tier_refusal(target, key) {
        return Err(refusal);
    }
    config_write::set_member_str(target, key, value)
        .map_err(|e| persist_error_message(target, e))?;
    let mut out = String::new();
    if created {
        out.push_str(&format!("created {}\n", target.display()));
    }
    out.push_str(&set_line(target, key, value));
    if key == "api_key" && value_on_argv {
        out.push_str(ARGV_SECRET_NOTE);
    }
    if key == "api_key" && env_key.is_some() {
        out.push_str("\n  note: PHIMINT_API_KEY is set and overrides this file");
    }
    if let Some(shadow) = tier_shadow_note(target, key) {
        out.push_str(&shadow);
    }
    Ok(out)
}

/// Name the tier-precedence case: when a tier resolves `key` to something
/// other than the top-level member the user just edited, that edit is
/// shadowed at runtime (the one resolution rule in
/// [`ModelConfig::tier_config`]). Silence here would look like the edit did
/// not take; a note where the edit DID take would be a lie. The comparison is
/// against what actually resolves — declaration shapes (a name-only pointer,
/// the anchor chain) decide who wins (review findings
/// v11/adversarial-native#8, fix-check#1).
fn tier_shadow_note(target: &Path, key: &str) -> Option<String> {
    let cfg = ModelConfig::from_file(&target.to_path_buf()).ok()?;
    let top = match key {
        "base_url" => cfg.base_url.clone(),
        "api_key" => cfg.api_key.clone(),
        "protocol" => cfg.protocol.clone(),
        _ => return None,
    }?;
    let field = |t: (String, String, String, String)| match key {
        "base_url" => t.1,
        "api_key" => t.2,
        _ => t.3,
    };
    let mut notes = String::new();
    let mut seen: Vec<String> = Vec::new();
    let mut note = |label: &str, resolved: String| {
        if resolved != top && !seen.contains(&resolved) {
            seen.push(resolved);
            notes.push_str(&format!(
                "\n  note: at runtime {label}'s {key} wins over this top-level member \
                 (edit the tier itself to change it)"
            ));
        }
    };
    for tier in ["main", "lite", "advanced"] {
        if let Some(t) = cfg.tier_config(tier) {
            let label = format!("the `{tier}` tier");
            note(&label, field(t));
        }
    }
    // The configured `main`'s own member is what "Clear selection" restores:
    // it wins over the top-level member the moment the model pointer is
    // cleared (review finding fix-check2#3).
    note(
        "the `main` tier (once the selection is cleared)",
        field(cfg.configured_main()),
    );
    (!notes.is_empty()).then_some(notes)
}

/// `main` is the one writable member whose value may be a whole tier block.
/// A plain name spliced over a Full tier deletes the tier's own
/// api_key/base_url/protocol from disk while the command prints success —
/// silent data loss. Refuse, name the members that would go, and point at
/// the file where the tier is edited.
fn full_tier_refusal(target: &Path, key: &str) -> Option<String> {
    if key != "main" {
        return None;
    }
    let cfg = ModelConfig::from_file(&target.to_path_buf()).ok()?;
    let TierConfig::Full {
        base_url,
        api_key,
        protocol,
        ..
    } = &cfg.main
    else {
        return None;
    };
    let mut own = vec!["model"];
    if api_key.is_some() {
        own.push("api_key");
    }
    if base_url.is_some() {
        own.push("base_url");
    }
    if protocol.is_some() {
        own.push("protocol");
    }
    Some(format!(
        "cannot set `main` to a plain name: it holds a full tier block (own {}) and a name \
         would drop those members from the file\n  edit the `main` block in {} instead",
        own.join(", "),
        target.display()
    ))
}

fn init_created_message(path: &Path) -> String {
    format!(
        "created {} (0600)\n\n  Next: open that file and fill in api_key, then run phimint.\n  Other providers: edit base_url / protocol / main (see the comment block at the top of the file).",
        path.display()
    )
}

fn init_existing_message(path: &Path) -> String {
    format!(
        "config already exists: {}\n  Left untouched. To inspect it: phimint config show",
        path.display()
    )
}

fn set_line(path: &Path, key: &str, value: &str) -> String {
    let shown = if key == "api_key" {
        bootstrap::mask_key(value)
    } else {
        value.to_string()
    };
    format!("{key} = {shown}  (saved to {})", path.display())
}

fn config_error_message(path: &Path, e: ConfigError) -> String {
    match e {
        ConfigError::ParseError(reason) => bootstrap::parse_error_message(path, &reason),
        other => format!("cannot read {}: {other}", path.display()),
    }
}

fn persist_error_message(path: &Path, e: PersistError) -> String {
    match e {
        PersistError::Parse(reason) => bootstrap::parse_error_message(path, &reason),
        other => format!("cannot write {}: {other}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::bootstrap::CONFIG_TEMPLATE;

    /// protects: the config commands act on the file the user named
    /// (`--config`), else the file the loader would read, else the skeleton
    /// default — so `config set` never creates a second config beside an
    /// existing one. fails_when: an existing ~/.config/phimint/config.json is
    /// ignored and a fresh ~/.phimint/config.json appears next to it.
    /// no_claim: what happens when several locations hold files
    /// (resolve_path decides at load).
    #[test]
    fn the_command_target_is_the_flag_then_the_loaded_file_then_the_default() {
        assert_eq!(
            command_target_from(
                Some(PathBuf::from("/x.json")),
                Some(PathBuf::from("/e.json")),
                Some("/l.json"),
                Some(PathBuf::from("/h")),
            ),
            PathBuf::from("/x.json"),
            "the --config flag wins"
        );
        assert_eq!(
            command_target_from(
                None,
                Some(PathBuf::from("/e.json")),
                Some("/l.json"),
                Some(PathBuf::from("/h")),
            ),
            PathBuf::from("/e.json"),
            "the file the loader reads is the target"
        );
        assert_eq!(
            command_target_from(None, None, Some("/l.json"), Some(PathBuf::from("/h"))),
            PathBuf::from("/l.json"),
            "LLM_CONFIG is the location even before the file exists"
        );
        assert_eq!(
            command_target_from(None, None, None, Some(PathBuf::from("/h"))),
            PathBuf::from("/h/.phimint/config.json"),
            "the default location"
        );
    }

    /// protects: `init` writes the skeleton exactly once and never touches a
    /// config the user already has; `--if-missing` (the installer's mode) goes
    /// quiet when one exists instead of nagging on a configured machine.
    /// fails_when: a reinstall clobbers the user's config, or the message
    /// forgets to say what to fill in. no_claim: recovery of a corrupted file
    /// (delete + init is the path).
    #[test]
    fn the_init_command_writes_the_skeleton_once_and_respects_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");

        let msg = init(&target, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            CONFIG_TEMPLATE,
            "the skeleton lands byte for byte"
        );
        assert!(
            msg.contains(target.to_str().unwrap()),
            "names the file: {msg}"
        );
        assert!(msg.contains("api_key"), "says what to fill: {msg}");

        // An existing config: untouched, and the bare command says so.
        std::fs::write(&target, "{\"api_key\": \"sk-mine\"}").unwrap();
        let msg = init(&target, false).unwrap();
        assert!(msg.contains("already exists"), "reports the no-op: {msg}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{\"api_key\": \"sk-mine\"}",
            "the user's bytes survive"
        );

        // --if-missing on a configured machine: silent no-op.
        let msg = init(&target, true).unwrap();
        assert!(msg.is_empty(), "the installer mode stays quiet: {msg}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{\"api_key\": \"sk-mine\"}",
            "still untouched"
        );

        // --if-missing on an empty slot still creates (that is the install path).
        let fresh = dir.path().join("fresh.json");
        let msg = init(&fresh, true).unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), CONFIG_TEMPLATE);
        assert!(!msg.is_empty(), "a created file is still reported");
    }

    /// protects: `config show` reports the resolved endpoint with a masked
    /// key and names the env source; a missing config is a loud error pointing
    /// at `phimint init`, and an unparseable one offers the delete hatch.
    /// fails_when: show on a fresh machine prints success, or the raw key
    /// leaks into the report. no_claim: config discovery (resolve_path pins
    /// that).
    #[test]
    fn the_show_command_reports_the_config_and_the_fix_when_none_exists() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");

        let err = show(&target, None).unwrap_err();
        assert!(err.contains("phimint init"), "the fix is init: {err}");

        std::fs::write(
            &target,
            "{\"main\": \"m\", \"base_url\": \"u\", \"api_key\": \"sk-secret-123456\", \"protocol\": \"openai\"}",
        )
        .unwrap();
        let msg = show(&target, None).unwrap();
        assert!(msg.contains("sk-s…456"), "masked: {msg}");
        assert!(!msg.contains("sk-secret-123456"), "never raw: {msg}");

        let msg = show(&target, Some("sk-env-key-12345")).unwrap();
        assert!(msg.contains("PHIMINT_API_KEY"), "names the source: {msg}");
        assert!(
            !msg.contains("sk-env-key-12345"),
            "the env key is masked too: {msg}"
        );

        // Unparseable: the delete-to-regenerate hatch, never a dead end.
        std::fs::write(&target, "{ not json ").unwrap();
        let err = show(&target, None).unwrap_err();
        assert!(err.contains("delete"), "offers the hatch: {err}");
    }

    /// protects: `config set` fills a fresh machine in one command — a missing
    /// config is created first, then the member lands; the confirmation masks
    /// an api_key and names the env override when one would silently win at
    /// runtime. fails_when: set on a fresh machine fails with "no config
    /// file", or the confirmation prints the secret. no_claim: values with
    /// leading dashes (the shell must quote them).
    #[test]
    fn the_set_command_fills_a_fresh_machine_and_masks_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");

        let msg = set(&target, "api_key", "sk-live-secret-001", None, true).unwrap();
        let raw = std::fs::read_to_string(&target).unwrap();
        assert!(
            raw.contains("\"api_key\": \"sk-live-secret-001\""),
            "the key landed in the skeleton: {raw}"
        );
        assert!(
            msg.contains("sk-l…001"),
            "the confirmation is masked: {msg}"
        );
        assert!(!msg.contains("sk-live-secret-001"), "never raw: {msg}");

        // Setting the key while PHIMINT_API_KEY would override it: say so,
        // or the user's edit "takes effect" and nothing changes.
        let msg = set(
            &target,
            "api_key",
            "sk-newer-key-000222",
            Some("sk-env"),
            true,
        )
        .unwrap();
        assert!(
            msg.contains("PHIMINT_API_KEY"),
            "says why the file may not win: {msg}"
        );
        assert!(msg.contains("sk-n…222"), "still masked: {msg}");

        // A non-secret member echoes plainly, with no key-override noise.
        let msg = set(
            &target,
            "base_url",
            "https://e.example",
            Some("sk-env"),
            true,
        )
        .unwrap();
        assert!(
            msg.contains("https://e.example"),
            "non-secret values echo plainly: {msg}"
        );
        assert!(
            !msg.contains("PHIMINT_API_KEY"),
            "the key-override note is for key writes only: {msg}"
        );
    }

    /// protects: only the four writable members ever reach the file — `model`
    /// (the /model pointer's slot), `lite`, anything else is refused with the
    /// file untouched, and each writable member does land. fails_when:
    /// `config set model x` writes the selection pointer behind /model's back.
    /// no_claim: tier Full overrides (edit the file).
    #[test]
    fn the_set_command_refuses_members_outside_the_whitelist() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        let original = "{\n  \"main\": \"m\",\n  \"api_key\": \"sk\"\n}\n";
        std::fs::write(&target, original).unwrap();

        for key in ["model", "lite", "ui", "bogus"] {
            let err = set(&target, key, "v", None, true).unwrap_err();
            assert!(
                err.contains("api_key") && err.contains("main"),
                "the error lists the writable members: {err}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            original,
            "the file is untouched"
        );

        // The whitelist itself: every member in it lands.
        for key in WRITABLE_MEMBERS {
            set(&target, key, "v", None, true).unwrap();
        }
        let raw = std::fs::read_to_string(&target).unwrap();
        for key in WRITABLE_MEMBERS {
            assert!(
                raw.contains(&format!("\"{key}\": \"v\"")),
                "{key} landed: {raw}"
            );
        }
    }

    /// protects: a config that does not parse is refused with the
    /// delete-to-regenerate hatch and stays byte-identical on disk — `set`
    /// never "fixes" the user's file. fails_when: a syntax error leads to a
    /// rewrite or a dead-end error. no_claim: parser error quality.
    #[test]
    fn a_set_on_an_unparseable_file_is_refused_with_the_escape_hatch() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        std::fs::write(&target, "{ not json ").unwrap();

        let err = set(&target, "api_key", "sk", None, true).unwrap_err();
        assert!(err.contains("delete"), "offers the hatch: {err}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{ not json ",
            "the file survives untouched"
        );
    }

    /// protects: the two ways a fresh edit can mislead are named in the
    /// confirmation - a secret on argv points at the stdin route, and a Full
    /// `main` tier carrying its own member is named as the value that wins at
    /// runtime. fails_when: either note is missing and the user believes the
    /// edit is both private and effective. no_claim: tier precedence itself
    /// (`ModelConfig::tier_config` pins that).
    #[test]
    fn the_set_command_names_the_argv_leak_and_the_tier_shadow() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");

        // Secret on argv: point at stdin. Not on argv: no noise.
        let msg = set(&target, "api_key", "sk-argv-000111", None, true).unwrap();
        assert!(msg.contains("stdin"), "names the safer route: {msg}");
        let msg = set(&target, "api_key", "sk-stdin-000222", None, false).unwrap();
        assert!(
            !msg.contains("shell history"),
            "stdin is already safe: {msg}"
        );

        // A Full main tier carrying its own key shadows the top-level member.
        std::fs::write(
            &target,
            "{\n  \"main\": {\"model\": \"m\", \"api_key\": \"sk-tier\"},\n  \"api_key\": \"sk-top\"\n}\n",
        )
        .unwrap();
        let msg = set(&target, "api_key", "sk-top-2", None, false).unwrap();
        assert!(
            msg.contains("wins over this top-level member"),
            "names the tier shadow: {msg}"
        );

        // A Simple main tier inherits - no note.
        std::fs::write(
            &target,
            "{\n  \"main\": \"m\",\n  \"api_key\": \"sk-top\"\n}\n",
        )
        .unwrap();
        let msg = set(&target, "api_key", "sk-top-3", None, false).unwrap();
        assert!(
            !msg.contains("wins over this top-level member"),
            "nothing shadows here: {msg}"
        );

        // A Full `lite` tier shadows for its tier just like `main` does.
        std::fs::write(
            &target,
            "{\n  \"main\": \"m\",\n  \"lite\": {\"model\": \"l\", \"api_key\": \"sk-lite\"},\n  \"api_key\": \"sk-top\"\n}\n",
        )
        .unwrap();
        let msg = set(&target, "api_key", "sk-top-4", None, false).unwrap();
        assert!(
            msg.contains("`lite` tier's api_key wins over"),
            "names the lite tier shadow too: {msg}"
        );

        // A name-only pointer resolves through the chain — but the configured
        // `main`'s own key returns on "Clear selection" and must be named
        // (review findings fix-check#1, fix-check2#3).
        std::fs::write(
            &target,
            "{\n  \"main\": {\"model\": \"m\", \"api_key\": \"sk-tier\"},\n  \"model\": \"p\",\n  \"api_key\": \"sk-top\"\n}\n",
        )
        .unwrap();
        let msg = set(&target, "api_key", "sk-top-5", None, false).unwrap();
        assert!(
            msg.contains("once the selection is cleared"),
            "the configured main's key returns on clear — name it: {msg}"
        );
    }

    /// protects: a plain name is never spliced over a Full `main` tier — the
    /// command refuses, names the tier members that would have been dropped
    /// and leaves the file byte-identical, instead of printing success after
    /// deleting the tier's own api_key/base_url/protocol. fails_when: `set
    /// main gpt-5.4-mini` on a tier config silently flattens the block.
    /// no_claim: replacing the tier wholesale (edit the file).
    #[test]
    fn a_plain_name_over_a_full_main_tier_is_refused_with_the_members_named() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        let original = "{\n  \"main\": {\"model\": \"m\", \"api_key\": \"sk-tier\", \"base_url\": \"https://t.example\"}\n}\n";
        std::fs::write(&target, original).unwrap();

        let err = set(&target, "main", "gpt-5.4-mini", None, true).unwrap_err();
        assert!(err.contains("full tier block"), "names the shape: {err}");
        assert!(err.contains("api_key"), "names api_key: {err}");
        assert!(err.contains("base_url"), "names base_url: {err}");
        assert!(
            err.contains("edit the `main` block"),
            "says where to edit instead: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            original,
            "the tier survives byte for byte"
        );

        // A Simple main is not a tier block: the plain name lands (that is
        // the ordinary `config set main <model>` path).
        std::fs::write(&target, "{\n  \"main\": \"m\"\n}\n").unwrap();
        set(&target, "main", "gpt-5.4-mini", None, true).unwrap();
        let raw = std::fs::read_to_string(&target).unwrap();
        assert!(raw.contains("\"main\": \"gpt-5.4-mini\""), "lands: {raw}");
    }
}
