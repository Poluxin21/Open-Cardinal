//! The Raft state machine, as a pure deterministic core: no I/O, no clocks, no threads.
//!
//! Inputs are `tick()`, `step(from, message)`, `propose*()`; the output is a [`Ready`]
//! batch the driver persists, sends and applies — in that order. Keeping the core pure is
//! what lets `sim.rs` run whole clusters under partitions, message loss and reordering and
//! check the Raft safety properties after every step.
//!
//! Implemented: leader election with PreVote and leader stickiness (a partitioned or
//! restarted node cannot disrupt a healthy leader), CheckQuorum (a leader that loses its
//! majority steps down), log replication with fast backtracking, commit only of
//! current-term entries, single-server membership changes with learners and
//! auto-promotion, leadership transfer, and snapshot-based catch-up after log compaction.

use std::collections::{BTreeMap, VecDeque};

use super::types::*;

#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// Minimum election timeout in ticks; the actual one is random in `[n, 2n)`.
    pub election_ticks: u64,
    pub heartbeat_ticks: u64,
    pub max_entries_per_append: usize,
    /// Uncommitted entries a leader accepts before answering `Busy`.
    pub max_uncommitted: usize,
    /// A snapshot transfer with no answer for this many ticks is considered lost and retried.
    pub snapshot_timeout_ticks: u64,
    pub seed: u64,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            election_ticks: 10,
            heartbeat_ticks: 2,
            max_entries_per_append: 128,
            max_uncommitted: 4096,
            snapshot_timeout_ticks: 100,
            seed: 1,
        }
    }
}

/// What a node boots with, read back from disk.
#[derive(Clone, Debug, Default)]
pub struct PersistedState {
    pub hard_state: HardState,
    /// `(index, term)` of the last entry covered by the snapshot / compaction (0,0 when none).
    pub snapshot: (Index, Term),
    /// Membership as of `snapshot.0` (empty when there is no snapshot).
    pub snapshot_config: ClusterConfig,
    /// Entries after the snapshot, in order.
    pub entries: Vec<Entry>,
    /// Highest index already applied to the state machine.
    pub applied: Index,
}

struct Log {
    snap_index: Index,
    snap_term: Term,
    entries: VecDeque<Entry>,
}

impl Log {
    fn last_index(&self) -> Index {
        self.snap_index + self.entries.len() as u64
    }
    fn last_term(&self) -> Term {
        self.entries.back().map(|e| e.term).unwrap_or(self.snap_term)
    }
    fn term_at(&self, i: Index) -> Option<Term> {
        if i == self.snap_index {
            return Some(self.snap_term);
        }
        if i < self.snap_index || i > self.last_index() {
            return None;
        }
        self.entries.get((i - self.snap_index - 1) as usize).map(|e| e.term)
    }
    fn get(&self, i: Index) -> Option<&Entry> {
        if i <= self.snap_index || i > self.last_index() {
            return None;
        }
        self.entries.get((i - self.snap_index - 1) as usize)
    }
    fn slice(&self, from: Index, max: usize) -> Vec<Entry> {
        if from <= self.snap_index || from > self.last_index() {
            return Vec::new();
        }
        let start = (from - self.snap_index - 1) as usize;
        self.entries.iter().skip(start).take(max).cloned().collect()
    }
    fn truncate_from(&mut self, i: Index) {
        if i > self.snap_index && i <= self.last_index() {
            self.entries.truncate((i - self.snap_index - 1) as usize);
        }
    }
}

#[derive(Clone, Debug)]
struct Progress {
    next: Index,
    matched: Index,
    /// A request is in flight; wait for its answer (or `Unreachable`) before sending more.
    inflight: bool,
    /// Heard from this peer since the last CheckQuorum round.
    active: bool,
    snapshot_pending: bool,
    /// Ticks since the snapshot was handed to the transport.
    snapshot_age: u64,
    /// Tick at which this peer last answered us (any answer proves it still follows us).
    last_ack: u64,
}

/// A node removed from the membership keeps receiving appends until it has had the chance
/// to learn the removal (otherwise it would never know and keep campaigning).
struct Leaving {
    conf_index: Index,
    /// Starts counting down once the removal is committed.
    countdown: Option<u64>,
}

struct Transfer {
    to: NodeId,
    elapsed: u64,
    sent: bool,
}

pub struct Core {
    id: NodeId,
    cfg: CoreConfig,

    term: Term,
    voted_for: Option<NodeId>,
    log: Log,

    bootstrap: ClusterConfig,
    snap_config: ClusterConfig,
    config: ClusterConfig,

    role: Role,
    leader: Option<NodeId>,
    commit: Index,
    handed_to_apply: Index,

    election_elapsed: u64,
    election_timeout: u64,
    heartbeat_elapsed: u64,

