//! Service discovery: from a list of bare IPs to a running cluster node.
//!
//! 1. Work out which of the listed IPs is *this* node (explicit `self_ip` → a local
//!    interface address → the handshake: dial every peer and see whose `instance_id` is ours,
//!    which also works behind Docker port publishing → the outbound route).
//! 2. Probe the peers with `Hello` to learn whether a cluster already runs without us (then we
//!    ask its leader to add us) or everybody is blank (then all nodes found the cluster with
//!    the same static membership: the listed IPs).
//! 3. Keep checking while this node is blank, so a founder that raced a running cluster
//!    still ends up joining it.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

use super::auth::ClusterAuth;
use super::node::{self, Cmd, RaftHandle, StartArgs};
use super::storage::RaftStorage;
use super::transport::{CallError, Peers, RaftRpc, RpcState};
use super::types::*;
use crate::error::{Error, Result};
use crate::pb::raft as pb;
use crate::pb::raft::raft_server::RaftServer;

pub fn local_ips() -> Vec<IpAddr> {
    if_addrs::get_if_addrs().map(|v| v.into_iter().map(|i| i.ip()).collect()).unwrap_or_default()
}

/// The source address the OS would use to reach `target` (no packet is sent).
fn outbound_ip(target: IpAddr) -> Option<IpAddr> {
    let bind: SocketAddr =
        if target.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let sock = std::net::UdpSocket::bind(bind).ok()?;
    sock.connect((target, 9)).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// Pure part of self-detection: which listed peer is a local address?
pub fn match_local(peers: &[IpAddr], local: &[IpAddr]) -> std::result::Result<Option<IpAddr>, Vec<IpAddr>> {
    let matches: Vec<IpAddr> = peers.iter().copied().filter(|p| local.contains(p)).collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => Err(matches),
    }
}

async fn probe_all(peers: &Arc<Peers>, targets: &[IpAddr]) -> BTreeMap<IpAddr, pb::HelloReply> {
    let mut set = tokio::task::JoinSet::new();
    for t in targets {
        let p = peers.clone();
        let t = *t;
        set.spawn(async move { (t, p.hello(t).await) });
    }
    let mut out = BTreeMap::new();
    while let Some(Ok((ip, res))) = set.join_next().await {
        match res {
            Ok(reply) => {
                out.insert(ip, reply);
            }
            Err(CallError::Refused(m)) => tracing::warn!(peer = %ip, "peer refused the handshake: {m}"),
            Err(CallError::Unreachable(m)) => tracing::debug!(peer = %ip, "peer not reachable yet: {m}"),
        }
    }
    out
}

