//! First-run bootstrap: the config skeleton, the "not configured yet"
//! messages, and the key-presence rules behind them.
//!
//! Design: notes/install-onboarding-design.md (decision 1-3, 6). There is no
//! setup wizard — when phimint has no usable key it explains what is missing,
//! where the file lives and how to fix it, then gets out of the way. The
//! skeleton template below is the single source for the generated config
//! (`phimint init` and the first-run path both write it); the installer calls
//! `phimint init --if-missing` so every channel generates the same file.

use std::path::{Path, PathBuf};

use crate::model::config::{ConfigError, ModelConfig};

/// The generated config skeleton: a valid, runnable JSON5 config whose
/// `api_key` is empty — a fresh install always lands in the first-run
/// branch, and the user's only edit is the key line. The provider table in
/// the comment block is the onboarding surface; base_url/protocol/main
/// defaults must match `config.json.example` (pinned by test).
pub const CONFIG_TEMPLATE: &str = r#"{
  // phimint config (JSON5 - comments and trailing commas are fine)
  // Required: api_key. Other providers: copy the line you need and change
  // base_url / protocol / main together:
  //   OpenAI     https://api.openai.com/v1                          gpt-5.4-mini
  //   Anthropic  https://api.anthropic.com                          claude-opus-4
  //   DeepSeek   https://api.deepseek.com/v1                        deepseek-chat
  //   Qwen       https://dashscope.aliyuncs.com/compatible-mode/v1  qwen-plus
  //   Ollama     http://localhost:11434/v1                          (local model)
  // Any OpenAI-compatible endpoint works: change base_url / protocol / main.
  "base_url": "https://api.openai.com/v1",
  "protocol": "openai",
  "main": "gpt-5.4-mini",
  "api_key": ""
}
"#;

/// Placeholders our own docs show — never counts as a real key.
const PLACEHOLDER_KEYS: [&str; 2] = ["sk-xxx", "sk-your-key"];

/// Whether `key` is a usable API key: non-blank and not one of our doc
/// placeholders (a README-copied `sk-xxx` must land in the first-run branch,
/// not die on a 401 later).
pub fn key_is_usable(key: &str) -> bool {
    let k = key.trim();
    !k.is_empty() && !PLACEHOLDER_KEYS.contains(&k)
}

/// The key in force: `cli` > `env` (PHIMINT_API_KEY) > `file` — the same
/// precedence the provider is built with. `None` when nothing provides one.
pub fn effective_api_key<'a>(
    cli: Option<&'a str>,
    env: Option<&'a str>,
    file: Option<&'a str>,
) -> Option<&'a str> {
    cli.or(env).or(file)
}

/// Whether the session has a usable key at all — the first-run gate. A
/// set-but-blank override is still "not configured".
pub fn api_key_ready(cli: Option<&str>, env: Option<&str>, file: Option<&str>) -> bool {
    effective_api_key(cli, env, file).is_some_and(key_is_usable)
}

/// Where the running key came from, for the runtime 401/403 notice: the CLI
/// flag or `PHIMINT_API_KEY` only when that value is the key the provider
/// was built with, `None` when the file's own key runs (editing it is the
/// fix). A pinned tier key wins over both overrides at runtime, so it must
/// not be credited to them.
pub fn key_source_in_force(
    cli: Option<&str>,
    env: Option<&str>,
    running: &str,
) -> Option<&'static str> {
    if cli == Some(running) {
        Some("--api-key")
    } else if env == Some(running) {
        Some("PHIMINT_API_KEY")
    } else {
        None
    }
}

/// The file's share of the key in force: the effective main tier's resolved
/// key. A Full tier — or a `model` selection pointer — that pins `api_key`
/// wins over the top-level default, exactly as the provider resolves it, so
/// a config whose key lives only in its tier objects counts as configured.
fn file_key_in_force(cfg: &ModelConfig) -> String {
    let (_, _, key, _) = cfg.tier_config("main").unwrap_or_default();
    key
}

