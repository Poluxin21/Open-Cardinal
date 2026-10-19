//! Shared application state and the pieces every server needs.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use serde_json::json;
use tokio::sync::watch;

use crate::audit::{AuditLog, Event};
use crate::config::Settings;
use crate::engine::{Engine, RuleRegistry, RuleSet};
use crate::error::{Error, Result};
use crate::kernel::monitor::SysMonitor;
use crate::metrics::Metrics;
use crate::replication::{ClusterHost, Replicator};
use crate::store::Store;
use crate::tenant::TenantRegistry;
use crate::util;

/// Cooperative shutdown: every long-running task selects on [`Shutdown::wait`].
#[derive(Clone)]
pub struct Shutdown {
    tx: Arc<watch::Sender<bool>>,
}

impl Shutdown {
    pub fn new() -> Self {
        Self { tx: Arc::new(watch::channel(false).0) }
    }

    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once shutdown was requested (also if it already was).
    pub async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

/// Secret protecting the control plane and the HTTP admin API.
pub struct AdminAuth {
    token: String,
}

impl AdminAuth {
    /// `CARDINAL_ADMIN_TOKEN` / `CARDINAL_ADMIN_TOKEN_FILE` win (container secrets); otherwise `config/admin.token`,
    /// created with a random value on first start.
    pub fn load_or_create(file: &Path) -> Result<Self> {
        let from_env = std::env::var("CARDINAL_ADMIN_TOKEN")
            .ok()
            .or_else(|| std::env::var("CARDINAL_ADMIN_TOKEN_FILE").ok().and_then(|f| std::fs::read_to_string(f).ok()));
        if let Some(t) = from_env {
            let t = t.trim().to_string();
            if t.len() < 16 {
                return Err(Error::config(
                    "the admin token (CARDINAL_ADMIN_TOKEN[_FILE]) must be at least 16 characters",
                ));
            }
            return Ok(Self { token: t });
        }
        match std::fs::read_to_string(file) {
            Ok(t) if t.trim().len() >= 16 => Ok(Self { token: t.trim().to_string() }),
            _ => {
                let token = util::random_hex(32);
                util::write_atomic(file, token.as_bytes(), true)?;
                tracing::info!("generated a new admin token at {}", file.display());
                Ok(Self { token })
            }
        }
    }

    pub fn fixed(token: &str) -> Self {
        Self { token: token.to_string() }
    }

