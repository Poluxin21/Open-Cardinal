//! gRPC transport: protobuf conversions, the outgoing peer clients and the incoming service.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::oneshot;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status, Streaming};

use super::auth::{ClientAuth, ClusterAuth, PeerId};
use super::node::{Cmd, NodeShared, ProposeFailure};
use super::types::*;
use crate::pb::raft as pb;
use crate::pb::raft::raft_client::RaftClient;
use crate::pb::raft::raft_server::Raft;
use crate::store::{Command, Outcome, StateDump};

pub const SNAPSHOT_CHUNK_BYTES: usize = 256 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound for a reassembled snapshot (JSON of the whole state machine).
const MAX_SNAPSHOT_BYTES: usize = 512 * 1024 * 1024;

type Client = RaftClient<tonic::service::interceptor::InterceptedService<Channel, ClientAuth>>;

// ---- conversions ------------------------------------------------------------------------

pub fn encode_entry(e: &Entry) -> pb::LogEntry {
    pb::LogEntry { index: e.index, term: e.term, payload: serde_json::to_vec(&e.payload).unwrap_or_default() }
}

pub fn decode_entry(e: pb::LogEntry) -> Result<Entry, Status> {
    let payload = serde_json::from_slice(&e.payload).map_err(|_| Status::invalid_argument("bad entry payload"))?;
    Ok(Entry { index: e.index, term: e.term, payload })
}

fn parse_node(s: &str) -> Result<NodeId, Status> {
    s.parse().map_err(|_| Status::invalid_argument("bad node address"))
}

// ---- outgoing ---------------------------------------------------------------------------

pub struct Peers {
    me: NodeId,
    cluster_id: String,
    port: u16,
    auth: ClusterAuth,
    rpc_timeout: Duration,
    clients: Mutex<HashMap<NodeId, Client>>,
    #[cfg(feature = "tls")]
    tls: Option<tonic::transport::ClientTlsConfig>,
}

/// Failure modes the caller cares about.
#[derive(Debug)]
pub enum CallError {
    /// Could not reach the peer, or it did not answer in time.
    Unreachable(String),
    /// The peer answered with an application-level refusal.
    Refused(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unreachable(m) | CallError::Refused(m) => f.write_str(m),
        }
    }
}

fn call_err(s: Status) -> CallError {
    match s.code() {
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Cancelled | tonic::Code::Unknown => {
            CallError::Unreachable(format!("{}: {}", s.code(), s.message()))
        }
        _ => CallError::Refused(format!("{}: {}", s.code(), s.message())),
    }
}

impl Peers {
    pub fn new(me: NodeId, cluster_id: &str, port: u16, secret: &str, rpc_timeout: Duration) -> Arc<Self> {
        Self::new_tls(me, cluster_id, port, secret, rpc_timeout, None)
    }

    /// With `tls`, every connection uses mutual TLS (the node's certificate, verified against the cluster CA).
    pub fn new_tls(
        me: NodeId,
        cluster_id: &str,
        port: u16,
        secret: &str,
        rpc_timeout: Duration,
        tls: Option<&super::config::RaftTls>,
    ) -> Arc<Self> {
        #[cfg(not(feature = "tls"))]
        let _ = tls;
        Arc::new(Self {
            me,
            cluster_id: cluster_id.to_string(),
            port,
            auth: ClusterAuth::new(secret),
            rpc_timeout,
            clients: Mutex::new(HashMap::new()),
            #[cfg(feature = "tls")]
            tls: tls.map(|t| crate::tls::tonic_cluster_client(&t.material, &t.ca_pem)),
        })
    }

    pub fn me(&self) -> NodeId {
        self.me
    }

