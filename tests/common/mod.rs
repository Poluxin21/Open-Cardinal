//! Shared helpers for the process-level integration tests: every test starts the real
//! `open-cardinal` binary in a temporary home directory and talks to it over the network,
//! exactly like an operator and an agent would.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use open_cardinal::control::client::{Target, send};
use open_cardinal::control::protocol::{CliRequest, CliResponse};
use open_cardinal::pb::core::sentinel_client::SentinelClient;
use open_cardinal::pb::core::{Pulse, Reaction};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::metadata::MetadataValue;

pub const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

/// A port that is free on 127.0.0.1 (and, in practice, on the other loopback addresses).
pub fn free_port() -> u16 {
    StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
}

pub struct Daemon {
    child: Option<Child>,
    pub home: PathBuf,
    _tmp: Option<tempfile::TempDir>,
    pub ip: Ipv4Addr,
    pub grpc: u16,
    pub http: u16,
    pub control: SocketAddr,
    pub token: String,
    pub env: Vec<(String, String)>,
}

pub struct Spec {
    pub ip: Ipv4Addr,
    pub grpc: u16,
    pub http: u16,
    pub control_port: u16,
    pub config: Value,
    pub files: Vec<(String, Vec<u8>)>,
    pub env: Vec<(String, String)>,
}

impl Spec {
    pub fn single() -> Self {
        Self {
            ip: Ipv4Addr::LOCALHOST,
            grpc: free_port(),
            http: free_port(),
            control_port: free_port(),
            config: json!({}),
            files: vec![],
            env: vec![],
        }
    }

    pub fn config(mut self, extra: Value) -> Self {
        if let (Some(base), Some(add)) = (self.config.as_object_mut(), extra.as_object()) {
            for (k, v) in add {
                base.insert(k.clone(), v.clone());
            }
        }
        self
    }

    pub fn file(mut self, path: &str, content: impl AsRef<[u8]>) -> Self {
        self.files.push((path.to_string(), content.as_ref().to_vec()));
        self
    }

    pub fn with_files(mut self, files: &[(String, Vec<u8>)]) -> Self {
        self.files.extend(files.iter().cloned());
        self
    }

    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
}

pub const DEFAULT_RULE: &str = r#"
local combustivel = tonumber(pulse.telemetry["fuel"]) or 0
local status = pulse.telemetry["status"]

if combustivel < 70 then
   return {
        action = "SHUTDOWN",
        cmd_name = "EMERGENCY_CUTOFF",
        priority = 1000,
        params = { ["reason"] = "Overheating" }
    }
end
"#;

/// The strikes rule from the wiki (persistent state through `redb_api`).
pub const STRIKES_RULE: &str = r#"
local temp = tonumber(pulse.telemetry["cpu_temp"]) or 0
local key = pulse.agent_id .. "_overheat_strikes"
local strikes = tonumber(redb_api.get(key)) or 0
if temp > 90 then
    strikes = strikes + 1
    redb_api.set(key, strikes)
    if strikes >= 3 then
        redb_api.set(key, 0)
        return { action = "SHUTDOWN", cmd_name = "PERSISTENT_OVERHEAT", priority = 1000, params = { ["msg"] = "hot" } }
    end
else
    if strikes > 0 then redb_api.set(key, 0) end
end
return { action = "IDLE", priority = 0 }
"#;

impl Daemon {
    pub async fn start(spec: Spec) -> Daemon {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        Self::start_in(spec, home, Some(tmp)).await
    }

