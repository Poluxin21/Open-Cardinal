//! Persistent shared memory.
//!
//! One [`Store`] wraps a single redb database that is opened *once* for the whole process.
//! (The original code re-opened the file on every call, which made redb refuse concurrent
//! pulses with `Database already open`.)
//!
//! Everything that mutates replicated state goes through [`Store::apply`] with a
//! [`Command`]. That is the Raft state machine: a single node applies commands directly,
//! a cluster applies the same commands in the same order on every node.

mod command;

use std::collections::BTreeMap;
use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::error::{Error, Result};

pub use command::{Command, ForcedReaction, Outcome, StateDump};

/// Tenant used when no tenant is configured (the original single-tenant behaviour).
pub const DEFAULT_TENANT: &str = "default";

const KV: TableDefinition<(&str, &str), u64> = TableDefinition::new("kv_v2");
const FORCED: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("forced_v2");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// Number of `KV` entries per tenant, maintained in the same transaction as every write
/// so quotas can be checked in O(1).
const KV_COUNT: TableDefinition<&str, u64> = TableDefinition::new("kv_count");
/// Table of the original release (`<&str, u64>`), migrated into the `default` tenant.
const LEGACY_KV: TableDefinition<&str, u64> = TableDefinition::new("open_cardinal");

const META_LAST_APPLIED: &str = "last_applied";
const META_INSTANCE_ID: &str = "instance_id";
const META_LEGACY_DONE: &str = "legacy_kv_migrated";

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_cache(path, 32 * 1024 * 1024)
    }

    /// `cache_bytes` bounds redb's page cache (its default of 1 GiB is not sidecar-sized).
    pub fn open_with_cache(path: &Path, cache_bytes: usize) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut builder = Database::builder();
        builder.set_cache_size(cache_bytes);
        let db = builder.create(path).map_err(|e| match e {
            redb::DatabaseError::DatabaseAlreadyOpen => Error::Storage(format!(
                "{} is locked by another process (is Cardinal already running?)",
                path.display()
            )),
            other => other.into(),
        })?;
        Self::init(db)
    }

    /// RAM-backed store for tests.
    pub fn open_in_memory() -> Result<Self> {
        let db = Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?;
        Self::init(db)
    }

    fn init(db: Database) -> Result<Self> {
        let store = Self { db };
        {
            let tx = store.db.begin_write()?;
            tx.open_table(KV)?;
            tx.open_table(FORCED)?;
            tx.open_table(META)?;
            tx.open_table(KV_COUNT)?;
            tx.commit()?;
        }
        store.migrate_legacy()?;
        if store.meta_get(META_INSTANCE_ID)?.is_none() {
            store.meta_set(META_INSTANCE_ID, crate::util::random_hex(16).as_bytes())?;
        }
        Ok(store)
    }

    /// Copy the original `open_cardinal` table into the `default` tenant, once.
    fn migrate_legacy(&self) -> Result<()> {
        if self.meta_get(META_LEGACY_DONE)?.is_some() {
            return Ok(());
        }
        let rows: Vec<(String, u64)> = {
            let rx = self.db.begin_read()?;
            match rx.open_table(LEGACY_KV) {
                Ok(t) => t
                    .iter()?
                    .map(|r| r.map(|(k, v)| (k.value().to_string(), v.value())))
                    .collect::<std::result::Result<_, _>>()?,
                Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
                Err(e) => return Err(e.into()),
            }
        };
        let tx = self.db.begin_write()?;
        {
            let mut kv = tx.open_table(KV)?;
            for (k, v) in &rows {
                // never clobber a value written by the new code
                if kv.get((DEFAULT_TENANT, k.as_str()))?.is_none() {
                    kv.insert((DEFAULT_TENANT, k.as_str()), *v)?;
                }
            }
            tx.open_table(META)?.insert(META_LEGACY_DONE, &b"1"[..])?;
        }
        recount(&tx)?;
        tx.commit()?;
        if !rows.is_empty() {
            tracing::info!(entries = rows.len(), "migrated legacy key/value table into tenant 'default'");
        }
        Ok(())
    }

    /// Raw handle for sibling subsystems (Raft log) that live in the same database file.
    pub(crate) fn db(&self) -> &Database {
        &self.db
    }

    // ---- reads ----------------------------------------------------------------------

    pub fn kv_get(&self, tenant: &str, key: &str) -> Result<Option<u64>> {
        let rx = self.db.begin_read()?;
        let t = rx.open_table(KV)?;
        Ok(t.get((tenant, key))?.map(|v| v.value()))
    }

    /// Number of keys the tenant currently stores.
    pub fn kv_len(&self, tenant: &str) -> Result<u64> {
        let rx = self.db.begin_read()?;
        let t = rx.open_table(KV_COUNT)?;
        Ok(t.get(tenant)?.map(|v| v.value()).unwrap_or(0))
    }

    pub fn forced_get(&self, tenant: &str, agent: &str) -> Result<Option<ForcedReaction>> {
        let rx = self.db.begin_read()?;
        let t = rx.open_table(FORCED)?;
        match t.get((tenant, agent))? {
            Some(raw) => Ok(Some(serde_json::from_slice(raw.value())?)),
            None => Ok(None),
        }
    }

    pub fn forced_list(&self) -> Result<Vec<(String, String, ForcedReaction)>> {
        let rx = self.db.begin_read()?;
        let t = rx.open_table(FORCED)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (k, v) = row?;
            let (tenant, agent) = k.value();
            out.push((tenant.to_string(), agent.to_string(), serde_json::from_slice(v.value())?));
        }
        Ok(out)
    }

    /// Index of the last command applied by [`Store::apply`] (0 when none).
    pub fn last_applied(&self) -> Result<u64> {
        Ok(self.meta_get(META_LAST_APPLIED)?.map(|b| decode_u64(&b)).unwrap_or(0))
    }

    /// Stable random identity of this database, used to detect a node talking to itself.
    pub fn instance_id(&self) -> Result<String> {
        let raw = self.meta_get(META_INSTANCE_ID)?.ok_or_else(|| Error::Storage("missing instance id".into()))?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let rx = self.db.begin_read()?;
        let t = rx.open_table(META)?;
        Ok(t.get(key)?.map(|v| v.value().to_vec()))
    }

    pub fn meta_set(&self, key: &str, value: &[u8]) -> Result<()> {
        let tx = self.db.begin_write()?;
        tx.open_table(META)?.insert(key, value)?;
        tx.commit()?;
        Ok(())
    }

    // ---- state machine --------------------------------------------------------------

    /// Apply a replicated command. When `index` is given (Raft), the applied index is
    /// persisted in the *same* transaction, so a crash can never replay or skip an entry.
    pub fn apply(&self, cmd: &Command, index: Option<u64>) -> Result<Outcome> {
        let tx = self.db.begin_write()?;
        let outcome = {
            let mut kv = tx.open_table(KV)?;
            let mut forced = tx.open_table(FORCED)?;
            let mut counts = tx.open_table(KV_COUNT)?;
            match cmd {
                Command::Noop => Outcome::None,
                Command::KvSet { tenant, key, value } => {
                    let prev = kv.insert((tenant.as_str(), key.as_str()), *value)?;
                    if prev.is_none() {
                        bump(&mut counts, tenant, 1)?;
                    }
                    Outcome::Value(*value)
                }
                Command::KvAdd { tenant, key, delta } => {
                    let cur = kv.get((tenant.as_str(), key.as_str()))?.map(|v| v.value());
                    let next = (cur.unwrap_or(0) as i128 + *delta as i128).clamp(0, u64::MAX as i128) as u64;
                    kv.insert((tenant.as_str(), key.as_str()), next)?;
                    if cur.is_none() {
                        bump(&mut counts, tenant, 1)?;
                    }
                    Outcome::Value(next)
                }
                Command::KvDelete { tenant, key } => {
                    let existed = kv.remove((tenant.as_str(), key.as_str()))?.is_some();
                    if existed {
                        bump(&mut counts, tenant, -1)?;
                    }
                    Outcome::Deleted(existed)
                }
                Command::ForceSet { tenant, agent, reaction } => {
                    let raw = serde_json::to_vec(reaction)?;
                    forced.insert((tenant.as_str(), agent.as_str()), raw.as_slice())?;
                    Outcome::None
                }
                Command::ForceClear { tenant, agent } => {
                    let existed = forced.remove((tenant.as_str(), agent.as_str()))?.is_some();
                    Outcome::Deleted(existed)
                }
            }
        };
        if let Some(i) = index {
            tx.open_table(META)?.insert(META_LAST_APPLIED, &i.to_le_bytes()[..])?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    // ---- snapshots (Raft log compaction / catch-up) ---------------------------------

    pub fn export_state(&self) -> Result<StateDump> {
        self.export_state_at().map(|(_, d)| d)
    }

    /// The state and the applied index it corresponds to, read from one consistent snapshot
    /// of the database (a single read transaction).
    pub fn export_state_at(&self) -> Result<(u64, StateDump)> {
        let rx = self.db.begin_read()?;
        let applied = rx.open_table(META)?.get(META_LAST_APPLIED)?.map(|v| decode_u64(v.value())).unwrap_or(0);
        let kv_t = rx.open_table(KV)?;
        let mut kv = Vec::new();
        for row in kv_t.iter()? {
            let (k, v) = row?;
            let (tenant, key) = k.value();
            kv.push((tenant.to_string(), key.to_string(), v.value()));
        }
        let forced_t = rx.open_table(FORCED)?;
        let mut forced = BTreeMap::new();
        for row in forced_t.iter()? {
            let (k, v) = row?;
            let (tenant, agent) = k.value();
            forced.insert(format!("{tenant}\u{1f}{agent}"), serde_json::from_slice(v.value())?);
        }
        Ok((applied, StateDump { kv, forced }))
    }

    /// Replace the replicated state with `dump` and record `last_applied` atomically.
    pub fn import_state(&self, dump: &StateDump, last_applied: u64) -> Result<()> {
        let tx = self.db.begin_write()?;
        self.import_state_in(&tx, dump, last_applied)?;
        tx.commit()?;
        Ok(())
    }

    /// Same as [`Store::import_state`] inside a caller-owned transaction, so a Raft snapshot
    /// install can swap state, log and metadata in one atomic commit.
    pub(crate) fn import_state_in(
        &self,
        tx: &redb::WriteTransaction,
        dump: &StateDump,
        last_applied: u64,
    ) -> Result<()> {
        {
            let mut kv = tx.open_table(KV)?;
            kv.retain(|_, _| false)?;
            for (tenant, key, value) in &dump.kv {
                kv.insert((tenant.as_str(), key.as_str()), *value)?;
            }
            let mut forced = tx.open_table(FORCED)?;
            forced.retain(|_, _| false)?;
            for (composite, reaction) in &dump.forced {
                let (tenant, agent) = composite
                    .split_once('\u{1f}')
                    .ok_or_else(|| Error::Storage("corrupt snapshot: forced key".into()))?;
                forced.insert((tenant, agent), serde_json::to_vec(reaction)?.as_slice())?;
            }
            tx.open_table(META)?.insert(META_LAST_APPLIED, &last_applied.to_le_bytes()[..])?;
        }
        recount(tx)
    }

    /// Approximate number of replicated entries (metrics).
    pub fn entry_counts(&self) -> Result<(u64, u64)> {
        let rx = self.db.begin_read()?;
        Ok((rx.open_table(KV)?.len()?, rx.open_table(FORCED)?.len()?))
    }
}

fn bump(counts: &mut redb::Table<'_, &str, u64>, tenant: &str, delta: i64) -> Result<()> {
    let cur = counts.get(tenant)?.map(|v| v.value()).unwrap_or(0);
    let next = (cur as i128 + delta as i128).max(0) as u64;
    counts.insert(tenant, next)?;
    Ok(())
}

/// Rebuild `KV_COUNT` from `KV` (after a bulk import).
fn recount(tx: &redb::WriteTransaction) -> Result<()> {
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    {
        let kv = tx.open_table(KV)?;
        for row in kv.iter()? {
            let (k, _) = row?;
            *totals.entry(k.value().0.to_string()).or_default() += 1;
        }
    }
    let mut counts = tx.open_table(KV_COUNT)?;
    counts.retain(|_, _| false)?;
    for (tenant, n) in totals {
        counts.insert(tenant.as_str(), n)?;
    }
    Ok(())
}

pub(crate) fn decode_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    let n = b.len().min(8);
    a[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(t: &str, k: &str, v: u64) -> Command {
        Command::KvSet { tenant: t.into(), key: k.into(), value: v }
    }

    #[test]
    fn kv_is_tenant_scoped() {
        let s = Store::open_in_memory().unwrap();
        s.apply(&set("a", "k", 1), None).unwrap();
        s.apply(&set("b", "k", 2), None).unwrap();
        assert_eq!(s.kv_get("a", "k").unwrap(), Some(1));
        assert_eq!(s.kv_get("b", "k").unwrap(), Some(2));
        assert_eq!(s.kv_get("c", "k").unwrap(), None);
        assert_eq!(s.kv_len("a").unwrap(), 1);
    }

    #[test]
    fn key_counter_tracks_inserts_overwrites_and_deletes() {
        let s = Store::open_in_memory().unwrap();
        s.apply(&set("t", "a", 1), None).unwrap();
        s.apply(&set("t", "a", 2), None).unwrap();
        s.apply(&set("t", "b", 1), None).unwrap();
        s.apply(&Command::KvAdd { tenant: "t".into(), key: "c".into(), delta: 1 }, None).unwrap();
        assert_eq!(s.kv_len("t").unwrap(), 3);
        s.apply(&Command::KvDelete { tenant: "t".into(), key: "a".into() }, None).unwrap();
        s.apply(&Command::KvDelete { tenant: "t".into(), key: "zzz".into() }, None).unwrap();
        assert_eq!(s.kv_len("t").unwrap(), 2);
        assert_eq!(s.kv_len("other").unwrap(), 0);
    }

    #[test]
    fn missing_key_is_none_not_a_panic() {
        // regression: the original `redb_api.get` unwrapped this and crashed the worker
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.kv_get(DEFAULT_TENANT, "nope").unwrap(), None);
    }

    #[test]
    fn add_saturates_at_zero() {
        let s = Store::open_in_memory().unwrap();
        let add = |d| Command::KvAdd { tenant: "t".into(), key: "c".into(), delta: d };
        assert_eq!(s.apply(&add(5), None).unwrap(), Outcome::Value(5));
        assert_eq!(s.apply(&add(-2), None).unwrap(), Outcome::Value(3));
        assert_eq!(s.apply(&add(-10), None).unwrap(), Outcome::Value(0));
    }

    #[test]
    fn applied_index_is_atomic_with_the_command() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.last_applied().unwrap(), 0);
        s.apply(&set("t", "k", 9), Some(7)).unwrap();
        assert_eq!(s.last_applied().unwrap(), 7);
        assert_eq!(s.kv_get("t", "k").unwrap(), Some(9));
    }

    #[test]
    fn forced_reactions_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let r = ForcedReaction {
            kind: 1,
            command_name: String::new(),
            params: Default::default(),
            set_at_ms: 1,
            expires_at_ms: None,
        };
        s.apply(&Command::ForceSet { tenant: "t".into(), agent: "a".into(), reaction: r.clone() }, None).unwrap();
        assert_eq!(s.forced_get("t", "a").unwrap(), Some(r));
        assert_eq!(s.forced_get("t", "other").unwrap(), None);
        let out = s.apply(&Command::ForceClear { tenant: "t".into(), agent: "a".into() }, None).unwrap();
        assert_eq!(out, Outcome::Deleted(true));
        assert_eq!(s.forced_get("t", "a").unwrap(), None);
    }

    #[test]
    fn snapshot_roundtrip_replaces_state() {
        let a = Store::open_in_memory().unwrap();
        a.apply(&set("t", "x", 1), None).unwrap();
        a.apply(&Command::ForceSet { tenant: "t".into(), agent: "ag".into(), reaction: ForcedReaction::new(2) }, None)
            .unwrap();
        let dump = a.export_state().unwrap();

        let b = Store::open_in_memory().unwrap();
        b.apply(&set("t", "stale", 99), None).unwrap();
        b.import_state(&dump, 42).unwrap();
        assert_eq!(b.kv_get("t", "x").unwrap(), Some(1));
        assert_eq!(b.kv_get("t", "stale").unwrap(), None);
        assert_eq!(b.forced_get("t", "ag").unwrap().unwrap().kind, 2);
        assert_eq!(b.last_applied().unwrap(), 42);
        assert_eq!(b.kv_len("t").unwrap(), 1, "counters are rebuilt on import");
    }

    #[test]
    fn legacy_table_is_migrated_once_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.redb");
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx.open_table(LEGACY_KV).unwrap();
                t.insert("Victim_overheat_strikes", 2).unwrap();
                t.insert("other", 5).unwrap();
            }
            tx.commit().unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.kv_get(DEFAULT_TENANT, "Victim_overheat_strikes").unwrap(), Some(2));
        s.apply(&set(DEFAULT_TENANT, "other", 77), None).unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.kv_get(DEFAULT_TENANT, "other").unwrap(), Some(77), "second open must not re-migrate");
    }

    #[test]
    fn second_process_gets_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.redb");
        let _first = Store::open(&path).unwrap();
        let err = Store::open(&path).err().expect("must fail");
        assert!(err.to_string().contains("locked by another process"), "{err}");
    }

    #[test]
    fn instance_id_is_stable_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.redb");
        let id = Store::open(&path).unwrap().instance_id().unwrap();
        assert_eq!(Store::open(&path).unwrap().instance_id().unwrap(), id);
    }
}
