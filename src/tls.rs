//! TLS / mTLS: certificate loading for the agent API, the HTTP service and the Raft transport,
//! plus the certificate generator behind `open-cardinal tls ...`.
//!
//! Without the `tls` feature the daemon refuses to start if a TLS section is configured,
//! instead of silently serving plaintext.

use std::path::{Path, PathBuf};

use crate::config::TlsConfig;
use crate::error::{Error, Result};

/// DNS name every cluster certificate carries. Raft peers dial each other by IP, so they verify
/// the certificate against this fixed name instead of the address.
pub const CLUSTER_DNS_NAME: &str = "cardinal-raft";

/// PEM material read from disk.
#[derive(Clone)]
pub struct TlsMaterial {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// When present, clients must present a certificate signed by this CA (mTLS).
    pub client_ca_pem: Option<Vec<u8>>,
}

fn resolve(home: &Path, f: &str) -> PathBuf {
    let p = Path::new(f);
    if p.is_absolute() { p.to_path_buf() } else { home.join(p) }
}

pub fn load(cfg: &TlsConfig, home: &Path) -> Result<TlsMaterial> {
    let read = |what: &str, f: &str| {
        let path = resolve(home, f);
        std::fs::read(&path).map_err(|e| Error::config(format!("tls {what} {}: {e}", path.display())))
    };
    let m = TlsMaterial {
        cert_pem: read("cert_file", &cfg.cert_file)?,
        key_pem: read("key_file", &cfg.key_file)?,
        client_ca_pem: cfg.client_ca_file.as_deref().map(|f| read("client_ca_file", f)).transpose()?,
    };
    #[cfg(feature = "tls")]
    validate(&m)?;
    Ok(m)
}

/// Fail at configuration time when a TLS section is present but this build cannot honour it,
/// so the servers never have to second-guess a configured-but-unsupported TLS setting.
pub fn check_supported(configured: bool) -> Result<()> {
    if configured && !cfg!(feature = "tls") {
        return Err(Error::config(
            "a TLS section is configured but this build has no TLS support (rebuild with --features tls)",
        ));
    }
    Ok(())
}

#[cfg(feature = "tls")]
pub use enabled::*;

#[cfg(feature = "tls")]
mod enabled {
    use std::sync::Arc;

    use super::*;

