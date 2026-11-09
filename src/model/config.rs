//! Configuration for multi-model support.
//!
//! `ModelConfig` holds the configuration for model tiers and scene routing.
//! It supports loading from JSON config file, environment variables, and CLI arguments.
//!
//! # Configuration Format
//!
//! The JSON config supports flexible tier definitions:
//!
//! ## Simple (same provider):
//! ```json
//! {
//!   "base_url": "https://api.openai.com/v1",
//!   "api_key": "sk-xxx",
//!   "main": "gpt-5.4-mini",
//!   "lite": "gpt-4o-mini",
//!   "advanced": "o1-preview"
//! }
//! ```
//!
//! ## Full (different providers):
//! ```json
//! {
//!   "main": { "model": "gpt-5.4-mini", "base_url": "https://api.openai.com/v1", "api_key": "sk-openai" },
//!   "lite": { "model": "deepseek-chat", "base_url": "https://api.deepseek.com/v1", "api_key": "sk-deepseek" },
//!   "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant" }
//! }
//! ```
//!
//! ## Mixed:
//! ```json
//! {
//!   "base_url": "https://api.openai.com/v1",
//!   "api_key": "sk-xxx",
//!   "main": "gpt-5.4-mini",
//!   "lite": "gpt-4o-mini",
//!   "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant" }
//! }
//! ```
//!
//! ## Selection pointer (`model`):
//! ```json
//! {
//!   "base_url": "https://api.openai.com/v1",
//!   "api_key": "sk-xxx",
//!   "main": "gpt-5.4-mini",
//!   "model": "o1-preview"
//! }
//! ```
//! A `/model` pick writes only the top-level `model` key (same `TierConfig`
//! shape: bare name or full object). It selects the effective `main` without
//! touching the `main`/`lite`/`advanced` definitions; deleting it returns to
//! the configured `main`. Absent = follow the configuration (zero migration).

use std::collections::HashMap;
use std::path::PathBuf;

use phi_tui::popup_list::{PopupStyle, WidthSpec};
use serde::Deserialize;

/// The resolved endpoint tuple for one model: `(model, base_url, api_key,
/// protocol)`. One spelling across the family — the resolver, the store's
/// tier table and persistence all talk in this shape.
pub type EndpointTuple = (String, String, String, String);

/// Tier configuration - can be a simple model name or full config.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TierConfig {
    /// Simple: just model name, inherits base_url/api_key/protocol from parent
    Simple(String),
    /// Full: has own model, base_url, api_key, protocol
    Full {
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        protocol: Option<String>,
    },
}

impl TierConfig {
    /// Get the model name.
    pub fn model(&self) -> &str {
        match self {
            TierConfig::Simple(model) => model,
            TierConfig::Full { model, .. } => model,
        }
    }

    /// Resolve to (model, base_url, api_key, protocol) using defaults.
    pub fn resolve(
        &self,
        default_url: &str,
        default_key: &str,
        default_protocol: &str,
    ) -> EndpointTuple {
        match self {
            TierConfig::Simple(model) => (
                model.clone(),
                default_url.to_string(),
                default_key.to_string(),
                default_protocol.to_string(),
            ),
            TierConfig::Full {
                model,
                base_url,
                api_key,
                protocol,
            } => {
                let url = base_url.clone().unwrap_or_else(|| default_url.to_string());
                let key = api_key.clone().unwrap_or_else(|| default_key.to_string());
                let proto = protocol
                    .clone()
                    .unwrap_or_else(|| default_protocol.to_string());
                (model.clone(), url, key, proto)
            }
        }
    }
}

