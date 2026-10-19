//! Bridge between the application and the Raft cluster: status views and `raft ...` commands.

use std::net::IpAddr;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::control::protocol::{CliResponse, Flags};
use crate::raft::RaftStatus;

fn describe(st: &RaftStatus) -> String {
    match (st.role, st.leader) {
        ("leader", _) => format!("leader (term {}, {} voters)", st.term, st.voters.len()),
        (_, Some(l)) => format!("{} of {l} (term {})", st.role, st.term),
        (role, None) => format!("{role}, no leader (term {})", st.term),
    }
}

/// One-line description for `status`.
pub fn headline(app: &Arc<App>) -> String {
    match &app.raft {
        None => "disabled".into(),
        Some(r) => describe(&r.status()),
    }
}

pub fn summary(app: &Arc<App>) -> Value {
    let Some(r) = &app.raft else { return json!({ "enabled": false }) };
    let st = r.status();
    json!({
        "enabled": true,
        "node": st.node.to_string(),
        "cluster_id": st.cluster_id,
        "role": st.role,
        "term": st.term,
        "leader": st.leader.map(|l| l.to_string()),
        "commit_index": st.commit_index,
        "applied_index": st.applied_index,
        "last_index": st.last_index,
        "snapshot_index": st.snapshot_index,
        "voters": st.voters.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
        "learners": st.learners.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
        "peers": st.peers.iter().map(|p| json!({ "ip": p.ip.to_string(), "role": p.role, "match_index": p.match_index })).collect::<Vec<_>>(),
        "failed": st.failed,
    })
}

/// `(ready, details)` for `/readyz`. A clustered node is ready once it is a member that knows
/// the leader: that makes rolling updates wait for the cluster to be healthy between restarts.
pub fn readiness(app: &Arc<App>) -> (bool, Value) {
    let Some(r) = &app.raft else { return (true, json!({ "enabled": false })) };
    let st = r.status();
    (
        st.writable(),
        json!({
            "enabled": true,
            "role": st.role,
            "member": st.is_member,
            "has_leader": st.leader.is_some(),
            "failed": st.failed,
        }),
    )
}

pub fn gauges(app: &Arc<App>) -> Vec<(String, String, f64)> {
    let Some(r) = &app.raft else { return Vec::new() };
    let st = r.status();
    let g = |name: &str, help: &str, v: f64| (name.to_string(), help.to_string(), v);
    vec![
        g("cardinal_raft_enabled", "1 when clustering is on.", 1.0),
        g("cardinal_raft_term", "Current Raft term.", st.term as f64),
        g("cardinal_raft_is_leader", "1 when this node is the leader.", f64::from(st.role == "leader")),
        g("cardinal_raft_has_leader", "1 when a leader is known.", f64::from(st.leader.is_some())),
        g("cardinal_raft_commit_index", "Committed log index.", st.commit_index as f64),
        g("cardinal_raft_applied_index", "Applied log index.", st.applied_index as f64),
        g("cardinal_raft_last_index", "Last log index.", st.last_index as f64),
        g("cardinal_raft_voters", "Voting members.", st.voters.len() as f64),
        g("cardinal_raft_learners", "Non-voting members catching up.", st.learners.len() as f64),
    ]
}

pub async fn control(app: &Arc<App>, command: &str, args: &[String]) -> CliResponse {
    let Some(raft) = &app.raft else {
        return CliResponse::error(
            "raft is not enabled on this node (create config/raft.json with the IPs of the hosts)",
        );
    };
    let f = Flags::parse(args);
    let ip_arg = || -> Result<Option<IpAddr>, String> {
        match f.positional.first().map(String::as_str).or(f.get("ip")) {
            None => Ok(None),
            Some(s) => s.parse().map(Some).map_err(|_| {
                format!("'{}' is not an IP address (peers are identified by bare IPs)", crate::util::log_safe(s))
            }),
        }
    };
    match command {
        "status" => CliResponse::data(summary(app)),
        "members" => {
            let st = raft.status();
            let mut rows = vec![];
            for v in &st.voters {
                rows.push(json!({ "ip": v.to_string(), "role": "voter", "leader": st.leader == Some(*v), "self": *v == st.node,
                    "match_index": st.peers.iter().find(|p| p.ip == *v).and_then(|p| p.match_index) }));
            }
            for l in &st.learners {
                rows.push(json!({ "ip": l.to_string(), "role": "learner", "leader": false, "self": *l == st.node,
                    "match_index": st.peers.iter().find(|p| p.ip == *l).and_then(|p| p.match_index) }));
            }
            CliResponse::data(json!({ "members": rows, "leader": st.leader.map(|l| l.to_string()) }))
        }
        "add-peer" | "remove-peer" | "transfer-leader" => {
            let ip = match ip_arg() {
                Ok(ip) => ip,
                Err(e) => return CliResponse::error(e),
            };
            let op = match command {
                "add-peer" => "add_peer",
                "remove-peer" => "remove_peer",
                _ => "transfer_leader",
            };
            if op != "transfer_leader" && ip.is_none() {
                return CliResponse::error(format!("usage: raft {command} <ip>"));
            }
            let reply = raft.admin(op, ip).await;
            if reply.ok { CliResponse::ok(reply.message) } else { CliResponse::error(reply.error) }
        }
        "snapshot" => match raft.compact_now().await {
            Ok(i) => CliResponse::ok(format!("log compacted up to index {i}")),
            Err(e) => CliResponse::error(e),
        },
        "keygen" => CliResponse::ok(crate::raft::config::generate_secret()),
        other => CliResponse::error(format!(
            "Comando desconhecido: {other} (use status | members | add-peer <ip> | remove-peer <ip> | transfer-leader [ip] | snapshot | keygen)"
        )),
    }
}
