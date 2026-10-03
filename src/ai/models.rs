//! Model registry (`config/models.json`) and the factory that builds AI rules.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{TensorElementType, ValueType};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::classifier::ModelRule;
use crate::config::Paths;
use crate::engine::{AiRuleFactory, RuleBackend};
use crate::tenant::Tenant;
use crate::util;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    pub name: String,
    /// Relative to `<home>/models/`; `..` and absolute paths are refused.
    pub path: String,
    /// Lower-case hex SHA-256 of the model file; the model is refused if it does not match.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Intra-op threads. 1 (default) keeps results reproducible and leaves CPU to the rules.
    #[serde(default)]
    pub intra_threads: Option<usize>,
    /// Number of sessions (concurrent inferences) kept for this model.
    #[serde(default)]
    pub pool: Option<usize>,
    /// `tokenizer.json` (relative to `models/`), required by prompt rules.
    #[serde(default)]
    pub tokenizer: Option<String>,
    /// `plain` (default) or `chatml`.
    #[serde(default)]
    #[cfg_attr(not(feature = "prompt"), allow(dead_code))]
    pub chat_template: Option<String>,
    /// Longest prompt (in tokens) a prompt rule may build.
    #[serde(default)]
    #[cfg_attr(not(feature = "prompt"), allow(dead_code))]
    pub max_context: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ModelsFile {
    models: Vec<ModelDef>,
}

#[derive(Clone, Debug)]
pub struct IoInfo {
    pub name: String,
    pub ty: Option<TensorElementType>,
    /// -1 for dynamic dimensions.
    pub shape: Vec<i64>,
}

pub struct LoadedModel {
    pub def: ModelDef,
    pub sha256: String,
    pub inputs: Vec<IoInfo>,
    pub outputs: Vec<IoInfo>,
    sessions: Vec<Mutex<Session>>,
    next: AtomicUsize,
    #[cfg_attr(not(feature = "prompt"), allow(dead_code))]
    pub tokenizer_path: Option<PathBuf>,
}

impl LoadedModel {
    /// Run `f` on one of the pooled sessions (a free one when possible).
    pub fn with_session<R>(&self, f: impl FnOnce(&mut Session) -> Result<R, String>) -> Result<R, String> {
        let n = self.sessions.len();
        let start = self.next.fetch_add(1, Relaxed);
        for i in 0..n {
            if let Ok(mut g) = self.sessions[(start + i) % n].try_lock() {
                return f(&mut g);
            }
        }
        let mut g = self.sessions[start % n].lock().unwrap_or_else(|p| p.into_inner());
        f(&mut g)
    }

    pub fn evidence(&self) -> String {
        format!("model={} sha256={}", self.def.name, &self.sha256[..16])
    }

    pub fn input(&self, name: &str) -> Option<&IoInfo> {
        self.inputs.iter().find(|i| i.name == name)
    }
}

struct Defs {
    models: HashMap<String, ModelDef>,
    mtime: Option<SystemTime>,
}

pub struct OnnxFactory {
    paths: Paths,
    defs: Mutex<Defs>,
    loaded: Mutex<HashMap<String, Arc<LoadedModel>>>,
}

