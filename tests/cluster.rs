//! Raft cluster tests with real daemon processes on distinct loopback IPs (127.0.0.N), all
//! sharing one Raft port — exactly the "list of bare IPs" model. They exercise the whole stack:
//! discovery, election, replication of shared memory and overrides, failover after a hard
//! kill, rolling restarts, dynamic join, snapshot catch-up and peer authentication.
//!
//! Skipped on macOS, where only 127.0.0.1 is bound by default.

#![cfg(not(target_os = "macos"))]

mod common;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use common::*;
use open_cardinal::control::protocol::{CliRequest, CliResponse};
use serde_json::{Value, json};

struct Slot {
    n: u8,
    grpc: u16,
    http: u16,
    control: u16,
    home: PathBuf,
    _tmp: tempfile::TempDir,
    secret: String,
    raft: Value,
    /// Extra files written into the node's home (certificates).
    files: Vec<(String, Vec<u8>)>,
    d: Option<Daemon>,
}

struct Cluster {
    slots: Vec<Slot>,
    raft_port: u16,
}

fn raft_json(peers: &[u8], port: u16, n: u8, extra: &Value) -> Value {
    let mut v = json!({
        "peers": peers.iter().map(|p| format!("127.0.0.{p}")).collect::<Vec<_>>(),
        "port": port,
        "self_ip": format!("127.0.0.{n}"),
        "heartbeat_ms": 100,
        "election_timeout_ms": 600,
    });
    for (k, val) in extra.as_object().into_iter().flatten() {
        v[k] = val.clone();
    }
    v
}

impl Cluster {
    /// Create (but do not start) nodes 1..=count that know `peers`.
    fn new(count: u8, peers: &[u8], extra_raft: Value) -> Self {
        let raft_port = free_port();
        let slots = (1..=count)
            .map(|n| {
                let tmp = tempfile::tempdir().unwrap();
                Slot {
                    n,
                    grpc: free_port(),
                    http: free_port(),
                    control: free_port(),
                    home: tmp.path().to_path_buf(),
                    _tmp: tmp,
                    secret: SECRET.into(),
                    raft: raft_json(peers, raft_port, n, &extra_raft),
                    files: vec![],
                    d: None,
                }
            })
            .collect();
        Cluster { slots, raft_port }
    }

    fn spec(slot: &Slot) -> Spec {
        Spec {
            ip: Ipv4Addr::new(127, 0, 0, slot.n),
            grpc: slot.grpc,
            http: slot.http,
            control_port: slot.control,
            config: json!({}),
            files: vec![
                ("config/raft.json".into(), serde_json::to_vec(&slot.raft).unwrap()),
                ("rules/default/default.lua".into(), DEFAULT_RULE.as_bytes().to_vec()),
                ("rules/Victim/strikes.lua".into(), STRIKES_RULE.as_bytes().to_vec()),
                (
                    "rules/Counter/c.lua".into(),
                    b"local n = redb_api.incr('hits') return { action = 'CUSTOM', cmd_name = 'HITS:' .. n }".to_vec(),
                ),
            ],
            env: vec![("CARDINAL_CLUSTER_SECRET".into(), slot.secret.clone())],
        }
        .with_files(&slot.files)
    }

    async fn start(&mut self, i: usize) {
        let slot = &mut self.slots[i];
        assert!(slot.d.is_none(), "node {} already running", slot.n);
        let d = Daemon::start_in(Self::spec(slot), slot.home.clone(), None).await;
        slot.d = Some(d);
    }

    async fn start_all(&mut self) {
        for i in 0..self.slots.len() {
            self.start(i).await;
        }
    }

    fn node(&self, i: usize) -> &Daemon {
        self.slots[i].d.as_ref().unwrap_or_else(|| panic!("node {} is down", self.slots[i].n))
    }

    fn kill(&mut self, i: usize) {
        if let Some(mut d) = self.slots[i].d.take() {
            d.kill();
        }
    }

    async fn stop(&mut self, i: usize) {
        if let Some(mut d) = self.slots[i].d.take() {
            d.stop().await;
        }
    }