    pub fn verify(&self, candidate: &str) -> bool {
        util::secret_eq(self.token.as_bytes(), candidate.as_bytes())
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

pub struct App {
    pub settings: Settings,
    pub store: Arc<Store>,
    pub replicator: Arc<Replicator>,
    pub tenants: Arc<TenantRegistry>,
    pub engine: Arc<Engine>,
    pub audit: Arc<AuditLog>,
    pub metrics: Arc<Metrics>,
    pub admin: AdminAuth,
    pub shutdown: Shutdown,
    pub sys: Arc<SysMonitor>,
    /// Cluster handle when `config/raft.json` is present.
    pub raft: Option<crate::raft::RaftHandle>,
    /// `"community"` for the open build; whatever an extension crate registers otherwise.
    pub edition: String,
    pub started: Instant,
    /// Short stable id of this node (first 8 hex chars of the store instance id).
    pub node: String,
    rejected_window: AtomicU64,
}

impl App {
    /// Open every subsystem. Must run inside a Tokio runtime (the rule host needs a handle).
    pub async fn build(settings: Settings) -> Result<Arc<Self>> {
        Self::build_with(settings, crate::ext::Extensions::default()).await
    }

    /// Like [`App::build`], with extensions registered (see `docs/EDITIONS.md`).
    pub async fn build_with(settings: Settings, ext: crate::ext::Extensions) -> Result<Arc<Self>> {
        let paths = &settings.paths;
        // `config/` must exist (it holds the secrets); the rule and log directories are
        // convenience: they may legitimately be read-only mounts (containers, ConfigMaps).
        std::fs::create_dir_all(paths.config_dir())?;
        for dir in [paths.rules_dir().join("default"), paths.tenants_dir(), paths.logs_dir()] {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::debug!("not creating {}: {e}", dir.display());
            }
        }

        let cache = settings.config.db_cache_mb as usize * 1024 * 1024;
        let store = Arc::new(Store::open_with_cache(&settings.db_path(), cache)?);
        let node = store.instance_id()?.chars().take(8).collect::<String>();

        let tenants = Arc::new(
            TenantRegistry::load(paths.clone(), settings.config.limits.clone(), settings.auth_mode())?
                .with_authenticators(ext.authenticators.clone()),
        );
        let audit = AuditLog::open_full(
            &paths.audit_db(),
            &paths.audit_key_file(),
            &node,
            &settings.config.audit,
            (cache / 2).max(4 * 1024 * 1024),
            ext.audit_sinks.clone(),
        )?;
        let admin = AdminAuth::load_or_create(&paths.admin_token_file())?;
        let metrics = Arc::new(Metrics::default());

        let shutdown = Shutdown::new();
        let raft = match crate::raft::RaftSettings::load(paths)? {
            Some(rs) => {
                tracing::info!(peers = ?rs.peers, "clustering enabled");
                Some(
                    crate::raft::node::start(crate::raft::node::StartArgs {
                        settings: rs,
                        store: store.clone(),
                        audit: audit.clone(),
                        shutdown: shutdown.clone(),
                    })
                    .await?,
                )
            }
            None => None,
        };
        let replicator = Arc::new(match &raft {
            Some(h) => Replicator::Raft(h.clone()),
            None => Replicator::Local(store.clone()),
        });
        let host = Arc::new(ClusterHost::new(store.clone(), replicator.clone(), tokio::runtime::Handle::current()));
        let registry = Arc::new(RuleRegistry::new(crate::ai::factory(&settings)?));
        let engine = Arc::new(Engine::new(registry, host, &settings.config.limits));

        let app = Arc::new(Self {
            settings,
            store,
            replicator,
            tenants,
            engine,
            audit,
            metrics,
            admin,
            shutdown,
            sys: Arc::new(SysMonitor::new()),
            raft,
            edition: ext.edition.clone(),
            started: Instant::now(),
            node,
            rejected_window: AtomicU64::new(0),
        });
        app.load_rules_blocking();
        Ok(app)
    }

    /// Re-scan every tenant's rules (blocking: reads and compiles files).
    pub fn load_rules_blocking(&self) -> Arc<RuleSet> {
        self.engine.registry().reload(&self.tenants.list())
    }

    /// Re-read tenants and rules without blocking the async runtime. Returns the new rule set.
    pub async fn reload(self: &Arc<Self>) -> Result<Arc<RuleSet>> {
        let app = self.clone();
        let tenants_result = tokio::task::spawn_blocking(move || {
            let t = app.tenants.reload();
            let set = app.load_rules_blocking();
            (t, set)
        })
        .await
        .map_err(|e| Error::Other(format!("reload task failed: {e}")))?;
        let (t, set) = tenants_result;
        if let Err(e) = &t {
            tracing::error!("tenants.json not reloaded (previous registry stays active): {e}");
        }
        self.audit.record(Event::new(
            "rules_reload",
            "-",
            json!({
                "rules": set.total_rules(),
                "agents": set.total_agent_dirs(),
                "generation": set.generation,
                "issues": set.issues.len(),
                "tenants_reloaded": t.is_ok(),
            }),
        ));
        t.map(|_| set)
    }

    /// Audit a refused request without letting an attacker flood the audit log: at most
    /// 50 such records per second are written, the rest only increment the metrics.
    pub fn audit_rejected(&self, tenant: Option<&str>, reason: &str, agent: Option<&str>, peer: Option<String>) {
        let sec = util::now_secs();
        let window = self.rejected_window.load(Relaxed);
        let (win_sec, count) = (window >> 32, window & 0xffff_ffff);
        let next = if win_sec == sec { (sec << 32) | (count + 1) } else { (sec << 32) | 1 };
        self.rejected_window.store(next, Relaxed);
        if win_sec == sec && count >= 50 {
            return;
        }
        let mut ev = Event::new(
            "rejected",
            tenant.unwrap_or("-"),
            json!({ "reason": reason, "peer": peer.map(|p| util::log_safe(&p)) }),
        );
        if let Some(a) = agent {
            ev = ev.agent(util::log_safe(a));
        }
        self.audit.record(ev);
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}
