//! Multi-tenancy: tenant registry, API-key authentication, per-tenant limits.
//!
//! A tenant owns an isolated namespace: its rules (`tenants/<id>/rules`), its shared
//! memory keys, its forced reactions, its limits and its audit trail. The original
//! single-tenant layout (`rules/…`) is the built-in `default` tenant.

mod file;
mod limiter;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::Semaphore;

use crate::config::{AuthMode, Limits, Paths};
use crate::error::{Error, Result};
use crate::store::DEFAULT_TENANT;
use crate::util;

pub use file::{AiPolicy, ApiKeyDef, RuleKinds, TenantDef, TenantsFile, hash_key, valid_tenant_id};
pub use limiter::RateLimiter;

pub struct Tenant {
    pub id: String,
    pub enabled: bool,
    pub limits: Limits,
    pub lua_enabled: bool,
    pub wasm_enabled: bool,
    pub ai: AiPolicy,
    pub rules_dir: PathBuf,
    pub limiter: RateLimiter,
    /// Bounds concurrent rule evaluations of this tenant (noisy-neighbour protection).
    pub gate: Arc<Semaphore>,
}

impl Tenant {
    fn build(def: &TenantDef, global: &Limits, paths: &Paths) -> Self {
        let limits = def.limits.apply(global);
        Self::from_parts(def.id.clone(), def.enabled, limits, def.rules.clone(), def.ai.clone(), paths)
    }

    fn from_parts(id: String, enabled: bool, limits: Limits, kinds: RuleKinds, ai: AiPolicy, paths: &Paths) -> Self {
        let lua_enabled = kinds.lua.unwrap_or(id == DEFAULT_TENANT);
        let wasm_enabled = kinds.wasm.unwrap_or(true);
        let rules_dir =
            if id == DEFAULT_TENANT { paths.rules_dir() } else { paths.tenants_dir().join(&id).join("rules") };
        Self {
            limiter: RateLimiter::new(limits.rate_limit_per_sec, limits.rate_limit_burst),
            gate: Arc::new(Semaphore::new(limits.concurrency())),
            id,
            enabled,
            limits,
            lua_enabled,
            wasm_enabled,
            ai,
            rules_dir,
        }
    }
}

/// Who is calling, after authentication.
#[derive(Clone)]
pub struct Principal {
    pub tenant: Arc<Tenant>,
    /// Label of the API key used (`None` for unauthenticated local access).
    pub key_label: Option<String>,
    pub agent_prefix: Option<String>,
}

