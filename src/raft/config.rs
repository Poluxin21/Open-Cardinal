//! `config/raft.json`: the manually provided list of peer **IPs** (nothing else is required;
//! ports, node identities, roles and the rest of the membership are discovered).
//!
//! ```json
//! { "peers": ["10.0.0.11", "10.0.0.12", "10.0.0.13"] }
//! ```
//!
//! Optional: `port` (same on every host, default 50052), `self_ip` (when this host cannot
//! tell which of the peers it is), `cluster_id`, `bind`, timing knobs.

use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::error::{Error, Result};
use crate::util;

pub const DEFAULT_RAFT_PORT: u16 = 50052;
pub const DEFAULT_CLUSTER_ID: &str = "cardinal";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RaftFile {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Bare IP addresses of the hosts that share memory (this host may be listed too).
    #[serde(default)]
    pub peers: Vec<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// The IP the other nodes use to reach this node, when it cannot be detected.
    #[serde(default)]
    pub self_ip: Option<String>,
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// Interface to listen on (default: this node's IP when local, otherwise every interface).
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub heartbeat_ms: Option<u64>,
    #[serde(default)]
    pub election_timeout_ms: Option<u64>,
    /// Compact the log once this many applied entries pile up.
    #[serde(default)]
    pub snapshot_threshold: Option<u64>,
    /// `linearizable` (default): `redb_api.get` waits until this node has applied everything
    /// acknowledged cluster-wide. `local`: read this node's state as is (faster, but a read
    /// right after a write on another node can be stale).
    #[serde(default)]
    pub reads: Option<String>,
    /// Mutual TLS between the nodes. All three files are PEM; `ca_file` is the CA that signed
    /// every node certificate. Node certificates must carry the DNS name `cardinal-raft`
    /// (`open-cardinal tls issue` adds it).
    #[serde(default)]
    pub tls: Option<RaftTlsFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RaftTlsFile {
    pub cert_file: String,
    pub key_file: String,
    pub ca_file: String,
}

fn yes() -> bool {
    true
}

/// Validated settings.
#[derive(Clone, Debug)]
pub struct RaftSettings {
    pub peers: Vec<IpAddr>,
    pub port: u16,
    pub self_ip: Option<IpAddr>,
    pub cluster_id: String,
    pub bind: Option<IpAddr>,
    pub heartbeat: Duration,
    pub election_timeout: Duration,
    pub snapshot_threshold: u64,
    pub secret: String,
    /// How long a write waits for its entry to commit.
    pub propose_timeout: Duration,
    pub linearizable_reads: bool,
    /// Node identity + the CA that signed every node (mTLS between peers).
    pub tls: Option<RaftTls>,
}

#[derive(Clone)]
pub struct RaftTls {
    pub material: crate::tls::TlsMaterial,
    pub ca_pem: Vec<u8>,
}

impl std::fmt::Debug for RaftTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RaftTls(..)")
    }
}

impl RaftSettings {
    pub const TICK: Duration = Duration::from_millis(50);

