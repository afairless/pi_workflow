//! supervisor.config.json schema + precedence (Contract 4) — a port of the
//! TS supervisor's `coerceConfig`, scoped to the pi-plan fields (no
//! live-transcript-widget keys in v1).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Built-in per-worker stall ceiling when nothing is configured.
pub const DEFAULT_MAX_TURNS: u32 = 60;

/// Built-in verifier stall ceiling: verification is read-only and bounded,
/// so it must fail fast rather than hold a pass open.
pub const DEFAULT_VERIFIER_MAX_TURNS: u32 = 20;

/// Built-in verifier wall-clock ceiling (10 minutes). Deliberately much
/// shorter than the row worker's 6000 s [`crate::supervise::DEFAULT_TURN_TIMEOUT`].
pub const DEFAULT_VERIFIER_TURN_TIMEOUT_MS: u64 = 600_000;

/// Built-in worker model when nothing is configured.
pub const DEFAULT_MODEL: &str = "openrouter/deepseek/deepseek-v4.1-flash";

/// Model written to a freshly-created global config scaffold — the tuned,
/// dated snapshot, distinct from the rolling `DEFAULT_MODEL` fallback.
/// `DEFAULT_MODEL` stays the in-code fallback; the scaffold pin is what a
/// user sees once the global file exists.
pub const DEFAULT_GLOBAL_MODEL: &str = "openrouter/deepseek/deepseek-v4.1-flash";

/// Serialized default global config (scaffold file contents).
pub fn default_global_config_json() -> String {
    format!("{{ \"model\": \"{DEFAULT_GLOBAL_MODEL}\" }}\n")
}

/// Per-step override (Contract 4 config schema), keyed by row number.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepOverride {
    pub model: Option<String>,
    pub max_turns: Option<u32>,
}

/// The verifier's bounds (Contract 4, verifier section). The read-only
/// verifier is deliberately narrow: a hard turn ceiling and a short wall
/// clock, both applied as [`crate::worker::WorkerSpawnOpts`] fields (never
/// argv), plus an optional model of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifierConfig {
    /// Verifier model; falls back to the worker `model`, then `DEFAULT_MODEL`.
    pub model: Option<String>,
    /// Turn-based stall ceiling for the verifier.
    pub max_turns: u32,
    /// Wall-clock ceiling for the verifier, in milliseconds.
    pub turn_timeout_ms: u64,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        VerifierConfig {
            model: None,
            max_turns: DEFAULT_VERIFIER_MAX_TURNS,
            turn_timeout_ms: DEFAULT_VERIFIER_TURN_TIMEOUT_MS,
        }
    }
}

impl VerifierConfig {
    /// The wall-clock ceiling as a `Duration`.
    pub fn turn_timeout(&self) -> Duration {
        Duration::from_millis(self.turn_timeout_ms)
    }
}

/// supervisor.config.json schema (Contract 4). All fields optional; defaults
/// (60 turns, `DEFAULT_MODEL`, the built-in [`VerifierConfig`]) back the gaps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorConfig {
    /// Global per-worker stall ceiling.
    pub max_turns: Option<u32>,
    /// Model for spawned workers (overrides the default when set).
    pub model: Option<String>,
    /// Per-row overrides keyed by row number.
    #[serde(default)]
    pub steps: BTreeMap<u64, StepOverride>,
    /// The read-only verifier's bounds (absent → [`VerifierConfig::default`]).
    #[serde(default)]
    pub verifier: VerifierConfig,
}

/// Read a config file (`supervisor.config.json` or `--config PATH`).
/// Any read/parse failure yields the default config — never throws.
pub fn read_config_file(path: &Path) -> SupervisorConfig {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return SupervisorConfig::default(),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return SupervisorConfig::default(),
    };
    coerce_config(&parsed)
}

/// Shape-check an unknown config value; unknown fields are ignored, wrong
/// types fall back to defaults (mirrors the TS `coerceConfig`).
pub fn coerce_config(parsed: &serde_json::Value) -> SupervisorConfig {
    let Some(object) = parsed.as_object() else {
        return SupervisorConfig::default();
    };
    let mut steps = BTreeMap::new();
    if let Some(serde_json::Value::Object(step_map)) = object.get("steps") {
        for (key, value) in step_map {
            let Some(row_number) = key.parse::<u64>().ok() else {
                continue; // junk keys are ignored
            };
            let Some(step_obj) = value.as_object() else {
                continue;
            };
            let mut override_ = StepOverride::default();
            if let Some(model) = step_obj.get("model").and_then(serde_json::Value::as_str)
                && !model.is_empty()
            {
                override_.model = Some(model.to_string());
            }
            if let Some(max_turns) = int_option(step_obj.get("maxTurns")) {
                override_.max_turns = Some(max_turns);
            }
            if override_.model.is_some() || override_.max_turns.is_some() {
                steps.insert(row_number, override_);
            }
        }
    }
    SupervisorConfig {
        max_turns: int_option(object.get("maxTurns")),
        model: object
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|m| !m.is_empty())
            .map(String::from),
        steps,
        verifier: coerce_verifier(object.get("verifier")),
    }
}