pub async fn start(args: StartArgs) -> Result<RaftHandle> {
    let settings = args.settings.clone();
    let storage = Arc::new(RaftStorage::open(args.store.clone())?);
    match storage.cluster_id()? {
        Some(id) if id != settings.cluster_id => {
            return Err(Error::config(format!(
                "this data directory belongs to cluster '{id}' but raft.json says '{}'. Use another data directory or fix cluster_id",
                settings.cluster_id
            )));
        }
        None => storage.set_cluster_id(&settings.cluster_id)?,
        _ => {}
    }
    let instance_id = args.store.instance_id()?;

    // ---- 1. who am I? (first the cheap, local answers) --------------------------------
    let local = local_ips();
    let mut me = match settings.self_ip {
        Some(ip) => Some(ip),
        None => match match_local(&settings.peers, &local) {
            Ok(m) => m,
            Err(many) => {
                return Err(Error::config(format!(
                    "this host owns several of the listed peer addresses ({many:?}); set \"self_ip\" in raft.json"
                )));
            }
        },
    };

    // ---- 2. the RPC server must be up before anyone (including us) probes it ------------
    let unspecified = if settings.peers.iter().any(|p| p.is_ipv6()) {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    };
    // Listen on this node's own address when the host owns it (several nodes may then share a
    // machine and a port); behind NAT/port publishing that address is not local, so fall back
    // to every interface.
    let mut candidates = Vec::new();
    match (settings.bind, me) {
        (Some(b), _) => candidates.push(b),
        (None, Some(ip)) => candidates.extend([ip, unspecified]),
        (None, None) => candidates.push(unspecified),
    }
    let mut listener = None;
    let mut last_err = None;
    for ip in candidates {
        let addr = SocketAddr::new(ip, settings.port);
        match TcpListener::bind(addr).await {
            Ok(l) => {
                tracing::info!("raft listening on {addr}");
                listener = Some(l);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => last_err = Some((addr, e)),
            Err(e) => return Err(Error::cluster(format!("cannot listen for cluster traffic on {addr}: {e}"))),
        }
    }
    let Some(listener) = listener else {
        let (addr, e) = last_err.expect("at least one candidate");
        return Err(Error::cluster(format!("cannot listen for cluster traffic on {addr}: {e}")));
    };

    let rpc_state = Arc::new(RpcState {
        cluster_id: settings.cluster_id.clone(),
        instance_id: instance_id.clone(),
        node: OnceLock::new(),
        self_ip: OnceLock::new(),
    });
    let auth = ClusterAuth::new(&settings.secret);
    let inner = RaftServer::new(RaftRpc { state: rpc_state.clone() })
        .max_decoding_message_size(16 * 1024 * 1024)
        .max_encoding_message_size(16 * 1024 * 1024);
    let service = tonic::service::interceptor::InterceptedService::new(inner, auth.server_interceptor());
    #[cfg(feature = "tls")]
    let server_tls = settings.tls.as_ref().map(|t| crate::tls::tonic_server(&t.material));
    {
        let shutdown = args.shutdown.clone();
        let builder = tonic::transport::Server::builder().tcp_nodelay(true);
        #[cfg(feature = "tls")]
        let builder = match server_tls {
            Some(cfg) => builder.tls_config(cfg).map_err(|e| Error::config(format!("raft tls: {e}")))?,
            None => builder,
        };
        let mut builder = builder;
        tokio::spawn(async move {
            let res = builder
                .add_service(service)
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown.wait())
                .await;
            if let Err(e) = res {
                tracing::error!("raft server stopped: {e}");
            }
        });
    }

    // ---- 3. if still unknown: dial the peers and see whose instance id is ours ----------
    let provisional = Peers::new_tls(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        &settings.cluster_id,
        settings.port,
        &settings.secret,
        settings.election_timeout,
        settings.tls.as_ref(),
    );
    if me.is_none() {
        for _ in 0..5 {
            let replies = probe_all(&provisional, &settings.peers).await;
            if let Some((ip, _)) = replies.iter().find(|(_, r)| r.instance_id == instance_id) {
                me = Some(*ip);
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
    if me.is_none() {
        me = settings.peers.first().and_then(|p| outbound_ip(*p)).filter(|ip| !ip.is_unspecified());
        if let Some(ip) = me {
            tracing::warn!(
                "could not match this host to a listed peer; assuming it is reached at {ip} (set \"self_ip\" in raft.json to be explicit)"
            );
        }
    }
    let Some(me) = me else {
        return Err(Error::config("cannot work out which IP this node is reached at; set \"self_ip\" in raft.json"));
    };
    let _ = rpc_state.self_ip.set(me);
    tracing::info!("raft node identity: {me} (instance {})", &instance_id[..8.min(instance_id.len())]);

    // ---- 4. found a cluster or join one -------------------------------------------------
    let persisted = storage.load()?;
    let blank = persisted.entries.is_empty() && persisted.snapshot.0 == 0 && persisted.hard_state.term == 0;
    let mut members: Vec<IpAddr> = settings.peers.clone();
    if !members.contains(&me) {
        members.push(me);
    }
    let static_config = ClusterConfig::new(members.iter().copied());
    let others: Vec<IpAddr> = members.iter().copied().filter(|m| *m != me).collect();
    let peers = Peers::new_tls(
        me,
        &settings.cluster_id,
        settings.port,
        &settings.secret,
        settings.election_timeout.max(Duration::from_secs(1)),
        settings.tls.as_ref(),
    );

    let mut bootstrap = Some(static_config);
    if blank && !others.is_empty() {
        let replies = probe_all(&peers, &others).await;
        let existing = replies.values().any(|r| {
            let listed = r.voters.iter().chain(r.learners.iter()).any(|v| v == &me.to_string());
            (!r.leader.is_empty() || (r.bootstrapped && !r.voters.is_empty())) && !listed
        });
        if existing {
            tracing::info!(
                "a cluster is already running without this node ({}); joining it",
                super::transport::summarize(&replies)
            );
            bootstrap = None;
        }
    }

    // ---- 5. start the driver -------------------------------------------------------------
    let built = node::build(me, &args, persisted, bootstrap, storage, peers.clone());
    let handle = built.handle.clone();
    let _ = rpc_state.node.set(handle.shared.clone());
    tokio::spawn(built.driver.run());
    tokio::spawn(membership_task(handle.clone(), peers, others, args.shutdown.clone()));
    Ok(handle)
}

/// While this node is blank, make sure it is part of whatever cluster exists.
async fn membership_task(handle: RaftHandle, peers: Arc<Peers>, others: Vec<IpAddr>, shutdown: crate::app::Shutdown) {
    let me = handle.node();
    let mut warned_removed = false;
    loop {
        tokio::select! {
            _ = shutdown.wait() => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        let st = handle.status();
        if st.removed || st.failed.is_some() {
            if st.removed && !warned_removed {
                tracing::warn!(
                    "this node is no longer a member of the cluster; add it back with `raft add-peer {me}` on the leader"
                );
                warned_removed = true;
            }
            continue;
        }
        if st.last_index > 0 {
            continue; // already integrated: the log is flowing
        }
        // blank node: is there a cluster whose membership does not include us?
        let replies = probe_all(&peers, &others).await;
        let leader = replies.values().find(|r| !r.leader.is_empty()).and_then(|r| r.leader.parse::<IpAddr>().ok());
        let Some(leader) = leader else { continue };
        let reply = replies.get(&leader);
        let listed = reply.is_some_and(|r| r.voters.iter().chain(r.learners.iter()).any(|v| v == &me.to_string()));
        if listed {
            continue;
        }
        tracing::info!("asking the leader {leader} to add this node");
        let _ = handle.shared.tx.send(Cmd::BecomeJoiner).await;
        match peers.join(leader, me).await {
            Ok(()) => tracing::info!("join accepted by {leader}"),
            Err(e) => tracing::warn!("join request to {leader} failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_detection_from_local_addresses() {
        let peers: Vec<IpAddr> = ["10.0.0.1", "10.0.0.2", "10.0.0.3"].iter().map(|s| s.parse().unwrap()).collect();
        let local: Vec<IpAddr> = ["127.0.0.1", "10.0.0.2", "192.168.1.5"].iter().map(|s| s.parse().unwrap()).collect();
        assert_eq!(match_local(&peers, &local), Ok(Some("10.0.0.2".parse().unwrap())));
        assert_eq!(
            match_local(&peers, &["172.17.0.2".parse().unwrap()]),
            Ok(None),
            "NAT/bridge: fall back to the handshake"
        );
        let both: Vec<IpAddr> = ["10.0.0.1", "10.0.0.3"].iter().map(|s| s.parse().unwrap()).collect();
        assert!(match_local(&peers, &both).is_err(), "ambiguous: the user must say which one");
    }
}