    votes: BTreeMap<NodeId, bool>,
    progress: BTreeMap<NodeId, Progress>,
    /// Index of the no-op this leader appended in its term; membership changes wait until
    /// it is committed (Raft thesis §4.1 bug fix).
    noop_index: Index,
    transfer: Option<Transfer>,
    leaving: BTreeMap<NodeId, Leaving>,
    removed: bool,

    rng: u64,
    out: Ready,
    hs_dirty: bool,
    /// Monotonic tick counter (drives the leader lease).
    now: u64,
}

impl Core {
    /// `bootstrap` is the static initial membership used only when neither the log nor a
    /// snapshot carries one. `None` starts the node as a non-member waiting to be added.
    pub fn new(id: NodeId, cfg: CoreConfig, state: PersistedState, bootstrap: Option<ClusterConfig>) -> Self {
        let rng = cfg.seed ^ hash_ip(&id) ^ 0x9E37_79B9_7F4A_7C15;
        let mut core = Core {
            id,
            term: state.hard_state.term,
            voted_for: state.hard_state.voted_for,
            log: Log { snap_index: state.snapshot.0, snap_term: state.snapshot.1, entries: state.entries.into() },
            bootstrap: bootstrap.unwrap_or_default(),
            snap_config: state.snapshot_config,
            config: ClusterConfig::default(),
            role: Role::Follower,
            leader: None,
            commit: state.applied.max(state.snapshot.0),
            handed_to_apply: state.applied.max(state.snapshot.0),
            election_elapsed: 0,
            election_timeout: cfg.election_ticks,
            heartbeat_elapsed: 0,
            votes: BTreeMap::new(),
            progress: BTreeMap::new(),
            noop_index: 0,
            transfer: None,
            leaving: BTreeMap::new(),
            removed: false,
            rng,
            out: Ready::default(),
            hs_dirty: false,
            now: 0,
            cfg,
        };
        core.recompute_config();
        core.reset_election_timer();
        core
    }

    // ---- accessors ------------------------------------------------------------------

