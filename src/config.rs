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

use std::collections::HashMap;
use std::path::PathBuf;

use phi_tui::popup_list::{PopupStyle, WidthSpec};
use serde::Deserialize;

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
    ) -> (String, String, String, String) {
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

    /// Scene-to-tier mapping overrides (optional)
    #[serde(default)]
    pub scene_tiers: Option<HashMap<String, String>>,

    /// Update checker configuration (optional, defaults apply if absent).
    #[serde(default)]
    pub update: UpdateConfig,

    /// UI configuration (optional, defaults apply if absent).
    #[serde(default)]
    pub ui: UiConfig,
}

/// UI configuration. Currently only the completion popup band.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub popup: PopupConfig,
}

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
        "https://github.com/hibuka-labs/phimint/releases/latest/download/manifest.json".to_string(),
    ]
}

fn default_channel() -> String {
    "stable".to_string()
}

fn default_auto_check() -> bool {
    true
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

    /// Load configuration from default locations.
    ///
    /// Checks in order:
    /// 1. LLM_CONFIG environment variable
    /// 2. ~/.phimint/config.json
    /// 3. ~/.config/phimint/config.json
    pub fn from_default_location() -> Result<Option<Self>, ConfigError> {
        // Check LLM_CONFIG env var first
        if let Ok(path) = std::env::var("LLM_CONFIG") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Ok(Some(Self::from_file(&path)?));
            }
        }

        // Check default locations
        let home = dirs::home_dir()
            .ok_or_else(|| ConfigError::IoError("Home directory not found".to_string()))?;

        let paths = vec![
            home.join(".phimint").join("config.json"),
            home.join(".config").join("phimint").join("config.json"),
        ];

        for path in paths {
            if path.exists() {
                return Ok(Some(Self::from_file(&path)?));
            }
        }

        Ok(None)
    }

    /// Get resolved config for a tier.
    pub fn tier_config(&self, tier: &str) -> Option<(String, String, String, String)> {
        let default_url = self.base_url.as_deref().unwrap_or("");
        let default_key = self.api_key.as_deref().unwrap_or("");
        let default_protocol = self.protocol.as_deref().unwrap_or("");

        let tier_config = match tier {
            "main" => Some(&self.main),
            "lite" => self.lite.as_ref().or(Some(&self.main)),
            "advanced" => self.advanced.as_ref().or(Some(&self.main)),
            _ => None,
        };

        tier_config.map(|tc| tc.resolve(default_url, default_key, default_protocol))
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

    #[test]
    fn validate_empty_model_fails() {
        let config = ModelConfig {
            base_url: Some("http://localhost".to_string()),
            api_key: None,
            protocol: None,
            main: TierConfig::Simple("".to_string()),
            lite: None,
            advanced: None,
            scene_tiers: None,
            update: UpdateConfig::default(),
            ui: Default::default(),
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
}
