//! Hash chain primitives.

use std::path::Path;

use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{Body, Record};
use crate::error::Result;
use crate::util;

type HmacSha256 = Hmac<Sha256>;

/// Hash of "nothing": the `prev` of the very first record.
pub fn genesis_hash() -> String {
    "0".repeat(64)
}

/// Secret that authenticates the chain.
pub struct ChainKey {
    bytes: Vec<u8>,
}

impl ChainKey {
    pub fn none() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => Ok(Self { bytes: s.trim().as_bytes().to_vec() }),
            _ => {
                let key = util::random_hex(32);
                util::write_atomic(path, key.as_bytes(), true)?;
                Ok(Self { bytes: key.into_bytes() })
            }
        }
    }

    /// Public identifier of the key (never reveals it).
    pub fn fingerprint(&self) -> String {
        util::hex(&Sha256::digest([b"cardinal-audit-fp:".as_slice(), &self.bytes].concat()))[..16].to_string()
    }

    fn mac(&self, prev: &str, body_json: &[u8]) -> String {
        if self.bytes.is_empty() {
            let mut h = Sha256::new();
            h.update(prev.as_bytes());
            h.update(body_json);
            return util::hex(&h.finalize());
        }
        let mut m = HmacSha256::new_from_slice(&self.bytes).expect("hmac accepts any key length");
        m.update(prev.as_bytes());
        m.update(body_json);
        util::hex(&m.finalize().into_bytes())
    }
}

pub fn seal(key: &ChainKey, body: Body) -> Record {
    let json = serde_json::to_vec(&body).expect("body serializes");
    let hash = key.mac(&body.prev, &json);
    Record { body, hash }
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub ok: bool,
    pub checked: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub broken_at: Option<u64>,
    pub reason: Option<String>,
}

impl VerifyReport {
    pub fn ok(checked: u64, first: Option<u64>, last: Option<u64>) -> Self {
        Self { ok: true, checked, first_seq: first, last_seq: last, broken_at: None, reason: None }
    }
    pub fn broken(checked: u64, at: Option<u64>, reason: String) -> Self {
        Self { ok: false, checked, first_seq: None, last_seq: None, broken_at: at, reason: Some(reason) }
    }
}

/// Walk records in sequence order. The first record's `prev` is trusted (older records may
/// have been pruned by retention); every later record must link to its predecessor and
/// carry a valid MAC.
pub fn verify_chain(key: &ChainKey, records: impl Iterator<Item = Result<Record>>) -> Result<VerifyReport> {
    let mut checked = 0u64;
    let mut first = None;
    let mut last: Option<(u64, String)> = None;
    for rec in records {
        let rec = rec?;
        let seq = rec.body.seq;
        match &last {
            Some((prev_seq, prev_hash)) => {
                if seq != prev_seq + 1 {
                    return Ok(VerifyReport::broken(
                        checked,
                        Some(prev_seq + 1),
                        format!("sequence gap: record {} is missing", prev_seq + 1),
                    ));
                }
                if &rec.body.prev != prev_hash {
                    return Ok(VerifyReport::broken(
                        checked,
                        Some(seq),
                        "prev hash does not match the previous record".into(),
                    ));
                }
            }
            None => first = Some(seq),
        }
        let expected = key.mac(&rec.body.prev, &serde_json::to_vec(&rec.body)?);
        if !util::secret_eq(expected.as_bytes(), rec.hash.as_bytes()) {
            return Ok(VerifyReport::broken(checked, Some(seq), "hash mismatch: the record was altered".into()));
        }
        checked += 1;
        last = Some((seq, rec.hash));
    }
    Ok(VerifyReport::ok(checked, first, last.map(|l| l.0)))
}
