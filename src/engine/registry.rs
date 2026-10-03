//! Rule discovery and the hot-swappable rule set.
//!
//! Layout (the first two lines are the original single-tenant layout and keep working):
//!
//! ```text
//! rules/default/*.lua            fallback rules for every agent        (tenant "default")
//! rules/<agent_id>/*.lua|wasm    rules of one agent                    (tenant "default")
//! tenants/<tenant>/rules/default/...      same, for another tenant
//! tenants/<tenant>/rules/<agent_id>/...
//! <rule>.rule.json               optional manifest (metadata, or AI rule definition)
//! ```
//!
//! An agent directory *replaces* `default` for that agent (same as before). Rules are
//! compiled once at load time; evaluating a pulse never touches the disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::Serialize;
use serde_json::Value;

use super::types::{RuleBackend, RuleKind};
use crate::tenant::Tenant;
use crate::util;

#[cfg(feature = "lua")]
const MAX_LUA_BYTES: u64 = 1024 * 1024;
#[cfg(feature = "wasm")]
const MAX_WASM_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const DEFAULT_DIR: &str = "default";

/// Outcome of compiling one rule file.
type BuildResult = Result<(RuleKind, Arc<dyn RuleBackend>), String>;

pub struct Rule {
    pub name: String,
    pub kind: RuleKind,
    pub path: PathBuf,
    pub backend: Arc<dyn RuleBackend>,
    /// Decisions of this rule never exceed this priority.
    pub priority_cap: Option<i32>,
    pub timeout_ms: Option<u64>,
}

#[derive(Default)]
pub struct TenantRules {
    pub default: Option<Vec<Arc<Rule>>>,
    pub agents: HashMap<String, Vec<Arc<Rule>>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoadIssue {
    pub tenant: String,
    pub path: String,
    pub message: String,
}

#[derive(Default)]
pub struct RuleSet {
    pub tenants: HashMap<String, TenantRules>,
    pub issues: Vec<LoadIssue>,
    pub loaded_at_ms: u64,
    pub generation: u64,
}

pub enum Lookup<'a> {
    Agent(&'a [Arc<Rule>]),
    Default(&'a [Arc<Rule>]),
    None,
}

impl RuleSet {
    pub fn lookup(&self, tenant: &str, agent: &str) -> Lookup<'_> {
        let Some(t) = self.tenants.get(tenant) else { return Lookup::None };
        if let Some(rules) = t.agents.get(agent) {
            return Lookup::Agent(rules);
        }
        match &t.default {
            Some(rules) => Lookup::Default(rules),
            None => Lookup::None,
        }
    }

    pub fn total_rules(&self) -> usize {
        self.tenants
            .values()
            .map(|t| t.default.as_ref().map_or(0, Vec::len) + t.agents.values().map(Vec::len).sum::<usize>())
            .sum()
    }

    /// Agent-specific directories (the legacy `agents_detected` metric).
    pub fn total_agent_dirs(&self) -> usize {
        self.tenants.values().map(|t| t.agents.len()).sum()
    }

    pub fn by_kind(&self) -> HashMap<RuleKind, usize> {
        let mut m = HashMap::new();
        for t in self.tenants.values() {
            for r in t.default.iter().flatten().chain(t.agents.values().flatten()) {
                *m.entry(r.kind).or_insert(0) += 1;
            }
        }
        m
    }
}

/// Builds AI rules (ONNX models / prompts). Provided by the `onnx` feature.
pub trait AiRuleFactory: Send + Sync {
    fn build(&self, tenant: &Tenant, name: &str, manifest: &Value, dir: &Path) -> Result<Arc<dyn RuleBackend>, String>;
}

pub struct RuleRegistry {
    current: ArcSwap<RuleSet>,
    ai: Option<Arc<dyn AiRuleFactory>>,
}

impl RuleRegistry {
    pub fn new(ai: Option<Arc<dyn AiRuleFactory>>) -> Self {
        Self { current: ArcSwap::from_pointee(RuleSet::default()), ai }
    }

    pub fn current(&self) -> Arc<RuleSet> {
        self.current.load_full()
    }