/// Shape-check the `verifier` block: a wrong-typed or non-positive bound
/// falls back to the built-in default rather than disabling the guard.
fn coerce_verifier(value: Option<&serde_json::Value>) -> VerifierConfig {
    let mut verifier = VerifierConfig::default();
    let Some(object) = value.and_then(serde_json::Value::as_object) else {
        return verifier;
    };
    if let Some(model) = object
        .get("model")
        .and_then(serde_json::Value::as_str)
        .filter(|m| !m.is_empty())
    {
        verifier.model = Some(model.to_string());
    }
    if let Some(max_turns) = int_option(object.get("maxTurns")).filter(|n| *n > 0) {
        verifier.max_turns = max_turns;
    }
    if let Some(timeout) = int_option_u64(object.get("turnTimeoutMs")).filter(|n| *n > 0) {
        verifier.turn_timeout_ms = timeout;
    }
    verifier
}

/// Non-negative integer JSON field → `u32` (clamped); anything else → `None`.
fn int_option(value: Option<&serde_json::Value>) -> Option<u32> {
    let n = value?.as_f64()?;
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 {
        Some(n.min(u32::MAX as f64) as u32)
    } else {
        None
    }
}

/// Non-negative integer JSON field → `u64` (clamped); anything else → `None`.
fn int_option_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    let n = value?.as_f64()?;
    if n.is_finite() && n >= 0.0 && n.fract() == 0.0 {
        Some(n.min(u64::MAX as f64) as u64)
    } else {
        None
    }
}

/// Stall ceiling for one row: `steps.<n>.maxTurns` > config `max_turns` >
/// default 40.
pub fn resolve_max_turns(config: &SupervisorConfig, row_number: u64) -> u32 {
    config
        .steps
        .get(&row_number)
        .and_then(|s| s.max_turns)
        .or(config.max_turns)
        .unwrap_or(DEFAULT_MAX_TURNS)
}

