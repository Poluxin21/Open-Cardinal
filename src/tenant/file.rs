//! On-disk schema of `config/tenants.json` and the operations the CLI performs on it.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::LimitsPatch;
use crate::error::{Error, Result};
use crate::util;

pub const API_KEY_PREFIX: &str = "ck_";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TenantsFile {
    pub tenants: Vec<TenantDef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantDef {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub api_keys: Vec<ApiKeyDef>,
    #[serde(default)]
    pub limits: LimitsPatch,
    #[serde(default)]
    pub rules: RuleKinds,
    #[serde(default)]
    pub ai: AiPolicy,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyDef {
    pub label: String,
    /// Lower-case hex SHA-256 of the key. The key itself is never stored.
    pub sha256: String,
    /// When set, this key may only act as agents whose id starts with the prefix.
    #[serde(default)]
    pub agent_prefix: Option<String>,
}

/// Which rule backends the tenant may use. Unset flags take a safe default: WASM is on,
/// Lua is on only for the built-in `default` tenant (Lua is trusted-author code).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleKinds {
    pub lua: Option<bool>,
    pub wasm: Option<bool>,
}

/// Guard-rails for AI rules of a tenant.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiPolicy {
    pub enabled: bool,
    /// Names from `config/models.json` this tenant may use (empty = none).
    pub models: Vec<String>,
    /// AI decisions are clamped to this priority so deterministic rules above it always win.
    pub max_priority: i32,
}

impl Default for AiPolicy {
    fn default() -> Self {
        Self { enabled: false, models: Vec::new(), max_priority: 500 }
    }
}

pub fn hash_key(key: &str) -> String {
    util::hex(&Sha256::digest(key.as_bytes()))
}

pub fn new_api_key() -> String {
    format!("{API_KEY_PREFIX}{}", util::random_hex(32))
}

/// `^[a-z0-9][a-z0-9_-]{0,62}$` — safe as a directory name and as a DB key component.
pub fn valid_tenant_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

impl TenantsFile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let file: Self = serde_json::from_str(&text).map_err(|e| Error::config(format!("{}: {e}", path.display())))?;
        file.validate()?;
        Ok(file)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        // hashes are not secret, but the file decides who may act — keep it owner-only
        util::write_atomic(path, &bytes, true)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let mut hashes = std::collections::HashSet::new();
        for t in &self.tenants {
            if !valid_tenant_id(&t.id) {
                return Err(Error::config(format!(
                    "tenant id '{}' is invalid (use a-z, 0-9, '_' or '-', max 63, not starting with '-' or '_')",
                    util::log_safe(&t.id)
                )));
            }
            if !seen.insert(t.id.clone()) {
                return Err(Error::config(format!("duplicate tenant '{}'", t.id)));
            }
            for k in &t.api_keys {
                if k.sha256.len() != 64 || util::from_hex(&k.sha256).is_none() {
                    return Err(Error::config(format!(
                        "tenant '{}' key '{}': sha256 must be 64 hex chars",
                        t.id, k.label
                    )));
                }
                if !hashes.insert(k.sha256.to_ascii_lowercase()) {
                    return Err(Error::config(format!(
                        "tenant '{}' key '{}': the same key is registered twice",
                        t.id, k.label
                    )));
                }
            }
            t.limits.apply(&crate::config::Limits::default()).validate()?;
        }
        Ok(())
    }

    pub fn add_tenant(&mut self, id: &str) -> Result<()> {
        if !valid_tenant_id(id) {
            return Err(Error::invalid(format!("invalid tenant id '{}'", util::log_safe(id))));
        }
        if self.tenants.iter().any(|t| t.id == id) {
            return Err(Error::invalid(format!("tenant '{id}' already exists")));
        }
        self.tenants.push(TenantDef {
            id: id.to_string(),
            display_name: None,
            enabled: true,
            api_keys: Vec::new(),
            limits: LimitsPatch::default(),
            rules: RuleKinds::default(),
            ai: AiPolicy::default(),
        });
        Ok(())
    }

    /// Create a key for `tenant` (creating the tenant when missing). Returns the plaintext
    /// key, which cannot be recovered later.
    pub fn issue_key(&mut self, tenant: &str, label: &str, agent_prefix: Option<String>) -> Result<String> {
        if self.tenants.iter().all(|t| t.id != tenant) {
            self.add_tenant(tenant)?;
        }
        let def = self.tenants.iter_mut().find(|t| t.id == tenant).expect("just ensured");
        if def.api_keys.iter().any(|k| k.label == label) {
            return Err(Error::invalid(format!("key label '{label}' already exists for '{tenant}'")));
        }
        let key = new_api_key();
        def.api_keys.push(ApiKeyDef { label: label.to_string(), sha256: hash_key(&key), agent_prefix });
        Ok(key)
    }

    pub fn revoke_key(&mut self, tenant: &str, label: &str) -> Result<()> {
        let def = self
            .tenants
            .iter_mut()
            .find(|t| t.id == tenant)
            .ok_or_else(|| Error::invalid(format!("unknown tenant '{tenant}'")))?;
        let before = def.api_keys.len();
        def.api_keys.retain(|k| k.label != label);
        if def.api_keys.len() == before {
            return Err(Error::invalid(format!("tenant '{tenant}' has no key '{label}'")));
        }
        Ok(())
    }

    pub fn set_enabled(&mut self, tenant: &str, enabled: bool) -> Result<()> {
        let def = self
            .tenants
            .iter_mut()
            .find(|t| t.id == tenant)
            .ok_or_else(|| Error::invalid(format!("unknown tenant '{tenant}'")))?;
        def.enabled = enabled;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_id_rules() {
        for ok in ["acme", "a", "team-1", "x_y", "0day"] {
            assert!(valid_tenant_id(ok), "{ok}");
        }
        for bad in ["", "-a", "_a", "A", "a/b", "a..b", "a b", "../x", &"a".repeat(64)] {
            assert!(!valid_tenant_id(bad), "{bad}");
        }
    }

    #[test]
    fn issued_key_is_stored_hashed_only() {
        let mut f = TenantsFile::default();
        let key = f.issue_key("acme", "prod", None).unwrap();
        assert!(key.starts_with(API_KEY_PREFIX));
        let json = serde_json::to_string(&f).unwrap();
        assert!(!json.contains(&key), "plaintext key must never be persisted");
        assert!(json.contains(&hash_key(&key)));
        f.validate().unwrap();
    }

    #[test]
    fn duplicate_labels_and_tenants_rejected() {
        let mut f = TenantsFile::default();
        f.issue_key("acme", "prod", None).unwrap();
        assert!(f.issue_key("acme", "prod", None).is_err());
        assert!(f.add_tenant("acme").is_err());
    }

    #[test]
    fn revoke_removes_only_that_key() {
        let mut f = TenantsFile::default();
        f.issue_key("acme", "a", None).unwrap();
        f.issue_key("acme", "b", None).unwrap();
        f.revoke_key("acme", "a").unwrap();
        assert_eq!(f.tenants[0].api_keys.len(), 1);
        assert!(f.revoke_key("acme", "a").is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_bad_hashes() {
        let bad = r#"{"tenants":[{"id":"a","api_keys":[{"label":"x","sha256":"zz"}]}]}"#;
        let f: TenantsFile = serde_json::from_str(bad).unwrap();
        assert!(f.validate().is_err());
        assert!(serde_json::from_str::<TenantsFile>(r#"{"tenants":[{"id":"a","oops":1}]}"#).is_err());
    }
}
