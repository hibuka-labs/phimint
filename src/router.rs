//! Model router for scene-based model selection.
//!
//! `PhimintRouter` maps scenes (e.g., "main_chat", "sub_agent") to tier names
//! (e.g., "main", "lite"). The mapping is configuration-driven, so different
//! applications can use different routing strategies.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// Default scene-to-tier mapping for phimint.
fn default_scene_tiers() -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert("main_chat".to_string(), "main".to_string());
    map.insert("sub_agent".to_string(), "lite".to_string());
    map.insert("tool_summary".to_string(), "lite".to_string());
    map.insert("compression".to_string(), "lite".to_string());
    map.insert("guard".to_string(), "main".to_string());
    map.insert("architecture".to_string(), "advanced".to_string());
    map.insert("review".to_string(), "lite".to_string());
    map.insert("quick_check".to_string(), "lite".to_string());
    map
}

/// Configuration-driven model router.
///
/// Maps scenes to tier names. The mapping can be customized via configuration.
/// Also supports a "focus" mode that forces the main chat to use the lite tier.
pub struct PhimintRouter {
    /// Scene → tier mapping (from configuration)
    scene_tiers: HashMap<String, String>,

    /// Focus mode flag (forces main chat to use lite)
    focus_mode: AtomicBool,
}

impl PhimintRouter {
    /// Create a new router with default scene-to-tier mapping.
    pub fn new() -> Self {
        Self {
            scene_tiers: default_scene_tiers(),
            focus_mode: AtomicBool::new(false),
        }
    }

    /// Create a router with custom scene-to-tier mapping.
    pub fn with_scene_tiers(scene_tiers: HashMap<String, String>) -> Self {
        let mut default = default_scene_tiers();
        // Override defaults with custom mapping
        for (scene, tier) in scene_tiers {
            default.insert(scene, tier);
        }
        Self {
            scene_tiers: default,
            focus_mode: AtomicBool::new(false),
        }
    }

    /// Set focus mode (forces main chat to use lite tier).
    pub fn set_focus_mode(&self, enabled: bool) {
        self.focus_mode.store(enabled, Ordering::Relaxed);
    }

    /// Get focus mode status.
    pub fn is_focus_mode(&self) -> bool {
        self.focus_mode.load(Ordering::Relaxed)
    }

    /// Route a scene to a tier name.
    ///
    /// If focus mode is enabled and the scene is "main_chat", returns "lite".
    /// Otherwise, looks up the scene in the mapping, falling back to "main".
    pub fn route(&self, scene: &str) -> &str {
        // Focus mode override
        if scene == "main_chat" && self.focus_mode.load(Ordering::Relaxed) {
            return "lite";
        }

        // Look up scene in mapping
        self.scene_tiers
            .get(scene)
            .map(|s| s.as_str())
            .unwrap_or("main")
    }

    /// Get all configured scene-to-tier mappings.
    pub fn scene_tiers(&self) -> &HashMap<String, String> {
        &self.scene_tiers
    }
}

impl Default for PhimintRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_routing() {
        let router = PhimintRouter::new();

        assert_eq!(router.route("main_chat"), "main");
        assert_eq!(router.route("sub_agent"), "lite");
        assert_eq!(router.route("tool_summary"), "lite");
        assert_eq!(router.route("compression"), "lite");
        assert_eq!(router.route("guard"), "main");
        assert_eq!(router.route("architecture"), "advanced");
        assert_eq!(router.route("review"), "lite");
        assert_eq!(router.route("quick_check"), "lite");
        assert_eq!(router.route("unknown"), "main");
    }

    #[test]
    fn focus_mode_overrides_main_chat() {
        let router = PhimintRouter::new();

        assert_eq!(router.route("main_chat"), "main");

        router.set_focus_mode(true);
        assert_eq!(router.route("main_chat"), "lite");
        assert!(router.is_focus_mode());

        router.set_focus_mode(false);
        assert_eq!(router.route("main_chat"), "main");
    }

    #[test]
    fn focus_mode_does_not_affect_other_scenes() {
        let router = PhimintRouter::new();

        router.set_focus_mode(true);
        assert_eq!(router.route("sub_agent"), "lite");
        assert_eq!(router.route("architecture"), "advanced");
    }

    #[test]
    fn custom_scene_tiers() {
        let mut custom = HashMap::new();
        custom.insert("main_chat".to_string(), "default".to_string());
        custom.insert("sub_agent".to_string(), "fast".to_string());

        let router = PhimintRouter::with_scene_tiers(custom);

        assert_eq!(router.route("main_chat"), "default");
        assert_eq!(router.route("sub_agent"), "fast");
        // Other scenes use defaults
        assert_eq!(router.route("architecture"), "advanced");
    }

    #[test]
    fn scene_tiers_returns_mapping() {
        let router = PhimintRouter::new();
        let tiers = router.scene_tiers();

        assert!(tiers.contains_key("main_chat"));
        assert!(tiers.contains_key("sub_agent"));
    }
}