    fn client(&self, to: NodeId) -> Client {
        let mut map = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(to)
            .or_insert_with(|| {
                let addr = SocketAddr::new(to, self.port);
                #[cfg(feature = "tls")]
                let scheme = if self.tls.is_some() { "https" } else { "http" };
                #[cfg(not(feature = "tls"))]
                let scheme = "http";
                let endpoint = Endpoint::from_shared(format!("{scheme}://{addr}"))
                    .expect("socket address forms a valid URI")
                    .connect_timeout(Duration::from_millis(800))
                    .timeout(self.rpc_timeout)
                    .tcp_nodelay(true)
                    .http2_keep_alive_interval(Duration::from_secs(10))
                    .keep_alive_while_idle(true);
                #[cfg(feature = "tls")]
                let endpoint = match &self.tls {
                    Some(tls) => endpoint.tls_config(tls.clone()).expect("static TLS configuration is valid"),
                    None => endpoint,
                };
                let channel = endpoint.connect_lazy();
                RaftClient::with_interceptor(channel, self.auth.client_interceptor(self.me))
                    .max_decoding_message_size(MAX_MESSAGE_BYTES)
                    .max_encoding_message_size(MAX_MESSAGE_BYTES)
            })
            .clone()
    }

    /// Deliver a protocol message and return the peer's answer, if the RPC has one.
    pub async fn send(&self, to: NodeId, msg: Message) -> Result<Option<Message>, CallError> {
        let mut c = self.client(to);
        let cid = self.cluster_id.clone();
        let from = self.me.to_string();
        match msg {
            Message::PreVote(r) => {
                let reply = c.pre_vote(vote_req(&cid, &from, &r)).await.map_err(call_err)?.into_inner();
                Ok(Some(Message::PreVoteResp(VoteResp { term: reply.term, granted: reply.granted })))
            }
            Message::Vote(r) => {
                let reply = c.request_vote(vote_req(&cid, &from, &r)).await.map_err(call_err)?.into_inner();
                Ok(Some(Message::VoteResp(VoteResp { term: reply.term, granted: reply.granted })))
            }
            Message::Append(r) => {
                let req = pb::AppendRequest {
                    cluster_id: cid,
                    from,
                    term: r.term,
                    prev_index: r.prev_index,
                    prev_term: r.prev_term,
                    entries: r.entries.iter().map(encode_entry).collect(),
                    commit: r.commit,
                };
                let reply = c.append_entries(req).await.map_err(call_err)?.into_inner();
                Ok(Some(Message::AppendResp(AppendResp {
                    term: reply.term,
                    success: reply.success,
                    match_index: reply.match_index,
                    hint_index: reply.hint_index,
                })))
            }
            Message::TimeoutNow { term } => {
                let reply = c
                    .timeout_now(pb::TimeoutNowRequest { cluster_id: cid, from, term })
                    .await
                    .map_err(call_err)?
                    .into_inner();
                let _ = reply;
                Ok(None)
            }
            // responses and snapshots do not travel as plain messages
            _ => Ok(None),
        }
    }

    pub async fn send_snapshot(&self, to: NodeId, meta: &SnapshotMeta, dump: &StateDump) -> Result<Message, CallError> {
        let data = serde_json::to_vec(dump).map_err(|e| CallError::Refused(e.to_string()))?;
        let config = serde_json::to_vec(&meta.config).map_err(|e| CallError::Refused(e.to_string()))?;
        let mut chunks = Vec::new();
        let mut first = true;
        let pieces: Vec<&[u8]> =
            if data.is_empty() { vec![&[][..]] } else { data.chunks(SNAPSHOT_CHUNK_BYTES).collect() };
        let n = pieces.len();
        for (i, piece) in pieces.into_iter().enumerate() {
            chunks.push(pb::SnapshotChunk {
                cluster_id: if first { self.cluster_id.clone() } else { String::new() },
                from: if first { self.me.to_string() } else { String::new() },
                term: if first { meta.term } else { 0 },
                last_index: if first { meta.last_index } else { 0 },
                last_term: if first { meta.last_term } else { 0 },
                config: if first { config.clone() } else { Vec::new() },
                data: piece.to_vec(),
                done: i + 1 == n,
            });
            first = false;
        }
        let mut c = self.client(to);
        // large transfers get a longer budget than ordinary RPCs
        let mut req = Request::new(tokio_stream::iter(chunks));
        req.set_timeout(self.rpc_timeout.max(Duration::from_secs(60)));
        let reply = c.install_snapshot(req).await.map_err(call_err)?.into_inner();
        Ok(Message::SnapshotResp(SnapshotResp {
            term: reply.term,
            success: reply.success,
            last_index: reply.last_index,
        }))
    }