/// The ONE key resolution — the first-run gate's readiness and the provider
/// build both read this value: `cli` > `env` > the effective main tier's
/// key (a Full tier's pin beats the top-level member, blank or not — a
/// set-but-blank value blocks, it does not fall through, exactly as a blank
/// `PHIMINT_API_KEY` already blocks the file key). An explicit override
/// therefore beats a pin, so `--api-key` is never silently eaten and a
/// blank pin cannot pass the gate on an override's readiness and then reach
/// `create_provider` as `""`.
pub fn key_in_force(cfg: &ModelConfig, cli: Option<&str>, env: Option<&str>) -> String {
    let file_side = file_key_in_force(cfg);
    effective_api_key(cli, env, Some(file_side.as_str()))
        .unwrap_or("")
        .to_string()
}

/// The key for display: first 4 + `…` + last 3 characters. Short keys hide
/// entirely; empty reads "(not set)". Char-based (a CJK key must not slice
/// at a non-boundary).
pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return "(not set)".to_string();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 8 {
        return "••••".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 3..].iter().collect();
    format!("{head}…{tail}")
}

/// Where init/first-run writes the skeleton when no config exists anywhere:
/// `LLM_CONFIG` when set (that IS the config location), else
/// `~/.phimint/config.json`. Pure half for testing — [`skeleton_target`]
/// reads the real environment.
pub fn skeleton_target_from(llm_config: Option<&str>, home: Option<PathBuf>) -> PathBuf {
    if let Some(p) = llm_config {
        return PathBuf::from(p);
    }
    home.unwrap_or_else(|| PathBuf::from("."))
        .join(".phimint")
        .join("config.json")
}

/// [`skeleton_target_from`] against the live environment.
pub fn skeleton_target() -> PathBuf {
    skeleton_target_from(
        std::env::var("LLM_CONFIG").ok().as_deref(),
        dirs::home_dir(),
    )
}

/// Write the skeleton at `path` unless a file is already there. Returns
/// whether it created the file. Never overwrites (create-new semantics, so
/// a racing writer cannot clobber either); the file lands 0600 — it is
/// about to hold an API key.
pub fn ensure_skeleton(path: &Path) -> std::io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    std::io::Write::write_all(&mut file, CONFIG_TEMPLATE.as_bytes())?;
    Ok(true)
}

/// The first-run message (decision 2): what is missing, where the file is,
/// a fill-in snippet, and the exact remedy. Product strings are English.
pub fn missing_key_message(path: &Path) -> String {
    format!(
        "phimint is not configured yet.\n\
         \n\
         \x20 config file: {path}\n\
         \x20 missing:     api_key\n\
         \n\
         \x20 Open that file and fill in api_key (JSON5 - comments and trailing commas are fine):\n\
         \n\
         \x20   {{\n\
         \x20     \"base_url\": \"https://api.openai.com/v1\",\n\
         \x20     \"protocol\": \"openai\",\n\
         \x20     \"main\": \"gpt-5.4-mini\",\n\
         \x20     \"api_key\": \"sk-your-key\"\n\
         \x20   }}\n\
         \n\
         \x20 Other providers: copy base_url / main from the comment block at the top of the file.\n\
         \x20 Save it, then run phimint again.",
        path = path.display()
    )
}

/// The parse-error message: the file, the reason verbatim, and the
/// delete-to-regenerate escape hatch.
pub fn parse_error_message(path: &Path, reason: &str) -> String {
    format!(
        "phimint could not read its config file: {path}\n\
         \x20 reason: {reason}\n\
         \x20 Fix it - or delete the file and phimint will regenerate a template - then run phimint again.",
        path = path.display()
    )
}

/// `phimint config show`: the resolved main endpoint with a masked key —
/// never the raw secret (users paste this into issues). An active `model`
/// selection pointer is named, so the report never claims `main` is in
/// force when a pick has displaced it. `env_key` names PHIMINT_API_KEY as
/// the source when one is overriding the file.
pub fn show_report(path: &Path, cfg: &ModelConfig, env_key: Option<&str>) -> String {
    let (model, url, file_key, proto) = cfg.tier_config("main").unwrap_or_default();
    let (key, source) = match env_key {
        Some(k) => (k, "  (from PHIMINT_API_KEY)"),
        None => (file_key.as_str(), ""),
    };
    let mut out = format!(
        "config file: {}\n  main:     {}\n  base_url: {}\n  protocol: {}\n  api_key:  {}{}\n",
        path.display(),
        model,
        url,
        proto,
        mask_key(key),
        source
    );
    if let Some(pointer) = &cfg.model {
        out.push_str(&format!(
            "  pointer:  {} (overrides main)\n",
            pointer.model()
        ));
    }
    out
}

