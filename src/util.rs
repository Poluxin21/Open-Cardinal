//! Small shared helpers (hex, randomness, atomic file writes, time).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngCore;
use rand::rngs::OsRng;
use subtle::ConstantTimeEq;

use crate::error::Result;

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// `n` bytes from the OS CSPRNG, hex encoded.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    hex(&buf)
}

pub fn random_u64() -> u64 {
    OsRng.next_u64()
}

/// Constant-time equality for secrets.
pub fn secret_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Write `data` to `path` atomically (tmp file + rename). `secret` restricts the
/// permissions to the owner on Unix.
pub fn write_atomic(path: &Path, data: &[u8], secret: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", random_hex(4)));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = secret;
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    #[cfg(windows)]
    if secret {
        restrict_to_current_user(path);
    }
    Ok(())
}

/// Windows has no mode bits: drop inherited permissions and grant access to the current user
/// only (what `0600` does on Unix). Best effort: a failure is logged, not fatal.
#[cfg(windows)]
fn restrict_to_current_user(path: &Path) {
    let Ok(user) = std::env::var("USERNAME") else { return };
    let account = match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{user}"),
        _ => user,
    };
    let res = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{account}:F"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(res, Ok(s) if s.success()) {
        tracing::warn!("could not restrict the permissions of {}; protect it manually", path.display());
    }
}

/// Escape untrusted text before it is written to a log line (log-forging defence).
pub fn log_safe(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(256));
    for c in s.chars().take(256) {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&b), "0001abff");
        assert_eq!(from_hex("0001abff").unwrap(), b);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }

    #[test]
    fn log_safe_escapes_newlines() {
        assert_eq!(log_safe("a\nb\r\x1b[0m"), "a\\nb\\r\\u{1b}[0m");
    }

    #[test]
    fn atomic_write_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        write_atomic(&p, b"one", false).unwrap();
        write_atomic(&p, b"two", false).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
    }
}
