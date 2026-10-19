//! Deterministic cluster simulation: N cores, an in-memory lossy network, crash/restart
//! with durable "disks", and the Raft safety invariants checked after every step.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr};

use super::core::{Core, CoreConfig, PersistedState};
use super::types::*;
use crate::store::Command;

pub fn ip(n: u8) -> NodeId {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
}

pub fn cmd(n: u64) -> Payload {
    Payload::Command(Command::KvSet { tenant: "t".into(), key: format!("k{n}"), value: n })
}

#[derive(Clone, Default)]
struct Disk {
    hs: HardState,
    snapshot: (Index, Term),
    snap_config: ClusterConfig,
    entries: Vec<Entry>,
}

/// State machine, durable like the real redb apply.
#[derive(Clone, Default)]
struct Machine {
    applied: Index,
    config: ClusterConfig,
    log: Vec<(Index, Term, Payload)>,
}

struct SimNode {
    core: Option<Core>,
    disk: Disk,
    machine: Machine,
    bootstrap: Option<ClusterConfig>,
    removed_seen: bool,
}

enum Packet {
    Msg(Message),
    Snap(SnapshotMeta, Machine),
}

struct InFlight {
    at: u64,
    from: NodeId,
    to: NodeId,
    packet: Packet,
}

pub struct Cluster {
    nodes: BTreeMap<NodeId, SimNode>,
    net: VecDeque<InFlight>,
    pub time: u64,
    rng: u64,
    cfg: CoreConfig,
    /// Directed links that drop everything.
    blocked: BTreeSet<(NodeId, NodeId)>,
    pub drop_pct: u64,
    pub dup_pct: u64,
    pub max_delay: u64,
    leaders_by_term: BTreeMap<Term, NodeId>,
    committed_global: BTreeMap<Index, (Term, Payload)>,
    pub messages_sent: u64,
    /// Snapshot payloads that arrived and wait for the core to ask for installation.
    pending_snapshot_state: BTreeMap<NodeId, (SnapshotMeta, Machine)>,
    history: Vec<String>,
}

impl Cluster {
    pub fn new(ids: &[u8], seed: u64) -> Self {
        let voters: Vec<NodeId> = ids.iter().map(|n| ip(*n)).collect();
        let mut c = Self::empty(seed);
        let boot = ClusterConfig::new(voters.clone());
        for v in voters {
            c.add_node(v, Some(boot.clone()));
        }
        c
    }

    pub fn empty(seed: u64) -> Self {
        Cluster {
            nodes: BTreeMap::new(),
            net: VecDeque::new(),
            time: 0,
            rng: seed.max(1),
            cfg: CoreConfig {
                election_ticks: 10,
                heartbeat_ticks: 2,
                max_entries_per_append: 16,
                max_uncommitted: 4096,
                snapshot_timeout_ticks: 30,
                seed,
            },
            blocked: BTreeSet::new(),
            drop_pct: 0,
            dup_pct: 0,
            max_delay: 2,
            leaders_by_term: BTreeMap::new(),
            committed_global: BTreeMap::new(),
            messages_sent: 0,
            pending_snapshot_state: BTreeMap::new(),
            history: Vec::new(),
        }
    }

    fn note(&mut self, line: String) {
        if self.history.len() > 200_000 {
            self.history.drain(..100_000);
        }
        let t = self.time;
        self.history.push(format!("[t={t}] {line}"));
    }

    /// Last events mentioning any of `needles`.
    pub fn history_tail(&self, needles: &[&str], n: usize) -> String {
        let hits: Vec<&String> = self.history.iter().filter(|l| needles.iter().any(|x| l.contains(x))).collect();
        hits[hits.len().saturating_sub(n)..].iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
    }