    /// `Ok(None)` when clustering is not configured (single-node mode).
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        Self::load_with(paths, |k| std::env::var(k).ok())
    }

    pub fn load_with(paths: &Paths, env: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let file_path = paths.raft_file();
        let mut file = read_file(&file_path)?;
        let env_peers = env("CARDINAL_RAFT_PEERS");
        if file.is_none() && env_peers.is_none() {
            return Ok(None);
        }
        let mut file = file.take().unwrap_or(RaftFile {
            enabled: true,
            peers: Vec::new(),
            port: None,
            self_ip: None,
            cluster_id: None,
            bind: None,
            heartbeat_ms: None,
            election_timeout_ms: None,
            snapshot_threshold: None,
            reads: None,
            tls: None,
        });
        if let Some(p) = env_peers {
            file.peers = p.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        if let Some(v) = env("CARDINAL_RAFT_PORT") {
            file.port = Some(v.trim().parse().map_err(|_| Error::config("CARDINAL_RAFT_PORT must be a port number"))?);
        }
        if let Some(v) = env("CARDINAL_RAFT_SELF_IP") {
            file.self_ip = Some(v);
        }
        if let Some(v) = env("CARDINAL_RAFT_CLUSTER_ID") {
            file.cluster_id = Some(v);
        }
        if !file.enabled {
            return Ok(None);
        }

        let peers = parse_ips("peers", &file.peers)?;
        let self_ip = file.self_ip.as_deref().map(|s| parse_ip("self_ip", s)).transpose()?;
        let bind = file.bind.as_deref().map(|s| parse_ip_allow_unspecified("bind", s)).transpose()?;
        if peers.is_empty() && self_ip.is_none() {
            return Err(Error::config("raft.json: \"peers\" must list at least one IP address"));
        }
        let cluster_id = file.cluster_id.unwrap_or_else(|| DEFAULT_CLUSTER_ID.to_string());
        if cluster_id.is_empty()
            || cluster_id.len() > 64
            || !cluster_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::config("raft.json: cluster_id must be 1-64 characters of [A-Za-z0-9_-]"));
        }
        let heartbeat = Duration::from_millis(file.heartbeat_ms.unwrap_or(200).clamp(20, 10_000));
        let election_timeout = Duration::from_millis(file.election_timeout_ms.unwrap_or(1000).clamp(100, 60_000));
        if election_timeout < heartbeat * 3 {
            return Err(Error::config("raft.json: election_timeout_ms must be at least 3x heartbeat_ms"));
        }
        let secret = load_secret(&paths.cluster_secret_file(), &env)?;
        crate::tls::check_supported(file.tls.is_some())?;
        let tls = file
            .tls
            .as_ref()
            .map(|t| -> Result<RaftTls> {
                let cfg = crate::config::TlsConfig {
                    cert_file: t.cert_file.clone(),
                    key_file: t.key_file.clone(),
                    // requiring client certificates from the CA = mutual TLS
                    client_ca_file: Some(t.ca_file.clone()),
                };
                let material = crate::tls::load(&cfg, &paths.home)?;
                let ca_pem = material.client_ca_pem.clone().unwrap_or_default();
                Ok(RaftTls { material, ca_pem })
            })
            .transpose()?;
        let linearizable_reads = match file.reads.as_deref() {
            None | Some("linearizable") => true,
            Some("local") => false,
            Some(other) => {
                return Err(Error::config(format!(
                    "raft.json: reads must be \"linearizable\" or \"local\", got '{}'",
                    util::log_safe(other)
                )));
            }
        };

        Ok(Some(Self {
            peers,
            port: file.port.unwrap_or(DEFAULT_RAFT_PORT),
            self_ip,
            cluster_id,
            bind,
            heartbeat,
            election_timeout,
            snapshot_threshold: file.snapshot_threshold.unwrap_or(10_000).max(10),
            secret,
            propose_timeout: (election_timeout * 4).max(Duration::from_secs(3)),
            linearizable_reads,
            tls,
        }))
    }

    pub fn core_config(&self, seed: u64) -> super::core::CoreConfig {
        let tick = Self::TICK.as_millis() as u64;
        super::core::CoreConfig {
            election_ticks: (self.election_timeout.as_millis() as u64 / tick).max(4),
            heartbeat_ticks: (self.heartbeat.as_millis() as u64 / tick).max(1),
            max_entries_per_append: 256,
            max_uncommitted: 8192,
            snapshot_timeout_ticks: 30_000 / tick,
            seed,
        }
    }
}

fn read_file(path: &Path) -> Result<Option<RaftFile>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text).map(Some).map_err(|e| Error::config(format!("{}: {e}", path.display())))
}

fn parse_ip(field: &str, s: &str) -> Result<IpAddr> {
    let ip: IpAddr = s.trim().parse().map_err(|_| {
        Error::config(format!(
            "raft.json: {field} must be bare IP addresses (no port, no hostname): got '{}'",
            util::log_safe(s)
        ))
    })?;
    if ip.is_unspecified() || ip.is_multicast() {
        return Err(Error::config(format!("raft.json: {field}: {ip} is not a usable node address")));
    }
    Ok(ip)
}

fn parse_ip_allow_unspecified(field: &str, s: &str) -> Result<IpAddr> {
    s.trim()
        .parse()
        .map_err(|_| Error::config(format!("raft.json: {field} must be an IP address, got '{}'", util::log_safe(s))))
}

