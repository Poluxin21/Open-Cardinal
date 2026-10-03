//! Tamper-evident audit trail.
//!
//! Every decision, override, rejected request and administrative action becomes a record
//! in an append-only redb table. Records are chained: `hash = HMAC-SHA256(key, prev ‖ body)`,
//! so altering, deleting or reordering any record breaks every hash after it, and
//! `/v1/audit/verify` detects that. The key lives in `config/audit.key`; without it an
//! attacker who edits the database cannot recompute a valid chain.
//!
//! Writes never block the pulse path: events are queued and a dedicated thread commits
//! them in batches (one fsync per batch). If the queue overflows, the loss is itself
//! recorded as an `audit_gap` event, so gaps are visible rather than silent.

mod chain;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::config::AuditConfig;
use crate::error::{Error, Result};
use crate::util;

use chain::{ChainKey, seal};
pub use chain::{VerifyReport, genesis_hash};

const RECORDS: TableDefinition<u64, &[u8]> = TableDefinition::new("audit_records");
const AMETA: TableDefinition<&str, &[u8]> = TableDefinition::new("audit_meta");
const META_HEAD_SEQ: &str = "head_seq";
const META_HEAD_HASH: &str = "head_hash";
const META_KEY_FP: &str = "key_fingerprint";

const QUEUE_CAPACITY: usize = 16_384;
const MAX_BATCH: usize = 1024;
const MAX_QUERY: usize = 1000;
const MAX_TELEMETRY_BYTES: usize = 4096;

/// What gets queued. `kind` is a stable event name (`decision`, `forced`, ...).
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: &'static str,
    pub tenant: String,
    pub agent: Option<String>,
    pub trace_id: Option<String>,
    pub data: Value,
}

impl Event {
    pub fn new(kind: &'static str, tenant: impl Into<String>, data: Value) -> Self {
        Self { kind, tenant: tenant.into(), agent: None, trace_id: None, data }
    }
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(agent.into());
        self
    }
    pub fn trace(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }
}

/// The fields covered by the hash (everything but `hash`). Field order is the canonical order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Body {
    pub seq: u64,
    pub ts_ms: u64,
    pub node: String,
    pub tenant: String,
    pub agent: Option<String>,
    pub event: String,
    pub trace_id: Option<String>,
    pub data: Value,
    pub prev: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    #[serde(flatten)]
    pub body: Body,
    pub hash: String,
}

