//! Filesystem layout, rooted at the *home* directory (`--home`, `CARDINAL_HOME`, or the
//! current directory — which is what the original release used for everything).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `--home` > `CARDINAL_HOME` > current directory.
    pub fn resolve(explicit: Option<&Path>) -> Self {
        if let Some(p) = explicit {
            return Self::new(p);
        }
        if let Some(p) = std::env::var_os("CARDINAL_HOME")
            && !p.is_empty()
        {
            return Self::new(p);
        }
        Self::new(".")
    }

    pub fn config_dir(&self) -> PathBuf {
        self.home.join("config")
    }
    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.json")
    }
    pub fn tenants_file(&self) -> PathBuf {
        self.config_dir().join("tenants.json")
    }
    pub fn raft_file(&self) -> PathBuf {
        self.config_dir().join("raft.json")
    }
    pub fn models_file(&self) -> PathBuf {
        self.config_dir().join("models.json")
    }
    /// Shared secret for the local control plane and the HTTP admin API.
    pub fn admin_token_file(&self) -> PathBuf {
        self.config_dir().join("admin.token")
    }
    /// Secret for the Raft peer protocol.
    pub fn cluster_secret_file(&self) -> PathBuf {
        self.config_dir().join("cluster.secret")
    }
    pub fn audit_key_file(&self) -> PathBuf {
        self.config_dir().join("audit.key")
    }
    pub fn rules_dir(&self) -> PathBuf {
        self.home.join("rules")
    }
    /// Per-tenant rule trees: `tenants/<id>/rules/...`.
    pub fn tenants_dir(&self) -> PathBuf {
        self.home.join("tenants")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    pub fn info_dir(&self) -> PathBuf {
        self.home.join("info")
    }
    pub fn audit_db(&self) -> PathBuf {
        self.home.join("audit.redb")
    }
    pub fn models_dir(&self) -> PathBuf {
        self.home.join("models")
    }
}