    pub async fn hello(&self, to: NodeId) -> Result<pb::HelloReply, CallError> {
        let req = pb::HelloRequest { cluster_id: self.cluster_id.clone(), from_ip: self.me.to_string() };
        let mut c = self.client(to);
        let mut r = Request::new(req);
        r.set_timeout(Duration::from_millis(1500));
        Ok(c.hello(r).await.map_err(call_err)?.into_inner())
    }

    pub async fn propose(&self, to: NodeId, command: &Command) -> Result<Outcome, ProposeFailure> {
        let req = pb::ProposeRequest {
            cluster_id: self.cluster_id.clone(),
            from: self.me.to_string(),
            command: serde_json::to_vec(command).map_err(|e| ProposeFailure::Rejected(e.to_string()))?,
        };
        let mut c = self.client(to);
        let reply = match c.propose(req).await {
            Ok(r) => r.into_inner(),
            // we do not know whether the leader appended it before the connection broke
            Err(s) => return Err(ProposeFailure::Indeterminate(format!("forwarding to {to}: {}", s.message()))),
        };
        if reply.ok {
            return serde_json::from_slice::<OutcomeWire>(&reply.outcome)
                .map(Into::into)
                .map_err(|e| ProposeFailure::Rejected(format!("bad outcome: {e}")));
        }
        Err(match reply.error.as_str() {
            "not_leader" => ProposeFailure::NotLeader(reply.leader_hint.parse().ok()),
            "indeterminate" => ProposeFailure::Indeterminate("leader could not confirm the write".into()),
            other => ProposeFailure::Rejected(other.to_string()),
        })
    }

    pub async fn join(&self, leader: NodeId, ip: NodeId) -> Result<(), CallError> {
        let mut c = self.client(leader);
        let reply = c
            .join(pb::JoinRequest { cluster_id: self.cluster_id.clone(), ip: ip.to_string() })
            .await
            .map_err(call_err)?
            .into_inner();
        if reply.ok { Ok(()) } else { Err(CallError::Refused(reply.error)) }
    }

    /// Ask the leader for a safe read index. The inner `Result` is the leader's own verdict.
    pub async fn read_index(&self, leader: NodeId) -> Result<Result<Index, ReadError>, CallError> {
        let mut c = self.client(leader);
        let mut r = Request::new(pb::ReadIndexRequest { cluster_id: self.cluster_id.clone() });
        r.set_timeout(Duration::from_millis(1500));
        let reply = c.read_index(r).await.map_err(call_err)?.into_inner();
        Ok(if reply.ok {
            Ok(reply.index)
        } else {
            Err(match reply.error.as_str() {
                "not_leader" => ReadError::NotLeader(reply.leader_hint.parse().ok()),
                "not_ready" => ReadError::NotReady,
                _ => ReadError::LeaseExpired,
            })
        })
    }

    pub async fn admin(&self, to: NodeId, op: &str, ip: Option<NodeId>) -> Result<pb::AdminReply, CallError> {
        let mut c = self.client(to);
        let mut r = Request::new(pb::AdminRequest {
            cluster_id: self.cluster_id.clone(),
            op: op.into(),
            ip: ip.map(|i| i.to_string()).unwrap_or_default(),
        });
        r.set_timeout(Duration::from_secs(10));
        Ok(c.admin(r).await.map_err(call_err)?.into_inner())
    }
}

fn vote_req(cluster_id: &str, from: &str, r: &VoteReq) -> pb::VoteRequest {
    pb::VoteRequest {
        cluster_id: cluster_id.to_string(),
        from: from.to_string(),
        term: r.term,
        last_index: r.last_index,
        last_term: r.last_term,
        transfer: r.transfer,
    }
}

/// `Outcome` on the wire.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum OutcomeWire {
    None,
    Value(u64),
    Deleted(bool),
}

impl From<Outcome> for OutcomeWire {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::None => OutcomeWire::None,
            Outcome::Value(v) => OutcomeWire::Value(v),
            Outcome::Deleted(b) => OutcomeWire::Deleted(b),
        }
    }
}