/// Configuration for model tiers and routing.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    /// Default base_url (tier can override)
    #[serde(default)]
    pub base_url: Option<String>,

    /// Default api_key (tier can override)
    #[serde(default)]
    pub api_key: Option<String>,

    /// Default protocol (tier can override)
    /// Values: openai, anthropic, deepseek, aliyun, moonshot, gemini, ollama
    #[serde(default)]
    pub protocol: Option<String>,

    /// Main tier (required)
    pub main: TierConfig,

    /// Lite tier (optional, falls back to main)
    #[serde(default)]
    pub lite: Option<TierConfig>,

    /// Advanced tier (optional, falls back to main)
    #[serde(default)]
    pub advanced: Option<TierConfig>,

    /// Selection pointer (`model` key): the switched-to model. `None` = no
    /// switch has been made and the configured `main` definition is the
    /// effective main. The three fields above are DEFINITIONS — a switch
    /// writes only this pointer, never a definition. Parse is lenient
    /// (`lenient_pointer`): a malformed value warns and reads as no pointer
    /// instead of failing the whole file.
    #[serde(default, deserialize_with = "lenient_pointer")]
    pub model: Option<TierConfig>,

    /// Scene-to-tier mapping overrides (optional)
    #[serde(default)]
    pub scene_tiers: Option<HashMap<String, String>>,

    /// Update checker configuration (optional, defaults apply if absent).
    #[serde(default)]
    pub update: UpdateConfig,

    /// UI configuration (optional, defaults apply if absent).
    #[serde(default)]
    pub ui: UiConfig,

    /// CodeAct engine wiring (optional, defaults apply if absent — `mode`
    /// defaults to `tool`, today's shipped behavior).
    #[serde(default)]
    pub code_exec: CodeExecConfig,
}

/// `code_exec` — the runtime switch for the code road (M3 T10d, disable
/// layer ③ on top of the `code-exec` compile feature).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CodeExecConfig {
    /// Which road code work takes. Absent → [`CodeExecMode::Tool`].
    #[serde(default)]
    pub mode: CodeExecMode,
}

/// `code_exec.mode` — which road code work takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CodeExecMode {
    /// The `code_cell` tool on the tool-call road (shipped M2 behavior).
    #[default]
    Tool,
    /// The CodeAct action backend on text turns (M3 road): a code step is
    /// the model's action itself, not a tool call.
    Act,
    /// No code road: `code_cell` is not registered and text turns never
    /// reach a code backend.
    Off,
}

impl CodeExecMode {
    /// The road this binary can actually deliver (disable layer ②): without
    /// the `code-exec` compile feature there is no engine at all, so every
    /// mode resolves to [`CodeExecMode::Off`] — prompt and assembly agree.
    #[cfg(not(feature = "code-exec"))]
    pub fn resolved(self) -> Self {
        let _ = self;
        CodeExecMode::Off
    }

    /// With the engine compiled in, the config says what it says.
    #[cfg(feature = "code-exec")]
    pub fn resolved(self) -> Self {
        self
    }
}

/// UI configuration: the completion popup band, the tips line, and the exit
/// transcript replay.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub popup: PopupConfig,
    #[serde(default)]
    pub tips: TipsConfig,
    /// What the restored terminal keeps when phimint exits.
    #[serde(default)]
    pub exit_dump: ExitDumpMode,
}

/// `ui.exit_dump` — what the restored terminal shows after phimint quits.
///
/// phimint draws on the alternate screen, whose content the terminal discards
/// on exit, so "keep the record" is a replay printed to the primary screen
/// (see `phi_agent_tui::exit_dump`). The mode is the replay's completeness —
/// its vocabulary is the shell's; the config parsing that feeds it is product.
pub use phi_agent_tui::ExitDumpMode;

/// `ui.popup` — the `@`/`/` popup band. Missing fields take the widget defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct PopupConfig {
    /// Draw a rounded border around the band.
    #[serde(default)]
    pub frame: bool,
    /// `"fill"` (match the composer width) or a column count.
    #[serde(default)]
    pub width: WidthConfig,
    /// Maximum rows shown.
    #[serde(default = "default_popup_height")]
    pub height: u16,
    /// Draw the dim rule under the rows.
    #[serde(default = "default_true")]
    pub separator: bool,
}

fn default_popup_height() -> u16 {
    PopupStyle::default().height
}

fn default_true() -> bool {
    true
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            frame: false,
            width: WidthConfig::default(),
            height: default_popup_height(),
            separator: true,
        }
    }
}

/// `"fill"` or a column count.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WidthConfig {
    Fill(String),
    Fixed(u16),
}

impl Default for WidthConfig {
    fn default() -> Self {
        Self::Fill("fill".to_string())
    }
}