    /// Re-scan every tenant. Blocking (reads and compiles files); call from a blocking
    /// context. The previous set stays active until the new one is complete.
    pub fn reload(&self, tenants: &[Arc<Tenant>]) -> Arc<RuleSet> {
        let prev_gen = self.current.load().generation;
        let mut set = RuleSet { generation: prev_gen + 1, loaded_at_ms: util::now_ms(), ..Default::default() };
        for tenant in tenants {
            if !tenant.enabled {
                continue;
            }
            let rules = load_tenant(tenant, self.ai.as_deref(), &mut set.issues);
            set.tenants.insert(tenant.id.clone(), rules);
        }
        for issue in &set.issues {
            tracing::error!(tenant = %issue.tenant, path = %util::log_safe(&issue.path), "rule not loaded: {}", util::log_safe(&issue.message));
        }
        tracing::info!(
            rules = set.total_rules(),
            agents = set.total_agent_dirs(),
            generation = set.generation,
            "rule set loaded"
        );
        let set = Arc::new(set);
        self.current.store(set.clone());
        set
    }
}

/// Directory names usable as agent ids: no separators, no dots-only, bounded.
pub fn safe_dir_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn load_tenant(tenant: &Tenant, ai: Option<&dyn AiRuleFactory>, issues: &mut Vec<LoadIssue>) -> TenantRules {
    let mut out = TenantRules::default();
    let Ok(entries) = std::fs::read_dir(&tenant.rules_dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.is_dir() {
            continue; // symlinks and stray files are ignored on purpose
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !safe_dir_name(&name) {
            issues.push(LoadIssue {
                tenant: tenant.id.clone(),
                path: path.display().to_string(),
                message: "directory name is not a valid agent id; ignored".into(),
            });
            continue;
        }
        let rules = load_dir(tenant, &path, ai, issues);
        if name == DEFAULT_DIR {
            out.default = Some(rules);
        } else {
            out.agents.insert(name, rules);
        }
    }
    out
}

fn load_dir(
    tenant: &Tenant,
    dir: &Path,
    ai: Option<&dyn AiRuleFactory>,
    issues: &mut Vec<LoadIssue>,
) -> Vec<Arc<Rule>> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => {
            rd.flatten().filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false)).map(|e| e.path()).collect()
        }
        Err(_) => return Vec::new(),
    };
    files.sort();

    let issue = |issues: &mut Vec<LoadIssue>, path: &Path, msg: String| {
        issues.push(LoadIssue { tenant: tenant.id.clone(), path: path.display().to_string(), message: msg })
    };

    // manifests by rule name
    let mut manifests: HashMap<String, (PathBuf, Value)> = HashMap::new();
    for path in &files {
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(stem) = fname.strip_suffix(".rule.json") else { continue };
        match read_limited(path, MAX_MANIFEST_BYTES)
            .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
        {
            Ok(v) if v.is_object() => {
                manifests.insert(stem.to_string(), (path.clone(), v));
            }
            Ok(_) => issue(issues, path, "manifest must be a JSON object".into()),
            Err(e) => issue(issues, path, format!("invalid manifest: {e}")),
        }
    }

    let mut rules: Vec<Arc<Rule>> = Vec::new();
    let mut consumed = std::collections::HashSet::new();

    for path in &files {
        let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let (stem, ext) = match fname.rsplit_once('.') {
            Some((s, e)) => (s.to_string(), e.to_ascii_lowercase()),
            None => continue,
        };
        let manifest = manifests.get(&stem).map(|(_, v)| v);
        if manifest.and_then(|m| m.get("enabled")).and_then(Value::as_bool) == Some(false) {
            consumed.insert(stem.clone());
            continue;
        }
        let (priority_cap, timeout_ms) = manifest_meta(manifest);

        let built: Option<BuildResult> = match ext.as_str() {
            "lua" => Some(build_lua(tenant, path, &stem)),
            "wasm" => Some(build_wasm(tenant, path, &stem)),
            _ => None,
        };
        if let Some(res) = built {
            consumed.insert(stem.clone());
            match res {
                Ok((kind, backend)) => rules.push(Arc::new(Rule {
                    name: stem,
                    kind,
                    path: path.clone(),
                    backend,
                    priority_cap,
                    timeout_ms,
                })),
                Err(e) => issue(issues, path, e),
            }
        }
    }

    // AI rules are defined purely by a manifest (`type: model | prompt`)
    for (stem, (path, manifest)) in &manifests {
        if consumed.contains(stem) {
            continue;
        }
        let Some(ty) = manifest.get("type").and_then(Value::as_str) else {
            issue(issues, path, "manifest has no sibling rule file and no \"type\"".into());
            continue;
        };
        if manifest.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let kind = match ty {
            "model" => RuleKind::Model,
            "prompt" => RuleKind::Prompt,
            other => {
                issue(issues, path, format!("unknown rule type '{other}' (expected model|prompt)"));
                continue;
            }
        };
        if !tenant.ai.enabled {
            issue(issues, path, "AI rules are disabled for this tenant (ai.enabled = false)".into());
            continue;
        }
        let Some(factory) = ai else {
            issue(issues, path, "this build has no AI support (rebuild with --features onnx)".into());
            continue;
        };
        let (priority_cap, timeout_ms) = manifest_meta(Some(manifest));
        match factory.build(tenant, stem, manifest, dir) {
            Ok(backend) => {
                let cap = Some(priority_cap.map_or(tenant.ai.max_priority, |c| c.min(tenant.ai.max_priority)));
                rules.push(Arc::new(Rule {
                    name: stem.clone(),
                    kind,
                    path: path.clone(),
                    backend,
                    priority_cap: cap,
                    timeout_ms,
                }));
            }
            Err(e) => issue(issues, path, e),
        }
    }

    // deterministic rules first (cheap), AI last (slow, skippable); stable by name
    rules.sort_by(|a, b| (a.kind.is_ai(), &a.name).cmp(&(b.kind.is_ai(), &b.name)));
    rules
}

