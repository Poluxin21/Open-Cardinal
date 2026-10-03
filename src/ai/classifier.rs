//! `type: "model"` rules: telemetry → feature vector → ONNX model → reaction.
//!
//! ```json
//! {
//!   "type": "model", "model": "anomaly-v1",
//!   "features": [ { "key": "cpu_temp", "default": 0 }, { "key": "fuel", "scale": 0.01 } ],
//!   "decide": { "mode": "threshold",
//!               "bands": [ { "ge": 0.9, "action": "SHUTDOWN", "cmd_name": "AI_ANOMALY", "priority": 400 },
//!                          { "ge": 0.6, "action": "RESTART",  "priority": 200 } ] }
//! }
//! ```
//! or `"decide": { "mode": "argmax", "classes": [ {"action":"IDLE"}, {"action":"SHUTDOWN"} ], "min_confidence": 0.7 }`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use ort::value::{Tensor, TensorElementType};
use serde::Deserialize;
use serde_json::Value;

use super::models::LoadedModel;
use crate::engine::types::{EvalCtx, RuleBackend, RuleError, RuleErrorKind, RuleKind, RuleOutput, parse_action};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    features: Vec<FeatureSpec>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    output: Option<String>,
    decide: DecideSpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeatureSpec {
    key: String,
    /// Used when the telemetry key is absent; without it a missing key is an error.
    #[serde(default)]
    default: Option<f32>,
    #[serde(default = "one")]
    scale: f32,
    #[serde(default)]
    offset: f32,
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReactionSpec {
    pub action: String,
    #[serde(default)]
    pub cmd_name: Option<String>,
    #[serde(default, deserialize_with = "string_map")]
    pub params: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub priority: Option<i32>,
}

fn string_map<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<BTreeMap<String, String>>, D::Error> {
    let m: Option<BTreeMap<String, Value>> = Option::deserialize(d)?;
    Ok(m.map(|m| {
        m.into_iter()
            .map(|(k, v)| {
                (
                    k,
                    match v {
                        Value::String(s) => s,
                        other => other.to_string(),
                    },
                )
            })
            .collect()
    }))
}

impl ReactionSpec {
    pub(super) fn validate(&self) -> Result<(), String> {
        parse_action(&self.action)
            .map(|_| ())
            .ok_or_else(|| format!("unknown action '{}'", crate::util::log_safe(&self.action)))
    }

    pub(super) fn to_output(&self, evidence: String) -> RuleOutput {
        RuleOutput {
            action: self.action.clone(),
            cmd_name: self.cmd_name.clone(),
            params: self.params.clone(),
            priority: self.priority,
            evidence: Some(evidence),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
enum DecideSpec {
    Threshold {
        #[serde(default)]
        index: usize,
        bands: Vec<BandSpec>,
    },
    Argmax {
        classes: Vec<ReactionSpec>,
        #[serde(default)]
        min_confidence: f32,
        /// Treat outputs as logits and apply softmax (default: they are already probabilities).
        #[serde(default)]
        softmax: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BandSpec {
    ge: f32,
    #[serde(flatten)]
    reaction: ReactionSpec,
}

enum Decide {
    Threshold { index: usize, bands: Vec<(f32, ReactionSpec)> },
    Argmax { classes: Vec<ReactionSpec>, min_confidence: f32, softmax: bool },
}

struct Feature {
    key: String,
    default: Option<f32>,
    scale: f32,
    offset: f32,
}

pub struct ModelRule {
    name: String,
    model: Arc<LoadedModel>,
    features: Vec<Feature>,
    input: String,
    output: String,
    decide: Decide,
}

impl ModelRule {
    pub fn new(name: &str, model: Arc<LoadedModel>, manifest: &Value) -> Result<Self, String> {
        let spec: Spec =
            serde_json::from_value(super::models::definition(manifest)).map_err(|e| format!("rule '{name}': {e}"))?;
        if spec.features.is_empty() || spec.features.len() > 1024 {
            return Err("features must list between 1 and 1024 telemetry keys".into());
        }
        // contract with the model
        let input = match spec.input {
            Some(n) => model.input(&n).ok_or_else(|| format!("model has no input '{n}'"))?.clone(),
            None => model.inputs.first().ok_or("model has no inputs")?.clone(),
        };
        if input.ty != Some(TensorElementType::Float32) {
            return Err(format!("model input '{}' must be a float32 tensor", input.name));
        }
        if input.shape.len() != 2 || (input.shape[1] != -1 && input.shape[1] as usize != spec.features.len()) {
            return Err(format!(
                "model input '{}' has shape {:?}, expected [batch, {}] for {} features",
                input.name,
                input.shape,
                spec.features.len(),
                spec.features.len()
            ));
        }
        let output = match spec.output {
            Some(n) => {
                model.outputs.iter().find(|o| o.name == n).ok_or_else(|| format!("model has no output '{n}'"))?.clone()
            }
            None => model.outputs.first().ok_or("model has no outputs")?.clone(),
        };
        if output.ty != Some(TensorElementType::Float32) {
            return Err(format!("model output '{}' must be a float32 tensor", output.name));
        }

        let decide = match spec.decide {
            DecideSpec::Threshold { index, mut bands } => {
                if bands.is_empty() {
                    return Err("threshold decision needs at least one band".into());
                }
                for b in &bands {
                    b.reaction.validate()?;
                    if !b.ge.is_finite() {
                        return Err("band thresholds must be finite numbers".into());
                    }
                }
                bands.sort_by(|a, b| b.ge.total_cmp(&a.ge));
                Decide::Threshold { index, bands: bands.into_iter().map(|b| (b.ge, b.reaction)).collect() }
            }
            DecideSpec::Argmax { classes, min_confidence, softmax } => {
                if classes.is_empty() {
                    return Err("argmax decision needs at least one class".into());
                }
                for c in &classes {
                    c.validate()?;
                }
                if !(0.0..=1.0).contains(&min_confidence) {
                    return Err("min_confidence must be between 0 and 1".into());
                }
                Decide::Argmax { classes, min_confidence, softmax }
            }
        };
        Ok(Self {
            name: name.to_string(),
            model,
            features: spec
                .features
                .into_iter()
                .map(|f| Feature { key: f.key, default: f.default, scale: f.scale, offset: f.offset })
                .collect(),
            input: input.name,
            output: output.name,
            decide,
        })
    }

    fn feature_vector(&self, cx: &EvalCtx<'_>) -> Result<Vec<f32>, RuleError> {
        let mut v = Vec::with_capacity(self.features.len());
        for f in &self.features {
            let x = match cx.input.telemetry.get(&f.key) {
                Some(s) => parse_number(s)
                    .ok_or_else(|| RuleError::contract(format!("telemetry '{}' is not a number", f.key)))?,
                None => f.default.ok_or_else(|| RuleError::contract(format!("telemetry '{}' is missing", f.key)))?,
            };
            let x = x * f.scale + f.offset;
            if !x.is_finite() {
                return Err(RuleError::contract(format!("feature '{}' is not finite", f.key)));
            }
            v.push(x);
        }
        Ok(v)
    }
}

/// `"80%"`, `" 12.5 "`, `"1e3"` → number. NaN/inf are refused.
pub(super) fn parse_number(s: &str) -> Option<f32> {
    let t = s.trim().trim_end_matches('%').trim();
    t.parse::<f32>().ok().filter(|v| v.is_finite())
}

fn softmax(v: &[f32]) -> Vec<f32> {
    let max = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = v.iter().map(|x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

impl RuleBackend for ModelRule {
    fn kind(&self) -> RuleKind {
        RuleKind::Model
    }

    fn evaluate(&self, cx: &EvalCtx<'_>) -> Result<Option<RuleOutput>, RuleError> {
        let features = self.feature_vector(cx)?;
        let n = features.len();
        let scores: Vec<f32> = self
            .model
            .with_session(|session| {
                let tensor = Tensor::from_array((vec![1usize, n], features)).map_err(|e| e.to_string())?;
                let outputs = session.run(ort::inputs![self.input.as_str() => tensor]).map_err(|e| e.to_string())?;
                let (_, data) = outputs[self.output.as_str()].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
                Ok(data.to_vec())
            })
            .map_err(|e| RuleError::runtime(format!("inference failed: {e}")))?;
        if Instant::now() >= cx.deadline {
            return Err(RuleError::new(
                RuleErrorKind::Timeout,
                "inference finished after the deadline; result discarded",
            ));
        }
        if scores.iter().any(|s| !s.is_finite()) {
            return Err(RuleError::runtime("model produced a non-finite score"));
        }
        let evidence = |detail: String| format!("{} {detail}", self.model.evidence());

        match &self.decide {
            Decide::Threshold { index, bands } => {
                let score = *scores.get(*index).ok_or_else(|| {
                    RuleError::contract(format!("model output has {} values, index {index} requested", scores.len()))
                })?;
                Ok(bands
                    .iter()
                    .find(|(ge, _)| score >= *ge)
                    .map(|(ge, r)| r.to_output(evidence(format!("score={score:.4} >= {ge}")))))
            }
            Decide::Argmax { classes, min_confidence, softmax: apply } => {
                let probs = if *apply { softmax(&scores) } else { scores.clone() };
                let (best, p) = probs
                    .iter()
                    .copied()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .ok_or_else(|| RuleError::runtime("model produced no output"))?;
                if p < *min_confidence {
                    return Ok(None);
                }
                let r = classes.get(best).ok_or_else(|| {
                    RuleError::contract(format!("model predicted class {best} but only {} are declared", classes.len()))
                })?;
                Ok(Some(r.to_output(evidence(format!("class={best} confidence={p:.4}")))))
            }
        }
    }
}

impl std::fmt::Debug for ModelRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ModelRule({})", self.name)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ai::models::OnnxFactory;
    use crate::ai::testutil;
    use crate::config::{Limits, Paths};
    use crate::engine::AiRuleFactory;
    use crate::engine::mem::testing::mem;
    use crate::engine::types::PulseInput;
    use crate::store::Store;
    use crate::tenant::TenantRegistry;
    use std::collections::HashMap;
    use std::time::Duration;

    pub(crate) struct Env {
        pub _dir: tempfile::TempDir,
        pub paths: Paths,
        pub factory: OnnxFactory,
        pub tenants: TenantRegistry,
    }

    pub(crate) fn env(models_json: &str, files: &[(&str, Vec<u8>)]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::create_dir_all(paths.models_dir()).unwrap();
        std::fs::write(paths.models_file(), models_json).unwrap();
        for (name, bytes) in files {
            let p = paths.models_dir().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        let tenants = TenantRegistry::load(paths.clone(), Limits::default(), crate::config::AuthMode::Open).unwrap();
        tenants
            .edit(|f| {
                f.add_tenant("acme")?;
                let t = f.tenants.iter_mut().find(|t| t.id == "acme").unwrap();
                t.ai.enabled = true;
                t.ai.models = vec!["m".into()];
                Ok(())
            })
            .unwrap();
        let factory = OnnxFactory::new(&paths);
        Env { _dir: dir, paths, factory, tenants }
    }

    fn build(env: &Env, manifest: serde_json::Value) -> Result<Arc<dyn RuleBackend>, String> {
        let tenant = env.tenants.get("acme").unwrap();
        env.factory.build(&tenant, "r", &manifest, env.paths.rules_dir().as_path())
    }

    fn run(rule: &Arc<dyn RuleBackend>, telemetry: &[(&str, &str)]) -> Result<Option<RuleOutput>, RuleError> {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let inp = PulseInput {
            agent_id: "a".into(),
            tenant: "acme".into(),
            timestamp: 0,
            trace_id: "t".into(),
            telemetry: telemetry.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>(),
        };
        let limits = Limits::default();
        let cx = EvalCtx {
            input: &inp,
            mem: mem(&store, "acme"),
            limits: &limits,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        rule.evaluate(&cx)
    }

    /// score = 1.0 * x0 + 0.0 (identity on one feature)
    fn identity_model() -> Vec<u8> {
        testutil::linear_model(&[1.0], &[0.0], 1, 1)
    }

    const MODELS: &str = r#"{"models":[{"name":"m","path":"m.onnx"}]}"#;

    #[test]
    fn threshold_bands_pick_the_highest_matching_reaction() {
        let env = env(MODELS, &[("m.onnx", identity_model())]);
        let rule = build(&env, serde_json::json!({
            "type": "model", "model": "m",
            "features": [{ "key": "temp", "scale": 0.01 }],
            "decide": { "mode": "threshold", "bands": [
                { "ge": 0.6, "action": "RESTART", "priority": 200 },
                { "ge": 0.9, "action": "SHUTDOWN", "cmd_name": "AI_ANOMALY", "priority": 400, "params": { "why": "hot" } }
            ]}
        }))
        .unwrap();

        let out = run(&rule, &[("temp", "95")]).unwrap().unwrap();
        assert_eq!(
            (out.action.as_str(), out.priority, out.cmd_name.as_deref()),
            ("SHUTDOWN", Some(400), Some("AI_ANOMALY"))
        );
        assert_eq!(out.params.unwrap()["why"], "hot");
        assert!(out.evidence.unwrap().contains("score=0.9500"));

        assert_eq!(run(&rule, &[("temp", "70")]).unwrap().unwrap().action, "RESTART");
        assert!(run(&rule, &[("temp", "10")]).unwrap().is_none(), "below every band: no opinion");
    }

    #[test]
    fn argmax_with_confidence_floor() {
        // 1 feature → 2 classes: logits [x, -x]
        let env = env(MODELS, &[("m.onnx", testutil::linear_model(&[1.0, -1.0], &[0.0, 0.0], 1, 2))]);
        let rule = build(
            &env,
            serde_json::json!({
                "type": "model", "model": "m",
                "features": [{ "key": "x" }],
                "decide": { "mode": "argmax", "softmax": true, "min_confidence": 0.9,
                    "classes": [ { "action": "SHUTDOWN", "cmd_name": "POS" }, { "action": "IDLE" } ] }
            }),
        )
        .unwrap();
        assert_eq!(run(&rule, &[("x", "5")]).unwrap().unwrap().cmd_name.as_deref(), Some("POS"));
        assert_eq!(run(&rule, &[("x", "-5")]).unwrap().unwrap().action, "IDLE");
        assert!(run(&rule, &[("x", "0.1")]).unwrap().is_none(), "not confident enough");
    }

    #[test]
    fn missing_or_garbage_telemetry_is_a_contract_error_unless_defaulted() {
        let env = env(MODELS, &[("m.onnx", identity_model())]);
        let strict = build(
            &env,
            serde_json::json!({
                "type": "model", "model": "m", "features": [{ "key": "t" }],
                "decide": { "mode": "threshold", "bands": [{ "ge": 0.5, "action": "RESTART" }] }
            }),
        )
        .unwrap();
        assert_eq!(run(&strict, &[]).unwrap_err().kind, RuleErrorKind::Contract);
        assert_eq!(run(&strict, &[("t", "hot")]).unwrap_err().kind, RuleErrorKind::Contract);
        assert_eq!(run(&strict, &[("t", "NaN")]).unwrap_err().kind, RuleErrorKind::Contract);
        let lenient = build(
            &env,
            serde_json::json!({
                "type": "model", "model": "m", "features": [{ "key": "t", "default": 1.0 }],
                "decide": { "mode": "threshold", "bands": [{ "ge": 0.5, "action": "RESTART" }] }
            }),
        )
        .unwrap();
        assert_eq!(run(&lenient, &[]).unwrap().unwrap().action, "RESTART");
        assert_eq!(run(&lenient, &[("t", "80%")]).unwrap().unwrap().action, "RESTART", "percent signs are accepted");
    }

    #[test]
    fn manifest_must_match_the_model() {
        let env = env(MODELS, &[("m.onnx", identity_model())]);
        let wrong_features = build(
            &env,
            serde_json::json!({
                "type": "model", "model": "m", "features": [{ "key": "a" }, { "key": "b" }],
                "decide": { "mode": "threshold", "bands": [{ "ge": 0.5, "action": "RESTART" }] }
            }),
        );
        assert!(wrong_features.err().unwrap().contains("expected [batch, 2]"));
        let bad_action = build(
            &env,
            serde_json::json!({
                "type": "model", "model": "m", "features": [{ "key": "a" }],
                "decide": { "mode": "threshold", "bands": [{ "ge": 0.5, "action": "EXPLODE" }] }
            }),
        );
        assert!(bad_action.err().unwrap().contains("unknown action"));
        let typo = build(
            &env,
            serde_json::json!({
                "type": "model", "model": "m", "features": [{ "key": "a" }], "decied": {}
            }),
        );
        assert!(typo.is_err(), "typos in manifests must not be silently ignored");
    }

    #[test]
    fn tenants_cannot_use_models_outside_their_allow_list() {
        let env = env(
            r#"{"models":[{"name":"m","path":"m.onnx"},{"name":"secret","path":"m.onnx"}]}"#,
            &[("m.onnx", identity_model())],
        );
        let r = build(
            &env,
            serde_json::json!({
                "type": "model", "model": "secret", "features": [{ "key": "a" }],
                "decide": { "mode": "threshold", "bands": [{ "ge": 0.5, "action": "RESTART" }] }
            }),
        );
        assert!(r.err().unwrap().contains("not allowed"));
    }

    #[test]
    fn model_files_are_confined_and_can_be_pinned() {
        // traversal out of models/
        let env1 = env(r#"{"models":[{"name":"m","path":"../config/admin.token"}]}"#, &[]);
        let r = env1.factory.model("m");
        assert!(r.err().unwrap().contains("no '..'"));
        let env2 = env(r#"{"models":[{"name":"m","path":"/etc/passwd"}]}"#, &[]);
        assert!(env2.factory.model("m").is_err());
        // hash pin
        let env3 = env(
            r#"{"models":[{"name":"m","path":"m.onnx","sha256":"00000000000000000000000000000000000000000000000000000000deadbeef"}]}"#,
            &[("m.onnx", identity_model())],
        );
        assert!(env3.factory.model("m").err().unwrap().contains("pinned sha256"));
    }

    #[test]
    fn unregistered_model_is_a_clear_error() {
        let env = env(r#"{"models":[]}"#, &[]);
        assert!(env.factory.model("m").err().unwrap().contains("not registered"));
    }
}
