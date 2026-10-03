//! Peer authentication. Every RPC carries `x-cardinal-from: <ip>` and
//! `x-cardinal-auth: <unix-seconds>.<hex HMAC-SHA256(secret, "cardinal-raft|ts|from")>`.
//! The secret itself never travels; a captured header is only replayable inside a
//! 60-second window (enable TLS to close that too).

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tonic::{Request, Status};

use super::types::NodeId;
use crate::util;

type HmacSha256 = Hmac<Sha256>;

pub const HDR_FROM: &str = "x-cardinal-from";
pub const HDR_AUTH: &str = "x-cardinal-auth";
const MAX_SKEW_SECS: u64 = 60;

#[derive(Clone)]
pub struct ClusterAuth {
    secret: Vec<u8>,
}

/// Verified identity of the calling peer, attached to the request by the server interceptor.
#[derive(Clone, Copy, Debug)]
pub struct PeerId(pub NodeId);

impl ClusterAuth {
    pub fn new(secret: &str) -> Self {
        Self { secret: secret.as_bytes().to_vec() }
    }

    fn mac(&self, ts: u64, from: &str) -> HmacSha256 {
        let mut m = HmacSha256::new_from_slice(&self.secret).expect("hmac accepts any key length");
        m.update(format!("cardinal-raft|{ts}|{from}").as_bytes());
        m
    }

    pub fn sign(&self, from: &str, now_secs: u64) -> String {
        format!("{now_secs}.{}", util::hex(&self.mac(now_secs, from).finalize().into_bytes()))
    }

    pub fn verify(&self, from: &str, header: &str, now_secs: u64) -> bool {
        let Some((ts, mac_hex)) = header.split_once('.') else { return false };
        let Ok(ts) = ts.parse::<u64>() else { return false };
        if ts.abs_diff(now_secs) > MAX_SKEW_SECS {
            return false;
        }
        let Some(mac) = util::from_hex(mac_hex) else { return false };
        // `verify_slice` compares in constant time
        self.mac(ts, from).verify_slice(&mac).is_ok()
    }

    /// Outgoing side.
    pub fn client_interceptor(&self, me: NodeId) -> ClientAuth {
        ClientAuth { auth: self.clone(), from: me.to_string() }
    }

    /// Incoming side.
    pub fn server_interceptor(
        &self,
    ) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone + Send + 'static {
        let auth = self.clone();
        move |mut req: Request<()>| {
            let md = req.metadata();
            let from = md.get(HDR_FROM).and_then(|v| v.to_str().ok()).map(str::to_string);
            let header = md.get(HDR_AUTH).and_then(|v| v.to_str().ok()).map(str::to_string);
            let (Some(from), Some(header)) = (from, header) else {
                return Err(Status::unauthenticated("missing peer credentials"));
            };
            if !auth.verify(&from, &header, util::now_secs()) {
                return Err(Status::unauthenticated("bad peer credentials (wrong cluster secret or clock skew > 60s)"));
            }
            let ip: NodeId = from.parse().map_err(|_| Status::unauthenticated("bad peer id"))?;
            req.extensions_mut().insert(PeerId(ip));
            Ok(req)
        }
    }
}

#[derive(Clone)]
pub struct ClientAuth {
    auth: ClusterAuth,
    from: String,
}

impl tonic::service::Interceptor for ClientAuth {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let token = self.auth.sign(&self.from, util::now_secs());
        let md = req.metadata_mut();
        md.insert(HDR_FROM, self.from.parse().map_err(|_| Status::internal("bad from"))?);
        md.insert(HDR_AUTH, token.parse().map_err(|_| Status::internal("bad token"))?);
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_token_verifies_and_is_bound_to_the_sender() {
        let a = ClusterAuth::new("0123456789abcdef");
        let t = a.sign("10.0.0.1", 1_000);
        assert!(a.verify("10.0.0.1", &t, 1_000));
        assert!(a.verify("10.0.0.1", &t, 1_050));
        assert!(!a.verify("10.0.0.2", &t, 1_000), "token must not be reusable for another sender");
    }

    #[test]
    fn stale_future_and_garbage_tokens_fail() {
        let a = ClusterAuth::new("0123456789abcdef");
        let t = a.sign("10.0.0.1", 1_000);
        assert!(!a.verify("10.0.0.1", &t, 1_000 + 61), "replay after the window");
        assert!(!a.verify("10.0.0.1", &t, 1_000 - 61), "future-dated");
        for bad in ["", "x", "1000.zz", "abc.00", "1000."] {
            assert!(!a.verify("10.0.0.1", bad, 1_000), "{bad}");
        }
    }

    #[test]
    fn a_different_secret_is_rejected() {
        let a = ClusterAuth::new("0123456789abcdef");
        let b = ClusterAuth::new("fedcba9876543210");
        assert!(!b.verify("10.0.0.1", &a.sign("10.0.0.1", 5), 5));
    }

    #[test]
    fn server_interceptor_requires_credentials_and_injects_the_peer() {
        let a = ClusterAuth::new("0123456789abcdef");
        let mut check = a.server_interceptor();
        assert!(check(Request::new(())).is_err());

        let mut ok = Request::new(());
        ok.metadata_mut().insert(HDR_FROM, "10.0.0.7".parse().unwrap());
        ok.metadata_mut().insert(HDR_AUTH, a.sign("10.0.0.7", util::now_secs()).parse().unwrap());
        let out = check(ok).unwrap();
        assert_eq!(out.extensions().get::<PeerId>().unwrap().0.to_string(), "10.0.0.7");

        let mut forged = Request::new(());
        forged.metadata_mut().insert(HDR_FROM, "10.0.0.7".parse().unwrap());
        forged.metadata_mut().insert(HDR_AUTH, format!("{}.{}", util::now_secs(), "0".repeat(64)).parse().unwrap());
        assert!(check(forged).is_err());
    }
}