/// Model for one row: `steps.<n>.model` > config `model` > default model.
pub fn resolve_model(config: &SupervisorConfig, row_number: u64) -> String {
    config
        .steps
        .get(&row_number)
        .and_then(|s| s.model.clone())
        .or_else(|| config.model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Model for the verifier: `verifier.model` > config `model` > default model.
/// The verifier deliberately has no per-step override — it is never a row.
pub fn resolve_verifier_model(config: &SupervisorConfig) -> String {
    config
        .verifier
        .model
        .clone()
        .or_else(|| config.model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(v: serde_json::Value) -> SupervisorConfig {
        coerce_config(&v)
    }

    #[test]
    fn coerce_returns_default_for_non_object_input() {
        for junk in [
            serde_json::Value::Null,
            serde_json::Value::String("x".into()),
            serde_json::Value::from(42),
            serde_json::Value::Bool(true),
            serde_json::Value::Array(vec![]),
        ] {
            assert_eq!(
                cfg(junk.clone()),
                SupervisorConfig::default(),
                "junk {junk:?}"
            );
        }
    }

    #[test]
    fn coerce_parses_max_turns_and_model() {
        let c = cfg(serde_json::json!({"maxTurns": 30, "model": "claude-3.5-sonnet"}));
        assert_eq!(c.max_turns, Some(30));
        assert_eq!(c.model.as_deref(), Some("claude-3.5-sonnet"));
    }

    #[test]
    fn coerce_ignores_wrong_types_and_empty_model() {
        let c = cfg(serde_json::json!({
            "maxTurns": "30",
            "model": "",
            "steps": "not-an-object",
        }));
        assert_eq!(c.max_turns, None);
        assert_eq!(c.model, None);
        assert!(c.steps.is_empty());
    }

    #[test]
    fn coerce_parses_step_overrides_and_skips_junk_keys() {
        let c = cfg(serde_json::json!({
            "steps": {
                "1": {"maxTurns": 5, "model": "fast"},
                "2": {"model": "big"},
                "junk": {"maxTurns": 1},
                "3": "not-an-object",
            }
        }));
        assert_eq!(c.steps.len(), 2);
        assert_eq!(c.steps[&1].max_turns, Some(5));
        assert_eq!(c.steps[&1].model.as_deref(), Some("fast"));
        assert_eq!(c.steps[&2].model.as_deref(), Some("big"));
        assert_eq!(c.steps[&2].max_turns, None);
    }

    #[test]
    fn missing_steps_field_is_empty() {
        let c = cfg(serde_json::json!({"maxTurns": 10}));
        assert!(c.steps.is_empty());
    }

    #[test]
    fn resolve_max_turns_precedence_is_step_then_config_then_default() {
        let c = cfg(serde_json::json!({
            "maxTurns": 20,
            "steps": {"7": {"maxTurns": 9}}
        }));
        assert_eq!(resolve_max_turns(&c, 7), 9, "step override wins");
        assert_eq!(
            resolve_max_turns(&c, 8),
            20,
            "config wins without step override"
        );
        let empty = SupervisorConfig::default();
        assert_eq!(resolve_max_turns(&empty, 1), DEFAULT_MAX_TURNS);
    }

    #[test]
    fn resolve_model_precedence_is_step_then_config_then_default() {
        let c = cfg(serde_json::json!({
            "model": "config-model",
            "steps": {"3": {"model": "step-model"}}
        }));
        assert_eq!(resolve_model(&c, 3), "step-model");
        assert_eq!(resolve_model(&c, 4), "config-model");
        let empty = SupervisorConfig::default();
        assert_eq!(resolve_model(&empty, 1), DEFAULT_MODEL);
    }

    #[test]
    fn absent_verifier_block_is_the_built_in_default() {
        let c = cfg(serde_json::json!({"model": "worker-model"}));
        assert_eq!(c.verifier, VerifierConfig::default());
        assert_eq!(c.verifier.max_turns, DEFAULT_VERIFIER_MAX_TURNS);
        assert_eq!(c.verifier.max_turns, 20);
        assert_eq!(c.verifier.turn_timeout_ms, DEFAULT_VERIFIER_TURN_TIMEOUT_MS);
        assert_eq!(c.verifier.turn_timeout_ms, 600_000);
        assert_eq!(c.verifier.turn_timeout(), Duration::from_secs(600));
        assert_eq!(c.verifier.model, None);
    }

    #[test]
    fn coerce_parses_the_verifier_block() {
        let c = cfg(serde_json::json!({
            "verifier": {"model": "verifier-model", "maxTurns": 5, "turnTimeoutMs": 90000}
        }));
        assert_eq!(c.verifier.model.as_deref(), Some("verifier-model"));
        assert_eq!(c.verifier.max_turns, 5);
        assert_eq!(c.verifier.turn_timeout(), Duration::from_millis(90_000));
    }

    #[test]
    fn verifier_bounds_reject_junk_rather_than_disabling_the_guard() {
        for junk in [
            serde_json::json!("verifier"),
            serde_json::json!({"verifier": 7}),
            serde_json::json!({"verifier": {"maxTurns": "5"}}),
            serde_json::json!({"verifier": {"maxTurns": 0, "turnTimeoutMs": 0}}),
            serde_json::json!({"verifier": {"maxTurns": -1, "turnTimeoutMs": 1.5}}),
            serde_json::json!({"verifier": {"model": ""}}),
        ] {
            let c = cfg(junk.clone());
            assert_eq!(c.verifier, VerifierConfig::default(), "junk {junk:?}");
        }
    }

    #[test]
    fn resolve_verifier_model_precedence_is_verifier_then_worker_then_default() {
        let both = cfg(serde_json::json!({
            "model": "worker-model",
            "verifier": {"model": "verifier-model"},
            "steps": {"1": {"model": "step-model"}}
        }));
        assert_eq!(resolve_verifier_model(&both), "verifier-model");

        let worker_only = cfg(serde_json::json!({
            "model": "worker-model",
            "steps": {"1": {"model": "step-model"}}
        }));
        assert_eq!(
            resolve_verifier_model(&worker_only),
            "worker-model",
            "the verifier is never a row: no step override applies"
        );

        let empty = SupervisorConfig::default();
        assert_eq!(resolve_verifier_model(&empty), DEFAULT_MODEL);
    }

    #[test]
    fn default_global_config_json_is_a_valid_scaffold() {
        let parsed: serde_json::Value = serde_json::from_str(default_global_config_json().as_str())
            .expect("scaffold parses as JSON");
        let c = coerce_config(&parsed);
        assert_eq!(c.model.as_deref(), Some(DEFAULT_GLOBAL_MODEL));
        assert_eq!(c.max_turns, None);
    }

    #[test]
    fn read_config_file_tolerates_missing_and_corrupt_files() {
        let missing = Path::new("/nonexistent-config.json");
        assert_eq!(read_config_file(missing), SupervisorConfig::default());

        let dir = std::env::temp_dir().join(format!("pi-plan-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("bad.json"), "{ nope").expect("write");
        let bad = read_config_file(&dir.join("bad.json"));
        assert_eq!(bad, SupervisorConfig::default());
        std::fs::write(dir.join("good.json"), r#"{"maxTurns": 33}"#).expect("write");
        let good = read_config_file(&dir.join("good.json"));
        assert_eq!(good.max_turns, Some(33));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
