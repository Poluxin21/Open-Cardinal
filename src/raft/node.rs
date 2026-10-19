//! The Raft driver: an async actor that owns the pure [`Core`], persists what it asks for,
//! sends its messages, applies committed entries to the store, and exposes a small handle
//! to the rest of the process.
//!
//! Order of work for every batch (this is what makes the protocol crash-safe):
//! persist (hard state, log) → send → apply committed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};

use super::config::RaftSettings;
use super::core::{Core, PersistedState};
use super::storage::RaftStorage;
use super::transport::{CallError, Peers};
use super::types::*;
use crate::app::Shutdown;
use crate::audit::{AuditLog, Event as AuditEvent};
use crate::error::{Error, Result};
use crate::pb::raft as pb;
use crate::store::{Command, Outcome, StateDump};
use serde_json::json;

const WORKER_QUEUE: usize = 64;
const MAX_BATCH: usize = 128;

#[derive(Debug, Clone)]
pub enum ProposeFailure {
    /// Nothing was appended: safe to retry (elsewhere).
    NotLeader(Option<NodeId>),
    /// The entry may or may not be committed; do not blindly retry.
    Indeterminate(String),
    Rejected(String),
}

pub enum Cmd {
    Step {
        from: NodeId,
        msg: Message,
        reply: Option<oneshot::Sender<Message>>,
    },
    InstallSnapshot {
        from: NodeId,
        meta: SnapshotMeta,
        dump: StateDump,
        reply: oneshot::Sender<Message>,
    },
    Propose {
        command: Command,
        reply: oneshot::Sender<std::result::Result<Outcome, ProposeFailure>>,
    },
    ConfChange {
        change: ConfChange,
        reply: oneshot::Sender<std::result::Result<(), ProposeFailure>>,
    },
    Transfer {
        to: Option<NodeId>,
        reply: oneshot::Sender<std::result::Result<NodeId, String>>,
    },
    Compact {
        reply: oneshot::Sender<std::result::Result<Index, String>>,
    },
    /// A blank node learnt that a cluster already exists without it: stop acting as a founder.
    BecomeJoiner,
    /// Leader only: a commit index that is safe to read at.
    ReadIndex {
        reply: oneshot::Sender<std::result::Result<Index, ReadError>>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct PeerStatus {
    pub ip: NodeId,
    pub role: &'static str,
    pub match_index: Option<Index>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub node: NodeId,
    pub cluster_id: String,
    pub role: &'static str,
    pub term: Term,
    pub leader: Option<NodeId>,
    pub commit_index: Index,
    pub last_index: Index,
    pub applied_index: Index,
    pub snapshot_index: Index,
    pub voters: Vec<NodeId>,
    pub learners: Vec<NodeId>,
    pub is_member: bool,
    pub removed: bool,
    pub failed: Option<String>,
    /// Filled on the leader only.
    pub peers: Vec<PeerStatus>,
}

impl Status {
    fn initial(node: NodeId, cluster_id: &str) -> Self {
        Self {
            node,
            cluster_id: cluster_id.into(),
            role: "starting",
            term: 0,
            leader: None,
            commit_index: 0,
            last_index: 0,
            applied_index: 0,
            snapshot_index: 0,
            voters: vec![],
            learners: vec![],
            is_member: false,
            removed: false,
            failed: None,
            peers: vec![],
        }
    }

    /// Can this node serve writes right now (it is, or can reach, a leader)?
    pub fn writable(&self) -> bool {
        self.is_member && self.leader.is_some() && self.failed.is_none() && !self.removed
    }
}

pub struct NodeShared {
    pub me: NodeId,
    pub cluster_id: String,
    pub tx: mpsc::Sender<Cmd>,
    pub status: watch::Receiver<Status>,
    pub peers: Arc<Peers>,
    pub settings: RaftSettings,
}

#[derive(Clone)]
pub struct RaftHandle {
    pub(crate) shared: Arc<NodeShared>,
}

impl RaftHandle {
    pub fn node(&self) -> NodeId {
        self.shared.me
    }

    pub fn status(&self) -> Status {
        self.shared.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.shared.status.clone()
    }

    /// Replicate `cmd` through the cluster and return its outcome once it is committed and
    /// applied on this node. Followers forward to the leader.
    pub async fn propose(&self, cmd: Command) -> Result<Outcome> {
        let s = &self.shared;
        let deadline = Instant::now() + s.settings.propose_timeout;
        let mut rx = s.status.clone();
        loop {
            let st = rx.borrow().clone();
            if st.removed {
                return Err(Error::cluster("this node was removed from the cluster"));
            }
            if let Some(f) = &st.failed {
                return Err(Error::cluster(format!("raft failed on this node: {f}")));
            }
            match st.leader {
                Some(l) if l == s.me => match self.propose_local(cmd.clone(), deadline).await {
                    Ok(o) => return Ok(o),
                    Err(ProposeFailure::NotLeader(_)) => {}
                    Err(ProposeFailure::Indeterminate(m)) => {
                        return Err(Error::cluster(format!("write outcome unknown: {m}")));
                    }
                    Err(ProposeFailure::Rejected(m)) => return Err(Error::cluster(m)),
                },
                Some(l) => match s.peers.propose(l, &cmd).await {
                    Ok(o) => return Ok(o),
                    Err(ProposeFailure::NotLeader(_)) => {}
                    Err(ProposeFailure::Indeterminate(m)) => {
                        return Err(Error::cluster(format!("write outcome unknown: {m}")));
                    }
                    Err(ProposeFailure::Rejected(m)) => return Err(Error::cluster(m)),
                },
                None => {}
            }
            // no leader yet (election in progress) or it just changed: wait and retry
            if Instant::now() >= deadline {
                return Err(Error::NotLeader(st.leader.map(|l| l.to_string())));
            }
            let _ = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        }
    }

    async fn propose_local(&self, command: Command, deadline: Instant) -> std::result::Result<Outcome, ProposeFailure> {
        let (tx, rx) = oneshot::channel();
        self.shared
            .tx
            .send(Cmd::Propose { command, reply: tx })
            .await
            .map_err(|_| ProposeFailure::Rejected("raft is shutting down".into()))?;
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(200));
        match tokio::time::timeout(left, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(ProposeFailure::Indeterminate("driver stopped".into())),
            Err(_) => Err(ProposeFailure::Indeterminate("timed out waiting for commit".into())),
        }
    }

    /// Operator action, forwarded to the leader when this node is not it.
    pub async fn admin(&self, op: &str, ip: Option<NodeId>) -> pb::AdminReply {
        let st = self.status();
        match st.leader {
            Some(l) if l == self.shared.me => run_admin(&self.shared, op, ip).await,
            Some(l) => match self.shared.peers.admin(l, op, ip).await {
                Ok(r) => r,
                Err(e) => pb::AdminReply {
                    ok: false,
                    error: format!("could not reach the leader {l}: {e}"),
                    ..Default::default()
                },
            },
            None => pb::AdminReply { ok: false, error: "no leader elected yet".into(), ..Default::default() },
        }
    }

    /// Hand leadership to another voter before shutting down, so a rolling restart does not
    /// cost an election timeout of write unavailability.
    pub async fn step_down(&self) {
        let st = self.status();
        if st.leader != Some(self.shared.me) || st.voters.len() < 2 {
            return;
        }
        let (tx, rx) = oneshot::channel();
        if self.shared.tx.send(Cmd::Transfer { to: None, reply: tx }).await.is_err() {
            return;
        }
        if !matches!(rx.await, Ok(Ok(_))) {
            return;
        }
        let mut sub = self.shared.status.clone();
        let wait = self.shared.settings.election_timeout * 2;
        let me = self.shared.me;
        let _ = tokio::time::timeout(wait, async {
            loop {
                if sub.borrow().leader != Some(me) {
                    return;
                }
                if sub.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    /// Wait until this node has applied everything that was acknowledged cluster-wide before
    /// the call (no-op when `reads = "local"`). `redb_api.get` calls this once per pulse.
    pub async fn read_barrier(&self, deadline: Instant) -> Result<()> {
        let s = &self.shared;
        if !s.settings.linearizable_reads {
            return Ok(());
        }
        let mut rx = s.status.clone();
        let index = loop {
            let st = rx.borrow().clone();
            if st.removed {
                return Err(Error::cluster("this node was removed from the cluster"));
            }
            let attempt: std::result::Result<Index, ReadError> = match st.leader {
                Some(l) if l == s.me => {
                    let (tx, rrx) = oneshot::channel();
                    if s.tx.send(Cmd::ReadIndex { reply: tx }).await.is_err() {
                        return Err(Error::cluster("raft is shutting down"));
                    }
                    rrx.await.unwrap_or(Err(ReadError::NotLeader(None)))
                }
                // an unreachable leader cannot vouch for anything
                Some(l) => match s.peers.read_index(l).await {
                    Ok(verdict) => verdict,
                    Err(_) => Err(ReadError::LeaseExpired),
                },
                None => Err(ReadError::NotLeader(None)),
            };
            match attempt {
                Ok(i) => break i,
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(Error::cluster(format!(
                            "cannot read consistently right now ({e:?}); is there a quorum?"
                        )));
                    }
                    let _ = tokio::time::timeout(Duration::from_millis(25), rx.changed()).await;
                }
            }
        };
        // our own state machine must have caught up with that index
        loop {
            if rx.borrow().applied_index >= index {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::cluster("timed out waiting to catch up with the cluster"));
            }
            let _ = tokio::time::timeout(left.min(Duration::from_millis(50)), rx.changed()).await;
        }
    }

    pub async fn compact_now(&self) -> std::result::Result<Index, String> {
        let (tx, rx) = oneshot::channel();
        self.shared.tx.send(Cmd::Compact { reply: tx }).await.map_err(|_| "raft is shutting down".to_string())?;
        rx.await.map_err(|_| "raft is shutting down".to_string())?
    }
}

/// Execute an operator request on *this* node, which must be the leader.
pub async fn run_admin(node: &Arc<NodeShared>, op: &str, ip: Option<NodeId>) -> pb::AdminReply {
    let fail = |m: String| pb::AdminReply { ok: false, error: m, ..Default::default() };
    match op {
        "add_peer" | "remove_peer" => {
            let Some(ip) = ip else { return fail("an ip is required".into()) };
            let change = if op == "add_peer" { ConfChange::AddLearner(ip) } else { ConfChange::Remove(ip) };
            let (tx, rx) = oneshot::channel();
            if node.tx.send(Cmd::ConfChange { change, reply: tx }).await.is_err() {
                return fail("raft is shutting down".into());
            }
            match rx.await {
                Ok(Ok(())) => pb::AdminReply {
                    ok: true,
                    message: if op == "add_peer" {
                        format!("{ip} added as a learner; it is promoted to voter automatically once it caught up")
                    } else {
                        format!("{ip} is being removed from the cluster")
                    },
                    ..Default::default()
                },
                Ok(Err(ProposeFailure::NotLeader(h))) => pb::AdminReply {
                    ok: false,
                    error: "not the leader".into(),
                    leader_hint: h.map(|h| h.to_string()).unwrap_or_default(),
                    ..Default::default()
                },
                Ok(Err(ProposeFailure::Rejected(m))) | Ok(Err(ProposeFailure::Indeterminate(m))) => fail(m),
                Err(_) => fail("raft is shutting down".into()),
            }
        }
        "transfer_leader" => {
            let (tx, rx) = oneshot::channel();
            if node.tx.send(Cmd::Transfer { to: ip, reply: tx }).await.is_err() {
                return fail("raft is shutting down".into());
            }
            match rx.await {
                Ok(Ok(to)) => {
                    pb::AdminReply { ok: true, message: format!("leadership is moving to {to}"), ..Default::default() }
                }
                Ok(Err(m)) => fail(m),
                Err(_) => fail("raft is shutting down".into()),
            }
        }
        "compact" => {
            let (tx, rx) = oneshot::channel();
            if node.tx.send(Cmd::Compact { reply: tx }).await.is_err() {
                return fail("raft is shutting down".into());
            }
            match rx.await {
                Ok(Ok(i)) => {
                    pb::AdminReply { ok: true, message: format!("log compacted up to index {i}"), ..Default::default() }
                }
                Ok(Err(m)) => fail(m),
                Err(_) => fail("raft is shutting down".into()),
            }
        }
        other => fail(format!("unknown operation '{other}'")),
    }
}

// ---------------------------------------------------------------------------------------
// driver
// ---------------------------------------------------------------------------------------

enum Outbound {
    Msg(Message),
    Snapshot(SnapshotMeta, Arc<StateDump>),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum ReqKind {
    PreVote,
    Vote,
    Append,
    Snapshot,
}

struct PendingReply {
    from: NodeId,
    kind: ReqKind,
    tx: oneshot::Sender<Message>,
}

struct Waiter {
    term: Term,
    deadline: Instant,
    tx: oneshot::Sender<std::result::Result<Outcome, ProposeFailure>>,
}

pub struct Driver {
    me: NodeId,
    cluster_id: String,
    core: Core,
    storage: Arc<RaftStorage>,
    peers: Arc<Peers>,
    settings: RaftSettings,
    audit: Arc<AuditLog>,
    shutdown: Shutdown,

    inbox: mpsc::Receiver<Cmd>,
    self_tx: mpsc::Sender<Cmd>,
    status_tx: watch::Sender<Status>,
    workers: HashMap<NodeId, mpsc::Sender<Outbound>>,

    waiters: BTreeMap<Index, Waiter>,
    pending_replies: Vec<PendingReply>,
    snapshot_in: Option<(SnapshotMeta, StateDump)>,

    applied: Index,
    base_config: ClusterConfig,
    conf_log: Vec<(Index, ClusterConfig)>,
    failed: Option<String>,
    last_sweep: Instant,
}

impl Driver {
    fn config_at(&self, index: Index) -> ClusterConfig {
        self.conf_log
            .iter()
            .rev()
            .find(|(i, _)| *i <= index)
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| self.base_config.clone())
    }

    pub async fn run(mut self) {
        let mut ticker = tokio::time::interval(RaftSettings::TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.wait() => break,
                _ = ticker.tick() => self.core.tick(),
                cmd = self.inbox.recv() => match cmd {
                    Some(c) => self.handle_cmd(c).await,
                    None => break,
                },
            }
            for _ in 0..MAX_BATCH {
                match self.inbox.try_recv() {
                    Ok(c) => self.handle_cmd(c).await,
                    Err(_) => break,
                }
            }
            if let Err(e) = self.process_ready().await {
                // The state machine can no longer be trusted to match the log: stop taking
                // part in the cluster rather than risk diverging from it.
                tracing::error!("raft storage failure, this node stops participating: {e}");
                self.failed = Some(e.to_string());
                self.publish_status();
                break;
            }
            if self.last_sweep.elapsed() > Duration::from_secs(1) {
                self.sweep_waiters();
                self.last_sweep = Instant::now();
            }
        }
        // anything still waiting will never be answered by this driver
        for (_, w) in std::mem::take(&mut self.waiters) {
            let _ = w.tx.send(Err(ProposeFailure::Indeterminate("raft stopped".into())));
        }
        tracing::info!("raft driver stopped");
    }

    fn sweep_waiters(&mut self) {
        let now = Instant::now();
        let expired: Vec<Index> = self.waiters.iter().filter(|(_, w)| w.deadline <= now).map(|(i, _)| *i).collect();
        for i in expired {
            if let Some(w) = self.waiters.remove(&i) {
                let _ = w.tx.send(Err(ProposeFailure::Indeterminate("timed out waiting for commit".into())));
            }
        }
    }

    async fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Step { from, msg, reply } => {
                if let Some(tx) = reply {
                    let kind = match &msg {
                        Message::PreVote(_) => Some(ReqKind::PreVote),
                        Message::Vote(_) => Some(ReqKind::Vote),
                        Message::Append(_) => Some(ReqKind::Append),
                        _ => None,
                    };
                    match kind {
                        Some(kind) => self.pending_replies.push(PendingReply { from, kind, tx }),
                        None => drop(tx),
                    }
                }
                self.core.step(from, msg);
            }
            Cmd::InstallSnapshot { from, meta, dump, reply } => {
                self.snapshot_in = Some((meta.clone(), dump));
                self.pending_replies.push(PendingReply { from, kind: ReqKind::Snapshot, tx: reply });
                self.core.step(from, Message::Snapshot(meta));
            }
            Cmd::Propose { command, reply } => match self.core.propose(Payload::Command(command)) {
                Ok((index, term)) => {
                    let deadline = Instant::now() + self.settings.propose_timeout;
                    self.waiters.insert(index, Waiter { term, deadline, tx: reply });
                }
                Err(ProposeError::NotLeader(h)) => {
                    let _ = reply.send(Err(ProposeFailure::NotLeader(h)));
                }
                Err(e) => {
                    let _ = reply.send(Err(ProposeFailure::Rejected(e.to_string())));
                }
            },
            Cmd::ConfChange { change, reply } => {
                // adding a node that is already a member is a successful no-op (idempotent join)
                if let ConfChange::AddLearner(n) = &change
                    && self.core.config().contains(n)
                {
                    let _ = reply.send(Ok(()));
                    return;
                }
                let r = match self.core.propose_conf_change(change) {
                    Ok(_) => Ok(()),
                    Err(ProposeError::NotLeader(h)) => Err(ProposeFailure::NotLeader(h)),
                    Err(e) => Err(ProposeFailure::Rejected(e.to_string())),
                };
                let _ = reply.send(r);
            }
            Cmd::Transfer { to, reply } => {
                let target = to.or_else(|| {
                    self.core
                        .config()
                        .voters
                        .iter()
                        .copied()
                        .filter(|v| *v != self.me)
                        .max_by_key(|v| self.core.match_index_of(v).unwrap_or(0))
                });
                let r = match target {
                    None => Err("there is no other voter to hand leadership to".to_string()),
                    Some(t) => self.core.transfer_leadership(t).map(|_| t).map_err(|e| e.to_string()),
                };
                let _ = reply.send(r);
            }
            Cmd::Compact { reply } => {
                // "nothing to do" is a success: the log is already as small as it can get
                let r = self
                    .compact(true)
                    .await
                    .map(|o| o.unwrap_or_else(|| self.core.snapshot_index()))
                    .map_err(|e| e.to_string());
                let _ = reply.send(r);
            }
            Cmd::BecomeJoiner => {
                self.core.become_joiner();
            }
            Cmd::ReadIndex { reply } => {
                let _ = reply.send(self.core.read_index());
            }
        }
    }

    async fn persist(&self, hs: Option<HardState>, truncate: Option<Index>, append: Vec<Entry>) -> Result<()> {
        if hs.is_none() && truncate.is_none() && append.is_empty() {
            return Ok(());
        }
        let storage = self.storage.clone();
        tokio::task::spawn_blocking(move || storage.persist(hs, truncate, &append))
            .await
            .map_err(|e| Error::Storage(format!("persist task: {e}")))?
    }

    async fn process_ready(&mut self) -> Result<()> {
        while self.core.has_ready() {
            let rd = self.core.ready();

            // 1. durability first: never answer or acknowledge what is not on disk
            self.persist(rd.hard_state, rd.truncate_from, rd.append).await?;

            // snapshot installation replaces state and log atomically
            if let Some(inst) = rd.install_snapshot {
                self.install_snapshot(inst).await;
            }

            // 2. send (or answer the RPC that is waiting for this very response)
            for (to, msg) in rd.messages {
                self.route(to, msg);
            }
            self.answer_unanswered();
            if !rd.send_snapshot.is_empty() {
                self.send_snapshots(&rd.send_snapshot).await;
            }

            // 3. apply
            if !rd.committed.is_empty() {
                self.apply(rd.committed).await?;
            }

            for ev in rd.events {
                self.on_event(ev);
            }
            self.compact(false).await?;
        }
        self.publish_status();
        Ok(())
    }

    /// A response to a request we are currently answering goes back on that RPC; everything
    /// else is queued for the peer's worker.
    fn route(&mut self, to: NodeId, msg: Message) {
        let kind = match &msg {
            Message::PreVoteResp(_) => Some(ReqKind::PreVote),
            Message::VoteResp(_) => Some(ReqKind::Vote),
            Message::AppendResp(_) => Some(ReqKind::Append),
            Message::SnapshotResp(_) => Some(ReqKind::Snapshot),
            _ => None,
        };
        if let Some(kind) = kind {
            if let Some(pos) = self.pending_replies.iter().position(|p| p.from == to && p.kind == kind) {
                let p = self.pending_replies.remove(pos);
                let _ = p.tx.send(msg);
                return;
            }
            // a response nobody asked for (cannot happen with a well-behaved core)
            return;
        }
        self.enqueue(to, Outbound::Msg(msg));
    }

    /// Requests the core ignored (stickiness, stale terms it chose not to answer...) must
    /// still get *some* reply, or the caller would wait for its timeout.
    fn answer_unanswered(&mut self) {
        let term = self.core.term();
        for p in std::mem::take(&mut self.pending_replies) {
            let reply = match p.kind {
                ReqKind::PreVote => Message::PreVoteResp(VoteResp { term, granted: false }),
                ReqKind::Vote => Message::VoteResp(VoteResp { term, granted: false }),
                ReqKind::Append => {
                    Message::AppendResp(AppendResp { term, success: false, match_index: 0, hint_index: 0 })
                }
                ReqKind::Snapshot => {
                    // the snapshot may still be waiting for installation: keep it
                    if self.snapshot_in.is_some() {
                        self.pending_replies.push(p);
                        continue;
                    }
                    Message::SnapshotResp(SnapshotResp { term, success: false, last_index: 0 })
                }
            };
            let _ = p.tx.send(reply);
        }
    }

    fn enqueue(&mut self, to: NodeId, out: Outbound) {
        let tx = self.workers.entry(to).or_insert_with(|| {
            let (tx, rx) = mpsc::channel(WORKER_QUEUE);
            tokio::spawn(peer_worker(self.peers.clone(), to, rx, self.self_tx.clone()));
            tx
        });
        // a full queue means the peer is slow or down; heartbeats will retry
        let _ = tx.try_send(out);
    }

    async fn send_snapshots(&mut self, peers: &[NodeId]) {
        let storage = self.storage.clone();
        let exported = tokio::task::spawn_blocking(move || storage.export_snapshot()).await;
        let (index, dump) = match exported {
            Ok(Ok(x)) => x,
            other => {
                tracing::error!("cannot export a snapshot: {other:?}");
                return;
            }
        };
        let Some(last_term) = self.core.term_at(index) else {
            tracing::error!("snapshot index {index} has no known term");
            return;
        };
        let meta = SnapshotMeta { term: self.core.term(), last_index: index, last_term, config: self.config_at(index) };
        let dump = Arc::new(dump);
        for p in peers {
            tracing::info!(peer = %p, index, "sending snapshot");
            self.enqueue(*p, Outbound::Snapshot(meta.clone(), dump.clone()));
        }
    }

    async fn install_snapshot(&mut self, inst: InstallSnapshot) {
        let Some((meta, dump)) = self.snapshot_in.take() else { return };
        let storage = self.storage.clone();
        let m = meta.clone();
        let res = tokio::task::spawn_blocking(move || storage.install_snapshot(&m, &dump)).await;
        let ok = matches!(res, Ok(Ok(())));
        if ok {
            self.core.snapshot_installed(&meta);
            self.applied = meta.last_index;
            self.base_config = meta.config.clone();
            self.conf_log.clear();
            for (_, w) in std::mem::take(&mut self.waiters) {
                let _ = w.tx.send(Err(ProposeFailure::Indeterminate("state replaced by a snapshot".into())));
            }
            tracing::info!(index = meta.last_index, "installed snapshot from {}", inst.from);
            self.audit.record(AuditEvent::new(
                "cluster",
                "-",
                json!({ "event": "snapshot_installed", "index": meta.last_index, "from": inst.from.to_string() }),
            ));
        } else {
            tracing::error!("snapshot install failed: {res:?}");
        }
        let term = self.core.term();
        let resp =
            Message::SnapshotResp(SnapshotResp { term, success: ok, last_index: if ok { meta.last_index } else { 0 } });
        if let Some(pos) = self.pending_replies.iter().position(|p| p.from == inst.from && p.kind == ReqKind::Snapshot)
        {
            let p = self.pending_replies.remove(pos);
            let _ = p.tx.send(resp);
        }
    }

    async fn apply(&mut self, entries: Vec<Entry>) -> Result<()> {
        let storage = self.storage.clone();
        let applied = tokio::task::spawn_blocking(move || {
            entries
                .into_iter()
                .map(|e| {
                    let out = storage.apply(&e);
                    (e, out)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| Error::Storage(format!("apply task: {e}")))?;

        for (entry, outcome) in applied {
            let outcome = outcome?; // a failed apply is fatal (see `run`)
            self.applied = entry.index;
            if let Payload::Config(c) = &entry.payload {
                self.conf_log.push((entry.index, c.clone()));
            }
            if let Some(w) = self.waiters.remove(&entry.index) {
                let res = if w.term == entry.term {
                    Ok(outcome)
                } else {
                    // our proposal was overwritten by another leader's entry: definitely not applied
                    Err(ProposeFailure::NotLeader(self.core.leader()))
                };
                let _ = w.tx.send(res);
            }
        }
        Ok(())
    }

    async fn compact(&mut self, force: bool) -> Result<Option<Index>> {
        let snap = self.core.snapshot_index();
        let threshold = self.settings.snapshot_threshold;
        if !force && self.applied.saturating_sub(snap) < threshold {
            return Ok(None);
        }
        let keep = if force { 10 } else { (threshold / 10).clamp(10, 1000) };
        let upto = self.applied.saturating_sub(keep);
        if upto <= snap {
            return Ok(None);
        }
        let Some(term) = self.core.term_at(upto) else { return Ok(None) };
        let cfg = self.config_at(upto);
        let storage = self.storage.clone();
        let c2 = cfg.clone();
        tokio::task::spawn_blocking(move || storage.compact(upto, term, &c2))
            .await
            .map_err(|e| Error::Storage(format!("compact task: {e}")))??;
        self.core.compacted(upto, term, cfg.clone());
        self.base_config = cfg;
        self.conf_log.retain(|(i, _)| *i > upto);
        tracing::debug!(upto, "raft log compacted");
        Ok(Some(upto))
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::BecameLeader { term } => {
                tracing::info!(term, "this node is now the cluster LEADER");
            }
            Event::LeaderChanged { leader } => {
                tracing::info!(leader = ?leader.map(|l| l.to_string()), term = self.core.term(), "leader changed");
                self.audit.record(AuditEvent::new(
                    "cluster",
                    "-",
                    json!({ "event": "leader_changed", "leader": leader.map(|l| l.to_string()), "term": self.core.term() }),
                ));
            }
            Event::ConfigChanged(c) => {
                tracing::info!(voters = ?c.voters, learners = ?c.learners, "cluster membership changed");
                self.audit.record(AuditEvent::new(
                    "cluster",
                    "-",
                    json!({
                        "event": "membership_changed",
                        "voters": c.voters.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
                        "learners": c.learners.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
                    }),
                ));
            }
            Event::Removed => {
                tracing::warn!("this node was removed from the cluster; it no longer takes part in consensus");
                self.audit.record(AuditEvent::new("cluster", "-", json!({ "event": "removed" })));
            }
            Event::BecameFollower { .. } => {}
        }
    }

    fn publish_status(&self) {
        let cfg = self.core.config();
        let is_member = cfg.contains(&self.me);
        let role = if self.core.is_removed() {
            "removed"
        } else if cfg.is_empty() {
            "joining"
        } else if !is_member {
            "non-member"
        } else if cfg.learners.contains(&self.me) {
            "learner"
        } else {
            self.core.role().as_str()
        };
        let peers = if self.core.is_leader() {
            cfg.members()
                .filter(|m| **m != self.me)
                .map(|m| PeerStatus {
                    ip: *m,
                    role: if cfg.learners.contains(m) { "learner" } else { "voter" },
                    match_index: self.core.match_index_of(m),
                })
                .collect()
        } else {
            Vec::new()
        };
        let st = Status {
            node: self.me,
            cluster_id: self.cluster_id.clone(),
            role,
            term: self.core.term(),
            leader: self.core.leader(),
            commit_index: self.core.commit_index(),
            last_index: self.core.last_index(),
            applied_index: self.applied,
            snapshot_index: self.core.snapshot_index(),
            voters: cfg.voters.iter().copied().collect(),
            learners: cfg.learners.iter().copied().collect(),
            is_member,
            removed: self.core.is_removed(),
            failed: self.failed.clone(),
            peers,
        };
        self.status_tx.send_if_modified(|cur| {
            let changed = cur.role != st.role
                || cur.term != st.term
                || cur.leader != st.leader
                || cur.commit_index != st.commit_index
                || cur.last_index != st.last_index
                || cur.applied_index != st.applied_index
                || cur.snapshot_index != st.snapshot_index
                || cur.voters != st.voters
                || cur.learners != st.learners
                || cur.removed != st.removed
                || cur.failed != st.failed
                || cur.peers.len() != st.peers.len()
                || cur.peers.iter().zip(&st.peers).any(|(a, b)| a.match_index != b.match_index);
            if changed {
                *cur = st;
            }
            changed
        });
    }
}

async fn peer_worker(peers: Arc<Peers>, to: NodeId, mut rx: mpsc::Receiver<Outbound>, back: mpsc::Sender<Cmd>) {
    while let Some(out) = rx.recv().await {
        let result: std::result::Result<Option<Message>, CallError> = match out {
            Outbound::Msg(m) => peers.send(to, m).await,
            Outbound::Snapshot(meta, dump) => peers.send_snapshot(to, &meta, &dump).await.map(Some),
        };
        let msg = match result {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(peer = %to, "rpc failed: {e}");
                Message::Unreachable
            }
        };
        if back.send(Cmd::Step { from: to, msg, reply: None }).await.is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------
// startup
// ---------------------------------------------------------------------------------------

/// Everything `start` needs from the surrounding process.
pub struct StartArgs {
    pub settings: RaftSettings,
    pub store: Arc<crate::store::Store>,
    pub audit: Arc<AuditLog>,
    pub shutdown: Shutdown,
}

pub async fn start(args: StartArgs) -> Result<RaftHandle> {
    super::discovery::start(args).await
}

pub(crate) struct Built {
    pub handle: RaftHandle,
    pub driver: Driver,
}

/// Assemble the driver around a core. Called once the node's own address is known.
pub(crate) fn build(
    me: NodeId,
    args: &StartArgs,
    persisted: PersistedState,
    bootstrap: Option<ClusterConfig>,
    storage: Arc<RaftStorage>,
    peers: Arc<Peers>,
) -> Built {
    let settings = args.settings.clone();
    let seed = crate::util::random_u64();
    let boot_for_base = bootstrap.clone().unwrap_or_default();

    let mut base_config =
        if persisted.snapshot_config.is_empty() { boot_for_base } else { persisted.snapshot_config.clone() };
    let mut conf_log = Vec::new();
    for e in persisted.entries.iter().filter(|e| e.index <= persisted.applied) {
        if let Payload::Config(c) = &e.payload {
            conf_log.push((e.index, c.clone()));
        }
    }
    if base_config.is_empty() {
        base_config = ClusterConfig::default();
    }
    let applied = persisted.applied;
    let core = Core::new(me, settings.core_config(seed), persisted, bootstrap);

    let (tx, inbox) = mpsc::channel(4096);
    let (status_tx, status_rx) = watch::channel(Status::initial(me, &settings.cluster_id));
    let shared = Arc::new(NodeShared {
        me,
        cluster_id: settings.cluster_id.clone(),
        tx: tx.clone(),
        status: status_rx,
        peers: peers.clone(),
        settings: settings.clone(),
    });
    let driver = Driver {
        me,
        cluster_id: settings.cluster_id.clone(),
        core,
        storage,
        peers,
        settings,
        audit: args.audit.clone(),
        shutdown: args.shutdown.clone(),
        inbox,
        self_tx: tx,
        status_tx,
        workers: HashMap::new(),
        waiters: BTreeMap::new(),
        pending_replies: Vec::new(),
        snapshot_in: None,
        applied,
        base_config,
        conf_log,
        failed: None,
        last_sweep: Instant::now(),
    };
    Built { handle: RaftHandle { shared }, driver }
}