impl Principal {
    /// Enforce the key's agent-id scope.
    pub fn check_agent(&self, agent_id: &str) -> Result<()> {
        match &self.agent_prefix {
            Some(p) if !agent_id.starts_with(p.as_str()) => {
                Err(Error::PermissionDenied("this API key is not allowed to act as that agent".into()))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone)]
struct KeyEntry {
    tenant: String,
    label: String,
    agent_prefix: Option<String>,
}

struct Snapshot {
    tenants: HashMap<String, Arc<Tenant>>,
    /// SHA-256(key) → owner. Lookup by hash, so no secret comparison happens on the map.
    keys: HashMap<[u8; 32], KeyEntry>,
}

pub struct TenantRegistry {
    paths: Paths,
    global: Limits,
    auth: AuthMode,
    snap: ArcSwap<Snapshot>,
    external: Vec<Arc<dyn crate::ext::Authenticator>>,
}

impl TenantRegistry {
    pub fn load(paths: Paths, global: Limits, auth: AuthMode) -> Result<Self> {
        let snap = Self::build(&paths, &global)?;
        Ok(Self { paths, global, auth, snap: ArcSwap::from_pointee(snap), external: Vec::new() })
    }

    /// Add identity providers consulted when a bearer token is not a known API key.
    pub fn with_authenticators(mut self, external: Vec<Arc<dyn crate::ext::Authenticator>>) -> Self {
        self.external = external;
        self
    }

    fn build(paths: &Paths, global: &Limits) -> Result<Snapshot> {
        let file = TenantsFile::load(&paths.tenants_file())?;
        Ok(Self::snapshot_from(&file, paths, global))
    }

    fn snapshot_from(file: &TenantsFile, paths: &Paths, global: &Limits) -> Snapshot {
        let mut tenants: HashMap<String, Arc<Tenant>> = HashMap::new();
        let mut keys = HashMap::new();
        for def in &file.tenants {
            tenants.insert(def.id.clone(), Arc::new(Tenant::build(def, global, paths)));
            if !def.enabled {
                continue;
            }
            for k in &def.api_keys {
                if let Some(raw) = util::from_hex(&k.sha256)
                    && let Ok(h) = <[u8; 32]>::try_from(raw.as_slice())
                {
                    keys.insert(
                        h,
                        KeyEntry {
                            tenant: def.id.clone(),
                            label: k.label.clone(),
                            agent_prefix: k.agent_prefix.clone(),
                        },
                    );
                }
            }
        }
        // The legacy single-tenant namespace always exists.
        tenants.entry(DEFAULT_TENANT.to_string()).or_insert_with(|| {
            Arc::new(Tenant::from_parts(
                DEFAULT_TENANT.to_string(),
                true,
                global.clone(),
                RuleKinds::default(),
                AiPolicy::default(),
                paths,
            ))
        });
        Snapshot { tenants, keys }
    }

    /// Re-read `tenants.json`. On any error the previous (known good) registry stays
    /// active: a typo must never silently turn authentication off.
    pub fn reload(&self) -> Result<()> {
        let snap = Self::build(&self.paths, &self.global)?;
        self.snap.store(Arc::new(snap));
        Ok(())
    }

    /// Whether callers must present credentials right now.
    pub fn auth_required(&self) -> bool {
        match self.auth {
            AuthMode::Required => true,
            AuthMode::Open => false,
            AuthMode::WhenKeysExist => self.has_any_key(),
        }
    }

    pub fn has_any_key(&self) -> bool {
        !self.snap.load().keys.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Tenant>> {
        self.snap.load().tenants.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Tenant>> {
        let mut v: Vec<_> = self.snap.load().tenants.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Resolve the caller from a bearer token.
    ///
    /// * valid key → that key's tenant (even in open mode)
    /// * no key, open mode → the `default` tenant (original behaviour)
    /// * otherwise → `Unauthenticated`
    pub fn authenticate(&self, bearer: Option<&str>) -> Result<Principal> {
        let snap = self.snap.load();
        match bearer {
            Some(token) => {
                let digest = <[u8; 32]>::from(sha2::Sha256::digest(token.as_bytes()));
                let Some(entry) = snap.keys.get(&digest) else {
                    return self.authenticate_external(&snap, token);
                };
                let tenant = snap
                    .tenants
                    .get(&entry.tenant)
                    .cloned()
                    .ok_or_else(|| Error::Unauthenticated("invalid credentials".into()))?;
                if !tenant.enabled {
                    return Err(Error::PermissionDenied("tenant is disabled".into()));
                }
                Ok(Principal { tenant, key_label: Some(entry.label.clone()), agent_prefix: entry.agent_prefix.clone() })
            }
            None if !self.auth_required() => {
                let tenant = snap.tenants.get(DEFAULT_TENANT).cloned().expect("default tenant always exists");
                Ok(Principal { tenant, key_label: None, agent_prefix: None })
            }
            None => Err(Error::Unauthenticated("missing credentials".into())),
        }
    }

    fn authenticate_external(&self, snap: &Snapshot, token: &str) -> Result<Principal> {
        for provider in &self.external {
            let Some(id) = provider.authenticate(token) else { continue };
            let tenant = snap
                .tenants
                .get(&id.tenant)
                .cloned()
                .ok_or_else(|| Error::Unauthenticated("invalid credentials".into()))?;
            if !tenant.enabled {
                return Err(Error::PermissionDenied("tenant is disabled".into()));
            }
            return Ok(Principal {
                tenant,
                key_label: Some(format!("ext:{}", id.subject)),
                agent_prefix: id.agent_prefix,
            });
        }
        Err(Error::Unauthenticated("invalid credentials".into()))
    }

    // ---- administration (used by the control plane) ----------------------------------

    pub fn file(&self) -> Result<TenantsFile> {
        TenantsFile::load(&self.paths.tenants_file())
    }

    /// Mutate `tenants.json` and activate the result. Nothing is written if the change
    /// does not validate.
    pub fn edit<T>(&self, f: impl FnOnce(&mut TenantsFile) -> Result<T>) -> Result<T> {
        let mut file = self.file()?;
        let out = f(&mut file)?;
        file.validate()?;
        file.save(&self.paths.tenants_file())?;
        self.snap.store(Arc::new(Self::snapshot_from(&file, &self.paths, &self.global)));
        Ok(out)
    }
}

use sha2::Digest;

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(auth: bool) -> (tempfile::TempDir, TenantRegistry) {
        registry_with(if auth { AuthMode::Required } else { AuthMode::Open })
    }

    fn registry_with(mode: AuthMode) -> (tempfile::TempDir, TenantRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let reg = TenantRegistry::load(paths, Limits::default(), mode).unwrap();
        (dir, reg)
    }

    #[test]
    fn external_authenticators_map_tokens_to_tenants() {
        use crate::ext::{Authenticator, ExternalIdentity};
        struct Oidc;
        impl Authenticator for Oidc {
            fn authenticate(&self, token: &str) -> Option<ExternalIdentity> {
                (token == "jwt-for-acme").then(|| ExternalIdentity {
                    tenant: "acme".into(),
                    subject: "alice@acme".into(),
                    agent_prefix: Some("a-".into()),
                })
            }
        }
        let (_d, reg) = registry(true);
        reg.edit(|f| f.add_tenant("acme")).unwrap();
        let reg = reg.with_authenticators(vec![Arc::new(Oidc)]);
        let p = reg.authenticate(Some("jwt-for-acme")).unwrap();
        assert_eq!((p.tenant.id.as_str(), p.key_label.as_deref()), ("acme", Some("ext:alice@acme")));
        assert!(p.check_agent("b-1").is_err());
        assert!(reg.authenticate(Some("other")).is_err());
        // a provider cannot mint access to a tenant that does not exist
        struct Bad;
        impl Authenticator for Bad {
            fn authenticate(&self, _: &str) -> Option<ExternalIdentity> {
                Some(ExternalIdentity { tenant: "ghost".into(), subject: "x".into(), agent_prefix: None })
            }
        }
        let (_d2, reg2) = registry(true);
        let reg2 = reg2.with_authenticators(vec![Arc::new(Bad)]);
        assert!(reg2.authenticate(Some("anything")).is_err());
    }

    #[test]
    fn issuing_the_first_key_switches_authentication_on() {
        // PoC 1 follow-up: an anonymous caller must stop being accepted once keys exist
        let (_d, reg) = registry_with(AuthMode::WhenKeysExist);
        assert!(reg.authenticate(None).is_ok(), "open until a key exists");
        let key = reg.edit(|f| f.issue_key("default", "agents", None)).unwrap();
        assert!(matches!(reg.authenticate(None), Err(Error::Unauthenticated(_))));
        assert_eq!(reg.authenticate(Some(&key)).unwrap().tenant.id, "default");
        reg.edit(|f| f.revoke_key("default", "agents")).unwrap();
        assert!(reg.authenticate(None).is_ok(), "no keys left: back to open");
    }

    #[test]
    fn open_mode_maps_anonymous_to_default_tenant() {
        let (_d, reg) = registry(false);
        let p = reg.authenticate(None).unwrap();
        assert_eq!(p.tenant.id, DEFAULT_TENANT);
        assert!(p.key_label.is_none());
    }

    #[test]
    fn required_mode_rejects_anonymous_and_unknown_keys() {
        let (_d, reg) = registry(true);
        assert!(matches!(reg.authenticate(None), Err(Error::Unauthenticated(_))));
        assert!(matches!(reg.authenticate(Some("ck_bogus")), Err(Error::Unauthenticated(_))));
    }

    #[test]
    fn issued_key_authenticates_as_its_tenant_and_scope() {
        let (_d, reg) = registry(true);
        let key = reg.edit(|f| f.issue_key("acme", "fleet-a", Some("a-".into()))).unwrap();
        let p = reg.authenticate(Some(&key)).unwrap();
        assert_eq!(p.tenant.id, "acme");
        assert_eq!(p.tenant.rules_dir.file_name().unwrap(), "rules");
        assert!(p.check_agent("a-pump-1").is_ok());
        assert!(matches!(p.check_agent("b-pump-1"), Err(Error::PermissionDenied(_))));
    }

    #[test]
    fn revoked_and_disabled_keys_stop_working() {
        let (_d, reg) = registry(true);
        let key = reg.edit(|f| f.issue_key("acme", "k", None)).unwrap();
        assert!(reg.authenticate(Some(&key)).is_ok());
        reg.edit(|f| f.set_enabled("acme", false)).unwrap();
        assert!(reg.authenticate(Some(&key)).is_err());
        reg.edit(|f| f.set_enabled("acme", true)).unwrap();
        reg.edit(|f| f.revoke_key("acme", "k")).unwrap();
        assert!(matches!(reg.authenticate(Some(&key)), Err(Error::Unauthenticated(_))));
    }

    #[test]
    fn reload_failure_keeps_previous_registry() {
        let (dir, reg) = registry(true);
        let key = reg.edit(|f| f.issue_key("acme", "k", None)).unwrap();
        std::fs::write(Paths::new(dir.path()).tenants_file(), "{ not json").unwrap();
        assert!(reg.reload().is_err());
        assert!(reg.authenticate(Some(&key)).is_ok(), "known-good registry must stay active");
    }

    #[test]
    fn tenants_get_isolated_rule_dirs() {
        let (_d, reg) = registry(false);
        reg.edit(|f| f.add_tenant("acme")).unwrap();
        let acme = reg.get("acme").unwrap();
        let default = reg.get(DEFAULT_TENANT).unwrap();
        assert_ne!(acme.rules_dir, default.rules_dir);
        assert!(acme.rules_dir.ends_with("tenants/acme/rules") || acme.rules_dir.ends_with("tenants\\acme\\rules"));
    }
}