fn manifest_meta(m: Option<&Value>) -> (Option<i32>, Option<u64>) {
    let cap = m
        .and_then(|m| m.get("priority_cap"))
        .and_then(Value::as_i64)
        .map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    let timeout = m.and_then(|m| m.get("timeout_ms")).and_then(Value::as_u64);
    (cap, timeout)
}

fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > max {
        return Err(format!("file is {} bytes, limit is {max}", meta.len()));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

#[cfg(feature = "lua")]
fn build_lua(tenant: &Tenant, path: &Path, name: &str) -> BuildResult {
    if !tenant.lua_enabled {
        return Err("Lua rules are not enabled for this tenant (rules.lua = false)".into());
    }
    let bytes = read_limited(path, MAX_LUA_BYTES)?;
    let src = String::from_utf8(bytes).map_err(|_| "rule is not valid UTF-8".to_string())?;
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or(name);
    super::backends::lua::LuaRule::new(file_name, src)
        .map(|r| (RuleKind::Lua, Arc::new(r) as Arc<dyn RuleBackend>))
        .map_err(|e| e.message)
}

#[cfg(not(feature = "lua"))]
fn build_lua(_: &Tenant, _: &Path, _: &str) -> BuildResult {
    Err("this build has no Lua support (rebuild with --features lua)".into())
}

#[cfg(feature = "wasm")]
fn build_wasm(tenant: &Tenant, path: &Path, name: &str) -> BuildResult {
    if !tenant.wasm_enabled {
        return Err("WASM rules are not enabled for this tenant (rules.wasm = false)".into());
    }
    let bytes = read_limited(path, MAX_WASM_BYTES)?;
    super::backends::wasm::WasmRule::new(name, &bytes)
        .map(|r| (RuleKind::Wasm, Arc::new(r) as Arc<dyn RuleBackend>))
        .map_err(|e| e.message)
}

#[cfg(not(feature = "wasm"))]
fn build_wasm(_: &Tenant, _: &Path, _: &str) -> BuildResult {
    Err("this build has no WASM support (rebuild with --features wasm)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Limits, Paths};
    use crate::tenant::TenantRegistry;

    fn setup() -> (tempfile::TempDir, Paths, TenantRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let reg = TenantRegistry::load(paths.clone(), Limits::default(), crate::config::AuthMode::Open).unwrap();
        (dir, paths, reg)
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn legacy_layout_agent_dir_replaces_default() {
        let (_d, paths, reg) = setup();
        write(&paths.rules_dir().join("default/default.lua"), "return nil");
        write(&paths.rules_dir().join("Rocket_01/a.lua"), "return nil");
        write(&paths.rules_dir().join("Rocket_01/b.lua"), "return nil");
        let rr = RuleRegistry::new(None);
        let set = rr.reload(&reg.list());
        assert_eq!(set.total_rules(), 3);
        assert_eq!(set.total_agent_dirs(), 1);
        match set.lookup("default", "Rocket_01") {
            Lookup::Agent(r) => assert_eq!(r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["a", "b"]),
            _ => panic!("expected agent rules"),
        }
        assert!(matches!(set.lookup("default", "Other"), Lookup::Default(r) if r.len() == 1));
        assert!(matches!(set.lookup("nope", "Other"), Lookup::None));
    }

    #[test]
    fn tenants_are_isolated() {
        let (_d, paths, reg) = setup();
        reg.edit(|f| {
            f.add_tenant("acme")?;
            f.tenants[0].rules.lua = Some(true);
            Ok(())
        })
        .unwrap();
        write(&paths.rules_dir().join("default/d.lua"), "return nil");
        write(&paths.tenants_dir().join("acme/rules/default/x.lua"), "return nil");
        let set = RuleRegistry::new(None).reload(&reg.list());
        assert!(matches!(set.lookup("acme", "any"), Lookup::Default(r) if r[0].name == "x"));
        assert!(matches!(set.lookup("default", "any"), Lookup::Default(r) if r[0].name == "d"));
    }

    #[test]
    fn broken_rules_are_reported_not_fatal() {
        let (_d, paths, reg) = setup();
        write(&paths.rules_dir().join("default/good.lua"), "return nil");
        write(&paths.rules_dir().join("default/bad.lua"), "if then");
        let set = RuleRegistry::new(None).reload(&reg.list());
        assert_eq!(set.total_rules(), 1);
        assert_eq!(set.issues.len(), 1);
        assert!(set.issues[0].path.ends_with("bad.lua"));
    }

    #[test]
    fn unsafe_directory_names_are_ignored() {
        assert!(safe_dir_name("Rocket_01"));
        assert!(safe_dir_name("pump-3.east"));
        for bad in ["", ".", "..", ".hidden", "a/b", "a\\b", "C:", "x y", "\u{202e}evil", "svc:1"] {
            assert!(!safe_dir_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn manifest_can_disable_and_cap_a_rule() {
        let (_d, paths, reg) = setup();
        write(&paths.rules_dir().join("default/a.lua"), "return nil");
        write(&paths.rules_dir().join("default/a.rule.json"), r#"{"priority_cap": 10}"#);
        write(&paths.rules_dir().join("default/b.lua"), "return nil");
        write(&paths.rules_dir().join("default/b.rule.json"), r#"{"enabled": false}"#);
        let set = RuleRegistry::new(None).reload(&reg.list());
        let Lookup::Default(r) = set.lookup("default", "x") else { panic!() };
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].priority_cap, Some(10));
    }

    #[test]
    fn ai_rule_without_support_is_a_clear_issue() {
        let (_d, paths, reg) = setup();
        write(&paths.rules_dir().join("default/ai.rule.json"), r#"{"type":"model","model":"m"}"#);
        let set = RuleRegistry::new(None).reload(&reg.list());
        assert_eq!(set.total_rules(), 0);
        assert!(
            set.issues[0].message.contains("disabled for this tenant")
                || set.issues[0].message.contains("no AI support")
        );
    }

    #[test]
    fn symlinked_rule_dirs_are_not_followed() {
        #[cfg(unix)]
        {
            let (_d, paths, reg) = setup();
            let outside = tempfile::tempdir().unwrap();
            write(&outside.path().join("evil.lua"), "return nil");
            std::fs::create_dir_all(paths.rules_dir()).unwrap();
            std::os::unix::fs::symlink(outside.path(), paths.rules_dir().join("Evil")).unwrap();
            let set = RuleRegistry::new(None).reload(&reg.list());
            assert_eq!(set.total_rules(), 0);
        }
    }
}