impl From<OutcomeWire> for Outcome {
    fn from(o: OutcomeWire) -> Self {
        match o {
            OutcomeWire::None => Outcome::None,
            OutcomeWire::Value(v) => Outcome::Value(v),
            OutcomeWire::Deleted(b) => Outcome::Deleted(b),
        }
    }
}

// ---- incoming ---------------------------------------------------------------------------

/// Answers `Hello` from the moment the server is up (needed to detect "I dialled myself"
/// before the node itself is ready); everything else waits for the node.
pub struct RpcState {
    pub cluster_id: String,
    pub instance_id: String,
    pub node: OnceLock<Arc<NodeShared>>,
    /// IP this node believes it has, once known.
    pub self_ip: OnceLock<NodeId>,
}

pub struct RaftRpc {
    pub state: Arc<RpcState>,
}

impl RaftRpc {
    fn node(&self) -> Result<&Arc<NodeShared>, Status> {
        self.state.node.get().ok_or_else(|| Status::unavailable("node is starting"))
    }

    fn check_cluster(&self, id: &str) -> Result<(), Status> {
        if id != self.state.cluster_id {
            return Err(Status::failed_precondition(format!(
                "cluster id mismatch: expected '{}'",
                self.state.cluster_id
            )));
        }
        Ok(())
    }

    fn peer<T>(req: &Request<T>) -> Result<NodeId, Status> {
        req.extensions().get::<PeerId>().map(|p| p.0).ok_or_else(|| Status::unauthenticated("no peer identity"))
    }

    /// Hand a request to the driver and wait for the response message it produced.
    async fn call(&self, from: NodeId, msg: Message) -> Result<Message, Status> {
        let node = self.node()?;
        let (tx, rx) = oneshot::channel();
        node.tx
            .send(Cmd::Step { from, msg, reply: Some(tx) })
            .await
            .map_err(|_| Status::unavailable("node is shutting down"))?;
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .map_err(|_| Status::deadline_exceeded("driver busy"))?
            .map_err(|_| Status::unavailable("node is shutting down"))
    }
}

#[tonic::async_trait]
impl Raft for RaftRpc {
    async fn hello(&self, request: Request<pb::HelloRequest>) -> Result<Response<pb::HelloReply>, Status> {
        let observed = request.remote_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let req = request.into_inner();
        self.check_cluster(&req.cluster_id)?;
        let mut reply = pb::HelloReply {
            cluster_id: self.state.cluster_id.clone(),
            instance_id: self.state.instance_id.clone(),
            node_ip: self.state.self_ip.get().map(|i| i.to_string()).unwrap_or_default(),
            observed_ip: observed,
            role: "starting".into(),
            ..Default::default()
        };
        if let Some(node) = self.state.node.get() {
            let st = node.status.borrow().clone();
            reply.term = st.term;
            reply.leader = st.leader.map(|l| l.to_string()).unwrap_or_default();
            reply.commit_index = st.commit_index;
            reply.last_index = st.last_index;
            reply.voters = st.voters.iter().map(|v| v.to_string()).collect();
            reply.learners = st.learners.iter().map(|v| v.to_string()).collect();
            reply.role = st.role.to_string();
            reply.bootstrapped = st.last_index > 0 || st.term > 0;
        }
        Ok(Response::new(reply))
    }