/// What the boot gate decided: the config boot runs with, or the message to
/// print before a non-zero exit.
pub enum Boot {
    /// Keep booting with this config.
    Run(Box<ModelConfig>),
    /// Print this and exit non-zero. The skeleton is ensured wherever the
    /// message says the file lives (the no-key branch writes it).
    Stop(String),
}

/// The boot state machine (decision 1 + the design's flow chart), in one
/// place: `load` is the loader's outcome for the target file, `cli_key` /
/// `env_key` are the overrides that never live in the file. A usable key in
/// force (cli > env > the file's effective main tier) runs — the env key is
/// the CI path and is never written to disk; a missing key stops with the
/// fill-in message; a broken config stops with the delete-to-regenerate
/// hatch whatever key exists.
pub fn gate(
    load: Result<Option<ModelConfig>, ConfigError>,
    cli_key: Option<&str>,
    env_key: Option<&str>,
    target: &Path,
) -> Boot {
    let file_cfg = match load {
        Ok(cfg) => cfg,
        // A named `--config` path that simply is not there is the same
        // first-run state as "no file anywhere", not a load failure.
        Err(ConfigError::IoError(_)) if !target.exists() => None,
        Err(ConfigError::ParseError(reason)) => {
            return Boot::Stop(parse_error_message(target, &reason));
        }
        Err(e) => return Boot::Stop(format!("cannot read {}: {e}", target.display())),
    };
    match file_cfg {
        Some(cfg) => {
            // The gate and the provider build read the same resolved value
            // ([`key_in_force`]): readiness is judged on the key the
            // provider will actually be built with, so the two can never
            // disagree about what "configured" means.
            let key = key_in_force(&cfg, cli_key, env_key);
            if key_is_usable(&key) {
                Boot::Run(Box::new(cfg))
            } else {
                Boot::Stop(missing_key_message(target))
            }
        }
        None => {
            if let Err(e) = ensure_skeleton(target) {
                return Boot::Stop(format!("cannot write {}: {e}", target.display()));
            }
            if api_key_ready(cli_key, env_key, None) {
                // Not first-run after all (decision 1: an env/CLI key means
                // configured) — run the documented defaults; the skeleton
                // above gives `config set` and `/model` picks a home.
                match json5::from_str(CONFIG_TEMPLATE) {
                    Ok(cfg) => Boot::Run(Box::new(cfg)),
                    Err(e) => Boot::Stop(format!("phimint's config template is broken: {e}")),
                }
            } else {
                Boot::Stop(missing_key_message(target))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::{ConfigError, ModelConfig};

    /// protects: only a real key counts as configured — a blank, whitespace
    /// or README placeholder must fall through to the first-run message.
    /// fails_when: `sk-xxx` (the README's example literal) boots phimint as
    /// if configured and dies on a 401 instead. no_claim: what a provider
    /// accepts as a key format.
    #[test]
    fn a_blank_or_placeholder_key_is_not_a_usable_key() {
        assert!(!key_is_usable(""), "empty string is not a key");
        assert!(!key_is_usable("   "), "whitespace is not a key");
        assert!(
            !key_is_usable("sk-xxx"),
            "the README placeholder is not a key"
        );
        assert!(
            !key_is_usable("sk-your-key"),
            "the message's example is not a key"
        );
        assert!(key_is_usable("sk-real-key-001"));
    }

    /// protects: the override chain is cli > env > file — first one present
    /// wins, exactly the precedence the provider is built with. fails_when:
    /// an env var silently loses to the file (CI key lands on a stale file
    /// key). no_claim: what happens when the winner itself is blank —
    /// `api_key_ready` judges that, not this.
    #[test]
    fn the_effective_key_follows_cli_then_env_then_file() {
        assert_eq!(
            effective_api_key(Some("cli"), Some("env"), Some("file")),
            Some("cli")
        );
        assert_eq!(
            effective_api_key(None, Some("env"), Some("file")),
            Some("env")
        );
        assert_eq!(effective_api_key(None, None, Some("file")), Some("file"));
        assert_eq!(effective_api_key(None, None, None), None);
    }

    /// protects: readiness is the usability check applied to the winning key
    /// — a set-but-empty PHIMINT_API_KEY is still "not configured". fails_when:
    /// env presence alone (even empty) counts as ready. no_claim: file
    /// permissions or parse state (separate branches).
    #[test]
    fn readiness_means_the_winning_key_is_usable() {
        assert!(!api_key_ready(None, None, None), "no key anywhere");
        assert!(
            !api_key_ready(None, Some(""), Some("sk-file")),
            "an empty env override blocks the file key"
        );
        assert!(
            !api_key_ready(None, None, Some("sk-xxx")),
            "placeholder file key"
        );
        assert!(api_key_ready(None, None, Some("sk-file-001")), "file key");
        assert!(api_key_ready(None, Some("sk-env-001"), None), "env key");
        assert!(api_key_ready(Some("sk-cli-001"), None, None), "cli key");
    }

    /// protects: the runtime notice credits an override only when that
    /// override is the key the provider was built with — the file's own key
    /// (a pinned tier key included) reads as "from the file", no source
    /// line. fails_when: the notice says "(from PHIMINT_API_KEY)" while a
    /// pinned tier key is what actually runs, sending the user to edit a
    /// value that would change nothing. no_claim: the notice copy itself
    /// (`auth_watch::notice_text` pins that).
    #[test]
    fn the_notice_source_names_only_the_override_that_actually_runs() {
        assert_eq!(
            key_source_in_force(Some("sk-cli-001"), None, "sk-cli-001"),
            Some("--api-key")
        );
        assert_eq!(
            key_source_in_force(None, Some("sk-env-001"), "sk-env-001"),
            Some("PHIMINT_API_KEY")
        );
        assert_eq!(
            key_source_in_force(None, Some("sk-env-001"), "mykey"),
            None,
            "the env override never reaches a pinned tier key"
        );
        assert_eq!(
            key_source_in_force(None, None, "mykey"),
            None,
            "the file's own key has no source"
        );
    }

    /// protects: the masked key shows only the first 4 and last 3 characters,
    /// and never panics on multi-byte keys. fails_when: a CJK key slices at
    /// a non-boundary (byte indexing), or a short key leaks its body.
    /// no_claim: any particular provider's key length.
    #[test]
    fn a_masked_key_leaks_only_its_head_and_tail() {
        assert_eq!(mask_key("sk-abcdef123456"), "sk-a…456");
        assert_eq!(mask_key(""), "(not set)");
        assert_eq!(mask_key("short"), "••••", "short keys are fully hidden");
        assert_eq!(
            mask_key("密钥abcdef尾部"),
            "密钥ab…f尾部",
            "multi-byte keys mask on char boundaries"
        );
    }

    /// protects: the skeleton is a valid, runnable ModelConfig whose key is
    /// empty (so a fresh install always lands in the first-run branch) with
    /// the documented defaults. fails_when: the template stops parsing or
    /// ships a placeholder key that reads as configured. no_claim: provider
    /// endpoint correctness beyond the documented defaults.
    #[test]
    fn the_template_parses_with_an_empty_key_and_known_defaults() {
        let cfg: ModelConfig = json5::from_str(CONFIG_TEMPLATE).expect("template must parse");
        assert_eq!(
            cfg.api_key.as_deref(),
            Some(""),
            "the skeleton leaves api_key empty for the user to fill"
        );
        assert!(!key_is_usable(cfg.api_key.as_deref().unwrap_or_default()));
        assert_eq!(cfg.base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(cfg.protocol.as_deref(), Some("openai"));
        assert_eq!(cfg.main.model(), "gpt-5.4-mini");
    }

    /// protects: the skeleton and config.json.example carry the same default
    /// endpoint — the one drift-catcher between the two copies. fails_when:
    /// someone edits one file's defaults and not the other. no_claim: that
    /// the example's commented-out sections stay in sync.
    #[test]
    fn the_template_defaults_match_the_example_file() {
        const EXAMPLE: &str = include_str!("../../config.json.example");
        let example: ModelConfig = json5::from_str(EXAMPLE).expect("example must parse");
        let template: ModelConfig = json5::from_str(CONFIG_TEMPLATE).expect("template must parse");
        assert_eq!(template.base_url, example.base_url, "base_url drifted");
        assert_eq!(template.protocol, example.protocol, "protocol drifted");
        assert_eq!(
            template.main.model(),
            example.main.model(),
            "main model drifted"
        );
    }

    /// protects: `phimint init` writes the skeleton exactly once, 0600, into
    /// a freshly created parent directory, and never touches a file the user
    /// already has. fails_when: an existing config is clobbered, the file
    /// lands world-readable, or the bytes differ from CONFIG_TEMPLATE.
    /// no_claim: recovery of a corrupted file (delete + re-init is the path).
    #[test]
    fn the_skeleton_is_written_once_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");
        assert!(ensure_skeleton(&path).unwrap(), "a missing file is created");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            CONFIG_TEMPLATE,
            "the created file is the template, byte for byte"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the skeleton holds a key: owner-only");
        }

        std::fs::write(&path, "{\"api_key\": \"sk-mine\"}").unwrap();
        assert!(
            !ensure_skeleton(&path).unwrap(),
            "an existing file is never overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"api_key\": \"sk-mine\"}",
            "the user's bytes survive"
        );
    }

    /// protects: the skeleton location honors LLM_CONFIG (that IS the config
    /// path) and otherwise defaults to ~/.phimint/config.json. fails_when: a
    /// custom LLM_CONFIG is ignored and a second config appears beside it.
    /// no_claim: what happens when both locations hold files (resolve_path
    /// decides at load).
    #[test]
    fn the_skeleton_target_follows_llm_config_then_home() {
        assert_eq!(
            skeleton_target_from(Some("/x/y.json"), None),
            PathBuf::from("/x/y.json"),
            "LLM_CONFIG is the config location"
        );
        assert_eq!(
            skeleton_target_from(None, Some(PathBuf::from("/h"))),
            PathBuf::from("/h/.phimint/config.json"),
            "default home location"
        );
    }

    /// protects: the first-run message names the file, the missing field and
    /// the exact remedy (edit the file, run phimint again) — the whole
    /// onboarding surface. fails_when: the copy sends users to a wizard or
    /// forgets the path. no_claim: localization (product strings are English).
    #[test]
    fn the_missing_key_message_names_the_file_and_the_remedy() {
        let msg = missing_key_message(Path::new("/root/.phimint/config.json"));
        assert!(
            msg.contains("/root/.phimint/config.json"),
            "names the file: {msg}"
        );
        assert!(msg.contains("api_key"), "names the missing field: {msg}");
        assert!(msg.contains("run phimint again"), "names the remedy: {msg}");
        assert!(msg.contains('"'), "carries a fill-in snippet: {msg}");
    }

    /// protects: the parse-error message carries the reason verbatim and the
    /// delete-to-regenerate escape hatch. fails_when: a syntax error becomes
    /// a dead end with no recovery path. no_claim: parser error quality.
    #[test]
    fn the_parse_error_message_carries_the_reason_and_the_escape_hatch() {
        let msg = parse_error_message(Path::new("/x/config.json"), "expected `}` at line 3");
        assert!(msg.contains("/x/config.json"), "names the file: {msg}");
        assert!(
            msg.contains("expected `}` at line 3"),
            "carries the reason: {msg}"
        );
        assert!(msg.contains("delete"), "offers the escape hatch: {msg}");
    }

    /// protects: `config show` prints the resolved endpoint with a masked key
    /// — never the raw secret — and names the env source when one is in force.
    /// fails_when: the raw key appears anywhere in the report (users paste
    /// this into issues). no_claim: tier-level resolution details beyond main.
    #[test]
    fn the_show_report_masks_the_key_and_names_its_source() {
        let cfg: ModelConfig = json5::from_str(CONFIG_TEMPLATE).unwrap();
        let path = Path::new("/h/config.json");

        let report = show_report(path, &cfg, Some("sk-env-key-12345"));
        assert!(
            report.contains("/h/config.json"),
            "names the file: {report}"
        );
        assert!(
            report.contains("sk-e…345"),
            "shows the masked env key: {report}"
        );
        assert!(
            !report.contains("sk-env-key-12345"),
            "the raw key never appears: {report}"
        );
        assert!(
            report.contains("PHIMINT_API_KEY"),
            "names the source: {report}"
        );

        let bare = show_report(path, &cfg, None);
        assert!(bare.contains("(not set)"), "an empty key says so: {bare}");
    }

    /// protects: show names the `model` selection pointer when one rides on
    /// main — the report must never claim `main` is in force when a pick has
    /// displaced it. fails_when: show prints the declared tier while the
    /// pointer silently runs a different model (the identity-stamp class of
    /// bug). no_claim: pointer endpoint details.
    #[test]
    fn the_show_report_names_an_active_selection_pointer() {
        let cfg: ModelConfig = json5::from_str(
            r#"{"base_url": "https://api.openai.com/v1", "api_key": "sk-a", "main": "gpt-5.4-mini", "model": "o1-preview"}"#,
        )
        .unwrap();
        let report = show_report(Path::new("/h/config.json"), &cfg, None);
        assert!(
            report.contains("o1-preview"),
            "the pointer model is named: {report}"
        );
        assert!(
            report.contains("pointer"),
            "the line says it is a pointer: {report}"
        );

        let plain = json5::from_str::<ModelConfig>(CONFIG_TEMPLATE).unwrap();
        assert!(
            !show_report(Path::new("/h/config.json"), &plain, None).contains("pointer"),
            "no pointer, no pointer line"
        );
    }

    /// protects: the boot gate runs whenever a key is in force — the CLI
    /// `--api-key` or PHIMINT_API_KEY counts as configured even when the file
    /// has no key (decision 1: the gate is "no usable key", not "no file").
    /// fails_when: a run with `--api-key` stops at the fill-in message.
    /// no_claim: what happens after boot (runtime notices cover 401/403).
    #[test]
    fn the_boot_gate_runs_whenever_a_key_is_in_force() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");

        let cfg: ModelConfig = json5::from_str(CONFIG_TEMPLATE).unwrap();
        match gate(Ok(Some(cfg)), None, Some("sk-env-001"), &target) {
            Boot::Run(config) => {
                assert_eq!(
                    config.main.model(),
                    "gpt-5.4-mini",
                    "the file's config runs"
                )
            }
            Boot::Stop(msg) => panic!("an env key means run, not stop: {msg}"),
        }

        let cfg: ModelConfig = json5::from_str(CONFIG_TEMPLATE).unwrap();
        match gate(Ok(Some(cfg)), Some("sk-cli-001"), None, &target) {
            Boot::Run(_) => {}
            Boot::Stop(msg) => panic!("a --api-key run is configured: {msg}"),
        }
    }

    /// protects: the key the provider is built with is the key the gate
    /// judged ([`key_in_force`]) — one resolution, one value. An explicit
    /// CLI/env override beats a pinned tier key (a pin must not silently eat
    /// `--api-key`), and a set-but-blank pin blocks rather than passing the
    /// gate on an override's readiness and then reaching `create_provider`
    /// as `""`. fails_when: a `--api-key` is ignored because the tier pins
    /// its own key, or a blank pin boots on the CLI key's readiness and dies
    /// building the provider from `""`. no_claim: keys on `lite`/`advanced`
    /// (the store resolves those per tier).
    #[test]
    fn the_provider_key_is_the_key_the_gate_judged() {
        // A pinned tier key, no override: the pin is the key in force.
        let cfg: ModelConfig =
            json5::from_str(r#"{"main": {"model": "m", "api_key": "sk-pin-000111"}}"#).unwrap();
        assert_eq!(
            key_in_force(&cfg, None, None),
            "sk-pin-000111",
            "the pin runs"
        );

        // An explicit override beats the pin — both routes.
        assert_eq!(
            key_in_force(&cfg, Some("sk-cli-000222"), None),
            "sk-cli-000222",
            "a --api-key is not eaten by the pin"
        );
        assert_eq!(
            key_in_force(&cfg, None, Some("sk-env-000333")),
            "sk-env-000333",
            "PHIMINT_API_KEY is not eaten by the pin"
        );

        // A set-but-blank pin: the override provides the one value both
        // halves use (the gate passes on it, and the provider is built from
        // it — never from `""`).
        let cfg: ModelConfig = json5::from_str(
            r#"{"api_key": "sk-top-000444", "main": {"model": "m", "api_key": ""}}"#,
        )
        .unwrap();
        assert_eq!(
            key_in_force(&cfg, Some("sk-cli-000555"), None),
            "sk-cli-000555",
            "the override, not the blank pin"
        );
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        match gate(Ok(Some(cfg)), Some("sk-cli-000555"), None, &target) {
            Boot::Run(_) => {}
            Boot::Stop(msg) => panic!("the override is a usable key: {msg}"),
        }

        // A set-but-blank pin with no override blocks: the gate refuses on
        // the very value the provider would have been built with.
        let cfg: ModelConfig = json5::from_str(
            r#"{"api_key": "sk-top-000444", "main": {"model": "m", "api_key": ""}}"#,
        )
        .unwrap();
        assert_eq!(
            key_in_force(&cfg, None, None),
            "",
            "the blank pin is the value"
        );
        match gate(Ok(Some(cfg)), None, None, &target) {
            Boot::Stop(msg) => assert!(msg.contains("api_key"), "names the fix: {msg}"),
            Boot::Run(_) => panic!("a blank pin is not a usable key"),
        }

        // No pin at all: the top-level member, with overrides beating it.
        let cfg: ModelConfig =
            json5::from_str(r#"{"api_key": "sk-top-000444", "main": "m"}"#).unwrap();
        assert_eq!(key_in_force(&cfg, None, None), "sk-top-000444");
        assert_eq!(
            key_in_force(&cfg, None, Some("sk-env-000666")),
            "sk-env-000666"
        );
    }

    /// protects: an unparseable config stops with the delete-to-regenerate
    /// hatch whatever key is in force — a broken file is a broken file, and
    /// the env key does not paper over it. fails_when: a syntax error boots
    /// on env and the session runs against defaults nobody chose.
    /// no_claim: parser error quality.
    #[test]
    fn the_boot_gate_stops_with_the_hatch_on_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        match gate(
            Err(ConfigError::ParseError("expected `}`".into())),
            None,
            Some("sk-env-001"),
            &target,
        ) {
            Boot::Stop(msg) => {
                assert!(msg.contains("delete"), "the hatch: {msg}");
                assert!(msg.contains("expected `}`"), "the reason: {msg}");
            }
            Boot::Run(_) => panic!("a broken config never boots"),
        }
    }

    /// protects: a key pinned inside the tier object counts as configured —
    /// the provider is built from the effective main tier's key
    /// (`TierConfig::resolve` lets a Full tier's own key win), so a config
    /// whose key lives only in `main` must boot. fails_when: the local-proxy
    /// shape (tier objects carrying api_key, no top-level key) is refused at
    /// boot while `config show` reports the key as set. no_claim: keys on
    /// `lite`/`advanced` alone — boot serves main.
    #[test]
    fn the_boot_gate_accepts_a_key_pinned_in_the_main_tier() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        let cfg: ModelConfig = json5::from_str(
            r#"{
                "main": {"model": "MiniMax-M3.1-Flash", "base_url": "http://localhost:3200",
                         "api_key": "mykey", "protocol": "anthropic"}
            }"#,
        )
        .unwrap();
        match gate(Ok(Some(cfg)), None, None, &target) {
            Boot::Run(config) => {
                let (_, _, key, _) = config.tier_config("main").unwrap();
                assert_eq!(key, "mykey", "the pinned key is the one that runs");
            }
            Boot::Stop(msg) => panic!("a pinned tier key means run, not stop: {msg}"),
        }
    }

    /// protects: a key pinned in the `model` selection pointer counts too —
    /// the pointer displaces `main`, so its key is the one in force.
    /// fails_when: a pointer carrying its own endpoint (the documented
    /// multi-provider shape) is refused at boot. no_claim: pointer writes
    /// (the /model picker owns those).
    #[test]
    fn the_boot_gate_accepts_a_key_pinned_in_the_selection_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        let cfg: ModelConfig = json5::from_str(
            r#"{
                "base_url": "https://api.openai.com/v1",
                "main": "gpt-5.4-mini",
                "model": {"model": "MiniMax-M3.1-Flash", "base_url": "http://localhost:3200",
                          "api_key": "mykey", "protocol": "anthropic"}
            }"#,
        )
        .unwrap();
        match gate(Ok(Some(cfg)), None, None, &target) {
            Boot::Run(config) => {
                let (_, _, key, _) = config.tier_config("main").unwrap();
                assert_eq!(key, "mykey", "the pointer's key is the one that runs");
            }
            Boot::Stop(msg) => panic!("a pinned pointer key means run: {msg}"),
        }
    }

    /// protects: a blank pin is not rescued by a top-level key — the runtime
    /// resolves `api_key: ""` to that blank (Some wins over the default), so
    /// booting on the top-level key would only die in create_provider.
    /// fails_when: the gate says configured and the session fails later with
    /// "LLM_API_KEY is required". no_claim: how a provider treats a blank key.
    #[test]
    fn a_blank_pinned_key_is_not_rescued_by_the_top_level_key() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        let cfg: ModelConfig = json5::from_str(
            r#"{"api_key": "sk-real-key-001", "main": {"model": "m", "api_key": ""}}"#,
        )
        .unwrap();
        match gate(Ok(Some(cfg)), None, None, &target) {
            Boot::Stop(msg) => assert!(msg.contains("api_key"), "names the field: {msg}"),
            Boot::Run(_) => panic!("the blank pin is what runs, and it is not a key"),
        }
    }

    /// protects: no usable key anywhere — no file, a file with an empty key,
    /// or a missing named --config path — is first-run: the skeleton is
    /// ensured where the message points and the answer is a stop. fails_when:
    /// a fresh install dies in create_provider's opaque failure, or the
    /// skeleton lands somewhere other than the message's path. no_claim: the
    /// message copy itself (missing_key_message pins it).
    #[test]
    fn the_boot_gate_stops_on_a_missing_key_and_ensures_the_skeleton() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");

        // No file anywhere, no key: first run — and the skeleton lands where
        // the message points.
        match gate(Ok(None), None, None, &target) {
            Boot::Stop(msg) => assert!(msg.contains("api_key"), "the one edit: {msg}"),
            Boot::Run(_) => panic!("no key, no run"),
        }
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            CONFIG_TEMPLATE,
            "the skeleton is where the message points"
        );

        // A file with an empty key is first-run too — and its bytes survive.
        let raw = "{\n  \"main\": \"m\",\n  \"api_key\": \"\"\n}\n";
        std::fs::write(&target, raw).unwrap();
        let cfg: ModelConfig = json5::from_str(raw).unwrap();
        match gate(Ok(Some(cfg)), None, None, &target) {
            Boot::Stop(_) => {}
            Boot::Run(_) => panic!("an empty key is not a key"),
        }
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            raw,
            "the user's bytes survive"
        );

        // A named --config path that does not exist is first-run at that path.
        let named = dir.path().join("named.json");
        match gate(
            Err(ConfigError::IoError("not found".into())),
            None,
            None,
            &named,
        ) {
            Boot::Stop(msg) => {
                assert!(
                    msg.contains("named.json"),
                    "the message names the path: {msg}"
                )
            }
            Boot::Run(_) => panic!("no file, no key, no run"),
        }
        assert_eq!(std::fs::read_to_string(&named).unwrap(), CONFIG_TEMPLATE);
    }

    /// protects: a key in force with no file at all (the CI case) is NOT
    /// first-run — boot continues on the template defaults with the key from
    /// env/CLI, and the skeleton is materialized with an EMPTY key (the env
    /// key is never written down). fails_when: a CI box carrying only
    /// PHIMINT_API_KEY is stopped for a fill-in, or the env key lands on
    /// disk. no_claim: that the template defaults fit the caller's provider
    /// (the 401/403 notice covers the mismatch).
    #[test]
    fn the_boot_gate_runs_on_template_defaults_when_only_the_ci_key_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        match gate(Ok(None), None, Some("sk-env-001"), &target) {
            Boot::Run(config) => {
                assert_eq!(
                    config.main.model(),
                    "gpt-5.4-mini",
                    "the documented defaults"
                );
                assert_eq!(
                    config.api_key.as_deref(),
                    Some(""),
                    "the env key is not written into the config"
                );
            }
            Boot::Stop(msg) => panic!("the CI case runs: {msg}"),
        }
        let raw = std::fs::read_to_string(&target).unwrap();
        assert_eq!(raw, CONFIG_TEMPLATE, "the skeleton is materialized");
        assert!(
            !raw.contains("sk-env-001"),
            "the env key never lands on disk"
        );
    }
}