impl PopupConfig {
    /// Convert to the widget style, validating `width` and `height`.
    pub fn to_style(&self) -> Result<PopupStyle, ConfigError> {
        let width = match &self.width {
            WidthConfig::Fixed(n) => WidthSpec::Fixed(*n),
            WidthConfig::Fill(s) if s == "fill" => WidthSpec::Fill,
            WidthConfig::Fill(other) => {
                return Err(ConfigError::ParseError(format!(
                    "ui.popup.width must be \"fill\" or a column count, got {other:?}"
                )));
            }
        };
        // Reject rather than clamp, matching `width` above: `height: 0` means
        // "show no rows", which leaves the band as bare chrome (an empty border
        // box when `frame` is on). Silently reinterpreting it hides the mistake;
        // the widget floors its content at one row anyway, so this only makes
        // the config surface honest.
        if self.height == 0 {
            return Err(ConfigError::ParseError(
                "ui.popup.height must be at least 1, got 0".to_string(),
            ));
        }
        Ok(PopupStyle {
            frame: self.frame,
            width,
            height: self.height,
            separator: self.separator,
            ..PopupStyle::default()
        })
    }
}

/// `ui.tips` — the rotating tips line under the process status line.
#[derive(Debug, Clone, Deserialize)]
pub struct TipsConfig {
    /// Show the tips line at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tips manifest endpoint URLs (tried in order, 3s timeout each).
    /// Each is a full URL to a tips JSON file.
    #[serde(default = "default_tips_endpoints")]
    pub endpoints: Vec<String>,
    /// Minimum seconds between cloud refresh attempts.
    #[serde(default = "default_tips_refresh_interval")]
    pub refresh_interval_secs: u64,
}

impl Default for TipsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            endpoints: default_tips_endpoints(),
            refresh_interval_secs: default_tips_refresh_interval(),
        }
    }
}

fn default_tips_endpoints() -> Vec<String> {
    vec![
        // Primary: GitHub (global). Fallback: Gitee (mainland China).
        // The checker tries endpoints in order, 3s timeout each.
        "https://github.com/hibuka-labs/phimint/releases/latest/download/tips.json".to_string(),
        "https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/tips.json".to_string(),
    ]
}

fn default_tips_refresh_interval() -> u64 {
    86_400
}

/// Configuration for the update checker.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateConfig {
    /// Manifest endpoint URLs (tried in order, 3s timeout each).
    /// Each is a full URL to a manifest JSON file.
    #[serde(default = "default_endpoints")]
    pub endpoints: Vec<String>,

    /// Release channel: "stable" or "beta".
    #[serde(default = "default_channel")]
    pub channel: String,

    /// Whether to check for updates on startup.
    #[serde(default = "default_auto_check")]
    pub auto_check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            endpoints: default_endpoints(),
            channel: default_channel(),
            auto_check: default_auto_check(),
        }
    }
}

fn default_endpoints() -> Vec<String> {
    vec![
        // Primary: GitHub (global). Fallback: Gitee (mainland China).
        // The checker tries endpoints in order, 3s timeout each.
        "https://github.com/hibuka-labs/phimint/releases/latest/download/manifest.json".to_string(),
        "https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/manifest.json"
            .to_string(),
    ]
}

fn default_channel() -> String {
    "stable".to_string()
}

fn default_auto_check() -> bool {
    true
}

/// Lenient parse of the `model` selection pointer. The key is user-editable
/// and older files may carry anything there (before this key was activated it
/// was ignored), so a value that is not a [`TierConfig`] shape must never
/// brick startup: warn and read it as "no pointer". `null` reads as no
/// pointer without a warning.
fn lenient_pointer<'de, D>(deserializer: D) -> Result<Option<TierConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    match serde_json::from_value::<TierConfig>(value) {
        Ok(pointer) => Ok(Some(pointer)),
        Err(err) => {
            tracing::warn!(
                "config `model` pointer is malformed ({err}); running without a pointer"
            );
            Ok(None)
        }
    }
}