    async fn pre_vote(&self, request: Request<pb::VoteRequest>) -> Result<Response<pb::VoteReply>, Status> {
        let from = Self::peer(&request)?;
        let r = request.into_inner();
        self.check_cluster(&r.cluster_id)?;
        let msg = Message::PreVote(VoteReq {
            term: r.term,
            last_index: r.last_index,
            last_term: r.last_term,
            transfer: r.transfer,
        });
        match self.call(from, msg).await? {
            Message::PreVoteResp(v) => Ok(Response::new(pb::VoteReply { term: v.term, granted: v.granted })),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn request_vote(&self, request: Request<pb::VoteRequest>) -> Result<Response<pb::VoteReply>, Status> {
        let from = Self::peer(&request)?;
        let r = request.into_inner();
        self.check_cluster(&r.cluster_id)?;
        let msg = Message::Vote(VoteReq {
            term: r.term,
            last_index: r.last_index,
            last_term: r.last_term,
            transfer: r.transfer,
        });
        match self.call(from, msg).await? {
            Message::VoteResp(v) => Ok(Response::new(pb::VoteReply { term: v.term, granted: v.granted })),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn append_entries(&self, request: Request<pb::AppendRequest>) -> Result<Response<pb::AppendReply>, Status> {
        let from = Self::peer(&request)?;
        let r = request.into_inner();
        self.check_cluster(&r.cluster_id)?;
        let entries = r.entries.into_iter().map(decode_entry).collect::<Result<Vec<_>, _>>()?;
        let msg = Message::Append(AppendReq {
            term: r.term,
            prev_index: r.prev_index,
            prev_term: r.prev_term,
            entries,
            commit: r.commit,
        });
        match self.call(from, msg).await? {
            Message::AppendResp(a) => Ok(Response::new(pb::AppendReply {
                term: a.term,
                success: a.success,
                match_index: a.match_index,
                hint_index: a.hint_index,
            })),
            _ => Err(Status::internal("unexpected reply")),
        }
    }

    async fn install_snapshot(
        &self,
        request: Request<Streaming<pb::SnapshotChunk>>,
    ) -> Result<Response<pb::SnapshotReply>, Status> {
        let from = Self::peer(&request)?;
        let mut stream = request.into_inner();
        let mut header: Option<pb::SnapshotChunk> = None;
        let mut data: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.message().await? {
            if header.is_none() {
                self.check_cluster(&chunk.cluster_id)?;
                header = Some(pb::SnapshotChunk { data: Vec::new(), ..chunk.clone() });
            }
            data.extend_from_slice(&chunk.data);
            if data.len() > MAX_SNAPSHOT_BYTES {
                return Err(Status::resource_exhausted("snapshot too large"));
            }
            if chunk.done {
                break;
            }
        }
        let h = header.ok_or_else(|| Status::invalid_argument("empty snapshot stream"))?;
        let config: ClusterConfig =
            serde_json::from_slice(&h.config).map_err(|_| Status::invalid_argument("bad snapshot config"))?;
        let dump: StateDump =
            serde_json::from_slice(&data).map_err(|_| Status::invalid_argument("bad snapshot data"))?;
        let meta = SnapshotMeta { term: h.term, last_index: h.last_index, last_term: h.last_term, config };

        let node = self.node()?;
        let (tx, rx) = oneshot::channel();
        node.tx
            .send(Cmd::InstallSnapshot { from, meta, dump, reply: tx })
            .await
            .map_err(|_| Status::unavailable("shutting down"))?;
        match tokio::time::timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(Message::SnapshotResp(r))) => {
                Ok(Response::new(pb::SnapshotReply { term: r.term, success: r.success, last_index: r.last_index }))
            }
            Ok(Ok(_)) => Err(Status::internal("unexpected reply")),
            _ => Err(Status::deadline_exceeded("snapshot install did not finish")),
        }
    }

    async fn timeout_now(
        &self,
        request: Request<pb::TimeoutNowRequest>,
    ) -> Result<Response<pb::TimeoutNowReply>, Status> {
        let from = Self::peer(&request)?;
        let r = request.into_inner();
        self.check_cluster(&r.cluster_id)?;
        let node = self.node()?;
        node.tx
            .send(Cmd::Step { from, msg: Message::TimeoutNow { term: r.term }, reply: None })
            .await
            .map_err(|_| Status::unavailable("shutting down"))?;
        Ok(Response::new(pb::TimeoutNowReply { term: r.term }))
    }

    async fn propose(&self, request: Request<pb::ProposeRequest>) -> Result<Response<pb::ProposeReply>, Status> {
        let _from = Self::peer(&request)?;
        let r = request.into_inner();
        self.check_cluster(&r.cluster_id)?;
        let command: Command =
            serde_json::from_slice(&r.command).map_err(|_| Status::invalid_argument("bad command"))?;
        let node = self.node()?;
        let (tx, rx) = oneshot::channel();
        node.tx.send(Cmd::Propose { command, reply: tx }).await.map_err(|_| Status::unavailable("shutting down"))?;
        let res = rx.await.map_err(|_| Status::unavailable("shutting down"))?;
        Ok(Response::new(match res {
            Ok(outcome) => pb::ProposeReply {
                ok: true,
                outcome: serde_json::to_vec(&OutcomeWire::from(outcome)).unwrap_or_default(),
                ..Default::default()
            },
            Err(ProposeFailure::NotLeader(hint)) => pb::ProposeReply {
                ok: false,
                error: "not_leader".into(),
                leader_hint: hint.map(|h| h.to_string()).unwrap_or_default(),
                ..Default::default()
            },
            Err(ProposeFailure::Indeterminate(_)) => {
                pb::ProposeReply { ok: false, error: "indeterminate".into(), ..Default::default() }
            }
            Err(ProposeFailure::Rejected(m)) => pb::ProposeReply { ok: false, error: m, ..Default::default() },
        }))
    }

    async fn join(&self, request: Request<pb::JoinRequest>) -> Result<Response<pb::JoinReply>, Status> {
        let from = Self::peer(&request)?;
        let r = request.into_inner();
        self.check_cluster(&r.cluster_id)?;
        let ip = parse_node(&r.ip)?;
        // a node may only ask to be added under its own authenticated address
        if ip != from {
            return Ok(Response::new(pb::JoinReply {
                ok: false,
                error: "a node can only join as itself".into(),
                ..Default::default()
            }));
        }
        let node = self.node()?;
        let (tx, rx) = oneshot::channel();
        node.tx
            .send(Cmd::ConfChange { change: ConfChange::AddLearner(ip), reply: tx })
            .await
            .map_err(|_| Status::unavailable("shutting down"))?;
        let res = rx.await.map_err(|_| Status::unavailable("shutting down"))?;
        Ok(Response::new(match res {
            Ok(()) => pb::JoinReply { ok: true, ..Default::default() },
            Err(ProposeFailure::NotLeader(h)) => pb::JoinReply {
                ok: false,
                error: "not_leader".into(),
                leader_hint: h.map(|h| h.to_string()).unwrap_or_default(),
            },
            Err(ProposeFailure::Rejected(m)) | Err(ProposeFailure::Indeterminate(m)) => {
                pb::JoinReply { ok: false, error: m, ..Default::default() }
            }
        }))
    }

    async fn read_index(&self, request: Request<pb::ReadIndexRequest>) -> Result<Response<pb::ReadIndexReply>, Status> {
        let _from = Self::peer(&request)?;
        self.check_cluster(&request.get_ref().cluster_id)?;
        let node = self.node()?;
        let (tx, rx) = oneshot::channel();
        node.tx.send(Cmd::ReadIndex { reply: tx }).await.map_err(|_| Status::unavailable("shutting down"))?;
        let res = rx.await.map_err(|_| Status::unavailable("shutting down"))?;
        Ok(Response::new(match res {
            Ok(index) => pb::ReadIndexReply { ok: true, index, ..Default::default() },
            Err(ReadError::NotLeader(h)) => pb::ReadIndexReply {
                ok: false,
                error: "not_leader".into(),
                leader_hint: h.map(|h| h.to_string()).unwrap_or_default(),
                ..Default::default()
            },
            Err(ReadError::NotReady) => {
                pb::ReadIndexReply { ok: false, error: "not_ready".into(), ..Default::default() }
            }
            Err(ReadError::LeaseExpired) => {
                pb::ReadIndexReply { ok: false, error: "lease_expired".into(), ..Default::default() }
            }
        }))
    }

    async fn admin(&self, request: Request<pb::AdminRequest>) -> Result<Response<pb::AdminReply>, Status> {
        let _from = Self::peer(&request)?;
        let r = request.into_inner();
        self.check_cluster(&r.cluster_id)?;
        let node = self.node()?;
        let reply =
            super::node::run_admin(node, &r.op, if r.ip.is_empty() { None } else { Some(parse_node(&r.ip)?) }).await;
        Ok(Response::new(reply))
    }
}

/// Count of distinct reachable members, for diagnostics.
pub fn summarize(replies: &BTreeMap<IpAddr, pb::HelloReply>) -> String {
    replies.iter().map(|(ip, r)| format!("{ip}={}", r.role)).collect::<Vec<_>>().join(", ")
}