    pub fn add_node(&mut self, id: NodeId, bootstrap: Option<ClusterConfig>) {
        let state = PersistedState::default();
        let core = Core::new(
            id,
            CoreConfig { seed: self.cfg.seed.wrapping_add(self.nodes.len() as u64), ..self.cfg.clone() },
            state,
            bootstrap.clone(),
        );
        self.nodes.insert(
            id,
            SimNode {
                core: Some(core),
                disk: Disk::default(),
                machine: Machine { config: bootstrap.clone().unwrap_or_default(), ..Default::default() },
                bootstrap,
                removed_seen: false,
            },
        );
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    // ---- fault injection ------------------------------------------------------------

    pub fn crash(&mut self, id: NodeId) {
        let n = &self.nodes[&id];
        let d = format!(
            "CRASH {id} (disk: term={} voted={:?} entries={} snap={:?} applied={})",
            n.disk.hs.term,
            n.disk.hs.voted_for,
            n.disk.entries.len(),
            n.disk.snapshot,
            n.machine.applied
        );
        self.note(d);
        self.nodes.get_mut(&id).unwrap().core = None;
    }

    pub fn restart(&mut self, id: NodeId) {
        let seed = self.cfg.seed.wrapping_add(self.time);
        let n = self.nodes.get_mut(&id).unwrap();
        if n.core.is_some() {
            return;
        }
        let state = PersistedState {
            hard_state: n.disk.hs,
            snapshot: n.disk.snapshot,
            snapshot_config: n.disk.snap_config.clone(),
            entries: n.disk.entries.clone(),
            applied: n.machine.applied,
        };
        n.core = Some(Core::new(id, CoreConfig { seed, ..self.cfg.clone() }, state, n.bootstrap.clone()));
        self.note(format!("RESTART {id}"));
    }

    pub fn is_up(&self, id: NodeId) -> bool {
        self.nodes[&id].core.is_some()
    }

    /// Cut every link between the two groups (both directions).
    pub fn partition(&mut self, a: &[u8], b: &[u8]) {
        for x in a {
            for y in b {
                self.blocked.insert((ip(*x), ip(*y)));
                self.blocked.insert((ip(*y), ip(*x)));
            }
        }
    }

    pub fn isolate(&mut self, id: u8) {
        let others: Vec<u8> = self.nodes.keys().map(|n| last_octet(*n)).filter(|o| *o != id).collect();
        self.partition(&[id], &others);
    }

    pub fn heal(&mut self) {
        self.blocked.clear();
    }

    // ---- running --------------------------------------------------------------------

    pub fn step_time(&mut self) {
        self.time += 1;
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for id in &ids {
            if let Some(core) = self.nodes.get_mut(id).unwrap().core.as_mut() {
                core.tick();
            }
        }
        self.deliver_due();
        for id in &ids {
            self.process_ready(*id);
        }
        self.check_cheap_invariants();
    }

    pub fn run(&mut self, ticks: u64) {
        for _ in 0..ticks {
            self.step_time();
        }
    }

    pub fn run_until(&mut self, max: u64, mut cond: impl FnMut(&Cluster) -> bool) -> bool {
        for _ in 0..max {
            if cond(self) {
                return true;
            }
            self.step_time();
        }
        cond(self)
    }

    fn deliver_due(&mut self) {
        // random delivery order among due packets exercises reordering
        loop {
            let due: Vec<usize> =
                self.net.iter().enumerate().filter(|(_, p)| p.at <= self.time).map(|(i, _)| i).collect();
            if due.is_empty() {
                break;
            }
            let pick = due[(self.rand() as usize) % due.len()];
            let p = self.net.remove(pick).unwrap();
            let blocked = self.blocked.contains(&(p.from, p.to));
            if blocked {
                continue;
            }
            let Some(node) = self.nodes.get_mut(&p.to) else { continue };
            let Some(core) = node.core.as_mut() else { continue };
            match p.packet {
                Packet::Msg(m) => core.step(p.from, m),
                Packet::Snap(meta, machine) => {
                    self.pending_snapshot_state.insert(p.to, (meta.clone(), machine));
                    let core = self.nodes.get_mut(&p.to).unwrap().core.as_mut().unwrap();
                    core.step(p.from, Message::Snapshot(meta));
                }
            }
            self.process_ready(p.to);
            self.pending_snapshot_state.remove(&p.to);
        }
    }

    fn enqueue(&mut self, from: NodeId, to: NodeId, packet: Packet) {
        self.messages_sent += 1;
        if self.rand() % 100 < self.drop_pct {
            return;
        }
        let delay = 1 + self.rand() % self.max_delay.max(1);
        let dup = self.rand() % 100 < self.dup_pct;
        if dup && let Packet::Msg(m) = &packet {
            let d2 = 1 + self.rand() % (self.max_delay.max(1) + 2);
            self.net.push_back(InFlight { at: self.time + d2, from, to, packet: Packet::Msg(m.clone()) });
        }
        self.net.push_back(InFlight { at: self.time + delay, from, to, packet });
    }

    fn process_ready(&mut self, id: NodeId) {
        loop {
            let Some(node) = self.nodes.get_mut(&id) else { return };
            let Some(core) = node.core.as_mut() else { return };
            if !core.has_ready() {
                return;
            }
            let rd = core.ready();

            // 1. persist
            if let Some(hs) = rd.hard_state {
                node.disk.hs = hs;
            }
            if let Some(from) = rd.truncate_from {
                let dropped: Vec<Index> =
                    node.disk.entries.iter().filter(|e| e.index >= from).map(|e| e.index).collect();
                node.disk.entries.retain(|e| e.index < from);
                let commit = node.core.as_ref().map(|c| c.commit_index()).unwrap_or(0);
                let applied = node.machine.applied;
                self.note(format!(
                    "TRUNCATE {id} from idx={from} dropped={dropped:?} (commit={commit} applied={applied})"
                ));
            }
            let node = self.nodes.get_mut(&id).unwrap();
            node.disk.entries.extend(rd.append.iter().cloned());

            // snapshot install requested by the core
            if let Some(inst) = rd.install_snapshot
                && let Some((meta, machine)) = self.pending_snapshot_state.remove(&id)
            {
                let node = self.nodes.get_mut(&id).unwrap();
                node.disk.entries.clear();
                node.disk.snapshot = (meta.last_index, meta.last_term);
                node.disk.snap_config = meta.config.clone();
                node.machine = machine;
                let (li, lt) = (meta.last_index, meta.last_term);
                self.history.push(format!("[t={}] SNAPSHOT installed on {id} up to idx={li} term={lt}", self.time));
                let node = self.nodes.get_mut(&id).unwrap();
                node.machine.applied = meta.last_index;
                node.machine.config = meta.config.clone();
                node.core.as_mut().unwrap().snapshot_installed(&meta);
                let term = node.core.as_ref().unwrap().term();
                self.enqueue(
                    id,
                    inst.from,
                    Packet::Msg(Message::SnapshotResp(SnapshotResp {
                        term,
                        success: true,
                        last_index: meta.last_index,
                    })),
                );
            }

            // 2. send
            for (to, m) in rd.messages {
                self.enqueue(id, to, Packet::Msg(m));
            }
            for peer in rd.send_snapshot {
                let node = self.nodes.get(&id).unwrap();
                let core = node.core.as_ref().unwrap();
                let last = node.machine.applied;
                let term = core.term_at(last).unwrap_or(0);
                let meta = SnapshotMeta {
                    term: core.term(),
                    last_index: last,
                    last_term: term,
                    config: node.machine.config.clone(),
                };
                let machine = node.machine.clone();
                self.enqueue(id, peer, Packet::Snap(meta, machine));
            }

            // 3. apply
            for e in rd.committed {
                self.apply(id, e);
            }
            for ev in rd.events {
                let node = self.nodes.get_mut(&id).unwrap();
                match ev {
                    Event::Removed => node.removed_seen = true,
                    Event::BecameLeader { term } => {
                        let line = format!("LEADER {id} term={term}");
                        self.history.push(format!("[t={}] {line}", self.time));
                        if let Some(prev) = self.leaders_by_term.insert(term, id) {
                            assert_eq!(
                                prev, id,
                                "ELECTION SAFETY VIOLATED: two leaders in term {term}: {prev} and {id}"
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn apply(&mut self, id: NodeId, e: Entry) {
        let node = self.nodes.get_mut(&id).unwrap();
        assert_eq!(
            e.index,
            node.machine.applied + 1,
            "{id}: applied out of order (got {}, had {})",
            e.index,
            node.machine.applied
        );
        node.machine.applied = e.index;
        if let Payload::Config(c) = &e.payload {
            node.machine.config = c.clone();
        }
        node.machine.log.push((e.index, e.term, e.payload.clone()));
        let line = format!("APPLY {id} idx={} term={} {:?}", e.index, e.term, short(&e.payload));
        self.note(line);
        match self.committed_global.get(&e.index) {
            Some((t, p)) => {
                if !(*t == e.term && *p == e.payload) {
                    let idx = e.index;
                    let ctx = self.history_tail(
                        &[&format!("idx={idx} "), "LEADER", "CRASH", "RESTART", "SNAPSHOT", "TRUNCATE"],
                        80,
                    );
                    panic!(
                        "STATE MACHINE SAFETY VIOLATED at index {}: committed {:?} vs now applying {:?} on {id}\n{}\n--- history ---\n{ctx}",
                        idx,
                        (t, p),
                        (e.term, &e.payload),
                        self.dump()
                    );
                }
            }
            None => {
                self.committed_global.insert(e.index, (e.term, e.payload));
            }
        }
    }

    /// Compact a node's log up to everything it applied.
    pub fn compact(&mut self, id: NodeId) {
        let node = self.nodes.get_mut(&id).unwrap();
        let Some(core) = node.core.as_mut() else { return };
        let idx = node.machine.applied;
        if idx <= core.snapshot_index() {
            return;
        }
        let term = core.term_at(idx).expect("applied entry has a term");
        node.disk.entries.retain(|e| e.index > idx);
        node.disk.snapshot = (idx, term);
        node.disk.snap_config = node.machine.config.clone();
        core.compacted(idx, term, node.machine.config.clone());
    }

    // ---- queries & helpers ------------------------------------------------------------

    pub fn core(&self, id: NodeId) -> &Core {
        self.nodes[&id].core.as_ref().expect("node is down")
    }

    pub fn core_mut(&mut self, id: NodeId) -> &mut Core {
        self.nodes.get_mut(&id).unwrap().core.as_mut().expect("node is down")
    }

    pub fn leaders(&self) -> Vec<NodeId> {
        self.nodes.iter().filter(|(_, n)| n.core.as_ref().is_some_and(|c| c.is_leader())).map(|(id, _)| *id).collect()
    }

    /// The leader with the highest term among reachable ones (stale leaders may linger
    /// on the minority side of a partition until CheckQuorum fires).
    pub fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter_map(|(id, n)| n.core.as_ref().filter(|c| c.is_leader()).map(|c| (c.term(), *id)))
            .max()
            .map(|(_, id)| id)
    }

    /// Human-readable state of every node, for assertion messages.
    pub fn dump(&self) -> String {
        let mut out = format!("t={} in-flight={}\n", self.time, self.net.len());
        for (id, n) in &self.nodes {
            match n.core.as_ref() {
                None => out.push_str(&format!("  {id}: DOWN applied={} disk_entries={}\n", n.machine.applied, n.disk.entries.len())),
                Some(c) => out.push_str(&format!(
                    "  {id}: {:?} term={} leader={:?} commit={} last={} snap={} applied={} voters={:?} learners={:?} voted_for={:?}\n",
                    c.role(), c.term(), c.leader(), c.commit_index(), c.last_index(), c.snapshot_index(), n.machine.applied,
                    c.config().voters, c.config().learners, c.voted_for()
                )),
            }
        }
        out
    }

    pub fn applied_index(&self, id: NodeId) -> Index {
        self.nodes[&id].machine.applied
    }

    pub fn removed_seen(&self, id: NodeId) -> bool {
        self.nodes[&id].removed_seen
    }

    pub fn disk_entries(&self, id: NodeId) -> usize {
        self.nodes[&id].disk.entries.len()
    }

    /// Commands (not noops/configs) a node has applied, in order.
    pub fn applied_commands(&self, id: NodeId) -> Vec<u64> {
        let mut out = Vec::new();
        // a node that installed a snapshot lost the individual entries; use the global record
        for i in 1..=self.applied_index(id) {
            if let Some((_, Payload::Command(Command::KvSet { value, .. }))) = self.committed_global.get(&i) {
                out.push(*value);
            }
        }
        out
    }

    pub fn propose(&mut self, n: u64) -> Result<(Index, Term), ProposeError> {
        let id = self.leader().ok_or(ProposeError::NotLeader(None))?;
        let r = self.core_mut(id).propose(cmd(n));
        self.process_ready(id);
        r
    }

    /// Wait until every live, non-removed node has applied exactly `n` commands.
    pub fn wait_commands(&mut self, n: usize, max: u64) -> bool {
        self.run_until(max, |c| {
            c.nodes
                .iter()
                .filter(|(_, x)| x.core.is_some() && !x.removed_seen)
                .all(|(id, _)| c.applied_commands(*id).len() == n)
        })
    }

    pub fn wait_leader(&mut self, max: u64) -> NodeId {
        assert!(self.run_until(max, |c| c.leader().is_some()), "no leader elected within {max} ticks");
        self.leader().unwrap()
    }

    /// Everything proposed through an elected leader eventually reaches every live node.
    pub fn converged(&self) -> bool {
        let live: Vec<NodeId> =
            self.nodes.iter().filter(|(_, n)| n.core.is_some() && !n.removed_seen).map(|(id, _)| *id).collect();
        let Some(max) = live.iter().map(|i| self.applied_index(*i)).max() else { return true };
        live.iter().all(|i| self.applied_index(*i) == max)
    }

    /// Every voter/learner of the leader's configuration has applied everything.
    pub fn converged_members(&self) -> bool {
        let Some(l) = self.leader() else { return false };
        let lead = self.core(l);
        // the leader must have committed its own term's noop and everything it holds
        if lead.commit_index() < lead.last_index() {
            return false;
        }
        lead.config()
            .members()
            .all(|m| self.nodes.get(m).is_some_and(|n| n.core.is_some() && n.machine.applied == self.applied_index(l)))
    }

    // ---- invariants -------------------------------------------------------------------

    fn check_cheap_invariants(&mut self) {
        // at most one leader per term among *current* leaders
        let mut seen: BTreeMap<Term, NodeId> = BTreeMap::new();
        for (id, n) in &self.nodes {
            if let Some(c) = n.core.as_ref().filter(|c| c.is_leader())
                && let Some(prev) = seen.insert(c.term(), *id)
            {
                panic!("two simultaneous leaders in term {}: {prev} and {id}", c.term());
            }
        }
        if self.time.is_multiple_of(7) {
            self.check_log_matching();
        }
    }

    /// Log Matching: same (index, term) ⇒ same entry and same prefix.
    pub fn check_log_matching(&self) {
        let ids: Vec<&NodeId> = self.nodes.iter().filter(|(_, n)| n.core.is_some()).map(|(i, _)| i).collect();
        for (ai, a) in ids.iter().enumerate() {
            for b in ids.iter().skip(ai + 1) {
                let (ca, cb) = (self.core(**a), self.core(**b));
                let lo = ca.snapshot_index().max(cb.snapshot_index()) + 1;
                let hi = ca.last_index().min(cb.last_index());
                let mut equal_so_far = true;
                for i in lo..=hi {
                    let (ea, eb) = (ca.entry(i), cb.entry(i));
                    let (Some(ea), Some(eb)) = (ea, eb) else { continue };
                    if ea.term == eb.term {
                        assert_eq!(ea.payload, eb.payload, "LOG MATCHING VIOLATED at {i} between {a} and {b}");
                        let _ = equal_so_far;
                    } else {
                        equal_so_far = false;
                    }
                    let _ = equal_so_far;
                }
                // prefix property: once terms are equal at i, every earlier overlapping index is equal too
                for i in (lo..=hi).rev() {
                    if let (Some(ea), Some(eb)) = (ca.entry(i), cb.entry(i))
                        && ea.term == eb.term
                    {
                        for j in lo..i {
                            if let (Some(xa), Some(xb)) = (ca.entry(j), cb.entry(j)) {
                                assert_eq!(
                                    xa, xb,
                                    "LOG MATCHING (prefix) VIOLATED: equal at {i} but differ at {j} between {a} and {b}"
                                );
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
}

fn short(p: &Payload) -> String {
    match p {
        Payload::Noop => "noop".into(),
        Payload::Config(_) => "config".into(),
        Payload::Command(Command::KvSet { value, .. }) => format!("cmd{value}"),
        Payload::Command(_) => "cmd".into(),
    }
}

fn last_octet(id: NodeId) -> u8 {
    match id {
        IpAddr::V4(v4) => v4.octets()[3],
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_node_elects_itself_and_commits_immediately() {
        let mut c = Cluster::new(&[1], 1);
        let l = c.wait_leader(100);
        assert_eq!(l, ip(1));
        c.propose(7).unwrap();
        c.run(3);
        assert_eq!(c.applied_commands(ip(1)), vec![7]);
    }

    #[test]
    fn three_nodes_elect_one_leader_and_replicate() {
        let mut c = Cluster::new(&[1, 2, 3], 7);
        c.wait_leader(200);
        for n in 1..=20 {
            c.propose(n).unwrap();
            c.run(1);
        }
        assert!(c.run_until(200, |c| c.converged() && c.applied_commands(ip(1)).len() == 20));
        let want: Vec<u64> = (1..=20).collect();
        for n in 1..=3 {
            assert_eq!(c.applied_commands(ip(n)), want, "node {n}");
        }
        assert_eq!(c.leaders().len(), 1);
    }

    #[test]
    fn follower_refuses_proposals_and_points_at_the_leader() {
        let mut c = Cluster::new(&[1, 2, 3], 3);
        let l = c.wait_leader(200);
        c.run(10);
        let follower = [ip(1), ip(2), ip(3)].into_iter().find(|n| *n != l).unwrap();
        assert_eq!(c.core_mut(follower).propose(cmd(1)), Err(ProposeError::NotLeader(Some(l))));
    }

    #[test]
    fn leader_crash_elects_a_new_leader_and_old_one_catches_up() {
        let mut c = Cluster::new(&[1, 2, 3], 11);
        let l1 = c.wait_leader(200);
        for n in 1..=5 {
            c.propose(n).unwrap();
        }
        assert!(c.run_until(200, |c| (1..=3).all(|i| c.applied_commands(ip(i)).len() == 5)), "{}", c.dump());
        c.crash(l1);
        // survivors elect someone else
        assert!(c.run_until(300, |c| c.leader().is_some_and(|l| l != l1)), "no new leader");
        for n in 6..=10 {
            c.propose(n).unwrap();
            c.run(2);
        }
        c.restart(l1);
        let ok = c.run_until(400, |c| c.converged() && c.applied_commands(l1).len() == 10);
        assert!(ok, "{}", c.dump());
        assert_eq!(c.applied_commands(l1), (1..=10).collect::<Vec<_>>());
    }

    #[test]
    fn partitioned_leader_steps_down_and_loses_uncommitted_entries() {
        let mut c = Cluster::new(&[1, 2, 3, 4, 5], 5);
        let old = c.wait_leader(300);
        c.propose(1).unwrap();
        assert!(c.wait_commands(1, 200), "{}", c.dump());
        // isolate the leader with one follower: a minority of 2
        let buddy = [1u8, 2, 3, 4, 5].into_iter().map(ip).find(|n| *n != old).unwrap();
        let minority = [last(old), last(buddy)];
        let majority: Vec<u8> = (1..=5).filter(|n| !minority.contains(n)).collect();
        c.partition(&minority, &majority);
        // the stale leader keeps accepting proposals it can never commit
        let r = c.core_mut(old).propose(cmd(999));
        assert!(r.is_ok());
        c.run(5);
        // majority elects a new leader and commits real work
        assert!(
            c.run_until(400, |c| c.leader().is_some_and(|l| majority.contains(&last(l)))),
            "majority has no leader"
        );
        for n in 2..=4 {
            c.propose(n).unwrap();
            c.run(2);
        }
        // CheckQuorum: the old leader notices it lost its majority
        assert!(c.run_until(200, |c| !c.core(old).is_leader()), "stale leader must step down");
        c.heal();
        assert!(c.run_until(500, |c| c.converged()));
        for n in 1..=5 {
            let cmds = c.applied_commands(ip(n));
            assert!(!cmds.contains(&999), "uncommitted entry from the stale leader must vanish (node {n})");
            assert_eq!(cmds, vec![1, 2, 3, 4], "node {n}");
        }
    }

    #[test]
    fn prevote_stops_a_rejoining_node_from_disrupting_the_leader() {
        let mut c = Cluster::new(&[1, 2, 3], 21);
        let leader = c.wait_leader(200);
        c.run(20);
        let term_before = c.core(leader).term();
        let victim = [1u8, 2, 3].into_iter().map(ip).find(|n| *n != leader).unwrap();
        c.isolate(last(victim));
        c.run(200); // the isolated node times out many times
        assert_eq!(c.core(victim).term(), term_before, "pre-vote must keep an isolated node from inflating its term");
        c.heal();
        c.run(100);
        assert_eq!(c.leader(), Some(leader), "the healthy leader must not be deposed by the rejoining node");
        assert_eq!(c.core(leader).term(), term_before);
    }

    #[test]
    fn without_a_quorum_nothing_commits_but_nothing_breaks() {
        let mut c = Cluster::new(&[1, 2, 3], 9);
        let l = c.wait_leader(200);
        let others: Vec<NodeId> = [ip(1), ip(2), ip(3)].into_iter().filter(|n| *n != l).collect();
        for o in &others {
            c.crash(*o);
        }
        let _ = c.core_mut(l).propose(cmd(1));
        c.run(100);
        assert!(c.applied_commands(l).is_empty(), "cannot commit without a majority");
        for o in &others {
            c.restart(*o);
        }
        assert!(c.run_until(400, |c| c.converged()));
        assert_eq!(c.applied_commands(ip(1)).len(), c.applied_commands(l).len());
    }

    #[test]
    fn restart_never_votes_twice_in_a_term() {
        // vote persisted => a restarted node cannot grant a second vote in the same term
        let mut c = Cluster::new(&[1, 2, 3], 4);
        c.wait_leader(200);
        c.run(20);
        for n in 1..=3 {
            c.crash(ip(n));
            c.restart(ip(n));
        }
        assert!(c.run_until(400, |c| c.leader().is_some()));
        c.run(50); // invariant checks (single leader per term) run every tick
    }

    #[test]
    fn lagging_follower_catches_up_through_a_snapshot() {
        let mut c = Cluster::new(&[1, 2, 3], 13);
        let l = c.wait_leader(200);
        let lag = [ip(1), ip(2), ip(3)].into_iter().find(|n| *n != l).unwrap();
        c.crash(lag);
        for n in 1..=60 {
            c.propose(n).unwrap();
            c.run(1);
        }
        c.run(20);
        // compact the survivors so the lagging node's entries no longer exist
        for n in [ip(1), ip(2), ip(3)] {
            if n != lag {
                c.compact(n);
            }
        }
        assert_eq!(c.disk_entries(l), 0);
        c.restart(lag);
        assert!(
            c.run_until(600, |c| c.converged() && c.applied_index(lag) == c.applied_index(l)),
            "follower did not catch up"
        );
        assert_eq!(c.applied_commands(lag), (1..=60).collect::<Vec<_>>());
        // and replication continues normally afterwards
        c.propose(61).unwrap();
        assert!(c.run_until(200, |c| c.converged() && c.applied_commands(lag).len() == 61));
    }

    #[test]
    fn leadership_transfer() {
        let mut c = Cluster::new(&[1, 2, 3], 17);
        let l = c.wait_leader(200);
        c.propose(1).unwrap();
        c.run(10);
        let target = [ip(1), ip(2), ip(3)].into_iter().find(|n| *n != l).unwrap();
        c.core_mut(l).transfer_leadership(target).unwrap();
        c.process_ready(l);
        assert!(c.run_until(200, |c| c.leader() == Some(target)), "leadership did not move");
        assert!(c.run_until(100, |c| c.leaders().len() == 1));
    }

    #[test]
    fn learner_is_added_caught_up_and_promoted_automatically() {
        let mut c = Cluster::new(&[1, 2, 3], 19);
        let l = c.wait_leader(200);
        for n in 1..=10 {
            c.propose(n).unwrap();
        }
        assert!(c.wait_commands(10, 300), "{}", c.dump());
        // node 4 starts blank, as a non-member waiting to be added
        c.add_node(ip(4), None);
        // a new leader must have committed its noop before accepting membership changes
        c.core_mut(l).propose_conf_change(ConfChange::AddLearner(ip(4))).unwrap();
        c.process_ready(l);
        assert!(
            c.run_until(500, |c| c.core(c.leader().unwrap()).config().voters.contains(&ip(4))),
            "learner was not promoted: {:?}",
            c.core(c.leader().unwrap()).config()
        );
        assert!(c.run_until(300, |c| c.converged() && c.applied_commands(ip(4)).len() == 10));
        // quorum is now 3 of 4: crash one old node, the cluster still commits
        let victim = [ip(1), ip(2), ip(3)].into_iter().find(|n| Some(*n) != c.leader()).unwrap();
        c.crash(victim);
        c.run(30);
        c.propose(11).unwrap();
        assert!(c.run_until(300, |c| c.leader().is_some() && c.applied_commands(c.leader().unwrap()).len() == 11));
    }

    #[test]
    fn only_one_membership_change_at_a_time() {
        let mut c = Cluster::new(&[1, 2, 3], 23);
        let l = c.wait_leader(200);
        assert!(c.run_until(100, |c| c.core(l).commit_index() >= 1));
        c.add_node(ip(4), None);
        c.add_node(ip(5), None);
        c.core_mut(l).propose_conf_change(ConfChange::AddLearner(ip(4))).unwrap();
        assert!(matches!(c.core_mut(l).propose_conf_change(ConfChange::AddLearner(ip(5))), Err(ProposeError::Busy(_))));
    }

    #[test]
    fn removing_a_follower_and_then_the_leader() {
        let mut c = Cluster::new(&[1, 2, 3, 4], 29);
        let l = c.wait_leader(200);
        c.run_until(100, |c| c.core(l).commit_index() >= 1);
        let gone = [ip(1), ip(2), ip(3), ip(4)].into_iter().find(|n| *n != l).unwrap();
        c.core_mut(l).propose_conf_change(ConfChange::Remove(gone)).unwrap();
        c.process_ready(l);
        assert!(c.run_until(300, |c| c.removed_seen(gone)), "removed node must learn it was removed");
        assert!(!c.core(l).config().contains(&gone));

        // now remove the leader itself
        assert!(c.run_until(100, |c| c.core(l).commit_index() >= c.core(l).last_index()));
        c.core_mut(l).propose_conf_change(ConfChange::Remove(l)).unwrap();
        c.process_ready(l);
        assert!(c.run_until(400, |c| c.removed_seen(l)));
        assert!(c.run_until(600, |c| c.leader().is_some_and(|n| n != l)), "remaining nodes must elect a leader");
        c.propose(1).unwrap();
        assert!(c.run_until(200, |c| c.leader().is_some_and(|n| c.applied_commands(n).contains(&1))));
    }

    #[test]
    fn membership_change_requires_the_new_leaders_noop_to_commit_first() {
        let mut c = Cluster::new(&[1, 2, 3], 31);
        let l = c.wait_leader(200);
        // immediately after election the noop is not committed yet
        let fresh = c.core(l).commit_index() < c.core(l).last_index();
        if fresh {
            c.add_node(ip(4), None);
            assert!(matches!(
                c.core_mut(l).propose_conf_change(ConfChange::AddLearner(ip(4))),
                Err(ProposeError::Busy(_))
            ));
        }
    }

    /// The big one: random crashes, partitions, message loss, duplication and reordering
    /// while clients keep proposing. Safety is asserted on every step; after healing the
    /// cluster must converge and have lost no committed command.
    pub(super) fn chaos_one(seed: u64, nodes: u8, ticks: u64) {
        chaos(seed, nodes, ticks)
    }

    fn chaos(seed: u64, nodes: u8, ticks: u64) {
        println!("chaos seed {seed} nodes {nodes}");
        let ids: Vec<u8> = (1..=nodes).collect();
        let mut c = Cluster::new(&ids, seed);
        c.drop_pct = 8;
        c.dup_pct = 5;
        c.max_delay = 4;
        let mut next_cmd = 1u64;
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut roll = move |m: u64| {
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            rng.wrapping_mul(0x2545_F491_4F6C_DD1D) % m
        };
        for t in 0..ticks {
            match roll(60) {
                0 => {
                    let n = ids[roll(ids.len() as u64) as usize];
                    if c.is_up(ip(n)) && (1..=nodes).filter(|x| c.is_up(ip(*x))).count() > (nodes as usize / 2 + 1) {
                        c.crash(ip(n));
                    }
                }
                1 => {
                    let n = ids[roll(ids.len() as u64) as usize];
                    c.restart(ip(n));
                }
                2 => c.isolate(ids[roll(ids.len() as u64) as usize]),
                3 => c.heal(),
                4 => {
                    let n = ids[roll(ids.len() as u64) as usize];
                    if c.is_up(ip(n)) {
                        c.compact(ip(n));
                    }
                }
                _ => {}
            }
            if t % 3 == 0
                && let Some(l) = c.leader()
            {
                let _ = c.core_mut(l).propose(cmd(next_cmd));
                c.process_ready(l);
                next_cmd += 1;
            }
            c.step_time();
        }
        // heal everything and let the cluster settle
        c.heal();
        c.drop_pct = 0;
        c.dup_pct = 0;
        for n in 1..=nodes {
            c.restart(ip(n));
        }
        let settled = c.run_until(3000, |c| c.leader().is_some() && c.converged());
        assert!(settled, "seed {seed}: cluster did not converge after healing\n{}", c.dump());
        c.run(100);
        assert!(c.converged());
        c.check_log_matching();
        // every node has the same applied sequence
        let first = c.applied_commands(ip(1));
        for n in 2..=nodes {
            assert_eq!(c.applied_commands(ip(n)), first, "seed {seed}: node {n} diverged");
        }
        // committed commands appear in proposal order (values are increasing)
        assert!(first.windows(2).all(|w| w[0] < w[1]), "seed {seed}: commands out of order: {first:?}");
    }

    /// `RAFT_CHAOS_SEEDS=5000 cargo test --release raft::sim::tests::chaos` for a long soak.
    fn seeds(default: u64) -> u64 {
        std::env::var("RAFT_CHAOS_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    /// Same chaos, plus random membership changes: nodes 4 and 5 start blank and are added
    /// and removed while the cluster is crashing and partitioning.
    fn chaos_membership(seed: u64, ticks: u64) {
        println!("chaos-membership seed {seed}");
        let mut c = Cluster::new(&[1, 2, 3], seed);
        c.add_node(ip(4), None);
        c.add_node(ip(5), None);
        c.drop_pct = 6;
        c.dup_pct = 4;
        c.max_delay = 4;
        let mut rng = seed.wrapping_mul(0xD6E8_FEB8_6659_FD93) | 1;
        let mut roll = move |m: u64| {
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            rng.wrapping_mul(0x2545_F491_4F6C_DD1D) % m
        };
        let mut next_cmd = 1u64;
        for t in 0..ticks {
            let pick = (roll(5) + 1) as u8;
            match roll(70) {
                0 => {
                    let up = (1..=5).filter(|n| c.is_up(ip(*n))).count();
                    if c.is_up(ip(pick)) && up > 3 {
                        c.crash(ip(pick));
                    }
                }
                1 => c.restart(ip(pick)),
                2 => c.isolate(pick),
                3 => c.heal(),
                4 | 5 => {
                    if let Some(l) = c.leader() {
                        let target = ip(pick);
                        let cfg = c.core(l).config().clone();
                        let change = if cfg.voters.contains(&target) || cfg.learners.contains(&target) {
                            if cfg.voters.len() > 2 { Some(ConfChange::Remove(target)) } else { None }
                        } else {
                            Some(ConfChange::AddLearner(target))
                        };
                        if let Some(ch) = change {
                            let _ = c.core_mut(l).propose_conf_change(ch);
                            c.process_ready(l);
                        }
                    }
                }
                6 => {
                    let id = ip(pick);
                    if c.is_up(id) {
                        c.compact(id);
                    }
                }
                _ => {}
            }
            if t % 3 == 0
                && let Some(l) = c.leader()
            {
                let _ = c.core_mut(l).propose(cmd(next_cmd));
                c.process_ready(l);
                next_cmd += 1;
            }
            c.step_time();
        }
        c.heal();
        c.drop_pct = 0;
        c.dup_pct = 0;
        for n in 1..=5 {
            c.restart(ip(n));
        }
        // the live membership as seen by the current leader
        let settled = c.run_until(4000, |c| c.leader().is_some() && c.converged_members());
        assert!(settled, "seed {seed}: members did not converge\n{}", c.dump());
        c.run(100);
        c.check_log_matching();
        let leader = c.leader().unwrap();
        let members: Vec<NodeId> = c.core(leader).config().members().copied().collect();
        let first = c.applied_commands(members[0]);
        for m in &members {
            assert_eq!(c.applied_commands(*m), first, "seed {seed}: member {m} diverged\n{}", c.dump());
        }
        assert!(first.windows(2).all(|w| w[0] < w[1]), "seed {seed}: out of order {first:?}");
    }

    #[test]
    fn chaos_with_membership_changes() {
        for seed in 5000..5000 + seeds(40) {
            chaos_membership(seed, 1500);
        }
    }

    #[test]
    fn chaos_three_nodes() {
        for seed in 1..=seeds(40) {
            chaos(seed, 3, 1500);
        }
    }

    #[test]
    fn chaos_five_nodes() {
        for seed in 1000..1000 + seeds(31) {
            chaos(seed, 5, 1500);
        }
    }

    fn last(id: NodeId) -> u8 {
        super::last_octet(id)
    }
}

/// Regression tests for bugs the chaos soak found. Each one replays the exact seed that exposed it.
#[cfg(test)]
mod regressions {
    /// A delayed snapshot used to replace the follower's log wholesale, discarding entries it had
    /// already acknowledged (and the leader had therefore committed). Found by seed 374 with 3 nodes:
    /// "STATE MACHINE SAFETY VIOLATED at index 337". Fixed by Raft thesis section 7, step 6.
    #[test]
    fn delayed_snapshot_must_not_discard_acknowledged_entries_seed_374() {
        super::tests::chaos_one(374, 3, 1500);
    }
}
