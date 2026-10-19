//! Typed, validated configuration (`config/config.json`) plus environment overrides.
//!
//! Backward compatible with the original three-key file
//! (`{"grpc_port":50051,"http_port":8080,"db_file":"open_cardinal.redb"}`), which is no
//! longer overwritten on every start: the file is only created when missing or blank.

mod limits;
mod paths;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::runtime::Runtime;

pub use limits::{Limits, LimitsPatch};
pub use paths::Paths;

pub const DEFAULT_GRPC_PORT: u16 = 50051;
pub const DEFAULT_HTTP_PORT: u16 = 8080;
pub const DEFAULT_CONTROL_ADDR: &str = "127.0.0.1:19876";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub grpc_port: u16,
    pub http_port: u16,
    /// Main database (shared memory, forced reactions, Raft state), relative to the home dir.
    pub db_file: String,
    /// Page cache of the main database in MiB. redb defaults to 1 GiB, far too much for a
    /// sidecar; the audit database gets half of this.
    pub db_cache_mb: u64,
    pub bind: Bind,
    /// Address of the local control server used by the CLI. Always keep it on loopback.
    pub control_addr: SocketAddr,
    pub security: SecurityConfig,
    pub limits: Limits,
    pub log: LogConfig,
    pub audit: AuditConfig,
    /// Keep writing `info/sys.json` and `info/metrics.json` (legacy integration).
    pub export_info_files: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            grpc_port: DEFAULT_GRPC_PORT,
            http_port: DEFAULT_HTTP_PORT,
            db_file: "open_cardinal.redb".into(),
            db_cache_mb: 32,
            bind: Bind::default(),
            control_addr: DEFAULT_CONTROL_ADDR.parse().expect("valid default"),
            security: SecurityConfig::default(),
            limits: Limits::default(),
            log: LogConfig::default(),
            audit: AuditConfig::default(),
            export_info_files: true,
        }
    }
}