    /// rustls needs a process-wide crypto provider; we ship `ring`.
    pub fn install_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    pub(super) fn validate(m: &TlsMaterial) -> Result<()> {
        let certs: Vec<_> = rustls_pemfile::certs(&mut m.cert_pem.as_slice())
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::config(format!("tls cert_file is not valid PEM: {e}")))?;
        if certs.is_empty() {
            return Err(Error::config("tls cert_file contains no certificate"));
        }
        match rustls_pemfile::private_key(&mut m.key_pem.as_slice()) {
            Ok(Some(_)) => {}
            _ => return Err(Error::config("tls key_file contains no usable private key")),
        }
        if let Some(ca) = &m.client_ca_pem {
            let n = rustls_pemfile::certs(&mut ca.as_slice()).filter(|c| c.is_ok()).count();
            if n == 0 {
                return Err(Error::config("tls client_ca_file contains no certificate"));
            }
        }
        Ok(())
    }

    /// tonic server configuration (agent API, Raft).
    pub fn tonic_server(m: &TlsMaterial) -> tonic::transport::ServerTlsConfig {
        use tonic::transport::{Certificate, Identity, ServerTlsConfig};
        let mut cfg = ServerTlsConfig::new().identity(Identity::from_pem(&m.cert_pem, &m.key_pem));
        if let Some(ca) = &m.client_ca_pem {
            cfg = cfg.client_ca_root(Certificate::from_pem(ca));
        }
        cfg
    }

    /// tonic client configuration for Raft peers (mutual TLS, fixed server name).
    pub fn tonic_cluster_client(m: &TlsMaterial, ca_pem: &[u8]) -> tonic::transport::ClientTlsConfig {
        use tonic::transport::{Certificate, ClientTlsConfig, Identity};
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca_pem))
            .identity(Identity::from_pem(&m.cert_pem, &m.key_pem))
            .domain_name(CLUSTER_DNS_NAME)
    }

    /// rustls server configuration for the HTTP service.
    pub fn rustls_server(m: &TlsMaterial) -> Result<Arc<rustls::ServerConfig>> {
        install_provider();
        let certs: Vec<_> = rustls_pemfile::certs(&mut m.cert_pem.as_slice())
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::config(format!("tls cert_file: {e}")))?;
        let key = rustls_pemfile::private_key(&mut m.key_pem.as_slice())
            .map_err(|e| Error::config(format!("tls key_file: {e}")))?
            .ok_or_else(|| Error::config("tls key_file contains no private key"))?;
        let builder = rustls::ServerConfig::builder();
        let builder = match &m.client_ca_pem {
            Some(ca) => {
                let mut roots = rustls::RootCertStore::empty();
                for c in rustls_pemfile::certs(&mut ca.as_slice()) {
                    roots
                        .add(c.map_err(|e| Error::config(format!("tls client_ca_file: {e}")))?)
                        .map_err(|e| Error::config(format!("tls client_ca_file: {e}")))?;
                }
                let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                    .build()
                    .map_err(|e| Error::config(format!("tls client verifier: {e}")))?;
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };
        let mut cfg = builder
            .with_single_cert(certs, key)
            .map_err(|e| Error::config(format!("tls certificate/key mismatch: {e}")))?;
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }

    // ---- certificate generator -------------------------------------------------------------

    pub struct Issued {
        pub cert_pem: String,
        pub key_pem: String,
    }

    /// A new private CA (`ca.pem` / `ca.key`). The key stays on the machine that issues certificates.
    pub fn new_ca(common_name: &str, days: u32) -> Result<Issued> {
        use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose};
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, common_name);
        dn.push(DnType::OrganizationName, "Open Cardinal");
        params.distinguished_name = dn;
        set_validity(&mut params, days);
        let key = KeyPair::generate().map_err(|e| Error::Other(format!("key generation: {e}")))?;
        let cert = params.self_signed(&key).map_err(|e| Error::Other(format!("ca generation: {e}")))?;
        Ok(Issued { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
    }

    /// A node/server certificate signed by the CA. It is valid as a server *and* client
    /// certificate (mTLS) and carries every given IP and DNS name plus [`CLUSTER_DNS_NAME`].
    pub fn issue(
        ca_cert_pem: &str,
        ca_key_pem: &str,
        name: &str,
        ips: &[std::net::IpAddr],
        dns: &[String],
        days: u32,
    ) -> Result<Issued> {
        use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, Issuer, KeyPair, SanType};
        let ca_key = KeyPair::from_pem(ca_key_pem).map_err(|e| Error::config(format!("ca key: {e}")))?;
        let issuer =
            Issuer::from_ca_cert_pem(ca_cert_pem, ca_key).map_err(|e| Error::config(format!("ca certificate: {e}")))?;

        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, name);
        params.distinguished_name = dn;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
        let mut sans = vec![SanType::DnsName(CLUSTER_DNS_NAME.try_into().map_err(|e| Error::Other(format!("{e}")))?)];
        for d in dns {
            sans.push(SanType::DnsName(
                d.as_str().try_into().map_err(|e| Error::config(format!("bad DNS name '{d}': {e}")))?,
            ));
        }
        for ip in ips {
            sans.push(SanType::IpAddress(*ip));
        }
        params.subject_alt_names = sans;
        set_validity(&mut params, days);
        let key = KeyPair::generate().map_err(|e| Error::Other(format!("key generation: {e}")))?;
        let cert = params.signed_by(&key, &issuer).map_err(|e| Error::Other(format!("certificate generation: {e}")))?;
        Ok(Issued { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
    }

    /// `open-cardinal tls init-ca|issue ...` (runs locally, never talks to a daemon).
    pub fn cli(command: &str, args: &[String]) -> i32 {
        use crate::control::protocol::Flags;
        let f = Flags::parse(args);
        let dir = PathBuf::from(f.get("dir").unwrap_or("certs"));
        let days = |default: u32| f.get("days").and_then(|d| d.parse().ok()).unwrap_or(default);
        let fail = |m: String| {
            eprintln!("❌ {m}");
            1
        };
        let write = |name: &str, content: &str, secret: bool| -> std::result::Result<(), String> {
            crate::util::write_atomic(&dir.join(name), content.as_bytes(), secret).map_err(|e| e.to_string())
        };
        match command {
            "init-ca" => {
                if dir.join("ca.key").exists() && !f.has("force") {
                    return fail(format!("{} already has a CA; pass --force to replace it", dir.display()));
                }
                let ca = match new_ca(f.get("name").unwrap_or("Open Cardinal CA"), days(3650)) {
                    Ok(c) => c,
                    Err(e) => return fail(e.to_string()),
                };
                if let Err(e) = write("ca.pem", &ca.cert_pem, false).and_then(|_| write("ca.key", &ca.key_pem, true)) {
                    return fail(e);
                }
                println!(
                    "✅ CA created: {0}/ca.pem (distribute) and {0}/ca.key (keep it private, offline if you can)",
                    dir.display()
                );
                0
            }
            "issue" => {
                let Some(name) = f.get("name").filter(|n| !n.is_empty()) else {
                    return fail(
                        "usage: tls issue --name <node> [--ip <ip>]... [--dns <name>]... [--dir certs] [--days 825]"
                            .into(),
                    );
                };
                if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.') {
                    return fail("--name may only contain letters, digits, '-', '_' and '.'".into());
                }
                let mut ips = Vec::new();
                for i in f.all("ip") {
                    match i.parse() {
                        Ok(ip) => ips.push(ip),
                        Err(_) => return fail(format!("'{i}' is not an IP address")),
                    }
                }
                let ca_cert = std::fs::read_to_string(dir.join("ca.pem"));
                let ca_key = std::fs::read_to_string(dir.join("ca.key"));
                let (Ok(ca_cert), Ok(ca_key)) = (ca_cert, ca_key) else {
                    return fail(format!(
                        "no CA in {}: run `open-cardinal tls init-ca --dir {}` first",
                        dir.display(),
                        dir.display()
                    ));
                };
                let node = match issue(&ca_cert, &ca_key, name, &ips, f.all("dns"), days(825)) {
                    Ok(n) => n,
                    Err(e) => return fail(e.to_string()),
                };
                let (c, k) = (format!("{name}.pem"), format!("{name}.key"));
                if let Err(e) = write(&c, &node.cert_pem, false).and_then(|_| write(&k, &node.key_pem, true)) {
                    return fail(e);
                }
                println!(
                    "✅ {0}/{c} and {0}/{k} created (valid for {1}; DNS {2} and the given names/IPs).",
                    dir.display(),
                    ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "),
                    CLUSTER_DNS_NAME
                );
                0
            }
            other => fail(format!("unknown tls command '{other}' (use init-ca | issue)")),
        }
    }

    fn set_validity(params: &mut rcgen::CertificateParams, days: u32) {
        let now = time::OffsetDateTime::now_utc();
        // an hour of backdating tolerates clock skew between the machines
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(days as i64);
    }
}

/// Without the `tls` feature the generator is not available.
#[cfg(not(feature = "tls"))]
pub fn cli(_command: &str, _args: &[String]) -> i32 {
    eprintln!("❌ this build has no TLS support (rebuild with --features tls)");
    1
}
