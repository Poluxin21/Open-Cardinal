//! Extension points: how a separate (for example proprietary) crate plugs into the daemon
//! without forking it. See `docs/EDITIONS.md`.
//!
//! ```ignore
//! // in the other crate:
//! let mut ext = open_cardinal::ext::Extensions::new("enterprise");
//! ext.audit_sinks.push(Arc::new(SiemSink::new(...)));
//! ext.authenticators.push(Arc::new(OidcAuthenticator::new(...)));
//! open_cardinal::daemon::run_with(paths, ext).await
//! ```
//!
//! The community build never needs any of this: `Extensions::default()` is "community" with
//! nothing registered.

use std::sync::Arc;

use crate::audit::Record;

/// Receives every audit record after it was durably written (and chained) locally.
///
/// Called from the audit writer thread, in sequence order, in batches; blocking is
/// acceptable. A sink must not panic and must handle its own retries: the local trail is the
/// source of truth and does not wait for sinks.
pub trait AuditSink: Send + Sync {
    fn name(&self) -> &str;
    fn write(&self, records: &[Record]);
}

/// An identity proven by something other than a Cardinal API key (an OIDC token, an mTLS
/// certificate...).
#[derive(Debug, Clone)]
pub struct ExternalIdentity {
    /// Must name an existing, enabled tenant.
    pub tenant: String,
    /// Who it is, for the audit trail.
    pub subject: String,
    /// Optional restriction of the agent ids this identity may act as.
    pub agent_prefix: Option<String>,
}

/// Consulted when a bearer token is not one of the tenant's API keys.
pub trait Authenticator: Send + Sync {
    fn authenticate(&self, token: &str) -> Option<ExternalIdentity>;
}

#[derive(Clone)]
pub struct Extensions {
    /// Name reported in `status` and `/v1/status` (`"community"` for the open build).
    pub edition: String,
    pub audit_sinks: Vec<Arc<dyn AuditSink>>,
    pub authenticators: Vec<Arc<dyn Authenticator>>,
}

impl Extensions {
    pub fn new(edition: &str) -> Self {
        Self { edition: edition.into(), audit_sinks: Vec::new(), authenticators: Vec::new() }
    }
}

impl Default for Extensions {
    fn default() -> Self {
        Self::new("community")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorder(Mutex<Vec<u64>>);
    impl AuditSink for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn write(&self, records: &[Record]) {
            self.0.lock().unwrap().extend(records.iter().map(|r| r.body.seq));
        }
    }

    #[tokio::test]
    async fn audit_sinks_receive_every_record_in_order() {
        use crate::audit::{AuditLog, Event};
        use crate::config::AuditConfig;
        let sink = Arc::new(Recorder(Mutex::new(Vec::new())));
        let log = AuditLog::open_in_memory_with_sinks("n", &AuditConfig::default(), vec![sink.clone()]).unwrap();
        for i in 0..25 {
            log.record(Event::new("decision", "t", serde_json::json!({ "i": i })));
        }
        log.flush().await;
        let seen = sink.0.lock().unwrap().clone();
        assert_eq!(seen, (1..=25).collect::<Vec<_>>());
    }

    #[test]
    fn community_is_the_default_edition() {
        let e = Extensions::default();
        assert_eq!(e.edition, "community");
        assert!(e.audit_sinks.is_empty() && e.authenticators.is_empty());
    }
}