    /// Start in an existing directory (restart tests keep the data).
    pub async fn start_in(spec: Spec, home: PathBuf, tmp: Option<tempfile::TempDir>) -> Daemon {
        std::fs::create_dir_all(home.join("config")).unwrap();
        let mut config = spec.config.clone();
        let obj = config.as_object_mut().unwrap();
        obj.entry("grpc_port").or_insert(json!(spec.grpc));
        obj.entry("http_port").or_insert(json!(spec.http));
        obj.entry("bind").or_insert(json!([spec.ip.to_string()]));
        obj.entry("control_addr").or_insert(json!(format!("{}:{}", spec.ip, spec.control_port)));
        obj.entry("export_info_files").or_insert(json!(false));
        std::fs::write(home.join("config/config.json"), serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        for (path, content) in &spec.files {
            let p = home.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }

        let out = std::fs::File::create(home.join("daemon.out")).unwrap();
        let err = std::fs::File::create(home.join("daemon.err")).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_open-cardinal"));
        cmd.current_dir(&home).stdout(out).stderr(err).env_remove("RUST_LOG").env("CARDINAL_RUNTIME", "daemon");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("cannot start open-cardinal");

        let control: SocketAddr = format!("{}:{}", spec.ip, spec.control_port).parse().unwrap();
        let mut d = Daemon {
            child: Some(child),
            home,
            _tmp: tmp,
            ip: spec.ip,
            grpc: spec.grpc,
            http: spec.http,
            control,
            token: String::new(),
            env: spec.env,
        };
        d.wait_ready().await;
        d
    }

    async fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(child) = self.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                let err = std::fs::read_to_string(self.home.join("daemon.err")).unwrap_or_default();
                let out = std::fs::read_to_string(self.home.join("daemon.out")).unwrap_or_default();
                panic!("daemon exited early with {status}\nstdout:\n{out}\nstderr:\n{err}");
            }
            if let Ok(t) = std::fs::read_to_string(self.home.join("config/admin.token")) {
                self.token = t.trim().to_string();
                if self.try_ctl(CliRequest::Status).await.is_ok() {
                    return;
                }
            }
            if Instant::now() > deadline {
                let err = std::fs::read_to_string(self.home.join("daemon.err")).unwrap_or_default();
                panic!("daemon did not become ready in time\nstderr:\n{err}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn target(&self) -> Target {
        Target { addr: self.control, token: self.token.clone() }
    }

    pub async fn try_ctl(&self, req: CliRequest) -> Result<CliResponse, String> {
        send(&self.target(), req).await
    }

    pub async fn ctl(&self, req: CliRequest) -> CliResponse {
        self.try_ctl(req).await.expect("control request failed")
    }

    /// Run a CLI command and return its JSON payload (panics on error responses).
    pub async fn data(&self, req: CliRequest) -> Value {
        match self.ctl(req).await {
            CliResponse::Data { payload } => payload,
            other => panic!("expected data, got {other:?}"),
        }
    }

    pub async fn ok(&self, req: CliRequest) -> String {
        match self.ctl(req).await {
            CliResponse::Ok { message } => message,
            other => panic!("expected ok, got {other:?}"),
        }
    }

    pub async fn err(&self, req: CliRequest) -> String {
        match self.ctl(req).await {
            CliResponse::Error { message } => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    pub fn heathcliff(command: &str, args: &[&str]) -> CliRequest {
        CliRequest::Heathcliff { command: command.into(), args: args.iter().map(|s| s.to_string()).collect() }
    }

    pub fn tenant(command: &str, args: &[&str]) -> CliRequest {
        CliRequest::Tenant { command: command.into(), args: args.iter().map(|s| s.to_string()).collect() }
    }

    pub async fn grpc_client(&self) -> SentinelClient<tonic::transport::Channel> {
        let ch = tonic::transport::Endpoint::from_shared(format!("http://{}:{}", self.ip, self.grpc))
            .unwrap()
            .connect_timeout(Duration::from_secs(5))
            .connect()
            .await
            .expect("cannot connect to the gRPC port");
        SentinelClient::new(ch)
    }

    /// One pulse; `key` is sent as a bearer token when given.
    pub async fn ping(
        &self,
        agent: &str,
        telemetry: &[(&str, &str)],
        key: Option<&str>,
    ) -> Result<Reaction, tonic::Status> {
        let mut client = self.grpc_client().await;
        ping_with(&mut client, agent, telemetry, key).await
    }

    pub async fn http_get(&self, path: &str, bearer: Option<&str>) -> (u16, HashMap<String, String>, String) {
        http_get(SocketAddr::new(self.ip.into(), self.http), path, bearer).await
    }

    /// Hard kill (like `kill -9` / a crash).
    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Ask the daemon to stop and wait for the process to exit.
    pub async fn stop(&mut self) {
        let _ = self.try_ctl(CliRequest::Stop).await;
        if let Some(mut c) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match c.try_wait() {
                    Ok(Some(_)) => break,
                    _ if Instant::now() > deadline => {
                        let _ = c.kill();
                        let _ = c.wait();
                        break;
                    }
                    _ => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    pub fn log(&self) -> String {
        let mut out = String::new();
        if let Ok(rd) = std::fs::read_dir(self.home.join("logs")) {
            for e in rd.flatten() {
                out.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
            }
        }
        out
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    /// Detach the temp dir so it outlives the daemon (restart tests).
    pub fn into_home(mut self) -> (PathBuf, Option<tempfile::TempDir>) {
        self.kill();
        (self.home.clone(), self._tmp.take())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

pub async fn ping_with(
    client: &mut SentinelClient<tonic::transport::Channel>,
    agent: &str,
    telemetry: &[(&str, &str)],
    key: Option<&str>,
) -> Result<Reaction, tonic::Status> {
    let pulse = Pulse {
        agent_id: agent.to_string(),
        timestamp: 0,
        telemetry: telemetry.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
    };
    let mut req = tonic::Request::new(pulse);
    if let Some(k) = key {
        req.metadata_mut().insert("authorization", MetadataValue::try_from(format!("Bearer {k}")).unwrap());
    }
    client.sync(req).await.map(|r| r.into_inner())
}

/// Minimal HTTP/1.1 GET (no external client crate needed).
pub async fn http_get(addr: SocketAddr, path: &str, bearer: Option<&str>) -> (u16, HashMap<String, String>, String) {
    let mut s = TcpStream::connect(addr).await.expect("http connect");
    let auth = bearer.map(|b| format!("Authorization: Bearer {b}\r\n")).unwrap_or_default();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    // chunked bodies are not used by our handlers (axum sets content-length), keep as is
    (status, headers, body.to_string())
}

/// Send raw bytes to the control port and return what comes back (the PoC attacker's view).
pub async fn raw_control(addr: SocketAddr, payload: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(payload).await.unwrap();
    s.shutdown().await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).into_owned()
}

pub async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if f().await {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn assert_path_missing(p: &Path) {
    assert!(!p.exists(), "{} must not exist", p.display());
}