enum Msg {
    Event(Event, u64),
    Flush(oneshot::Sender<()>),
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct AuditStats {
    pub enabled: bool,
    pub written: u64,
    pub dropped: u64,
    pub head_seq: u64,
    pub records: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Query {
    pub tenant: Option<String>,
    pub agent: Option<String>,
    pub event: Option<String>,
    pub trace_id: Option<String>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    /// Return records with `seq < before` (pagination, newest first).
    pub before: Option<u64>,
    pub limit: usize,
}

pub struct AuditLog {
    enabled: bool,
    db: Option<Arc<Database>>,
    key: Arc<ChainKey>,
    tx: Option<mpsc::Sender<Msg>>,
    config: AuditConfig,
    written: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    head_seq: Arc<AtomicU64>,
}

impl AuditLog {
    /// A log that records nothing (`audit.enabled = false`).
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            enabled: false,
            db: None,
            key: Arc::new(ChainKey::none()),
            tx: None,
            config: AuditConfig { enabled: false, ..AuditConfig::default() },
            written: Default::default(),
            dropped: Default::default(),
            head_seq: Default::default(),
        })
    }

    pub fn open(path: &Path, key_file: &Path, node: &str, config: &AuditConfig) -> Result<Arc<Self>> {
        Self::open_with_cache(path, key_file, node, config, 16 * 1024 * 1024)
    }

    pub fn open_with_cache(
        path: &Path,
        key_file: &Path,
        node: &str,
        config: &AuditConfig,
        cache_bytes: usize,
    ) -> Result<Arc<Self>> {
        Self::open_full(path, key_file, node, config, cache_bytes, Vec::new())
    }

    /// Like [`AuditLog::open_with_cache`], plus sinks that mirror every committed record.
    pub fn open_full(
        path: &Path,
        key_file: &Path,
        node: &str,
        config: &AuditConfig,
        cache_bytes: usize,
        sinks: Vec<Arc<dyn crate::ext::AuditSink>>,
    ) -> Result<Arc<Self>> {
        if !config.enabled {
            return Ok(Self::disabled());
        }
        let mut builder = Database::builder();
        builder.set_cache_size(cache_bytes);
        let db = builder.create(path).map_err(|e| match e {
            redb::DatabaseError::DatabaseAlreadyOpen => {
                Error::Storage(format!("{} is locked by another process", path.display()))
            }
            other => other.into(),
        })?;
        Self::open_db(db, ChainKey::load_or_create(key_file)?, node, config, sinks)
    }

    pub fn open_in_memory(node: &str, config: &AuditConfig) -> Result<Arc<Self>> {
        Self::open_in_memory_with_sinks(node, config, Vec::new())
    }

    pub fn open_in_memory_with_sinks(
        node: &str,
        config: &AuditConfig,
        sinks: Vec<Arc<dyn crate::ext::AuditSink>>,
    ) -> Result<Arc<Self>> {
        let db = Database::builder().create_with_backend(redb::backends::InMemoryBackend::new())?;
        Self::open_db(db, ChainKey::from_bytes(b"test-key".to_vec()), node, config, sinks)
    }

    fn open_db(
        db: Database,
        key: ChainKey,
        node: &str,
        config: &AuditConfig,
        sinks: Vec<Arc<dyn crate::ext::AuditSink>>,
    ) -> Result<Arc<Self>> {
        let db = Arc::new(db);
        {
            let tx = db.begin_write()?;
            tx.open_table(RECORDS)?;
            tx.open_table(AMETA)?;
            tx.commit()?;
        }
        let (head_seq, head_hash) = read_head(&db)?;

        // remember which key sealed this chain, so verification can say "wrong key"
        {
            let tx = db.begin_write()?;
            {
                let mut meta = tx.open_table(AMETA)?;
                if meta.get(META_KEY_FP)?.is_none() {
                    meta.insert(META_KEY_FP, key.fingerprint().as_bytes())?;
                }
            }
            tx.commit()?;
        }

        let key = Arc::new(key);
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let written = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let head = Arc::new(AtomicU64::new(head_seq));

        let writer = Writer {
            db: db.clone(),
            key: key.clone(),
            node: node.to_string(),
            retention: config.retention_records,
            head_seq,
            head_hash,
            written: written.clone(),
            dropped: dropped.clone(),
            head_atomic: head.clone(),
            reported_dropped: 0,
            sinks,
        };
        std::thread::Builder::new()
            .name("audit-writer".into())
            .spawn(move || writer.run(rx))
            .map_err(|e| Error::Other(format!("cannot start audit writer: {e}")))?;

        Ok(Arc::new(Self {
            enabled: true,
            db: Some(db),
            key,
            tx: Some(tx),
            config: config.clone(),
            written,
            dropped,
            head_seq: head,
        }))
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn decisions_mode(&self) -> crate::config::AuditDecisions {
        self.config.decisions
    }

    pub fn telemetry_mode(&self) -> crate::config::AuditTelemetry {
        self.config.telemetry
    }

    /// Render telemetry for a record according to `audit.telemetry`, size-capped.
    pub fn telemetry_value(&self, telemetry: &std::collections::HashMap<String, String>) -> Value {
        use crate::config::AuditTelemetry::*;
        match self.config.telemetry {
            None => Value::Null,
            Keys => {
                let mut keys: Vec<&String> = telemetry.keys().collect();
                keys.sort();
                json!({ "keys": keys })
            }
            Full => {
                let mut sorted: Vec<(&String, &String)> = telemetry.iter().collect();
                sorted.sort();
                let mut out = serde_json::Map::new();
                let mut bytes = 0;
                for (k, v) in sorted {
                    bytes += k.len() + v.len() + 6;
                    if bytes > MAX_TELEMETRY_BYTES {
                        out.insert("…".into(), Value::String("truncated".into()));
                        break;
                    }
                    out.insert(k.clone(), Value::String(v.clone()));
                }
                Value::Object(out)
            }
        }
    }

    /// Queue an event. Never blocks; on overflow the event is counted as dropped.
    pub fn record(&self, event: Event) {
        let Some(tx) = &self.tx else { return };
        if tx.try_send(Msg::Event(event, util::now_ms())).is_err() {
            self.dropped.fetch_add(1, Relaxed);
        }
    }

    /// Wait until everything queued so far is durable.
    pub async fn flush(&self) {
        let Some(tx) = &self.tx else { return };
        let (done_tx, done_rx) = oneshot::channel();
        if tx.send(Msg::Flush(done_tx)).await.is_ok() {
            let _ = done_rx.await;
        }
    }

    pub fn stats(&self) -> AuditStats {
        let records = self
            .db
            .as_ref()
            .and_then(|db| db.begin_read().ok())
            .and_then(|rx| rx.open_table(RECORDS).ok())
            .and_then(|t| t.len().ok())
            .unwrap_or(0);
        AuditStats {
            enabled: self.enabled,
            written: self.written.load(Relaxed),
            dropped: self.dropped.load(Relaxed),
            head_seq: self.head_seq.load(Relaxed),
            records,
        }
    }

    /// Newest-first scan with filters.
    pub fn query(&self, q: &Query) -> Result<Vec<Record>> {
        let Some(db) = &self.db else { return Ok(Vec::new()) };
        let limit = q.limit.clamp(1, MAX_QUERY);
        let rx = db.begin_read()?;
        let table = rx.open_table(RECORDS)?;
        let mut out = Vec::new();
        let iter = match q.before {
            Some(b) => table.range(..b)?,
            None => table.range::<u64>(..)?,
        };
        for row in iter.rev() {
            let (_, raw) = row?;
            let rec: Record = serde_json::from_slice(raw.value())?;
            if let Some(until) = q.until_ms
                && rec.body.ts_ms > until
            {
                continue;
            }
            if let Some(since) = q.since_ms
                && rec.body.ts_ms < since
            {
                break; // timestamps are non-decreasing in seq order
            }
            let matches = q.tenant.as_ref().is_none_or(|t| &rec.body.tenant == t)
                && q.agent.as_ref().is_none_or(|a| rec.body.agent.as_ref() == Some(a))
                && q.event.as_ref().is_none_or(|e| &rec.body.event == e)
                && q.trace_id.as_ref().is_none_or(|t| rec.body.trace_id.as_ref() == Some(t));
            if matches {
                out.push(rec);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Oldest-first iteration for exports.
    pub fn export_from(&self, from_seq: u64, max: usize) -> Result<Vec<Record>> {
        let Some(db) = &self.db else { return Ok(Vec::new()) };
        let rx = db.begin_read()?;
        let table = rx.open_table(RECORDS)?;
        let mut out = Vec::new();
        for row in table.range(from_seq..)? {
            let (_, raw) = row?;
            out.push(serde_json::from_slice(raw.value())?);
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    /// Recompute the whole chain and report the first inconsistency.
    pub fn verify(&self) -> Result<VerifyReport> {
        let Some(db) = &self.db else {
            return Ok(VerifyReport::ok(0, None, None));
        };
        let rx = db.begin_read()?;
        let meta = rx.open_table(AMETA)?;
        let sealed_with = meta.get(META_KEY_FP)?.map(|v| String::from_utf8_lossy(v.value()).into_owned());
        if sealed_with.as_deref().is_some_and(|fp| fp != self.key.fingerprint()) {
            return Ok(VerifyReport::broken(0, None, "audit key does not match the key that sealed this chain".into()));
        }
        let table = rx.open_table(RECORDS)?;
        chain::verify_chain(
            &self.key,
            table.iter()?.map(|r| {
                let (_, raw) = r?;
                let rec: Record = serde_json::from_slice(raw.value())?;
                Ok(rec)
            }),
        )
    }
}

fn read_head(db: &Database) -> Result<(u64, String)> {
    let rx = db.begin_read()?;
    let meta = rx.open_table(AMETA)?;
    let seq = meta.get(META_HEAD_SEQ)?.map(|v| crate::store::decode_u64(v.value())).unwrap_or(0);
    let hash =
        meta.get(META_HEAD_HASH)?.map(|v| String::from_utf8_lossy(v.value()).into_owned()).unwrap_or_else(genesis_hash);
    Ok((seq, hash))
}

struct Writer {
    db: Arc<Database>,
    key: Arc<ChainKey>,
    node: String,
    retention: u64,
    head_seq: u64,
    head_hash: String,
    written: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    head_atomic: Arc<AtomicU64>,
    reported_dropped: u64,
    sinks: Vec<Arc<dyn crate::ext::AuditSink>>,
}

impl Writer {
    fn run(mut self, mut rx: mpsc::Receiver<Msg>) {
        let mut batch: Vec<(Event, u64)> = Vec::with_capacity(MAX_BATCH);
        let mut waiters: Vec<oneshot::Sender<()>> = Vec::new();
        while let Some(first) = rx.blocking_recv() {
            self.take(first, &mut batch, &mut waiters);
            while batch.len() < MAX_BATCH {
                match rx.try_recv() {
                    Ok(m) => self.take(m, &mut batch, &mut waiters),
                    Err(_) => break,
                }
            }
            if let Err(e) = self.commit(&mut batch) {
                tracing::error!("audit commit failed, {} events lost: {e}", batch.len());
                self.dropped.fetch_add(batch.len() as u64, Relaxed);
                batch.clear();
            }
            for w in waiters.drain(..) {
                let _ = w.send(());
            }
        }
        // channel closed: nothing queued is left (we drain before blocking again)
    }

    fn take(&self, m: Msg, batch: &mut Vec<(Event, u64)>, waiters: &mut Vec<oneshot::Sender<()>>) {
        match m {
            Msg::Event(e, ts) => batch.push((e, ts)),
            Msg::Flush(w) => waiters.push(w),
        }
    }

    fn commit(&mut self, batch: &mut Vec<(Event, u64)>) -> Result<()> {
        // surface overflow losses inside the chain itself
        let dropped = self.dropped.load(Relaxed);
        if dropped > self.reported_dropped {
            let lost = dropped - self.reported_dropped;
            self.reported_dropped = dropped;
            batch.push((
                Event::new(
                    "audit_gap",
                    "-",
                    json!({ "lost_events": lost, "reason": "queue overflow or write failure" }),
                ),
                util::now_ms(),
            ));
        }
        if batch.is_empty() {
            return Ok(());
        }

        let tx = self.db.begin_write()?;
        let (mut seq, mut prev) = (self.head_seq, self.head_hash.clone());
        let mut sealed: Vec<Record> = Vec::with_capacity(if self.sinks.is_empty() { 0 } else { batch.len() });
        {
            let mut table = tx.open_table(RECORDS)?;
            for (ev, ts) in batch.iter() {
                seq += 1;
                let body = Body {
                    seq,
                    ts_ms: *ts,
                    node: self.node.clone(),
                    tenant: ev.tenant.clone(),
                    agent: ev.agent.clone(),
                    event: ev.kind.to_string(),
                    trace_id: ev.trace_id.clone(),
                    data: ev.data.clone(),
                    prev: prev.clone(),
                };
                let rec = seal(&self.key, body);
                table.insert(seq, serde_json::to_vec(&rec)?.as_slice())?;
                prev = rec.hash.clone();
                if !self.sinks.is_empty() {
                    sealed.push(rec);
                }
            }
            if self.retention > 0 {
                let len = table.len()?;
                if len > self.retention + self.retention / 10 + 1 {
                    let cut = seq - self.retention;
                    table.retain_in(..=cut, |_, _| false)?;
                }
            }
            let mut meta = tx.open_table(AMETA)?;
            meta.insert(META_HEAD_SEQ, &seq.to_le_bytes()[..])?;
            meta.insert(META_HEAD_HASH, prev.as_bytes())?;
        }
        tx.commit()?;
        self.written.fetch_add(batch.len() as u64, Relaxed);
        self.head_seq = seq;
        self.head_hash = prev;
        self.head_atomic.store(seq, Relaxed);
        batch.clear();
        // mirrors never delay or fail the local trail
        for sink in &self.sinks {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.write(&sealed)));
            if r.is_err() {
                tracing::error!(sink = sink.name(), "audit sink panicked; records stay in the local trail");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuditTelemetry;

    fn log() -> Arc<AuditLog> {
        AuditLog::open_in_memory("node-a", &AuditConfig::default()).unwrap()
    }

    fn decision(tenant: &str, agent: &str, n: u64) -> Event {
        Event::new("decision", tenant, json!({ "n": n })).agent(agent).trace(format!("t{n}"))
    }

    #[tokio::test]
    async fn records_are_chained_and_verifiable() {
        let a = log();
        for i in 0..50 {
            a.record(decision("acme", "A", i));
        }
        a.flush().await;
        let rep = a.verify().unwrap();
        assert!(rep.ok, "{rep:?}");
        assert_eq!(rep.checked, 50);
        let all = a.export_from(1, 100).unwrap();
        assert_eq!(all.len(), 50);
        assert_eq!(all[0].body.prev, genesis_hash());
        assert_eq!(all[1].body.prev, all[0].hash);
    }

    #[tokio::test]
    async fn query_filters_and_paginates_newest_first() {
        let a = log();
        for i in 0..30 {
            let tenant = if i % 2 == 0 { "acme" } else { "globex" };
            a.record(decision(tenant, if i % 3 == 0 { "A" } else { "B" }, i));
        }
        a.flush().await;
        let acme = a.query(&Query { tenant: Some("acme".into()), limit: 100, ..Default::default() }).unwrap();
        assert_eq!(acme.len(), 15);
        assert!(acme.iter().all(|r| r.body.tenant == "acme"));
        assert!(acme[0].body.seq > acme[1].body.seq, "newest first");

        let page1 = a.query(&Query { limit: 10, ..Default::default() }).unwrap();
        let page2 =
            a.query(&Query { limit: 10, before: Some(page1.last().unwrap().body.seq), ..Default::default() }).unwrap();
        assert_eq!(page1.len() + page2.len(), 20);
        assert!(page2[0].body.seq < page1.last().unwrap().body.seq);

        let one = a.query(&Query { trace_id: Some("t7".into()), limit: 10, ..Default::default() }).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[tokio::test]
    async fn tampering_is_detected() {
        let a = log();
        for i in 0..10 {
            a.record(decision("acme", "A", i));
        }
        a.flush().await;
        // an attacker with write access to the file rewrites record 5
        {
            let db = a.db.as_ref().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx.open_table(RECORDS).unwrap();
                let raw = t.get(5).unwrap().unwrap().value().to_vec();
                let mut rec: Record = serde_json::from_slice(&raw).unwrap();
                rec.body.data = json!({ "n": 999 });
                t.insert(5, serde_json::to_vec(&rec).unwrap().as_slice()).unwrap();
            }
            tx.commit().unwrap();
        }
        let rep = a.verify().unwrap();
        assert!(!rep.ok);
        assert_eq!(rep.broken_at, Some(5));
    }

    #[tokio::test]
    async fn deletion_and_reordering_are_detected() {
        let a = log();
        for i in 0..10 {
            a.record(decision("acme", "A", i));
        }
        a.flush().await;
        {
            let db = a.db.as_ref().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx.open_table(RECORDS).unwrap();
                t.remove(4).unwrap();
            }
            tx.commit().unwrap();
        }
        let rep = a.verify().unwrap();
        assert!(!rep.ok, "a missing record must break the chain");
        assert_eq!(rep.broken_at, Some(4), "reports the record that is missing");
    }

    #[tokio::test]
    async fn forged_record_without_the_key_does_not_verify() {
        // attacker rewrites a record AND recomputes every hash with plain SHA-256 / a guessed key
        let a = log();
        for i in 0..3 {
            a.record(decision("acme", "A", i));
        }
        a.flush().await;
        let attacker_key = ChainKey::from_bytes(b"guess".to_vec());
        {
            let db = a.db.as_ref().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut t = tx.open_table(RECORDS).unwrap();
                let mut prev = genesis_hash();
                for seq in 1..=3u64 {
                    let raw = t.get(seq).unwrap().unwrap().value().to_vec();
                    let rec: Record = serde_json::from_slice(&raw).unwrap();
                    let mut body = rec.body;
                    body.prev = prev.clone();
                    body.data = json!({ "forged": true });
                    let sealed = seal(&attacker_key, body);
                    prev = sealed.hash.clone();
                    t.insert(seq, serde_json::to_vec(&sealed).unwrap().as_slice()).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        assert!(!a.verify().unwrap().ok);
    }

    #[tokio::test]
    async fn retention_prunes_old_records_and_chain_still_verifies() {
        let cfg = AuditConfig { retention_records: 20, ..AuditConfig::default() };
        let a = AuditLog::open_in_memory("n", &cfg).unwrap();
        for i in 0..100 {
            a.record(decision("acme", "A", i));
            if i % 10 == 9 {
                a.flush().await;
            }
        }
        a.flush().await;
        let st = a.stats();
        assert!(st.records <= 24 && st.records >= 20, "records = {}", st.records);
        assert_eq!(st.head_seq, 100);
        let rep = a.verify().unwrap();
        assert!(rep.ok, "{rep:?}");
    }

    #[tokio::test]
    async fn overflow_is_recorded_as_a_gap() {
        let a = log();
        a.dropped.fetch_add(7, Relaxed); // simulate 7 events lost to a full queue
        a.record(decision("acme", "A", 1));
        a.flush().await;
        let gaps = a.query(&Query { event: Some("audit_gap".into()), limit: 5, ..Default::default() }).unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].body.data["lost_events"], 7);
    }

    #[tokio::test]
    async fn restart_continues_the_same_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (db, key) = (dir.path().join("audit.redb"), dir.path().join("audit.key"));
        {
            let a = AuditLog::open(&db, &key, "n", &AuditConfig::default()).unwrap();
            a.record(decision("acme", "A", 1));
            a.flush().await;
        }
        // writer thread holds the db until its channel closes; give it a moment
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let a = AuditLog::open(&db, &key, "n", &AuditConfig::default()).unwrap();
        a.record(decision("acme", "A", 2));
        a.flush().await;
        let rep = a.verify().unwrap();
        assert!(rep.ok, "{rep:?}");
        assert_eq!(rep.checked, 2);
    }

    #[tokio::test]
    async fn telemetry_modes() {
        let t: std::collections::HashMap<String, String> =
            [("fuel", "5"), ("alt", "9")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let full = log();
        assert_eq!(full.telemetry_value(&t)["fuel"], "5");
        let keys =
            AuditLog::open_in_memory("n", &AuditConfig { telemetry: AuditTelemetry::Keys, ..Default::default() })
                .unwrap();
        assert_eq!(keys.telemetry_value(&t), json!({"keys": ["alt", "fuel"]}));
        let none =
            AuditLog::open_in_memory("n", &AuditConfig { telemetry: AuditTelemetry::None, ..Default::default() })
                .unwrap();
        assert_eq!(none.telemetry_value(&t), Value::Null);
    }

    #[tokio::test]
    async fn disabled_log_is_inert() {
        let a = AuditLog::disabled();
        a.record(decision("a", "b", 1));
        a.flush().await;
        assert!(a.query(&Query { limit: 10, ..Default::default() }).unwrap().is_empty());
        assert!(a.verify().unwrap().ok);
    }
}