    fn running(&self) -> Vec<usize> {
        (0..self.slots.len()).filter(|i| self.slots[*i].d.is_some()).collect()
    }

    async fn raft(&self, i: usize) -> Value {
        match self.node(i).try_ctl(CliRequest::Raft { command: "status".into(), args: vec![] }).await {
            Ok(CliResponse::Data { payload }) => payload,
            _ => Value::Null,
        }
    }

    /// Index of the unique leader that every running node agrees on.
    async fn leader(&self) -> Option<usize> {
        let mut leader_ip: Option<String> = None;
        for i in self.running() {
            let st = self.raft(i).await;
            let l = st["leader"].as_str()?.to_string();
            match &leader_ip {
                None => leader_ip = Some(l),
                Some(prev) if *prev == l => {}
                _ => return None,
            }
        }
        let ip = leader_ip?;
        self.running().into_iter().find(|i| format!("127.0.0.{}", self.slots[*i].n) == ip)
    }

    async fn wait_leader(&self) -> usize {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(l) = self.leader().await {
                return l;
            }
            assert!(std::time::Instant::now() < deadline, "no stable leader: {}", self.dump().await);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_applied_equal(&self) {
        let ok = wait_until(Duration::from_secs(30), || async {
            let mut seen = None;
            for i in self.running() {
                let st = self.raft(i).await;
                let a = st["applied_index"].as_u64();
                let c = st["commit_index"].as_u64();
                if a.is_none() || a != c {
                    return false;
                }
                match seen {
                    None => seen = a,
                    Some(prev) if Some(prev) == a => {}
                    _ => return false,
                }
            }
            true
        })
        .await;
        assert!(ok, "nodes did not converge: {}", self.dump().await);
    }

    async fn dump(&self) -> String {
        let mut out = String::new();
        for i in self.running() {
            out.push_str(&format!("node {}: {}\n", self.slots[i].n, self.raft(i).await));
        }
        out
    }

    async fn force(&self, via: usize, agent: &str, kind: &str) {
        let msg = self.node(via).ok(Daemon::heathcliff("force", &["--agent", agent, "--force", kind])).await;
        assert!(msg.contains(agent), "{msg}");
    }

    async fn reaction(&self, via: usize, agent: &str) -> (i32, String) {
        let r = self.node(via).ping(agent, &[("fuel", "100")], None).await.unwrap();
        (r.r#type, r.command_name)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_form_a_cluster_and_share_overrides_and_rule_state() {
    let mut c = Cluster::new(3, &[1, 2, 3], json!({}));
    c.start_all().await;
    let leader = c.wait_leader().await;

    // an override issued on a FOLLOWER is visible on every node
    let follower = (0..3).find(|i| *i != leader).unwrap();
    c.force(follower, "Pump", "1").await;
    c.wait_applied_equal().await;
    for i in 0..3 {
        assert_eq!(c.reaction(i, "Pump").await, (1, "heathcliff".into()), "node {}", i + 1);
    }
    c.node(follower).ok(Daemon::heathcliff("revoke_force", &["--agent", "Pump"])).await;
    c.wait_applied_equal().await;
    for i in 0..3 {
        assert_eq!(c.reaction(i, "Pump").await.0, 0);
    }

    // rule state (the wiki's strikes counter) accumulates across DIFFERENT nodes
    let a = c.node(0).ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
    let b = c.node(1).ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
    let third = c.node(2).ping("Victim", &[("cpu_temp", "95")], None).await.unwrap();
    assert_eq!((a.r#type, b.r#type), (0, 0));
    assert_eq!(third.command_name, "PERSISTENT_OVERHEAT", "the 3rd consecutive hot reading, seen by a third node");

    // atomic counters never lose an increment, even from all nodes concurrently
    let mut tasks = Vec::new();
    for i in 0..3 {
        let mut client = c.node(i).grpc_client().await;
        tasks.push(tokio::spawn(async move {
            for _ in 0..10 {
                ping_with(&mut client, "Counter", &[], None).await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    c.wait_applied_equal().await;
    let r = c.node(0).ping("Counter", &[], None).await.unwrap();
    assert_eq!(r.command_name, "HITS:31", "30 concurrent increments from 3 nodes + this one");

    // cluster views
    let status = c.node(0).data(CliRequest::Status).await;
    assert!(status["cluster"].as_str().unwrap().contains("term"), "{status}");
    let ready = c.node(1).http_get("/readyz", None).await;
    assert_eq!(ready.0, 200, "{}", ready.2);
    let (_, _, prom) = c.node(0).http_get("/metrics?format=prometheus", None).await;
    assert!(prom.contains("cardinal_raft_has_leader 1") && prom.contains("cardinal_raft_voters 3"), "{prom}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hard_killed_leader_failover_keeps_every_committed_write_and_the_node_rejoins() {
    let mut c = Cluster::new(3, &[1, 2, 3], json!({}));
    c.start_all().await;
    let leader = c.wait_leader().await;
    c.force((leader + 1) % 3, "Before", "1").await;
    c.wait_applied_equal().await;

    // kill -9
    c.kill(leader);
    let started = std::time::Instant::now();
    let new_leader = c.wait_leader().await;
    assert_ne!(new_leader, leader);
    assert!(started.elapsed() < Duration::from_secs(10), "failover took {:?}", started.elapsed());

    // the 2-node majority keeps accepting writes, and nothing committed earlier is gone
    c.force(new_leader, "During", "2").await;
    // followers apply a committed override within about one network round trip
    c.wait_applied_equal().await;
    for i in c.running() {
        assert_eq!(c.reaction(i, "Before").await.0, 1, "write committed before the crash survived");
        assert_eq!(c.reaction(i, "During").await.0, 2);
    }

    // the killed node restarts from its own disk and catches up
    c.start(leader).await;
    c.wait_leader().await;
    c.wait_applied_equal().await;
    assert_eq!(c.reaction(leader, "During").await.0, 2, "the restarted node learned what it missed");
    assert_eq!(c.reaction(leader, "Before").await.0, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rolling_graceful_restarts_never_lose_writability() {
    let mut c = Cluster::new(3, &[1, 2, 3], json!({}));
    c.start_all().await;
    c.wait_leader().await;
    for round in 0..3 {
        // restart the *leader* each time (the worst case) with a graceful stop
        let leader = c.wait_leader().await;
        let t = std::time::Instant::now();
        c.stop(leader).await;
        let survivor = c.running()[0];
        // graceful stop transfers leadership first: writes resume quickly
        let agent = format!("Roll{round}");
        let ok = wait_until(Duration::from_secs(10), || async {
            matches!(
                c.node(survivor).try_ctl(Daemon::heathcliff("force", &["--agent", &agent, "--force", "1"])).await,
                Ok(CliResponse::Ok { .. })
            )
        })
        .await;
        assert!(ok, "writes did not resume after the leader stopped: {}", c.dump().await);
        assert!(t.elapsed() < Duration::from_secs(8));
        c.start(leader).await;
        c.wait_leader().await;
        c.wait_applied_equal().await;
    }
    for round in 0..3 {
        assert_eq!(c.reaction(0, &format!("Roll{round}")).await.0, 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_host_joins_knowing_only_the_ips_and_is_promoted_automatically() {
    // nodes 1-3 form the cluster; node 4 is configured with the other hosts' IPs only
    let mut c = Cluster::new(3, &[1, 2, 3], json!({}));
    c.start_all().await;
    c.wait_leader().await;
    c.force(0, "Old", "1").await;
    c.wait_applied_equal().await;

    // append node 4 (its raft.json lists nodes 1-3; the cluster does not know it yet)
    let tmp = tempfile::tempdir().unwrap();
    c.slots.push(Slot {
        n: 4,
        grpc: free_port(),
        http: free_port(),
        control: free_port(),
        home: tmp.path().to_path_buf(),
        _tmp: tmp,
        secret: SECRET.into(),
        raft: raft_json(&[1, 2, 3], c.raft_port, 4, &json!({})),
        files: vec![],
        d: None,
    });
    c.start(3).await;

    let ok = wait_until(Duration::from_secs(30), || async {
        let st = c.raft(0).await;
        st["voters"].as_array().is_some_and(|v| v.iter().any(|x| x == "127.0.0.4"))
    })
    .await;
    assert!(ok, "node 4 was not promoted to voter: {}", c.dump().await);
    c.wait_applied_equal().await;
    assert_eq!(c.reaction(3, "Old").await.0, 1, "the new node received the state that predates it");

    // it takes part in writes, and quorum is now 3 of 4
    c.force(3, "FromNew", "2").await;
    c.wait_applied_equal().await;
    assert_eq!(c.reaction(0, "FromNew").await.0, 2);
    let members = c.node(0).data(CliRequest::Raft { command: "members".into(), args: vec![] }).await;
    assert_eq!(members["members"].as_array().unwrap().len(), 4);
    c.kill(2);
    let leader = c.wait_leader().await;
    c.force(leader, "StillWritable", "1").await;

    // and it can be removed again with a bare IP
    let msg =
        c.node(leader).ok(CliRequest::Raft { command: "remove-peer".into(), args: vec!["127.0.0.3".into()] }).await;
    assert!(msg.contains("127.0.0.3"), "{msg}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_missed_a_compacted_log_catches_up_through_a_snapshot() {
    let mut c = Cluster::new(3, &[1, 2, 3], json!({ "snapshot_threshold": 10 }));
    c.start(0).await;
    c.start(1).await;
    c.wait_leader().await;
    for i in 0..60 {
        c.force(0, &format!("Agent_{i}"), "1").await;
    }
    // node 3 was never up: the leader's log no longer holds what it needs
    let leader = c.wait_leader().await;
    let st = c.raft(leader).await;
    assert!(st["snapshot_index"].as_u64().unwrap() > 10, "the log was compacted: {st}");
    c.start(2).await;
    c.wait_applied_equal().await;
    for i in [0, 30, 59] {
        assert_eq!(c.reaction(2, &format!("Agent_{i}")).await.0, 1, "agent {i}");
    }
    let listed = c.node(2).data(Daemon::heathcliff("list", &[])).await;
    assert_eq!(listed["overrides"].as_array().unwrap().len(), 60);
    assert!(c.node(2).log().contains("installed snapshot"), "node 3 must have used a snapshot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_with_the_wrong_secret_are_refused() {
    let mut c = Cluster::new(3, &[1, 2, 3], json!({}));
    c.slots[2].secret = "ffffffffffffffffffffffffffffffffffffffff".into();
    c.start_all().await;
    // nodes 1 and 2 still form a majority; node 3 is a stranger
    let ok = wait_until(Duration::from_secs(30), || async {
        let s1 = c.raft(0).await;
        s1["leader"].is_string() && s1["role"] != "pre-candidate"
    })
    .await;
    assert!(ok, "{}", c.dump().await);
    let st3 = c.raft(2).await;
    assert!(st3["leader"].is_null(), "the node with the wrong secret must not see a leader: {st3}");
    assert!(c.node(2).log().contains("bad peer credentials"), "{}", c.node(2).log());

    // and an anonymous caller cannot talk to the raft port at all
    use open_cardinal::pb::raft::HelloRequest;
    use open_cardinal::pb::raft::raft_client::RaftClient;
    let ch = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{}", c.raft_port))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let err = RaftClient::new(ch)
        .hello(HelloRequest { cluster_id: "cardinal".into(), from_ip: "6.6.6.6".into() })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_quorum_decisions_continue_but_replicated_writes_fail_fast() {
    let mut c = Cluster::new(3, &[1, 2, 3], json!({}));
    c.start_all().await;
    let leader = c.wait_leader().await;
    c.force(leader, "Pinned", "1").await;
    c.wait_applied_equal().await;

    let survivor = (leader + 1) % 3;
    for i in 0..3 {
        if i != survivor {
            c.kill(i);
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    // stateless rules and committed overrides keep deciding
    let t = std::time::Instant::now();
    assert_eq!(c.reaction(survivor, "Pinned").await.0, 1, "already-replicated overrides still apply");
    let r = c.node(survivor).ping("Rocket", &[("fuel", "10")], None).await.unwrap();
    assert_eq!(r.command_name, "EMERGENCY_CUTOFF");
    // a rule that needs to *write* shared state cannot commit: it fails within its time budget
    // instead of hanging the pulse for the whole Raft timeout
    let r = c.node(survivor).ping("Counter", &[], None).await.unwrap();
    assert_eq!(r.r#type, 0, "no decision from the stateful rule");
    assert!(t.elapsed() < Duration::from_secs(3), "pulses must not wait for a quorum: {:?}", t.elapsed());
    // operators get a clear error
    let e = c.node(survivor).err(Daemon::heathcliff("force", &["--agent", "X", "--force", "1"])).await;
    assert!(e.contains("could not apply override"), "{e}");
    // readiness reports the missing leader, so orchestrators do not roll more pods
    let (code, _, _) = c.node(survivor).http_get("/readyz", None).await;
    assert_eq!(code, 503);
}

#[cfg(feature = "tls")]
fn node_certs(ca: &open_cardinal::tls::Issued, n: u8) -> Vec<(String, Vec<u8>)> {
    let c = open_cardinal::tls::issue(
        &ca.cert_pem,
        &ca.key_pem,
        &format!("node{n}"),
        &[format!("127.0.0.{n}").parse().unwrap()],
        &[],
        30,
    )
    .unwrap();
    vec![
        ("certs/ca.pem".into(), ca.cert_pem.clone().into_bytes()),
        ("certs/node.pem".into(), c.cert_pem.into_bytes()),
        ("certs/node.key".into(), c.key_pem.into_bytes()),
    ]
}

#[cfg(feature = "tls")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raft_peers_use_mutual_tls_and_refuse_strangers() {
    let ca = open_cardinal::tls::new_ca("Cluster CA", 30).unwrap();
    let tls =
        json!({ "tls": { "cert_file": "certs/node.pem", "key_file": "certs/node.key", "ca_file": "certs/ca.pem" } });
    let mut c = Cluster::new(3, &[1, 2, 3], tls);
    for i in 0..3 {
        c.slots[i].files = node_certs(&ca, (i + 1) as u8);
    }
    // node 3 has a certificate from a different CA (and the right secret): a stranger
    let rogue = open_cardinal::tls::new_ca("Rogue CA", 30).unwrap();
    let mut rogue_files = node_certs(&rogue, 3);
    rogue_files[0].1 = ca.cert_pem.clone().into_bytes(); // it even trusts the real CA
    c.slots[2].files = rogue_files;
    c.start_all().await;

    // nodes 1 and 2 form a majority over mutual TLS
    let ok = wait_until(Duration::from_secs(30), || async {
        let a = c.raft(0).await;
        let b = c.raft(1).await;
        a["leader"].is_string() && a["leader"] == b["leader"]
    })
    .await;
    assert!(ok, "{}", c.dump().await);
    c.force(0, "Sealed", "1").await;
    assert_eq!(c.reaction(1, "Sealed").await.0, 1, "replication works over mTLS");

    // the stranger never sees a leader and never receives data
    let st = c.raft(2).await;
    assert!(st["leader"].is_null(), "{st}");
    assert_eq!(c.reaction(2, "Sealed").await.0, 0);

    // a plaintext (or certificate-less) caller cannot even speak to the raft port
    let plain =
        tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{}", c.raft_port)).unwrap().connect().await;
    let denied = match plain {
        Err(_) => true,
        Ok(ch) => {
            use open_cardinal::pb::raft::HelloRequest;
            open_cardinal::pb::raft::raft_client::RaftClient::new(ch)
                .hello(HelloRequest { cluster_id: "cardinal".into(), from_ip: "6.6.6.6".into() })
                .await
                .is_err()
        }
    };
    assert!(denied);
}
