//! Durable Raft state: the log and the hard state live in the same redb file as the
//! replicated state machine, so applying an entry, recording the applied index and (for
//! snapshots) swapping the whole state are each a single atomic commit.

use std::sync::Arc;

use redb::{ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};

use super::core::PersistedState;
use super::types::*;
use crate::error::{Error, Result};
use crate::store::{Command, Outcome, StateDump, Store};

const RAFT_LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");
const RAFT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");
const META_HARD_STATE: &str = "hard_state";
const META_SNAPSHOT: &str = "snapshot";
const META_CLUSTER: &str = "cluster_id";

#[derive(Serialize, Deserialize)]
struct StoredHardState {
    term: Term,
    voted_for: Option<NodeId>,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredSnapshot {
    index: Index,
    term: Term,
    config: ClusterConfig,
}

pub struct RaftStorage {
    store: Arc<Store>,
}

impl RaftStorage {
    pub fn open(store: Arc<Store>) -> Result<Self> {
        let tx = store.db().begin_write()?;
        tx.open_table(RAFT_LOG)?;
        tx.open_table(RAFT_META)?;
        tx.commit()?;
        Ok(Self { store })
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn cluster_id(&self) -> Result<Option<String>> {
        let rx = self.store.db().begin_read()?;
        let t = rx.open_table(RAFT_META)?;
        Ok(t.get(META_CLUSTER)?.map(|v| String::from_utf8_lossy(v.value()).into_owned()))
    }

    pub fn set_cluster_id(&self, id: &str) -> Result<()> {
        let tx = self.store.db().begin_write()?;
        tx.open_table(RAFT_META)?.insert(META_CLUSTER, id.as_bytes())?;
        tx.commit()?;
        Ok(())
    }

    /// Read everything the core needs to boot.
    pub fn load(&self) -> Result<PersistedState> {
        let rx = self.store.db().begin_read()?;
        let meta = rx.open_table(RAFT_META)?;
        let hard_state = match meta.get(META_HARD_STATE)? {
            Some(v) => {
                let h: StoredHardState = serde_json::from_slice(v.value())?;
                HardState { term: h.term, voted_for: h.voted_for }
            }
            None => HardState::default(),
        };
        let snap: StoredSnapshot = match meta.get(META_SNAPSHOT)? {
            Some(v) => serde_json::from_slice(v.value())?,
            None => StoredSnapshot::default(),
        };
        let log = rx.open_table(RAFT_LOG)?;
        let mut entries = Vec::new();
        for (i, row) in log.range(snap.index + 1..)?.enumerate() {
            let (k, v) = row?;
            let index = k.value();
            let expected = snap.index + 1 + i as u64;
            if index != expected {
                return Err(Error::Storage(format!("raft log is not contiguous: expected {expected}, found {index}")));
            }
            entries.push(serde_json::from_slice::<Entry>(v.value())?);
        }
        let applied = self.store.last_applied()?;
        Ok(PersistedState {
            hard_state,
            snapshot: (snap.index, snap.term),
            snapshot_config: snap.config,
            entries,
            applied,
        })
    }

    /// Persist what the core asks for, in one transaction: hard state, truncation, appends.
    pub fn persist(&self, hs: Option<HardState>, truncate_from: Option<Index>, append: &[Entry]) -> Result<()> {
        if hs.is_none() && truncate_from.is_none() && append.is_empty() {
            return Ok(());
        }
        let tx = self.store.db().begin_write()?;
        {
            if let Some(h) = hs {
                let raw = serde_json::to_vec(&StoredHardState { term: h.term, voted_for: h.voted_for })?;
                tx.open_table(RAFT_META)?.insert(META_HARD_STATE, raw.as_slice())?;
            }
            let mut log = tx.open_table(RAFT_LOG)?;
            if let Some(from) = truncate_from {
                log.retain_in(from.., |_, _| false)?;
            }
            for e in append {
                log.insert(e.index, serde_json::to_vec(e)?.as_slice())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Apply one committed entry to the state machine (and record the applied index) atomically.
    pub fn apply(&self, entry: &Entry) -> Result<Outcome> {
        match &entry.payload {
            Payload::Command(c) => self.store.apply(c, Some(entry.index)),
            // no state change, but the applied index must still advance
            Payload::Noop | Payload::Config(_) => self.store.apply(&Command::Noop, Some(entry.index)),
        }
    }

    /// Drop log entries up to and including `index`; they live on in the state machine.
    pub fn compact(&self, index: Index, term: Term, config: &ClusterConfig) -> Result<()> {
        let tx = self.store.db().begin_write()?;
        {
            tx.open_table(RAFT_LOG)?.retain_in(..=index, |_, _| false)?;
            let raw = serde_json::to_vec(&StoredSnapshot { index, term, config: config.clone() })?;
            tx.open_table(RAFT_META)?.insert(META_SNAPSHOT, raw.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace state machine and log with a snapshot received from the leader — one commit.
    pub fn install_snapshot(&self, meta: &SnapshotMeta, dump: &StateDump) -> Result<()> {
        let tx = self.store.db().begin_write()?;
        self.store.import_state_in(&tx, dump, meta.last_index)?;
        {
            tx.open_table(RAFT_LOG)?.retain(|_, _| false)?;
            let raw = serde_json::to_vec(&StoredSnapshot {
                index: meta.last_index,
                term: meta.last_term,
                config: meta.config.clone(),
            })?;
            tx.open_table(RAFT_META)?.insert(META_SNAPSHOT, raw.as_slice())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Consistent copy of the state machine and the index it corresponds to.
    pub fn export_snapshot(&self) -> Result<(Index, StateDump)> {
        self.store.export_state_at()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(n: u8) -> NodeId {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    fn entry(i: u64, t: u64) -> Entry {
        Entry {
            index: i,
            term: t,
            payload: Payload::Command(Command::KvSet { tenant: "t".into(), key: format!("k{i}"), value: i }),
        }
    }

    fn storage() -> RaftStorage {
        RaftStorage::open(Arc::new(Store::open_in_memory().unwrap())).unwrap()
    }

    #[test]
    fn empty_storage_loads_defaults() {
        let st = storage().load().unwrap();
        assert_eq!(st.hard_state, HardState::default());
        assert!(st.entries.is_empty());
        assert_eq!(st.snapshot, (0, 0));
    }

    #[test]
    fn hard_state_and_entries_roundtrip() {
        let s = storage();
        s.persist(Some(HardState { term: 4, voted_for: Some(ip(2)) }), None, &[entry(1, 1), entry(2, 1), entry(3, 2)])
            .unwrap();
        let st = s.load().unwrap();
        assert_eq!(st.hard_state, HardState { term: 4, voted_for: Some(ip(2)) });
        assert_eq!(st.entries.len(), 3);
        assert_eq!(st.entries[2], entry(3, 2));
    }

    #[test]
    fn truncate_then_append_replaces_the_suffix() {
        let s = storage();
        s.persist(None, None, &[entry(1, 1), entry(2, 1), entry(3, 1)]).unwrap();
        s.persist(None, Some(2), &[entry(2, 2)]).unwrap();
        let st = s.load().unwrap();
        assert_eq!(st.entries.iter().map(|e| (e.index, e.term)).collect::<Vec<_>>(), [(1, 1), (2, 2)]);
    }

    #[test]
    fn apply_records_the_index_and_state_atomically() {
        let s = storage();
        s.persist(None, None, &[entry(1, 1), entry(2, 1)]).unwrap();
        s.apply(&entry(1, 1)).unwrap();
        s.apply(&Entry { index: 2, term: 1, payload: Payload::Noop }).unwrap();
        assert_eq!(s.load().unwrap().applied, 2);
        assert_eq!(s.store().kv_get("t", "k1").unwrap(), Some(1));
    }

    #[test]
    fn compaction_keeps_the_tail_and_remembers_the_config() {
        let s = storage();
        s.persist(None, None, &(1..=10).map(|i| entry(i, 1)).collect::<Vec<_>>()).unwrap();
        let cfg = ClusterConfig::new([ip(1), ip(2)]);
        s.compact(7, 1, &cfg).unwrap();
        let st = s.load().unwrap();
        assert_eq!(st.snapshot, (7, 1));
        assert_eq!(st.snapshot_config, cfg);
        assert_eq!(st.entries.iter().map(|e| e.index).collect::<Vec<_>>(), [8, 9, 10]);
    }

    #[test]
    fn snapshot_install_swaps_state_log_and_meta_together() {
        let leader = storage();
        leader.persist(None, None, &[entry(1, 1), entry(2, 1)]).unwrap();
        leader.apply(&entry(1, 1)).unwrap();
        leader.apply(&entry(2, 1)).unwrap();
        let (idx, dump) = leader.export_snapshot().unwrap();
        assert_eq!(idx, 2);

        let follower = storage();
        follower.persist(None, None, &[entry(1, 9), entry(2, 9), entry(3, 9)]).unwrap();
        follower.store().apply(&Command::KvSet { tenant: "t".into(), key: "stale".into(), value: 1 }, Some(3)).unwrap();
        let meta = SnapshotMeta { term: 1, last_index: 2, last_term: 1, config: ClusterConfig::new([ip(1)]) };
        follower.install_snapshot(&meta, &dump).unwrap();
        let st = follower.load().unwrap();
        assert_eq!(st.snapshot, (2, 1));
        assert!(st.entries.is_empty(), "stale log is discarded");
        assert_eq!(st.applied, 2);
        assert_eq!(follower.store().kv_get("t", "stale").unwrap(), None);
        assert_eq!(follower.store().kv_get("t", "k2").unwrap(), Some(2));
    }

    #[test]
    fn cluster_id_is_remembered() {
        let s = storage();
        assert_eq!(s.cluster_id().unwrap(), None);
        s.set_cluster_id("prod").unwrap();
        assert_eq!(s.cluster_id().unwrap().as_deref(), Some("prod"));
    }
}