impl ModelConfig {
    /// Load configuration from a JSON file (supports JSON5 with comments).
    pub fn from_file(path: &PathBuf) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::IoError(format!("Failed to read config file: {}", e)))?;
        let config: ModelConfig = json5::from_str(&content)
            .map_err(|e| ConfigError::ParseError(format!("Failed to parse config: {}", e)))?;
        Ok(config)
    }

    /// The config file path, resolved the same way loading resolves it:
    /// `LLM_CONFIG` when it points at a real file, else the first existing
    /// default location. Shared by loading and `/model` persistence so a
    /// pick is always saved to the file the next boot will read.
    pub fn resolve_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("LLM_CONFIG") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
        let home = dirs::home_dir()?;
        [
            home.join(".phimint").join("config.json"),
            home.join(".config").join("phimint").join("config.json"),
        ]
        .into_iter()
        .find(|p| p.exists())
    }

    /// Load configuration from default locations.
    ///
    /// Checks in order (see [`Self::resolve_path`]):
    /// 1. LLM_CONFIG environment variable
    /// 2. ~/.phimint/config.json
    /// 3. ~/.config/phimint/config.json
    pub fn from_default_location() -> Result<Option<Self>, ConfigError> {
        match Self::resolve_path() {
            Some(path) => Ok(Some(Self::from_file(&path)?)),
            None => Ok(None),
        }
    }

    /// Get the resolved endpoint tuple for a tier — THE one resolution rule
    /// (there must never be a second: boot provider, store tier table and
    /// [`Self::validate`] all resolve through here). The effective `main` is
    /// the `model` pointer when set (a selection), else the configured
    /// `main`; `lite`/`advanced` use their own definition and fall back to
    /// the effective `main` when unconfigured (an unconfigured tier follows a
    /// switch).
    pub fn tier_config(&self, tier: &str) -> Option<EndpointTuple> {
        let default_url = self.base_url.as_deref().unwrap_or("");
        let default_key = self.api_key.as_deref().unwrap_or("");
        let default_protocol = self.protocol.as_deref().unwrap_or("");

        let effective_main = self.model.as_ref().unwrap_or(&self.main);
        let definition = match tier {
            "main" => Some(effective_main),
            "lite" | "advanced" => Some(self.tier_def(tier).unwrap_or(effective_main)),
            _ => None,
        };

        definition.map(|tc| tc.resolve(default_url, default_key, default_protocol))
    }

    /// The tier's declaration as configured, no fallback: what the file SAYS
    /// (`main` always has one; `lite`/`advanced` only when configured). The
    /// picker's shelf and tier tags read declarations — a selection never
    /// moves a definition.
    pub fn tier_def(&self, tier: &str) -> Option<&TierConfig> {
        match tier {
            "main" => Some(&self.main),
            "lite" => self.lite.as_ref(),
            "advanced" => self.advanced.as_ref(),
            _ => None,
        }
    }

    /// The configured `main` definition resolved with the top-level defaults
    /// — the tuple「clear selection」rolls back to. The `model` pointer and
    /// `--model` are selections; this is the definition underneath them.
    pub fn configured_main(&self) -> EndpointTuple {
        self.main.resolve(
            self.base_url.as_deref().unwrap_or(""),
            self.api_key.as_deref().unwrap_or(""),
            self.protocol.as_deref().unwrap_or(""),
        )
    }

    /// Apply the CLI `--model` session override: it writes the `model`
    /// selection slot — the same slot a file pointer loads into, so the
    /// precedence CLI > pointer > configured `main` falls out of one rule.
    /// Session-scoped and never persisted: the file keeps its own pointer for
    /// the next boot. The configured `main` definition is NOT rewritten (the
    /// old behavior lost it for the session and dropped a picker row).
    pub fn apply_cli_model(&mut self, model: Option<&str>) {
        if let Some(model) = model {
            self.model = Some(TierConfig::Simple(model.to_string()));
        }
    }

    /// Get all configured tier names.
    pub fn tier_names(&self) -> Vec<&str> {
        let mut names = vec!["main"];
        if self.lite.is_some() {
            names.push("lite");
        }
        if self.advanced.is_some() {
            names.push("advanced");
        }
        names
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Validate main tier has a model
        if self.main.model().is_empty() {
            return Err(ConfigError::ValidationError(
                "main tier model cannot be empty".to_string(),
            ));
        }

        // Validate base_url is set (either at top level or in main tier)
        let (_, url, _, _) = self.tier_config("main").unwrap();
        if url.is_empty() {
            return Err(ConfigError::ValidationError(
                "base_url must be set at top level or in main tier".to_string(),
            ));
        }

        Ok(())
    }
}

/// CLI arguments for model configuration.
#[derive(Debug, Clone, Default)]
pub struct CliModelArgs {
    /// Main model
    pub model: Option<String>,
    /// Lite model
    pub lite_model: Option<String>,
    /// Advanced model
    pub advanced_model: Option<String>,
    /// API base URL
    pub base_url: Option<String>,
    /// API key
    pub api_key: Option<String>,
}