fn parse_ips(field: &str, items: &[String]) -> Result<Vec<IpAddr>> {
    let mut out: Vec<IpAddr> = Vec::new();
    for s in items {
        let ip = parse_ip(field, s)?;
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    Ok(out)
}

/// The shared secret that authenticates peers. Every node of a cluster must have the same one
/// (`CARDINAL_CLUSTER_SECRET`, `CARDINAL_CLUSTER_SECRET_FILE` — what Docker/Swarm/Kubernetes
/// secrets are mounted as — or `config/cluster.secret`). It is never generated implicitly:
/// a node that invents its own secret could not talk to its peers, and a cluster port
/// without authentication would let anyone on the network rewrite shared state.
fn load_secret(file: &Path, env: &impl Fn(&str) -> Option<String>) -> Result<String> {
    let secret = env("CARDINAL_CLUSTER_SECRET")
        .or_else(|| env("CARDINAL_CLUSTER_SECRET_FILE").and_then(|f| std::fs::read_to_string(f).ok()))
        .or_else(|| std::fs::read_to_string(file).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if secret.len() < 16 {
        return Err(Error::config(format!(
            "clustering needs a shared secret of at least 16 characters, identical on every node. \
             Generate one with `open-cardinal raft keygen`, then provide it as CARDINAL_CLUSTER_SECRET \
             or in {}",
            file.display()
        )));
    }
    Ok(secret)
}

pub fn generate_secret() -> String {
    util::random_hex(32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(json: Option<&str>, env: &[(&str, &str)]) -> Result<Option<RaftSettings>> {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        if let Some(j) = json {
            std::fs::write(paths.raft_file(), j).unwrap();
        }
        let env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        RaftSettings::load_with(&paths, move |k| env.get(k).cloned())
    }

    const SECRET: (&str, &str) = ("CARDINAL_CLUSTER_SECRET", "0123456789abcdef0123456789abcdef");

    #[test]
    fn no_file_means_single_node_mode() {
        assert!(load(None, &[]).unwrap().is_none());
    }

    #[test]
    fn minimal_file_is_just_ips() {
        let s = load(Some(r#"{"peers":["10.0.0.1","10.0.0.2","fd00::3"]}"#), &[SECRET]).unwrap().unwrap();
        assert_eq!(s.peers.len(), 3);
        assert_eq!(s.port, DEFAULT_RAFT_PORT);
        assert_eq!(s.cluster_id, DEFAULT_CLUSTER_ID);
        assert!(s.self_ip.is_none());
    }

    #[test]
    fn ports_and_hostnames_are_rejected_with_a_clear_message() {
        for bad in ["10.0.0.1:50052", "node-1.local", "http://10.0.0.1", "10.0.0.256", "0.0.0.0"] {
            let e = load(Some(&format!(r#"{{"peers":["{bad}"]}}"#)), &[SECRET]).unwrap_err();
            assert!(e.to_string().contains("peers"), "{bad}: {e}");
        }
    }

    #[test]
    fn duplicate_peers_collapse() {
        let s = load(Some(r#"{"peers":["10.0.0.1","10.0.0.1"]}"#), &[SECRET]).unwrap().unwrap();
        assert_eq!(s.peers.len(), 1);
    }

    #[test]
    fn a_secret_is_mandatory() {
        let e = load(Some(r#"{"peers":["10.0.0.1"]}"#), &[]).unwrap_err();
        assert!(e.to_string().contains("shared secret"), "{e}");
        let e = load(Some(r#"{"peers":["10.0.0.1"]}"#), &[("CARDINAL_CLUSTER_SECRET", "short")]).unwrap_err();
        assert!(e.to_string().contains("16 characters"));
    }

    #[test]
    fn environment_alone_can_configure_a_cluster() {
        let s = load(None, &[("CARDINAL_RAFT_PEERS", "10.0.0.1, 10.0.0.2"), ("CARDINAL_RAFT_PORT", "7000"), SECRET])
            .unwrap()
            .unwrap();
        assert_eq!(s.peers.len(), 2);
        assert_eq!(s.port, 7000);
    }

    #[test]
    fn unknown_keys_and_bad_timings_are_rejected() {
        assert!(load(Some(r#"{"peers":["10.0.0.1"],"nodes":[]}"#), &[SECRET]).is_err());
        assert!(
            load(Some(r#"{"peers":["10.0.0.1"],"heartbeat_ms":500,"election_timeout_ms":600}"#), &[SECRET]).is_err()
        );
    }

    #[test]
    fn disabled_flag_turns_clustering_off() {
        assert!(load(Some(r#"{"enabled":false,"peers":["10.0.0.1"]}"#), &[SECRET]).unwrap().is_none());
    }

    #[test]
    fn core_config_uses_ticks() {
        let s = load(Some(r#"{"peers":["10.0.0.1"],"heartbeat_ms":100,"election_timeout_ms":500}"#), &[SECRET])
            .unwrap()
            .unwrap();
        let c = s.core_config(1);
        assert_eq!(c.heartbeat_ticks, 2);
        assert_eq!(c.election_ticks, 10);
    }
}