/// Which interfaces the gRPC and HTTP servers listen on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Bind {
    Keyword(BindKeyword),
    Addrs(Vec<IpAddr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindKeyword {
    /// Loopback for a bare daemon, every interface inside containers.
    Auto,
    Loopback,
    All,
}

impl Default for Bind {
    fn default() -> Self {
        Bind::Keyword(BindKeyword::Auto)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub mode: SecurityMode,
    /// Allow serving unauthenticated traffic on a non-loopback interface. Never enable
    /// this outside of a lab: anyone who can reach the port can spoof any agent.
    pub allow_insecure_remote: bool,
    pub tls: Option<TlsConfig>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self { mode: SecurityMode::Auto, allow_insecure_remote: false, tls: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecurityMode {
    /// Open on a bare-host loopback (original behaviour), required everywhere else.
    Auto,
    Open,
    Required,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_file: String,
    pub key_file: String,
    /// When set, clients must present a certificate signed by this CA (mTLS).
    pub client_ca_file: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub output: LogOutput,
    pub format: LogFormat,
    /// `tracing` filter directive, e.g. `info` or `open_cardinal=debug`.
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { output: LogOutput::Auto, format: LogFormat::Text, level: "info".into() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogOutput {
    /// `logs/cardinal_log.<date>` for a bare daemon, stdout inside containers.
    Auto,
    File,
    Stdout,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    pub enabled: bool,
    /// Which pulse decisions are recorded.
    pub decisions: AuditDecisions,
    /// Oldest records are pruned beyond this many (0 = keep everything).
    pub retention_records: u64,
    pub telemetry: AuditTelemetry,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            decisions: AuditDecisions::Actions,
            retention_records: 500_000,
            telemetry: AuditTelemetry::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditDecisions {
    /// Every decision, including routine IDLE ones (about 1 KiB each: size `retention_records`
    /// accordingly).
    All,
    /// Decisions that did something or went wrong: any reaction other than IDLE, any rule
    /// error, every operator override that fired. Rejections and admin actions are always recorded.
    Actions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditTelemetry {
    /// Record the whole (size-capped) telemetry map.
    Full,
    /// Record only the telemetry keys, never the values.
    Keys,
    None,
}

/// Effective authentication policy once the runtime and bind addresses are known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    /// Never ask for credentials.
    Open,
    /// Always require credentials.
    Required,
    /// Open until the first API key exists; from then on credentials are required.
    /// (Issuing a key is a clear statement of intent: it must switch protection on.)
    WhenKeysExist,
}

/// Resolved process settings: file config + environment + detected runtime.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config: Config,
    pub paths: Paths,
    pub runtime: Runtime,
}

impl Settings {
    /// Load `config/config.json` (creating it with defaults when missing or blank),
    /// then apply `CARDINAL_*` environment overrides and validate.
    pub fn load(paths: Paths, runtime: Runtime) -> Result<Self> {
        Self::load_with(paths, runtime, |k| std::env::var(k).ok())
    }

    pub fn load_with(paths: Paths, runtime: Runtime, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut config = read_config_file(&paths.config_file())?;
        apply_env(&mut config, &env)?;
        let settings = Self { config, paths, runtime };
        settings.validate()?;
        Ok(settings)
    }

    pub fn db_path(&self) -> std::path::PathBuf {
        self.paths.home.join(&self.config.db_file)
    }

    /// Addresses the gRPC server listens on.
    pub fn grpc_addrs(&self) -> Vec<SocketAddr> {
        self.bind_ips().into_iter().map(|ip| SocketAddr::new(ip, self.config.grpc_port)).collect()
    }

    pub fn http_addrs(&self) -> Vec<SocketAddr> {
        self.bind_ips().into_iter().map(|ip| SocketAddr::new(ip, self.config.http_port)).collect()
    }

    fn bind_ips(&self) -> Vec<IpAddr> {
        match &self.config.bind {
            Bind::Addrs(ips) => ips.clone(),
            Bind::Keyword(BindKeyword::All) => vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)],
            Bind::Keyword(BindKeyword::Loopback) => loopbacks(),
            Bind::Keyword(BindKeyword::Auto) => {
                if self.runtime.is_container() {
                    vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)]
                } else {
                    loopbacks()
                }
            }
        }
    }

    pub fn auth_mode(&self) -> AuthMode {
        match self.config.security.mode {
            SecurityMode::Required => AuthMode::Required,
            SecurityMode::Open => AuthMode::Open,
            SecurityMode::Auto => {
                if self.runtime.is_container() || self.bind_ips().iter().any(|ip| !ip.is_loopback()) {
                    AuthMode::Required
                } else {
                    AuthMode::WhenKeysExist
                }
            }
        }
    }

    pub fn log_output(&self) -> LogOutput {
        match self.config.log.output {
            LogOutput::Auto if self.runtime.is_container() => LogOutput::Stdout,
            LogOutput::Auto => LogOutput::File,
            other => other,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let c = &self.config;
        if c.grpc_port == 0 || c.http_port == 0 {
            return Err(Error::config("grpc_port/http_port must be non-zero"));
        }
        if c.grpc_port == c.http_port {
            return Err(Error::config("grpc_port and http_port must differ"));
        }
        if !c.control_addr.ip().is_loopback() {
            return Err(Error::config("control_addr must be a loopback address: the control plane is local-only"));
        }
        if c.db_cache_mb == 0 || c.db_cache_mb > 4096 {
            return Err(Error::config("db_cache_mb must be between 1 and 4096"));
        }
        if c.db_file.trim().is_empty() {
            return Err(Error::config("db_file must not be empty"));
        }
        crate::tls::check_supported(c.security.tls.is_some())?;
        // Exposing an open (unauthenticated) service beyond loopback lets anyone who can
        // reach the port forge agent state (see docs/SECURITY.md, finding F-02).
        let exposed = self.bind_ips().iter().any(|ip| !ip.is_loopback());
        if exposed && self.auth_mode() == AuthMode::Open && !c.security.allow_insecure_remote {
            return Err(Error::config(
                "refusing to listen on a non-loopback address without authentication; \
                 set security.mode=required (recommended) or security.allow_insecure_remote=true",
            ));
        }
        c.limits.validate()?;
        Ok(())
    }
}

fn loopbacks() -> Vec<IpAddr> {
    vec![IpAddr::V6(Ipv6Addr::LOCALHOST), IpAddr::V4(Ipv4Addr::LOCALHOST)]
}

fn read_config_file(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    if text.trim().is_empty() {
        let cfg = Config::default();
        let pretty = serde_json::to_vec_pretty(&cfg)?;
        crate::util::write_atomic(path, &pretty, false)?;
        return Ok(cfg);
    }
    serde_json::from_str(&text).map_err(|e| Error::config(format!("{}: {e}", path.display())))
}

fn apply_env(cfg: &mut Config, env: &impl Fn(&str) -> Option<String>) -> Result<()> {
    fn parse<T: std::str::FromStr>(key: &str, v: &str) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        v.trim().parse().map_err(|e| Error::config(format!("{key}: {e}")))
    }

    if let Some(v) = env("CARDINAL_GRPC_PORT") {
        cfg.grpc_port = parse("CARDINAL_GRPC_PORT", &v)?;
    }
    if let Some(v) = env("CARDINAL_HTTP_PORT") {
        cfg.http_port = parse("CARDINAL_HTTP_PORT", &v)?;
    }
    if let Some(v) = env("CARDINAL_DB_FILE") {
        cfg.db_file = v;
    }
    if let Some(v) = env("CARDINAL_CONTROL_ADDR") {
        cfg.control_addr = parse("CARDINAL_CONTROL_ADDR", &v)?;
    }
    if let Some(v) = env("CARDINAL_BIND") {
        cfg.bind = match v.trim().to_ascii_lowercase().as_str() {
            "auto" => Bind::Keyword(BindKeyword::Auto),
            "loopback" => Bind::Keyword(BindKeyword::Loopback),
            "all" | "any" => Bind::Keyword(BindKeyword::All),
            _ => Bind::Addrs(v.split(',').map(|s| parse::<IpAddr>("CARDINAL_BIND", s)).collect::<Result<_>>()?),
        };
    }
    if let Some(v) = env("CARDINAL_SECURITY") {
        cfg.security.mode = match v.trim().to_ascii_lowercase().as_str() {
            "auto" => SecurityMode::Auto,
            "open" => SecurityMode::Open,
            "required" => SecurityMode::Required,
            other => {
                return Err(Error::config(format!("CARDINAL_SECURITY: unknown mode '{other}' (auto|open|required)")));
            }
        };
    }
    if let Some(v) = env("CARDINAL_LOG") {
        cfg.log.level = v;
    }
    if let Some(v) = env("CARDINAL_LOG_OUTPUT") {
        cfg.log.output = match v.trim().to_ascii_lowercase().as_str() {
            "auto" => LogOutput::Auto,
            "file" => LogOutput::File,
            "stdout" => LogOutput::Stdout,
            "both" => LogOutput::Both,
            other => return Err(Error::config(format!("CARDINAL_LOG_OUTPUT: unknown '{other}'"))),
        };
    }
    if let Some(v) = env("CARDINAL_LOG_FORMAT") {
        cfg.log.format = match v.trim().to_ascii_lowercase().as_str() {
            "text" => LogFormat::Text,
            "json" => LogFormat::Json,
            other => return Err(Error::config(format!("CARDINAL_LOG_FORMAT: unknown '{other}'"))),
        };
    }
    if let Some(v) = env("CARDINAL_AUDIT") {
        cfg.audit.enabled = !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    fn settings(json: &str, rt: Runtime, e: &[(&str, &str)]) -> Result<Settings> {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), json).unwrap();
        // keep the tempdir alive only for the duration of loading
        Settings::load_with(paths, rt, env(e))
    }

    #[test]
    fn legacy_three_key_file_still_parses() {
        let s =
            settings(r#"{"grpc_port":50051,"http_port":8080,"db_file":"open_cardinal.redb"}"#, Runtime::Daemon, &[])
                .unwrap();
        assert_eq!(s.config.grpc_port, 50051);
        assert_eq!(s.auth_mode(), AuthMode::WhenKeysExist, "bare daemon on loopback stays open until a key is issued");
    }

    #[test]
    fn blank_file_is_replaced_with_defaults_not_clobbering_real_ones() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "").unwrap();
        Settings::load_with(paths.clone(), Runtime::Daemon, env(&[])).unwrap();
        let written = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(written.contains("grpc_port"));

        std::fs::write(paths.config_file(), r#"{"grpc_port": 6000}"#).unwrap();
        let s = Settings::load_with(paths, Runtime::Daemon, env(&[])).unwrap();
        assert_eq!(s.config.grpc_port, 6000);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = settings(r#"{"grpc_prot": 1}"#, Runtime::Daemon, &[]).unwrap_err();
        assert!(e.to_string().contains("grpc_prot"), "{e}");
    }

    #[test]
    fn containers_bind_everywhere_and_require_auth() {
        for rt in [Runtime::Docker, Runtime::Swarm, Runtime::Kubernetes] {
            let s = settings("{}", rt, &[]).unwrap();
            assert_eq!(s.auth_mode(), AuthMode::Required);
            assert_eq!(s.grpc_addrs(), vec!["0.0.0.0:50051".parse().unwrap()]);
            assert_eq!(s.log_output(), LogOutput::Stdout);
        }
    }

    #[test]
    fn open_remote_bind_is_refused() {
        let e = settings(r#"{"bind":"all","security":{"mode":"open"}}"#, Runtime::Daemon, &[]).unwrap_err();
        assert!(e.to_string().contains("non-loopback"), "{e}");
        settings(r#"{"bind":"all","security":{"mode":"open","allow_insecure_remote":true}}"#, Runtime::Daemon, &[])
            .unwrap();
    }

    #[test]
    fn env_overrides_apply() {
        let s = settings(
            "{}",
            Runtime::Daemon,
            &[("CARDINAL_GRPC_PORT", "7000"), ("CARDINAL_BIND", "127.0.0.2,127.0.0.3")],
        )
        .unwrap();
        assert_eq!(s.config.grpc_port, 7000);
        assert_eq!(s.grpc_addrs().len(), 2);
    }

    #[test]
    fn control_plane_must_stay_local() {
        let e = settings(r#"{"control_addr":"0.0.0.0:19876"}"#, Runtime::Daemon, &[]).unwrap_err();
        assert!(e.to_string().contains("loopback"));
    }
}