    pub fn id(&self) -> NodeId {
        self.id
    }
    pub fn term(&self) -> Term {
        self.term
    }
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn leader(&self) -> Option<NodeId> {
        self.leader
    }
    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }
    pub fn commit_index(&self) -> Index {
        self.commit
    }
    pub fn last_index(&self) -> Index {
        self.log.last_index()
    }
    pub fn last_term(&self) -> Term {
        self.log.last_term()
    }
    pub fn snapshot_index(&self) -> Index {
        self.log.snap_index
    }
    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }
    pub fn is_voter(&self) -> bool {
        self.config.voters.contains(&self.id)
    }
    pub fn is_removed(&self) -> bool {
        self.removed
    }
    pub fn has_state(&self) -> bool {
        self.log.last_index() > 0 || self.term > 0
    }
    pub fn term_at(&self, i: Index) -> Option<Term> {
        self.log.term_at(i)
    }
    pub fn voted_for(&self) -> Option<NodeId> {
        self.voted_for
    }
    pub fn entry(&self, i: Index) -> Option<&Entry> {
        self.log.get(i)
    }
    pub fn match_index_of(&self, peer: &NodeId) -> Option<Index> {
        self.progress.get(peer).map(|p| p.matched)
    }

    // ---- driver interface -----------------------------------------------------------

    pub fn ready(&mut self) -> Ready {
        let mut r = std::mem::take(&mut self.out);
        if self.hs_dirty {
            r.hard_state = Some(HardState { term: self.term, voted_for: self.voted_for });
            self.hs_dirty = false;
        }
        if self.commit > self.handed_to_apply {
            let from = self.handed_to_apply + 1;
            let to = self.commit;
            r.committed = self.log.slice(from, (to - from + 1) as usize);
            // entries below the snapshot were applied by definition
            self.handed_to_apply = to;
        }
        r
    }

    pub fn has_ready(&self) -> bool {
        self.hs_dirty || !self.out.is_empty() || self.commit > self.handed_to_apply
    }

    /// The driver compacted the log: entries up to `index` are gone (state machine holds them).
    pub fn compacted(&mut self, index: Index, term: Term, config: ClusterConfig) {
        if index <= self.log.snap_index || index > self.log.last_index() {
            return;
        }
        let drop = (index - self.log.snap_index) as usize;
        self.log.entries.drain(..drop);
        self.log.snap_index = index;
        self.log.snap_term = term;
        self.snap_config = config;
    }

    /// The driver finished installing a snapshot: state machine and storage now sit at
    /// `meta.last_index` and the log restarts there.
    pub fn snapshot_installed(&mut self, meta: &SnapshotMeta) {
        self.log.entries.clear();
        self.log.snap_index = meta.last_index;
        self.log.snap_term = meta.last_term;
        self.snap_config = meta.config.clone();
        self.commit = self.commit.max(meta.last_index);
        self.handed_to_apply = meta.last_index;
        self.recompute_config();
    }

    /// Leader: a snapshot was handed to the transport; stop re-requesting it.
    pub fn snapshot_in_flight(&mut self, peer: &NodeId) {
        if let Some(p) = self.progress.get_mut(peer) {
            p.snapshot_pending = true;
            p.snapshot_age = 0;
            p.inflight = true;
        }
    }

    /// A blank node (nothing replicated yet) that learnt a cluster already runs without it:
    /// drop the static founding membership and wait to be added by the leader.
    pub fn become_joiner(&mut self) {
        if self.log.last_index() > 0 || self.log.snap_index > 0 {
            return;
        }
        self.bootstrap = ClusterConfig::default();
        self.recompute_config();
        if self.role != Role::Follower {
            let term = self.term;
            self.become_follower(term, None);
        }
        self.votes.clear();
    }

    // ---- time -----------------------------------------------------------------------

    pub fn tick(&mut self) {
        self.now += 1;
        match self.role {
            Role::Leader => self.tick_leader(),
            _ => {
                self.election_elapsed += 1;
                if self.election_elapsed >= self.election_timeout && self.is_voter() && !self.removed {
                    self.campaign_pre();
                }
            }
        }
    }

    fn tick_leader(&mut self) {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;

        if let Some(t) = &mut self.transfer {
            t.elapsed += 1;
            if t.elapsed >= self.cfg.election_ticks {
                self.transfer = None; // the target did not take over in time
            }
        }

        if self.election_elapsed >= self.election_timeout {
            self.election_elapsed = 0;
            // CheckQuorum: have a majority of voters answered since the last round?
            let mut active = 1; // ourselves
            for (id, p) in self.progress.iter_mut() {
                if self.config.voters.contains(id) && p.active {
                    active += 1;
                }
                p.active = false;
            }
            if self.config.voters.contains(&self.id) && active < self.config.quorum() {
                let term = self.term;
                self.become_follower(term, None);
                return;
            }
        }
        // expire snapshot transfers that never got an answer
        let timeout = self.cfg.snapshot_timeout_ticks;
        for p in self.progress.values_mut() {
            if p.snapshot_pending {
                p.snapshot_age += 1;
                if p.snapshot_age >= timeout {
                    p.snapshot_pending = false;
                    p.snapshot_age = 0;
                    p.inflight = false;
                }
            }
        }
        // forget removed nodes once they had time to learn about their removal
        let mut done = Vec::new();
        for (id, l) in self.leaving.iter_mut() {
            if let Some(c) = &mut l.countdown {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    done.push(*id);
                }
            }
        }
        for id in done {
            self.leaving.remove(&id);
            self.progress.remove(&id);
        }
        if self.heartbeat_elapsed >= self.cfg.heartbeat_ticks {
            self.heartbeat_elapsed = 0;
            // a heartbeat round also recovers peers whose request or answer was lost
            for p in self.progress.values_mut() {
                if !p.snapshot_pending {
                    p.inflight = false;
                }
            }
            self.broadcast_append();
        }
    }

    // ---- linearizable reads ---------------------------------------------------------

    /// A commit index that includes every write acknowledged before this call, if this
    /// node can still vouch for its leadership.
    ///
    /// The vouching is a **leader lease**: a majority of voters answered us within half an
    /// election timeout. Followers refuse to vote for anyone else while they hear from a
    /// live leader (stickiness), so no other leader can have been elected inside that window.
    /// (It assumes clock *rates* do not differ by 2x, the usual lease assumption.)
    pub fn read_index(&self) -> Result<Index, ReadError> {
        if self.role != Role::Leader {
            return Err(ReadError::NotLeader(self.leader));
        }
        if self.commit < self.noop_index {
            return Err(ReadError::NotReady);
        }
        let need = self.config.quorum();
        if need > 1 {
            let lease = (self.cfg.election_ticks / 2).max(1);
            let mut acks: Vec<u64> = self
                .config
                .voters
                .iter()
                .filter(|v| **v != self.id)
                .map(|v| self.progress.get(v).map_or(0, |p| p.last_ack))
                .collect();
            acks.sort_unstable_by(|a, b| b.cmp(a));
            // we count as one of the quorum ourselves, so `need - 1` peers must be fresh
            let kth = acks.get(need - 2).copied().unwrap_or(0);
            if kth == 0 || self.now.saturating_sub(kth) > lease {
                return Err(ReadError::LeaseExpired);
            }
        }
        Ok(self.commit)
    }

    // ---- proposals ------------------------------------------------------------------

    pub fn propose(&mut self, payload: Payload) -> Result<(Index, Term), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader(self.leader));
        }
        if self.transfer.is_some() {
            return Err(ProposeError::Busy("leadership transfer in progress"));
        }
        if matches!(payload, Payload::Config(_) | Payload::Noop) {
            return Err(ProposeError::Invalid("use propose_conf_change for membership"));
        }
        if (self.log.last_index() - self.commit) as usize >= self.cfg.max_uncommitted {
            return Err(ProposeError::Busy("too many uncommitted entries"));
        }
        let index = self.append_local(payload);
        self.after_local_append();
        Ok((index, self.term))
    }

    pub fn propose_conf_change(&mut self, change: ConfChange) -> Result<(Index, Term), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader(self.leader));
        }
        if self.transfer.is_some() {
            return Err(ProposeError::Busy("leadership transfer in progress"));
        }
        if self.commit < self.noop_index {
            return Err(ProposeError::Busy("a new leader must commit an entry of its term before changing membership"));
        }
        if self.has_pending_conf_change() {
            return Err(ProposeError::Busy("a membership change is already in flight"));
        }
        let mut next = self.config.clone();
        match change {
            ConfChange::AddLearner(n) => {
                if next.contains(&n) {
                    return Err(ProposeError::Invalid("node is already a member"));
                }
                next.learners.insert(n);
            }
            ConfChange::Promote(n) => {
                if !next.learners.remove(&n) {
                    return Err(ProposeError::Invalid("node is not a learner"));
                }
                next.voters.insert(n);
            }
            ConfChange::Remove(n) => {
                if !next.contains(&n) {
                    return Err(ProposeError::Invalid("node is not a member"));
                }
                next.voters.remove(&n);
                next.learners.remove(&n);
                if next.voters.is_empty() {
                    return Err(ProposeError::Invalid("cannot remove the last voter"));
                }
            }
        }
        let index = self.append_local(Payload::Config(next));
        self.after_local_append();
        Ok((index, self.term))
    }

    pub fn transfer_leadership(&mut self, to: NodeId) -> Result<(), ProposeError> {
        if self.role != Role::Leader {
            return Err(ProposeError::NotLeader(self.leader));
        }
        if to == self.id {
            return Ok(());
        }
        if !self.config.voters.contains(&to) {
            return Err(ProposeError::Invalid("transfer target must be a voter"));
        }
        self.transfer = Some(Transfer { to, elapsed: 0, sent: false });
        self.maybe_send_timeout_now();
        Ok(())
    }

    fn has_pending_conf_change(&self) -> bool {
        ((self.commit + 1)..=self.log.last_index())
            .any(|i| matches!(self.log.get(i).map(|e| &e.payload), Some(Payload::Config(_))))
    }

    fn append_local(&mut self, payload: Payload) -> Index {
        let index = self.log.last_index() + 1;
        let e = Entry { index, term: self.term, payload };
        self.log.entries.push_back(e.clone());
        self.out.append.push(e);
        index
    }

    fn after_local_append(&mut self) {
        let before: Vec<NodeId> = self.config.members().copied().collect();
        self.recompute_config();
        if self.role == Role::Leader {
            let conf_index = self.last_config_index().unwrap_or(0);
            for gone in before.into_iter().filter(|n| !self.config.contains(n) && *n != self.id) {
                self.leaving.insert(gone, Leaving { conf_index, countdown: None });
            }
            // a node that was re-added is no longer leaving
            let members: Vec<NodeId> = self.config.members().copied().collect();
            self.leaving.retain(|id, _| !members.contains(id));
        }
        self.sync_progress_with_config();
        let last = self.log.last_index();
        if let Some(me) = self.progress.get_mut(&self.id) {
            me.matched = last;
            me.next = last + 1;
        }
        self.advance_commit();
        self.broadcast_append();
    }

    // ---- message handling -----------------------------------------------------------

    pub fn step(&mut self, from: NodeId, msg: Message) {
        if matches!(msg, Message::Unreachable) {
            if let Some(p) = self.progress.get_mut(&from) {
                p.inflight = false;
                p.snapshot_pending = false;
            }
            return;
        }
        if self.removed && !matches!(msg, Message::Append(_) | Message::Snapshot(_)) {
            return;
        }

        // ---- term handling ----
        let msg_term = message_term(&msg);
        let is_prevote = matches!(msg, Message::PreVote(_));
        let is_pre_resp = matches!(msg, Message::PreVoteResp(_));
        if let Some(t) = msg_term {
            if t > self.term && !is_prevote && !is_pre_resp {
                let from_leader = matches!(msg, Message::Append(_) | Message::Snapshot(_) | Message::TimeoutNow { .. });
                let transfer = matches!(&msg, Message::Vote(v) if v.transfer);
                // leader stickiness: ignore disruptive vote requests while a leader is alive
                if matches!(msg, Message::Vote(_)) && !transfer && self.leader_is_fresh() {
                    return;
                }
                self.become_follower(t, if from_leader { Some(from) } else { None });
            } else if t < self.term {
                self.reply_to_stale(from, &msg);
                return;
            }
        }

        match msg {
            Message::PreVote(r) => self.handle_prevote(from, r),
            Message::PreVoteResp(r) => self.handle_prevote_resp(from, r),
            Message::Vote(r) => self.handle_vote(from, r),
            Message::VoteResp(r) => self.handle_vote_resp(from, r),
            Message::Append(r) => self.handle_append(from, r),
            Message::AppendResp(r) => self.handle_append_resp(from, r),
            Message::Snapshot(m) => self.handle_snapshot(from, m),
            Message::SnapshotResp(r) => self.handle_snapshot_resp(from, r),
            Message::TimeoutNow { term } => {
                if term == self.term && self.is_voter() && self.role != Role::Leader {
                    self.campaign(true);
                }
            }
            Message::Unreachable => {}
        }
    }

    fn leader_is_fresh(&self) -> bool {
        self.leader.is_some() && self.election_elapsed < self.cfg.election_ticks
    }

    fn reply_to_stale(&mut self, from: NodeId, msg: &Message) {
        let t = self.term;
        match msg {
            Message::Append(_) => self
                .send(from, Message::AppendResp(AppendResp { term: t, success: false, match_index: 0, hint_index: 0 })),
            Message::Vote(_) => self.send(from, Message::VoteResp(VoteResp { term: t, granted: false })),
            Message::PreVote(_) => self.send(from, Message::PreVoteResp(VoteResp { term: t, granted: false })),
            Message::Snapshot(_) => {
                self.send(from, Message::SnapshotResp(SnapshotResp { term: t, success: false, last_index: 0 }))
            }
            _ => {}
        }
    }

    fn log_up_to_date(&self, last_index: Index, last_term: Term) -> bool {
        let (my_term, my_index) = (self.log.last_term(), self.log.last_index());
        last_term > my_term || (last_term == my_term && last_index >= my_index)
    }

    fn handle_prevote(&mut self, from: NodeId, r: VoteReq) {
        // Votes are not gated on the candidate being in *our* view of the membership: our
        // config may simply be stale (it is learnt through the log), and refusing would
        // deadlock a cluster whose only electable node we are not up to date with. Safety
        // comes from the log check, leader stickiness, and the candidate counting votes
        // against its own configuration.
        let grant = r.term > self.term
            && self.log_up_to_date(r.last_index, r.last_term)
            && (!self.leader_is_fresh() || r.transfer);
        // answer with the request's term when granting so the candidate can match it
        let term = if grant { r.term } else { self.term };
        self.send(from, Message::PreVoteResp(VoteResp { term, granted: grant }));
    }

    fn handle_prevote_resp(&mut self, from: NodeId, r: VoteResp) {
        if self.role != Role::PreCandidate {
            return;
        }
        if !r.granted && r.term > self.term {
            self.become_follower(r.term, None);
            return;
        }
        if r.granted && r.term != self.term + 1 {
            return; // an answer to an older pre-vote round
        }
        if !self.config.voters.contains(&from) {
            return;
        }
        self.votes.insert(from, r.granted);
        self.tally(true);
    }

    fn handle_vote(&mut self, from: NodeId, r: VoteReq) {
        let can_vote = self.voted_for.is_none() || self.voted_for == Some(from);
        let grant = can_vote && self.log_up_to_date(r.last_index, r.last_term);
        if grant {
            self.voted_for = Some(from);
            self.hs_dirty = true;
            self.election_elapsed = 0;
            self.reset_election_timer();
        }
        let t = self.term;
        self.send(from, Message::VoteResp(VoteResp { term: t, granted: grant }));
    }

    fn handle_vote_resp(&mut self, from: NodeId, r: VoteResp) {
        if self.role != Role::Candidate || r.term != self.term {
            return;
        }
        if !self.config.voters.contains(&from) {
            return;
        }
        self.votes.insert(from, r.granted);
        self.tally(false);
    }

    fn tally(&mut self, pre: bool) {
        let quorum = self.config.quorum();
        let granted = self.votes.values().filter(|g| **g).count();
        let rejected = self.votes.values().filter(|g| !**g).count();
        if granted >= quorum {
            if pre {
                self.campaign(false);
            } else {
                self.become_leader();
            }
        } else if rejected >= quorum {
            let term = self.term;
            self.become_follower(term, None);
        }
    }

    fn handle_append(&mut self, from: NodeId, r: AppendReq) {
        // a valid leader for our term
        if self.role != Role::Follower {
            let term = self.term;
            self.become_follower(term, Some(from));
        }
        if self.leader != Some(from) {
            self.leader = Some(from);
            self.out.events.push(Event::LeaderChanged { leader: Some(from) });
        }
        self.election_elapsed = 0;
        self.reset_election_timer();

        let t = self.term;
        let mut prev_index = r.prev_index;
        let mut prev_term = r.prev_term;
        let mut entries = r.entries;

        // part of the request may precede our snapshot: that prefix is already committed
        if prev_index < self.log.snap_index {
            let skip = (self.log.snap_index - prev_index) as usize;
            if skip >= entries.len() {
                self.send(
                    from,
                    Message::AppendResp(AppendResp {
                        term: t,
                        success: true,
                        match_index: self.log.snap_index,
                        hint_index: 0,
                    }),
                );
                return;
            }
            entries.drain(..skip);
            prev_index = self.log.snap_index;
            prev_term = self.log.snap_term;
        }

        // the highest index this request vouches for; what we may acknowledge and commit
        let last_new = prev_index + entries.len() as u64;

        match self.log.term_at(prev_index) {
            Some(pt) if pt == prev_term => {}
            _ => {
                let hint = self.log.last_index().min(prev_index.saturating_sub(1));
                self.send(
                    from,
                    Message::AppendResp(AppendResp {
                        term: t,
                        success: false,
                        match_index: 0,
                        hint_index: hint.max(self.log.snap_index),
                    }),
                );
                return;
            }
        }

        let mut conflict_at = None;
        let mut new_from = entries.len();
        for (k, e) in entries.iter().enumerate() {
            match self.log.term_at(e.index) {
                Some(existing) if existing == e.term => continue,
                Some(_) => {
                    conflict_at = Some(e.index);
                    new_from = k;
                    break;
                }
                None => {
                    new_from = k;
                    break;
                }
            }
        }
        if let Some(i) = conflict_at {
            // never truncate committed entries (would be a protocol violation)
            debug_assert!(i > self.commit, "leader asked to overwrite a committed entry");
            self.log.truncate_from(i);
            self.out.truncate_from = Some(self.out.truncate_from.map_or(i, |x| x.min(i)));
            self.out.append.retain(|e| e.index < i);
        }
        let had_config = entries[new_from..].iter().any(|e| matches!(e.payload, Payload::Config(_)));
        for e in entries.drain(new_from..) {
            self.log.entries.push_back(e.clone());
            self.out.append.push(e);
        }
        if conflict_at.is_some() || had_config {
            self.recompute_config();
        }

        // Acknowledge only what the leader sent: our log may still hold a stale suffix
        // beyond `last_new` that we know nothing about.
        let new_commit = r.commit.min(last_new);
        if new_commit > self.commit {
            self.commit = new_commit;
            self.after_commit();
        }
        self.send(
            from,
            Message::AppendResp(AppendResp { term: t, success: true, match_index: last_new, hint_index: 0 }),
        );
    }

    fn handle_append_resp(&mut self, from: NodeId, r: AppendResp) {
        if self.role != Role::Leader {
            return;
        }
        let now = self.now;
        let Some(p) = self.progress.get_mut(&from) else { return };
        p.inflight = false;
        p.active = true;
        p.last_ack = now;
        if r.success {
            if r.match_index > p.matched {
                p.matched = r.match_index;
            }
            p.next = p.matched + 1;
            self.advance_commit();
            self.maybe_promote(from);
            self.maybe_send_timeout_now();
            // keep streaming only while the follower is behind; an up-to-date follower is
            // served by the next heartbeat (answering every ack would ping-pong forever)
            let behind = self.progress.get(&from).is_some_and(|p| p.next <= self.log.last_index());
            if behind {
                self.send_append(from);
            }
        } else {
            // fast backtrack to what the follower says it can share
            p.next = (r.hint_index + 1).min(p.next.saturating_sub(1)).max(1);
            self.send_append(from);
        }
    }

    fn handle_snapshot(&mut self, from: NodeId, m: SnapshotMeta) {
        if self.role != Role::Follower {
            let term = self.term;
            self.become_follower(term, Some(from));
        }
        self.leader = Some(from);
        self.election_elapsed = 0;
        let t = self.term;
        if m.last_index <= self.commit {
            // already have everything this snapshot covers
            self.send(from, Message::SnapshotResp(SnapshotResp { term: t, success: true, last_index: self.commit }));
            return;
        }
        // Raft thesis §7 (InstallSnapshot, step 6): when our log already holds the entry the
        // snapshot ends with, the snapshot teaches us nothing but "this prefix is committed".
        // Installing it would discard the entries after it — entries we may already have
        // acknowledged to the leader. (A delayed snapshot did exactly that in the chaos
        // simulation and made a committed entry disappear.)
        if self.log.term_at(m.last_index) == Some(m.last_term) {
            self.commit = m.last_index;
            self.after_commit();
            self.send(from, Message::SnapshotResp(SnapshotResp { term: t, success: true, last_index: m.last_index }));
            return;
        }
        self.out.install_snapshot = Some(InstallSnapshot { from, meta: m });
    }

    fn handle_snapshot_resp(&mut self, from: NodeId, r: SnapshotResp) {
        if self.role != Role::Leader {
            return;
        }
        let now = self.now;
        let Some(p) = self.progress.get_mut(&from) else { return };
        p.inflight = false;
        p.snapshot_pending = false;
        p.active = true;
        p.last_ack = now;
        if r.success {
            p.matched = p.matched.max(r.last_index);
            p.next = p.matched + 1;
        }
        self.send_append(from);
    }

    // ---- elections ------------------------------------------------------------------

    fn campaign_pre(&mut self) {
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes.clear();
        self.votes.insert(self.id, true);
        self.election_elapsed = 0;
        self.reset_election_timer();
        if self.config.quorum() <= 1 {
            self.campaign(false);
            return;
        }
        let req = VoteReq {
            term: self.term + 1,
            last_index: self.log.last_index(),
            last_term: self.log.last_term(),
            transfer: false,
        };
        let peers: Vec<NodeId> = self.config.voters.iter().copied().filter(|n| *n != self.id).collect();
        for p in peers {
            self.send(p, Message::PreVote(req.clone()));
        }
    }

    fn campaign(&mut self, transfer: bool) {
        self.term += 1;
        self.voted_for = Some(self.id);
        self.hs_dirty = true;
        self.role = Role::Candidate;
        self.leader = None;
        self.votes.clear();
        self.votes.insert(self.id, true);
        self.election_elapsed = 0;
        self.reset_election_timer();
        if self.config.quorum() <= 1 {
            self.become_leader();
            return;
        }
        let req =
            VoteReq { term: self.term, last_index: self.log.last_index(), last_term: self.log.last_term(), transfer };
        let peers: Vec<NodeId> = self.config.voters.iter().copied().filter(|n| *n != self.id).collect();
        for p in peers {
            self.send(p, Message::Vote(req.clone()));
        }
    }

    fn become_follower(&mut self, term: Term, leader: Option<NodeId>) {
        let was_leader = self.role == Role::Leader;
        if term > self.term {
            self.term = term;
            self.voted_for = None;
            self.hs_dirty = true;
        }
        self.role = Role::Follower;
        self.votes.clear();
        self.progress.clear();
        self.leaving.clear();
        self.transfer = None;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.reset_election_timer();
        if self.leader != leader {
            self.leader = leader;
            self.out.events.push(Event::LeaderChanged { leader });
        }
        self.out.events.push(Event::BecameFollower { term: self.term, leader });
        let _ = was_leader;
    }

    fn become_leader(&mut self) {
        self.role = Role::Leader;
        self.leader = Some(self.id);
        self.votes.clear();
        self.transfer = None;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.out.events.push(Event::LeaderChanged { leader: Some(self.id) });
        self.out.events.push(Event::BecameLeader { term: self.term });

        self.progress.clear();
        let last = self.log.last_index();
        let members: Vec<NodeId> = self.config.members().copied().collect();
        for m in members {
            self.progress.insert(
                m,
                Progress {
                    next: last + 1,
                    matched: if m == self.id { last } else { 0 },
                    inflight: false,
                    active: m == self.id,
                    snapshot_pending: false,
                    snapshot_age: 0,
                    last_ack: 0,
                },
            );
        }
        // commit entries of previous terms indirectly by committing one of ours
        self.noop_index = self.append_local(Payload::Noop);
        self.after_local_append();
    }

    // ---- replication ----------------------------------------------------------------

    fn sync_progress_with_config(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        let last = self.log.last_index();
        let members: Vec<NodeId> = self.config.members().copied().collect();
        for m in &members {
            self.progress.entry(*m).or_insert(Progress {
                next: last + 1,
                matched: 0,
                inflight: false,
                active: false,
                snapshot_pending: false,
                snapshot_age: 0,
                last_ack: 0,
            });
        }
        let leaving: Vec<NodeId> = self.leaving.keys().copied().collect();
        self.progress.retain(|id, _| members.contains(id) || leaving.contains(id));
    }

    fn broadcast_append(&mut self) {
        let peers: Vec<NodeId> = self.progress.keys().copied().filter(|n| *n != self.id).collect();
        for p in peers {
            self.send_append(p);
        }
    }

    fn send_append(&mut self, to: NodeId) {
        if self.role != Role::Leader {
            return;
        }
        let Some(p) = self.progress.get(&to) else { return };
        if p.inflight || p.snapshot_pending {
            return;
        }
        let next = p.next.max(1);
        if next <= self.log.snap_index {
            // the entries this follower needs were compacted: it needs the whole state
            if !self.out.send_snapshot.contains(&to) {
                self.out.send_snapshot.push(to);
            }
            if let Some(p) = self.progress.get_mut(&to) {
                p.inflight = true;
                p.snapshot_pending = true;
                p.snapshot_age = 0;
            }
            return;
        }
        let prev_index = next - 1;
        let prev_term = self.log.term_at(prev_index).unwrap_or(0);
        let entries = self.log.slice(next, self.cfg.max_entries_per_append);
        let req = AppendReq { term: self.term, prev_index, prev_term, entries, commit: self.commit };
        if let Some(p) = self.progress.get_mut(&to) {
            p.inflight = true;
        }
        self.send(to, Message::Append(req));
    }

    fn advance_commit(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        let mut matched: Vec<Index> = self
            .config
            .voters
            .iter()
            .map(|v| if *v == self.id { self.log.last_index() } else { self.progress.get(v).map_or(0, |p| p.matched) })
            .collect();
        if matched.is_empty() {
            return;
        }
        matched.sort_unstable_by(|a, b| b.cmp(a));
        let candidate = matched[self.config.quorum() - 1];
        // only entries from the current term are committed by counting replicas
        if candidate > self.commit && self.log.term_at(candidate) == Some(self.term) {
            self.commit = candidate;
            self.after_commit();
            // followers learn the new commit index with the next append
            self.broadcast_append();
        }
    }

    /// Bookkeeping after `commit` moved.
    fn after_commit(&mut self) {
        let grace = self.cfg.election_ticks * 2;
        let commit = self.commit;
        for l in self.leaving.values_mut() {
            if l.conf_index <= commit && l.countdown.is_none() {
                l.countdown = Some(grace);
            }
        }
        // a committed membership change that excludes us ends our participation
        if self.config.is_empty() {
            return;
        }
        let me_gone = !self.config.contains(&self.id);
        let conf_committed = self.last_config_index().is_some_and(|i| i <= self.commit);
        if me_gone && conf_committed && !self.removed {
            self.removed = true;
            let term = self.term;
            if self.role == Role::Leader {
                self.become_follower(term, None);
            }
            self.out.events.push(Event::Removed);
        }
    }

    fn last_config_index(&self) -> Option<Index> {
        (self.log.snap_index + 1..=self.log.last_index())
            .rev()
            .find(|i| matches!(self.log.get(*i).map(|e| &e.payload), Some(Payload::Config(_))))
    }

    fn maybe_promote(&mut self, peer: NodeId) {
        if self.role != Role::Leader || !self.config.learners.contains(&peer) || self.transfer.is_some() {
            return;
        }
        let caught_up = self.progress.get(&peer).is_some_and(|p| p.matched + 1 >= self.commit);
        if caught_up && self.commit >= self.noop_index && !self.has_pending_conf_change() {
            let _ = self.propose_conf_change(ConfChange::Promote(peer));
        }
    }

    fn maybe_send_timeout_now(&mut self) {
        let Some(t) = &self.transfer else { return };
        if t.sent {
            return;
        }
        let to = t.to;
        let up_to_date = self.progress.get(&to).is_some_and(|p| p.matched == self.log.last_index());
        if up_to_date {
            let term = self.term;
            if let Some(t) = &mut self.transfer {
                t.sent = true;
            }
            self.send(to, Message::TimeoutNow { term });
        } else {
            self.send_append(to);
        }
    }

    // ---- config ---------------------------------------------------------------------

    /// The effective membership is the latest `Config` entry in the log (committed or not),
    /// else the snapshot's, else the static bootstrap.
    fn recompute_config(&mut self) {
        let from_log = (self.log.snap_index + 1..=self.log.last_index()).rev().find_map(|i| {
            match self.log.get(i).map(|e| &e.payload) {
                Some(Payload::Config(c)) => Some(c.clone()),
                _ => None,
            }
        });
        let next = from_log.unwrap_or_else(|| {
            if !self.snap_config.is_empty() { self.snap_config.clone() } else { self.bootstrap.clone() }
        });
        if next != self.config {
            self.config = next.clone();
            // a node that was removed and later added back takes part again
            if self.config.contains(&self.id) {
                self.removed = false;
            }
            self.out.events.push(Event::ConfigChanged(next));
        }
    }

    // ---- helpers --------------------------------------------------------------------

    fn send(&mut self, to: NodeId, msg: Message) {
        if to != self.id {
            self.out.messages.push((to, msg));
        }
    }

    fn reset_election_timer(&mut self) {
        let span = self.cfg.election_ticks.max(1);
        self.election_timeout = span + self.next_rand() % span;
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn message_term(m: &Message) -> Option<Term> {
    match m {
        Message::PreVote(r) | Message::Vote(r) => Some(r.term),
        Message::PreVoteResp(r) | Message::VoteResp(r) => Some(r.term),
        Message::Append(r) => Some(r.term),
        Message::AppendResp(r) => Some(r.term),
        Message::Snapshot(m) => Some(m.term),
        Message::SnapshotResp(r) => Some(r.term),
        Message::TimeoutNow { term } => Some(*term),
        Message::Unreachable => None,
    }
}

fn hash_ip(ip: &NodeId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ip.hash(&mut h);
    h.finish()
}
