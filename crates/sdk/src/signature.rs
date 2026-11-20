// SPDX-License-Identifier: Apache-2.0
//! Request signatures, as ferrobill makes them.
//!
//! Every call between ferrobill and a plugin, either way, carries
//! `ferrobill-signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>`.
//! A GET signs an empty body. The receiver recomputes the HMAC over the raw body, compares in
//! constant time and refuses a timestamp more than five minutes from its own clock.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The header carrying the signature.
pub const HEADER: &str = "ferrobill-signature";

/// How far a signature's timestamp may be from now (seconds), either way.
pub const TOLERANCE: i64 = 300;

fn mac(secret: &str, timestamp: i64, body: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// The header value for `body` sent at `timestamp` (unix seconds).
pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let hex: String = mac(secret, timestamp, body)
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={timestamp},v1={hex}")
}

/// Whether `header` is a valid signature of `body` with `secret`, made within [`TOLERANCE`] of
/// `now`. Compared in constant time.
pub fn verify(secret: &str, header: &str, body: &[u8], now: i64) -> bool {
    let mut t = None;
    let mut v1 = None;
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", x)) => t = x.parse::<i64>().ok(),
            Some(("v1", x)) => v1 = Some(x),
            _ => {}
        }
    }
    let (Some(t), Some(v1)) = (t, v1) else {
        return false;
    };
    if now.abs_diff(t) > TOLERANCE.unsigned_abs() || v1.len() != 64 || !v1.is_ascii() {
        return false;
    }
    let Some(given) = (0..32)
        .map(|i| u8::from_str_radix(&v1[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()
    else {
        return false;
    };
    mac(secret, t, body).verify_slice(&given).is_ok()
}

/// Now, in unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ferrobills_signature() {
        // The same known value as ferrobill's own test (printf '1.x' | openssl dgst -sha256 -hmac key).
        assert_eq!(
            sign("key", 1, b"x"),
            "t=1,v1=fc4913c41a7bf793267e2a384f3261c9436ec2cd097bd745e7b0d99113f510ec"
        );
        let s = sign("secret", 1_700_000_000, b"{}");
        assert!(s.starts_with("t=1700000000,v1="));
        assert_eq!(s.len(), "t=1700000000,v1=".len() + 64);
        assert!(verify("secret", &s, b"{}", 1_700_000_000));
    }

    #[test]
    fn refuses_wrong_secret_body_or_old_timestamp() {
        let s = sign("secret", 1_700_000_000, b"{}");
        assert!(!verify("other", &s, b"{}", 1_700_000_000));
        assert!(!verify("secret", &s, b"{ }", 1_700_000_000));
        assert!(!verify("secret", &s, b"{}", 1_700_000_000 + TOLERANCE + 1));
        assert!(verify("secret", &s, b"{}", 1_700_000_000 + TOLERANCE));
        assert!(!verify("secret", "t=1700000000", b"{}", 1_700_000_000));
        assert!(!verify("secret", "v1=zz", b"{}", 1_700_000_000));
    }
}
