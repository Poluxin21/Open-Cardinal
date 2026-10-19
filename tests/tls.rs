//! TLS / mTLS on the agent API, the HTTP service and (see `cluster.rs`) the Raft transport.
#![cfg(feature = "tls")]

mod common;

use std::sync::Arc;

use common::*;
use open_cardinal::pb::core::Pulse;
use open_cardinal::pb::core::sentinel_client::SentinelClient;
use open_cardinal::tls::{issue, new_ca};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

struct Pki {
    ca_pem: String,
    ca_key: String,
}

impl Pki {
    fn new() -> Self {
        let ca = new_ca("Test CA", 30).unwrap();
        Pki { ca_pem: ca.cert_pem, ca_key: ca.key_pem }
    }

    fn cert(&self, name: &str) -> (String, String) {
        let c =
            issue(&self.ca_pem, &self.ca_key, name, &["127.0.0.1".parse().unwrap()], &["localhost".to_string()], 30)
                .unwrap();
        (c.cert_pem, c.key_pem)
    }
}

fn pulse(agent: &str) -> Pulse {
    Pulse { agent_id: agent.into(), timestamp: 0, telemetry: [("fuel".to_string(), "10".to_string())].into() }
}

async fn tls_client(
    addr: &str,
    ca: &str,
    identity: Option<(&str, &str)>,
) -> Result<SentinelClient<tonic::transport::Channel>, tonic::transport::Error> {
    let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca)).domain_name("localhost");
    if let Some((c, k)) = identity {
        tls = tls.identity(Identity::from_pem(c, k));
    }
    let ch = Endpoint::from_shared(format!("https://{addr}")).unwrap().tls_config(tls).unwrap().connect().await?;
    Ok(SentinelClient::new(ch))
}

/// GET over TLS without an HTTP client crate.
async fn https_get(addr: std::net::SocketAddr, ca_pem: &str, path: &str) -> Result<String, String> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for c in rustls_pemfile_certs(ca_pem) {
        roots.add(c).unwrap();
    }
    let cfg = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut s = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut out = String::new();
    s.read_to_string(&mut out).await.map_err(|e| e.to_string())?;
    Ok(out)
}

fn rustls_pemfile_certs(pem: &str) -> Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>> {
    rustls_pemfile::certs(&mut pem.as_bytes()).collect::<Result<_, _>>().unwrap()
}

fn tls_spec(pki: &Pki, mtls: bool) -> Spec {
    let (cert, key) = pki.cert("server");
    let mut tls = json!({ "cert_file": "certs/server.pem", "key_file": "certs/server.key" });
    if mtls {
        tls["client_ca_file"] = json!("certs/ca.pem");
    }
    Spec::single()
        .config(json!({ "security": { "tls": tls } }))
        .file("certs/server.pem", cert)
        .file("certs/server.key", key)
        .file("certs/ca.pem", pki.ca_pem.clone())
        .file("rules/default/default.lua", DEFAULT_RULE)
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_api_and_http_speak_tls() {
    let pki = Pki::new();
    let d = Daemon::start(tls_spec(&pki, false)).await;
    let addr = format!("{}:{}", d.ip, d.grpc);

    // a client that trusts the CA gets decisions
    let mut c = tls_client(&addr, &pki.ca_pem, None).await.expect("TLS handshake");
    let r = c.sync(pulse("Rocket")).await.unwrap().into_inner();
    assert_eq!(r.command_name, "EMERGENCY_CUTOFF");

    // plaintext is refused, a client trusting another CA is refused
    let plain = SentinelClient::connect(format!("http://{addr}")).await;
    let refused = match plain {
        Err(_) => true,
        Ok(mut c) => c.sync(pulse("x")).await.is_err(),
    };
    assert!(refused, "the TLS port must not serve plaintext gRPC");
    let other = Pki::new();
    assert!(
        tls_client(&addr, &other.ca_pem, None).await.is_err()
            || tls_client(&addr, &other.ca_pem, None).await.unwrap().sync(pulse("x")).await.is_err()
    );

    // the HTTP service uses the same certificate
    let http = std::net::SocketAddr::new(d.ip.into(), d.http);
    let resp = https_get(http, &pki.ca_pem, "/healthz").await.expect("https");
    assert!(resp.starts_with("HTTP/1.1 200") && resp.ends_with("ok"), "{resp}");
    assert!(https_get(http, &other.ca_pem, "/healthz").await.is_err(), "an untrusted CA must fail verification");
}

#[tokio::test(flavor = "multi_thread")]
async fn mutual_tls_requires_a_client_certificate_from_the_trusted_ca() {
    let pki = Pki::new();
    let d = Daemon::start(tls_spec(&pki, true)).await;
    let addr = format!("{}:{}", d.ip, d.grpc);
    let (ccert, ckey) = pki.cert("agent-1");

    let mut ok = tls_client(&addr, &pki.ca_pem, Some((&ccert, &ckey))).await.expect("mTLS handshake");
    assert_eq!(ok.sync(pulse("Rocket")).await.unwrap().into_inner().r#type, 1);

    // no certificate
    let denied = match tls_client(&addr, &pki.ca_pem, None).await {
        Err(_) => true,
        Ok(mut c) => c.sync(pulse("x")).await.is_err(),
    };
    assert!(denied, "a client without a certificate must be refused");

    // a certificate from a different CA
    let rogue = Pki::new();
    let (rcert, rkey) = rogue.cert("agent-1");
    let denied = match tls_client(&addr, &pki.ca_pem, Some((&rcert, &rkey))).await {
        Err(_) => true,
        Ok(mut c) => c.sync(pulse("x")).await.is_err(),
    };
    assert!(denied, "a certificate signed by another CA must be refused");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tls_section_with_unreadable_files_fails_loudly_instead_of_serving_plaintext() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    std::fs::write(
        tmp.path().join("config/config.json"),
        r#"{"security":{"tls":{"cert_file":"nope.pem","key_file":"nope.key"}}}"#,
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_open-cardinal"))
        .current_dir(tmp.path())
        .env("CARDINAL_RUNTIME", "daemon")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("tls cert_file"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn certificate_generator_cli_creates_a_usable_chain() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_str().unwrap().to_string();
    let run =
        |args: &[&str]| std::process::Command::new(env!("CARGO_BIN_EXE_open-cardinal")).args(args).output().unwrap();
    let out = run(&["tls", "init-ca", "--dir", &d]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // refuses to overwrite a CA by accident
    assert!(!run(&["tls", "init-ca", "--dir", &d]).status.success());
    let out =
        run(&["tls", "issue", "--dir", &d, "--name", "node1", "--ip", "10.0.0.11", "--dns", "cardinal1.example.com"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for f in ["ca.pem", "ca.key", "node1.pem", "node1.key"] {
        assert!(dir.path().join(f).exists(), "{f}");
    }
    // bad input is rejected
    assert!(!run(&["tls", "issue", "--dir", &d, "--name", "bad name"]).status.success());
    assert!(!run(&["tls", "issue", "--dir", &d, "--name", "n2", "--ip", "not-an-ip"]).status.success());
    assert!(!run(&["tls", "issue", "--dir", "/nonexistent-dir", "--name", "n3"]).status.success());
}