impl OnnxFactory {
    pub fn new(paths: &Paths) -> Self {
        Self {
            paths: paths.clone(),
            defs: Mutex::new(Defs { models: HashMap::new(), mtime: None }),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// Re-read `models.json` when it changed (called on every rule reload).
    fn refresh_defs(&self) -> Result<(), String> {
        let path = self.paths.models_file();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut d = self.defs.lock().unwrap_or_else(|p| p.into_inner());
        if d.mtime == mtime && !d.models.is_empty() {
            return Ok(());
        }
        let file: ModelsFile = match std::fs::read_to_string(&path) {
            Ok(t) if !t.trim().is_empty() => {
                serde_json::from_str(&t).map_err(|e| format!("{}: {e}", path.display()))?
            }
            _ => ModelsFile::default(),
        };
        let mut models = HashMap::new();
        for m in file.models {
            if models.insert(m.name.clone(), m.clone()).is_some() {
                return Err(format!("models.json: duplicate model '{}'", m.name));
            }
        }
        // a changed definition invalidates the loaded session
        let mut loaded = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        loaded.retain(|name, lm| models.get(name).is_some_and(|d| d.path == lm.def.path && d.sha256 == lm.def.sha256));
        d.models = models;
        d.mtime = mtime;
        Ok(())
    }

    pub fn model(&self, name: &str) -> Result<Arc<LoadedModel>, String> {
        self.refresh_defs()?;
        if let Some(m) = self.loaded.lock().unwrap_or_else(|p| p.into_inner()).get(name) {
            return Ok(m.clone());
        }
        let def = self
            .defs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .models
            .get(name)
            .cloned()
            .ok_or_else(|| format!("model '{name}' is not registered in config/models.json"))?;
        let model = Arc::new(self.load(def)?);
        self.loaded.lock().unwrap_or_else(|p| p.into_inner()).insert(name.to_string(), model.clone());
        Ok(model)
    }

    fn confined(&self, rel: &str) -> Result<PathBuf, String> {
        let p = Path::new(rel);
        let ok = !rel.is_empty() && !p.is_absolute() && p.components().all(|c| matches!(c, Component::Normal(_)));
        if !ok {
            return Err(format!(
                "path '{}' must be relative to the models directory and contain no '..'",
                util::log_safe(rel)
            ));
        }
        Ok(self.paths.models_dir().join(p))
    }

    fn load(&self, def: ModelDef) -> Result<LoadedModel, String> {
        let path = self.confined(&def.path)?;
        let bytes = std::fs::read(&path).map_err(|e| format!("cannot read model {}: {e}", path.display()))?;
        let sha256 = util::hex(&Sha256::digest(&bytes));
        if let Some(pin) = &def.sha256
            && !pin.eq_ignore_ascii_case(&sha256)
        {
            return Err(format!("model '{}' does not match its pinned sha256 (file is {sha256})", def.name));
        }
        let threads = def.intra_threads.unwrap_or(1).max(1);
        let pool = def.pool.unwrap_or(2).clamp(1, 16);
        let mut sessions = Vec::new();
        let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
        for i in 0..pool {
            let build = || -> ort::Result<Session> {
                let mut b = Session::builder()?
                    .with_optimization_level(GraphOptimizationLevel::Level3)?
                    .with_intra_threads(threads)?
                    .with_inter_threads(1)?;
                b.commit_from_memory(&bytes)
            };
            let session = build().map_err(|e| format!("cannot load model '{}': {e}", def.name))?;
            if i == 0 {
                inputs = session.inputs().iter().map(|o| io_info(o.name(), o.dtype())).collect();
                outputs = session.outputs().iter().map(|o| io_info(o.name(), o.dtype())).collect();
            }
            sessions.push(Mutex::new(session));
        }
        let tokenizer_path = def.tokenizer.as_deref().map(|t| self.confined(t)).transpose()?;
        tracing::info!(model = %def.name, sha256 = %&sha256[..16], pool, "ONNX model loaded");
        Ok(LoadedModel { def, sha256, inputs, outputs, sessions, next: AtomicUsize::new(0), tokenizer_path })
    }
}

/// The rule definition proper: the manifest minus the keys the registry and this factory consume
/// (`type`, `model`, `enabled`, `priority_cap`, `timeout_ms`), so a rule type only declares what it uses.
pub(super) fn definition(manifest: &Value) -> Value {
    let mut def = manifest.clone();
    if let Some(obj) = def.as_object_mut() {
        for key in ["type", "model", "enabled", "priority_cap", "timeout_ms"] {
            obj.remove(key);
        }
    }
    def
}

fn io_info(name: &str, dtype: &ValueType) -> IoInfo {
    match dtype {
        ValueType::Tensor { ty, shape, .. } => {
            IoInfo { name: name.to_string(), ty: Some(*ty), shape: shape.iter().copied().collect() }
        }
        _ => IoInfo { name: name.to_string(), ty: None, shape: vec![] },
    }
}

impl AiRuleFactory for OnnxFactory {
    #[cfg_attr(not(feature = "prompt"), allow(unused_variables))]
    fn build(&self, tenant: &Tenant, name: &str, manifest: &Value, dir: &Path) -> Result<Arc<dyn RuleBackend>, String> {
        let ty = manifest.get("type").and_then(Value::as_str).unwrap_or("");
        let model_name = manifest
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| "AI rule needs a \"model\" (a name from config/models.json)".to_string())?;
        if !tenant.ai.models.iter().any(|m| m == model_name) {
            return Err(format!("tenant '{}' is not allowed to use model '{model_name}' (ai.models)", tenant.id));
        }
        let model = self.model(model_name)?;
        match ty {
            "model" => Ok(Arc::new(ModelRule::new(name, model, manifest)?)),
            #[cfg(feature = "prompt")]
            "prompt" => Ok(Arc::new(super::prompt::PromptRule::new(name, model, manifest, dir)?)),
            #[cfg(not(feature = "prompt"))]
            "prompt" => Err("this build has no prompt-rule support (rebuild with --features prompt)".into()),
            other => Err(format!("unknown AI rule type '{other}'")),
        }
    }
}
