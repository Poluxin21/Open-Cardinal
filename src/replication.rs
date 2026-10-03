//! Where state changes go: straight to the local store (single node) or through Raft.

use std::sync::Arc;
use std::time::Instant;

use crate::engine::Host;
use crate::error::Result;
use crate::store::{Command, Outcome, Store};

/// How a command becomes durable shared state.
pub enum Replicator {
    /// Single node: apply directly. This is the default and the original behaviour.
    Local(Arc<Store>),
    /// Cluster: propose to the Raft leader and wait until the entry is committed and applied.
    Raft(crate::raft::RaftHandle),
}

impl Replicator {
    pub async fn submit(&self, cmd: Command) -> Result<Outcome> {
        match self {
            Replicator::Local(store) => {
                let store = store.clone();
                // redb commits fsync; keep that off the async workers
                tokio::task::spawn_blocking(move || store.apply(&cmd, None))
                    .await
                    .map_err(|e| crate::Error::Other(format!("storage task failed: {e}")))?
            }
            Replicator::Raft(raft) => raft.propose(cmd).await,
        }
    }

    pub fn is_clustered(&self) -> bool {
        matches!(self, Replicator::Raft(_))
    }
}

/// [`Host`] used by rules: reads hit the local store (every node holds the full state),
/// writes are replicated.
pub struct ClusterHost {
    store: Arc<Store>,
    replicator: Arc<Replicator>,
    rt: tokio::runtime::Handle,
}

impl ClusterHost {
    pub fn new(store: Arc<Store>, replicator: Arc<Replicator>, rt: tokio::runtime::Handle) -> Self {
        Self { store, replicator, rt }
    }
}

impl Host for ClusterHost {
    fn kv_get(&self, tenant: &str, key: &str) -> Result<Option<u64>> {
        self.store.kv_get(tenant, key)
    }

    fn kv_len(&self, tenant: &str) -> Result<u64> {
        self.store.kv_len(tenant)
    }

    /// Called from rule-evaluation threads (`spawn_blocking`), never from an async worker,
    /// so blocking on the runtime here cannot deadlock it.
    fn read_barrier(&self, deadline: Instant) -> Result<()> {
        match &*self.replicator {
            Replicator::Local(_) => Ok(()),
            Replicator::Raft(raft) => self.rt.block_on(raft.read_barrier(deadline)),
        }
    }

    fn kv_apply(&self, cmd: Command, deadline: Instant) -> Result<Outcome> {
        match &*self.replicator {
            Replicator::Local(store) => store.apply(&cmd, None),
            Replicator::Raft(_) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(crate::Error::cluster("rule deadline reached before the write could be replicated"));
                }
                self.rt.block_on(async {
                    tokio::time::timeout(left, self.replicator.submit(cmd)).await.unwrap_or_else(|_| {
                        Err(crate::Error::cluster(
                            "write not replicated within the rule's time budget (is there a quorum?)",
                        ))
                    })
                })
            }
        }
    }
}
