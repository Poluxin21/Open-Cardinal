//! Tenant-scoped, quota-enforcing view of the shared memory for one evaluation.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::types::{Host, MAX_KEY_LEN, RuleError, RuleErrorKind, valid_key};
use crate::store::{Command, Outcome};

pub struct SharedMem {
    host: Arc<dyn Host>,
    tenant: String,
    max_entries: u64,
    max_writes: u32,
    writes: AtomicU32,
    /// Deadline of the rule currently running; replicated writes honour it.
    deadline: Mutex<Instant>,
    /// The linearizable-read barrier ran for this evaluation.
    barrier_done: std::sync::atomic::AtomicBool,
}

impl SharedMem {
    pub fn new(host: Arc<dyn Host>, tenant: &str, max_entries: u64, max_writes: u32) -> Self {
        Self {
            host,
            tenant: tenant.to_string(),
            max_entries,
            max_writes,
            writes: AtomicU32::new(0),
            deadline: Mutex::new(Instant::now() + Duration::from_secs(3600)),
            barrier_done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Set before each rule runs.
    pub fn set_deadline(&self, deadline: Instant) {
        *self.deadline.lock().unwrap_or_else(|p| p.into_inner()) = deadline;
    }

    fn deadline(&self) -> Instant {
        *self.deadline.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn host_err(e: impl std::fmt::Display) -> RuleError {
        RuleError::new(RuleErrorKind::Host, e.to_string())
    }

    fn check_key(key: &str) -> Result<(), RuleError> {
        if valid_key(key) { Ok(()) } else { Err(RuleError::runtime(format!("key must be 1..={MAX_KEY_LEN} bytes"))) }
    }

    fn charge_write(&self) -> Result<(), RuleError> {
        if self.writes.fetch_add(1, Relaxed) >= self.max_writes {
            return Err(RuleError::new(
                RuleErrorKind::Budget,
                format!("more than {} shared-memory writes in one evaluation", self.max_writes),
            ));
        }
        Ok(())
    }

    /// Refuse to create a *new* key once the tenant is at its quota.
    fn check_quota(&self, key: &str) -> Result<(), RuleError> {
        self.barrier()?;
        if self.host.kv_get(&self.tenant, key).map_err(Self::host_err)?.is_some() {
            return Ok(());
        }
        if self.host.kv_len(&self.tenant).map_err(Self::host_err)? >= self.max_entries {
            return Err(RuleError::new(
                RuleErrorKind::Budget,
                format!("shared-memory quota of {} keys reached", self.max_entries),
            ));
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<u64>, RuleError> {
        Self::check_key(key)?;
        self.barrier()?;
        self.host.kv_get(&self.tenant, key).map_err(Self::host_err)
    }

    /// First read of an evaluation: wait until this node is up to date with the cluster.
    fn barrier(&self) -> Result<(), RuleError> {
        if !self.barrier_done.load(Relaxed) {
            self.host.read_barrier(self.deadline()).map_err(Self::host_err)?;
            self.barrier_done.store(true, Relaxed);
        }
        Ok(())
    }

    pub fn set(&self, key: &str, value: u64) -> Result<(), RuleError> {
        Self::check_key(key)?;
        self.charge_write()?;
        self.check_quota(key)?;
        self.host
            .kv_apply(Command::KvSet { tenant: self.tenant.clone(), key: key.into(), value }, self.deadline())
            .map_err(Self::host_err)?;
        Ok(())
    }

    /// Atomic add; returns the new value.
    pub fn add(&self, key: &str, delta: i64) -> Result<u64, RuleError> {
        Self::check_key(key)?;
        self.charge_write()?;
        self.check_quota(key)?;
        match self
            .host
            .kv_apply(Command::KvAdd { tenant: self.tenant.clone(), key: key.into(), delta }, self.deadline())
            .map_err(Self::host_err)?
        {
            Outcome::Value(v) => Ok(v),
            _ => Err(Self::host_err("unexpected outcome")),
        }
    }

    pub fn delete(&self, key: &str) -> Result<bool, RuleError> {
        Self::check_key(key)?;
        self.charge_write()?;
        match self
            .host
            .kv_apply(Command::KvDelete { tenant: self.tenant.clone(), key: key.into() }, self.deadline())
            .map_err(Self::host_err)?
        {
            Outcome::Deleted(b) => Ok(b),
            _ => Err(Self::host_err("unexpected outcome")),
        }
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::store::Store;

    /// Host backed directly by a store (no replication), for backend tests.
    pub struct DirectHost(pub Arc<Store>);

    impl Host for DirectHost {
        fn kv_get(&self, tenant: &str, key: &str) -> crate::Result<Option<u64>> {
            self.0.kv_get(tenant, key)
        }
        fn kv_len(&self, tenant: &str) -> crate::Result<u64> {
            self.0.kv_len(tenant)
        }
        fn kv_apply(&self, cmd: Command, _deadline: Instant) -> crate::Result<Outcome> {
            self.0.apply(&cmd, None)
        }
    }

    pub fn mem(store: &Arc<Store>, tenant: &str) -> Arc<SharedMem> {
        Arc::new(SharedMem::new(Arc::new(DirectHost(store.clone())), tenant, 100, 64))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::store::Store;

    #[test]
    fn get_set_add_delete_roundtrip() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let m = mem(&store, "acme");
        assert_eq!(m.get("k").unwrap(), None);
        m.set("k", 5).unwrap();
        assert_eq!(m.add("k", 2).unwrap(), 7);
        assert_eq!(m.get("k").unwrap(), Some(7));
        assert!(m.delete("k").unwrap());
        assert_eq!(m.get("k").unwrap(), None);
        assert_eq!(store.kv_get("other", "k").unwrap(), None, "tenant scoping");
    }

    #[test]
    fn write_budget_per_evaluation() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let m = Arc::new(SharedMem::new(Arc::new(DirectHost(store)), "t", 1000, 3));
        for i in 0..3 {
            m.set(&format!("k{i}"), 1).unwrap();
        }
        let e = m.set("k4", 1).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::Budget);
    }

    #[test]
    fn quota_blocks_new_keys_but_not_updates() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let m = Arc::new(SharedMem::new(Arc::new(DirectHost(store)), "t", 2, 64));
        m.set("a", 1).unwrap();
        m.set("b", 1).unwrap();
        assert_eq!(m.set("c", 1).unwrap_err().kind, RuleErrorKind::Budget);
        m.set("a", 9).unwrap();
        assert_eq!(m.get("a").unwrap(), Some(9));
    }

    #[test]
    fn rejects_bad_keys() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let m = mem(&store, "t");
        assert!(m.get("").is_err());
        assert!(m.set(&"x".repeat(MAX_KEY_LEN + 1), 1).is_err());
    }
}
