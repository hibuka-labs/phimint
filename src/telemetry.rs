//! Skill telemetry — tracks skill load events for session metrics.
//!
//! Design: skill-injection M3b. Two trigger paths:
//! - **Slash** (`/review args`): recorded in `agent_loop` before `run_turn`
//! - **Model** (skill tool call): recorded in `SkillTool::call()`
//!
//! Both paths write to shared `SkillTelemetry` state. After each turn,
//! `agent_loop` calls `snapshot_and_reset()` to emit turn-level custom data
//! via `set_turn_custom`. Session totals accumulate separately.

use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::json;

/// Shared skill-load counters for telemetry.
///
/// Thread-safe (atomic); cloned freely. The slash/model paths each have
/// their own entry point, so no lock contention on the hot path.
#[derive(Debug)]
pub struct SkillTelemetry {
    /// Total slash-triggered skill loads this session.
    slash_count: AtomicU32,
    /// Total model-triggered skill loads this session.
    model_count: AtomicU32,
    /// Skill name from the most recent slash trigger this turn (if any).
    last_slash_name: std::sync::Mutex<Option<String>>,
    /// Skill name from the most recent model trigger this turn (if any).
    last_model_name: std::sync::Mutex<Option<String>>,
}

impl Clone for SkillTelemetry {
    fn clone(&self) -> Self {
        Self {
            slash_count: AtomicU32::new(self.slash_count.load(Ordering::Relaxed)),
            model_count: AtomicU32::new(self.model_count.load(Ordering::Relaxed)),
            last_slash_name: std::sync::Mutex::new(self.last_slash_name.lock().unwrap().clone()),
            last_model_name: std::sync::Mutex::new(self.last_model_name.lock().unwrap().clone()),
        }
    }
}

impl Default for SkillTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillTelemetry {
    pub fn new() -> Self {
        Self {
            slash_count: AtomicU32::new(0),
            model_count: AtomicU32::new(0),
            last_slash_name: std::sync::Mutex::new(None),
            last_model_name: std::sync::Mutex::new(None),
        }
    }

    /// Record a slash-triggered skill load (`/name args`).
    pub fn record_slash(&self, name: &str) {
        self.slash_count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut n) = self.last_slash_name.lock() {
            *n = Some(name.to_string());
        }
    }

    /// Record a model-triggered skill load (skill tool call).
    pub fn record_model(&self, name: &str) {
        self.model_count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut n) = self.last_model_name.lock() {
            *n = Some(name.to_string());
        }
    }

    /// Snapshot per-turn skill events and clear the per-turn state.
    ///
    /// Returns a `serde_json::Value` suitable for `set_turn_custom`.
    /// Session-level counters are NOT reset — they accumulate for the
    /// final `set_session_custom` call.
    pub fn snapshot_and_reset(&self) -> serde_json::Value {
        let slash = self.last_slash_name.lock().unwrap().take();
        let model = self.last_model_name.lock().unwrap().take();

        let mut obj = serde_json::Map::new();
        if let Some(name) = slash {
            obj.insert("skill_trigger".into(), json!("slash"));
            obj.insert("skill_name".into(), json!(name));
        }
        if let Some(name) = model {
            obj.insert("skill_model_trigger".into(), json!("tool"));
            obj.insert("skill_model_name".into(), json!(name));
        }
        serde_json::Value::Object(obj)
    }

    /// Session-level counters for `set_session_custom`.
    pub fn session_snapshot(&self) -> serde_json::Value {
        json!({
            "skill_slash_count": self.slash_count.load(Ordering::Relaxed),
            "skill_model_count": self.model_count.load(Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_is_null_object() {
        let tel = SkillTelemetry::new();
        let snap = tel.snapshot_and_reset();
        // Empty object (no skill events this turn)
        assert_eq!(snap, json!({}));
    }

    #[test]
    fn slash_record_appears_in_snapshot() {
        let tel = SkillTelemetry::new();
        tel.record_slash("review");
        let snap = tel.snapshot_and_reset();
        assert_eq!(snap["skill_trigger"], json!("slash"));
        assert_eq!(snap["skill_name"], json!("review"));
    }

    #[test]
    fn model_record_appears_in_snapshot() {
        let tel = SkillTelemetry::new();
        tel.record_model("code-review");
        let snap = tel.snapshot_and_reset();
        assert_eq!(snap["skill_model_trigger"], json!("tool"));
        assert_eq!(snap["skill_model_name"], json!("code-review"));
    }

    #[test]
    fn both_triggers_in_one_turn() {
        let tel = SkillTelemetry::new();
        tel.record_slash("review");
        tel.record_model("design-review");
        let snap = tel.snapshot_and_reset();
        assert_eq!(snap["skill_trigger"], json!("slash"));
        assert_eq!(snap["skill_model_trigger"], json!("tool"));
    }

    #[test]
    fn snapshot_clears_per_turn_state() {
        let tel = SkillTelemetry::new();
        tel.record_slash("review");
        tel.snapshot_and_reset();
        // Second snapshot should be empty
        let snap2 = tel.snapshot_and_reset();
        assert_eq!(snap2, json!({}));
    }

    #[test]
    fn session_counters_accumulate() {
        let tel = SkillTelemetry::new();
        tel.record_slash("a");
        tel.record_slash("b");
        tel.record_model("c");
        tel.snapshot_and_reset();

        let session = tel.session_snapshot();
        assert_eq!(session["skill_slash_count"], json!(2));
        assert_eq!(session["skill_model_count"], json!(1));

        // More events in next turn
        tel.record_model("d");
        tel.snapshot_and_reset();

        let session = tel.session_snapshot();
        assert_eq!(session["skill_slash_count"], json!(2));
        assert_eq!(session["skill_model_count"], json!(2));
    }
}