/// Configuration error types.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    IoError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_config_simple() {
        let config = TierConfig::Simple("gpt-4".to_string());
        assert_eq!(config.model(), "gpt-4");
        let (model, url, key, proto) =
            config.resolve("https://api.openai.com/v1", "sk-xxx", "openai");
        assert_eq!(model, "gpt-4");
        assert_eq!(url, "https://api.openai.com/v1");
        assert_eq!(key, "sk-xxx");
        assert_eq!(proto, "openai");
    }

    #[test]
    fn tier_config_full() {
        let config = TierConfig::Full {
            model: "deepseek-chat".to_string(),
            base_url: Some("https://api.deepseek.com/v1".to_string()),
            api_key: Some("sk-deepseek".to_string()),
            protocol: Some("openai".to_string()),
        };
        assert_eq!(config.model(), "deepseek-chat");
        let (model, url, key, proto) =
            config.resolve("https://api.openai.com/v1", "sk-xxx", "openai");
        assert_eq!(model, "deepseek-chat");
        assert_eq!(url, "https://api.deepseek.com/v1");
        assert_eq!(key, "sk-deepseek");
        assert_eq!(proto, "openai");
    }

    #[test]
    fn tier_config_full_inherits() {
        let config = TierConfig::Full {
            model: "gpt-4".to_string(),
            base_url: None,
            api_key: None,
            protocol: None,
        };
        let (model, url, key, proto) =
            config.resolve("https://api.openai.com/v1", "sk-xxx", "openai");
        assert_eq!(model, "gpt-4");
        assert_eq!(url, "https://api.openai.com/v1");
        assert_eq!(key, "sk-xxx");
        assert_eq!(proto, "openai");
    }

    #[test]
    fn model_config_simple_all_same_provider() {
        let json = r#"{
            "base_url": "https://api.openai.com/v1",
            "api_key": "sk-xxx",
            "protocol": "openai",
            "main": "gpt-5.4-mini",
            "lite": "gpt-4o-mini",
            "advanced": "o1-preview"
        }"#;

        let config: ModelConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.main.model(), "gpt-5.4-mini");
        assert_eq!(config.lite.as_ref().unwrap().model(), "gpt-4o-mini");
        assert_eq!(config.advanced.as_ref().unwrap().model(), "o1-preview");

        let (model, url, _, _) = config.tier_config("main").unwrap();
        assert_eq!(model, "gpt-5.4-mini");
        assert_eq!(url, "https://api.openai.com/v1");

        let (model, url, _, _) = config.tier_config("lite").unwrap();
        assert_eq!(model, "gpt-4o-mini");
        assert_eq!(url, "https://api.openai.com/v1");
    }

    #[test]
    fn model_config_all_different_providers() {
        let json = r#"{
            "main": { "model": "gpt-5.4-mini", "base_url": "https://api.openai.com/v1", "api_key": "sk-openai", "protocol": "openai" },
            "lite": { "model": "deepseek-chat", "base_url": "https://api.deepseek.com/v1", "api_key": "sk-deepseek", "protocol": "openai" },
            "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant", "protocol": "anthropic" }
        }"#;

        let config: ModelConfig = serde_json::from_str(json).unwrap();
        let (model, url, _, _) = config.tier_config("main").unwrap();
        assert_eq!(model, "gpt-5.4-mini");
        assert_eq!(url, "https://api.openai.com/v1");

        let (model, url, _, _) = config.tier_config("lite").unwrap();
        assert_eq!(model, "deepseek-chat");
        assert_eq!(url, "https://api.deepseek.com/v1");

        let (model, url, _, _) = config.tier_config("advanced").unwrap();
        assert_eq!(model, "claude-opus-4");
        assert_eq!(url, "https://api.anthropic.com");
    }

    #[test]
    fn model_config_mixed() {
        let json = r#"{
            "base_url": "https://api.openai.com/v1",
            "api_key": "sk-xxx",
            "protocol": "openai",
            "main": "gpt-5.4-mini",
            "lite": "gpt-4o-mini",
            "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant", "protocol": "anthropic" }
        }"#;

        let config: ModelConfig = serde_json::from_str(json).unwrap();
        let (model, url, _, _) = config.tier_config("main").unwrap();
        assert_eq!(model, "gpt-5.4-mini");
        assert_eq!(url, "https://api.openai.com/v1");

        let (model, url, _, _) = config.tier_config("lite").unwrap();
        assert_eq!(model, "gpt-4o-mini");
        assert_eq!(url, "https://api.openai.com/v1");

        let (model, url, _, _) = config.tier_config("advanced").unwrap();
        assert_eq!(model, "claude-opus-4");
        assert_eq!(url, "https://api.anthropic.com");
    }

    #[test]
    fn model_config_fallback_to_main() {
        let json = r#"{
            "base_url": "https://api.openai.com/v1",
            "main": "gpt-4"
        }"#;

        let config: ModelConfig = serde_json::from_str(json).unwrap();
        let (model, _, _, _) = config.tier_config("lite").unwrap();
        assert_eq!(model, "gpt-4"); // falls back to main
    }

    /// protects: THE one resolution rule under a selection pointer — the
    /// effective main is the pointer resolved over the top-level defaults,
    /// unconfigured lite/advanced follow it, configured ones keep their own
    /// definitions, and `configured_main` still reports the configured `main`
    /// (what「clear selection」rolls back to). fails_when: the pointer is
    /// ignored, a configured tier follows it, or the configured main is
    /// lost. no_claim: what a pointer value should be.
    #[test]
    fn the_pointer_resolves_the_effective_main_and_unconfigured_tiers_follow() {
        let json = r#"{
            "base_url": "https://api.example.com/v1",
            "api_key": "sk-default",
            "protocol": "openai",
            "main": "configured-main",
            "lite": "configured-lite",
            "model": { "model": "pointer-model", "base_url": "https://pointer.example.com/v1" }
        }"#;
        let config: ModelConfig = serde_json::from_str(json).unwrap();

        // Effective main = pointer delta over the top-level defaults.
        let (model, url, key, proto) = config.tier_config("main").unwrap();
        assert_eq!(model, "pointer-model");
        assert_eq!(url, "https://pointer.example.com/v1");
        assert_eq!(key, "sk-default", "unset delta fields inherit the defaults");
        assert_eq!(proto, "openai");

        // A configured lite keeps its own definition; the unconfigured
        // advanced follows the effective main.
        assert_eq!(config.tier_config("lite").unwrap().0, "configured-lite");
        assert_eq!(config.tier_config("advanced").unwrap().0, "pointer-model");

        // The configured definition survives for the clear to roll back to.
        assert_eq!(config.configured_main().0, "configured-main");
        assert_eq!(config.tier_def("main").unwrap().model(), "configured-main");
        assert_eq!(
            config.main.model(),
            "configured-main",
            "the field never moves"
        );
    }

    /// protects: the `model` pointer parses leniently — a `null` is silent
    /// and a malformed value warns and reads as no pointer, so a weird
    /// config value can never take the whole file down. fails_when: an
    /// unparseable pointer value fails the file parse or half-activates.
    /// no_claim: what the warning says beyond warning.
    #[test]
    fn a_malformed_pointer_reads_as_no_pointer_and_the_file_still_lands() {
        for value in ["null", "123", "[]", "{\"nonsense\": true}"] {
            let json = format!(
                r#"{{"base_url": "https://api.example.com/v1", "main": "configured-main", "model": {value}}}"#
            );
            let config: ModelConfig = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("the file parses with model = {value}: {e}"));
            assert!(
                config.model.is_none(),
                "model = {value} reads as no pointer: {:?}",
                config.model
            );
            assert_eq!(
                config.tier_config("main").unwrap().0,
                "configured-main",
                "model = {value}: no pointer, configured main rules"
            );
        }
    }

    /// protects: the CLI precedence chain — `--model` writes the in-memory
    /// `model` slot and outranks a file pointer, which outranks the
    /// configured `main`; a bare call leaves the slot alone. fails_when:
    /// the CLI is ignored under a pointer or a pointer outranks it.
    /// no_claim: persistence (the CLI override is memory-only by contract —
    /// nothing in `apply_cli_model` can touch a file).
    #[test]
    fn cli_model_outranks_the_pointer_in_memory_only() {
        let json = r#"{
            "base_url": "https://api.example.com/v1",
            "main": "configured-main",
            "model": "pointer-model"
        }"#;
        let mut config: ModelConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.tier_config("main").unwrap().0, "pointer-model");

        config.apply_cli_model(Some("cli-model"));
        assert_eq!(config.tier_config("main").unwrap().0, "cli-model");
        assert_eq!(
            config.main.model(),
            "configured-main",
            "the configured definition is preserved (the picker keeps its row)"
        );

        // A bare call is a no-op: the selection stands.
        config.apply_cli_model(None);
        assert_eq!(config.tier_config("main").unwrap().0, "cli-model");
    }

    #[test]
    fn validate_empty_model_fails() {
        let config = ModelConfig {
            base_url: Some("http://localhost".to_string()),
            api_key: None,
            protocol: None,
            main: TierConfig::Simple("".to_string()),
            lite: None,
            advanced: None,
            model: None,
            scene_tiers: None,
            update: UpdateConfig::default(),
            ui: Default::default(),
            code_exec: Default::default(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn ui_popup_defaults_to_the_widget_default_style() {
        let cfg: ModelConfig = json5::from_str(r#"{"main": "gpt-4"}"#).unwrap();
        assert_eq!(
            cfg.ui.popup.to_style().unwrap(),
            phi_tui::popup_list::PopupStyle::default()
        );
    }

    #[test]
    fn ui_popup_parses_fill_and_fixed_width() {
        let fill: ModelConfig =
            json5::from_str(r#"{"main":"gpt-4","ui":{"popup":{"width":"fill","height":5}}}"#)
                .unwrap();
        let style = fill.ui.popup.to_style().unwrap();
        assert_eq!(style.width, phi_tui::popup_list::WidthSpec::Fill);
        assert_eq!(style.height, 5);

        let fixed: ModelConfig = json5::from_str(
            r#"{"main":"gpt-4","ui":{"popup":{"width":64,"frame":true,"separator":false}}}"#,
        )
        .unwrap();
        let style = fixed.ui.popup.to_style().unwrap();
        assert_eq!(style.width, phi_tui::popup_list::WidthSpec::Fixed(64));
        assert!(style.frame);
        assert!(!style.separator);
    }

    #[test]
    fn ui_tips_defaults() {
        let cfg: ModelConfig = json5::from_str(r#"{"main": "gpt-4"}"#).unwrap();
        assert!(cfg.ui.tips.enabled);
        assert_eq!(cfg.ui.tips.refresh_interval_secs, 86_400);
        assert_eq!(cfg.ui.tips.endpoints.len(), 2);
    }

    #[test]
    fn ui_tips_parses_overrides() {
        let cfg: ModelConfig = json5::from_str(
            r#"{"main":"gpt-4","ui":{"tips":{
                "enabled": false,
                "refresh_interval_secs": 3600,
                "endpoints": ["https://example.com/tips.json"]
            }}}"#,
        )
        .unwrap();
        assert!(!cfg.ui.tips.enabled);
        assert_eq!(cfg.ui.tips.refresh_interval_secs, 3600);
        assert_eq!(
            cfg.ui.tips.endpoints,
            vec!["https://example.com/tips.json".to_string()]
        );
    }

    #[test]
    fn ui_popup_rejects_unknown_width_string() {
        let cfg: ModelConfig =
            json5::from_str(r#"{"main":"gpt-4","ui":{"popup":{"width":"wide"}}}"#).unwrap();
        assert!(cfg.ui.popup.to_style().is_err());
    }

    #[test]
    fn ui_popup_rejects_zero_height() {
        let cfg: ModelConfig =
            json5::from_str(r#"{"main":"gpt-4","ui":{"popup":{"width":"fill","height":0}}}"#)
                .unwrap();
        let err = cfg.ui.popup.to_style().unwrap_err();
        assert!(
            err.to_string()
                .contains("ui.popup.height must be at least 1"),
            "unhelpful error: {err}"
        );
    }

    // ── code_exec.mode (M3 T10d runtime switch) ──

    /// Absent `code_exec` = `tool` — a config that predates the switch keeps
    /// today's shipped behavior exactly.
    #[test]
    fn code_exec_absent_defaults_to_tool() {
        let json = r#"{"main": "gpt-5.4-mini"}"#;
        let config: ModelConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.code_exec.mode, CodeExecMode::Tool);
    }

    #[test]
    fn code_exec_mode_parses_all_three_roads() {
        for (mode, expected) in [
            ("tool", CodeExecMode::Tool),
            ("act", CodeExecMode::Act),
            ("off", CodeExecMode::Off),
        ] {
            let json = format!(r#"{{"main": "m", "code_exec": {{"mode": "{mode}"}}}}"#);
            let config: ModelConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(config.code_exec.mode, expected, "mode = {mode:?}");
        }
    }

    #[test]
    fn code_exec_mode_rejects_an_unknown_road() {
        let json = r#"{"main": "m", "code_exec": {"mode": "banana"}}"#;
        assert!(
            serde_json::from_str::<ModelConfig>(json).is_err(),
            "an unknown mode must fail loud at parse, not fall through"
        );
    }
}
