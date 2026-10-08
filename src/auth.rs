//! Request authentication: Ed25519 signatures over a per-request message,
//! enforced per source IP.
//!
//! A request from a source whose rule says `required = true` must carry,
//! signed with the client's Ed25519 private key:
//!
//!   message = "mademind-auth-v1\n{client}\n{ts}\n{METHOD} {path?query}\n{sha256-hex(body)}"
//!
//!   X-Mademind-Client:    client id (must match an allowed key)
//!   X-Mademind-Timestamp: unix seconds (accepted within ±tolerance_secs)
//!   X-Mademind-Signature: base64(Ed25519(message))
//!
//! Allowed clients come from [auth] clients: { id, public_key }, where
//! public_key is an OpenSSH ssh-ed25519 public key line body.
//! Replay protection: a signature that already passed within the timestamp
//! window is rejected (in-memory seen-cache). A captured request can't be
//! replayed or altered (method/path/body are bound into the signature).

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use sha2::Digest;

use crate::config::{AuthConfig, AuthRule};

pub const AUTH_VERSION: &str = "mademind-auth-v1";
pub const HDR_CLIENT: &str = "x-mademind-client";
pub const HDR_TIMESTAMP: &str = "x-mademind-timestamp";
pub const HDR_SIGNATURE: &str = "x-mademind-signature";

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    UnknownClient,
    BadTimestamp,
    Expired,
    BadSignature,
    Replay,
}

type SeenCache = HashMap<String, i64>; // signature -> expiry (unix secs)

#[derive(Debug)]
pub struct AuthState {
    keys: HashMap<String, ed25519_dalek::VerifyingKey>,
    tolerance: i64,
    seen: Mutex<SeenCache>,
    rules: Vec<AuthRule>,
}

impl AuthState {
    /// First rule (top to bottom) whose `ips` contains `src`. Always yields a
    /// rule when the config has an `any` catch-all; None otherwise.
    fn rule_for(&self, src: &str) -> Option<&AuthRule> {
        self.rules
            .iter()
            .find(|r| r.ips.iter().any(|ip| ip_in_cidr(src, ip)))
    }

    #[allow(clippy::too_many_arguments)]
    fn verify(
        &self,
        client: &str,
        ts: &str,
        sig: &str,
        method: &str,
        url: &str,
        body: &[u8],
        now: i64,
    ) -> Result<(), AuthError> {
        let key = self.keys.get(client).ok_or(AuthError::UnknownClient)?;
        if !ts.bytes().all(|b| b.is_ascii_digit()) || ts.len() > 20 {
            return Err(AuthError::BadTimestamp);
        }
        let ts: i64 = ts.parse().map_err(|_| AuthError::BadTimestamp)?;
        let dt = now - ts;
        if dt < -self.tolerance || dt > self.tolerance {
            return Err(AuthError::Expired);
        }
        let message = signing_message(client, ts, method, url, body);
        let sig_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, sig)
            .map_err(|_| AuthError::BadSignature)?;
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes)
            .map_err(|_| AuthError::BadSignature)?;
        key.verify_strict(message.as_bytes(), &sig)
            .map_err(|_| AuthError::BadSignature)?;
        // Signature valid: record it so the exact same signature is rejected
        // while it is still inside the accepted timestamp window.
        let mut seen = self.seen.lock().unwrap();
        if seen.len() > 4096 {
            seen.retain(|_, exp| *exp > now);
        }
        if seen.insert(sig.to_string(), ts + self.tolerance).is_some() {
            return Err(AuthError::Replay);
        }
        Ok(())
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Enforce authentication for one request from source `src`. `None` state =
/// auth disabled. Resolution: no matching rule -> 403; matching rule with
/// `required = false` -> allow (no signature); else the client id must be in
/// the rule's `clients` AND the signature must verify. `url` is the request
/// target as sent (path + query).
pub fn check(
    auth: Option<&AuthState>,
    src: &str,
    method: &str,
    url: &str,
    headers: &HeaderMap,
    body: &[u8],
    now: i64,
) -> Result<(), (u16, &'static str)> {
    let Some(a) = auth else {
        return Ok(());
    };
    let Some(rule) = a.rule_for(src) else {
        return Err((403, "no auth rule for source"));
    };
    if !rule.required {
        return Ok(());
    }
    let client = header_value(headers, HDR_CLIENT).ok_or((401, "missing X-Mademind-Client"))?;
    if !rule.clients.iter().any(|c| c == &client) {
        return Err((401, "client not allowed for this source"));
    }
    let ts = header_value(headers, HDR_TIMESTAMP).ok_or((401, "missing X-Mademind-Timestamp"))?;
    let sig = header_value(headers, HDR_SIGNATURE).ok_or((401, "missing X-Mademind-Signature"))?;
    a.verify(&client, &ts, &sig, method, url, body, now)
        .map_err(|e| match e {
            AuthError::UnknownClient => (401, "unknown client"),
            AuthError::BadTimestamp | AuthError::Expired => (401, "bad or expired timestamp"),
            AuthError::BadSignature => (401, "bad signature"),
            AuthError::Replay => (401, "replayed signature"),
        })
}

/// The exact bytes a client signs (and the server verifies) for one request.
/// `url` is the request target as sent: path plus `?query`.
pub fn signing_message(client: &str, ts: i64, method: &str, url: &str, body: &[u8]) -> String {
    let digest = sha256_hex(body);
    format!("{AUTH_VERSION}\n{client}\n{ts}\n{method} {url}\n{digest}")
}

/// `ssh-ed25519 <base64 blob>`: the public-key form `[auth] clients` takes.
pub fn public_key_line(raw: &[u8; 32]) -> String {
    let mut blob = Vec::with_capacity(4 + 11 + 4 + 32);
    blob.extend_from_slice(&11u32.to_be_bytes());
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&32u32.to_be_bytes());
    blob.extend_from_slice(raw);
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, blob);
    format!("ssh-ed25519 {b64}")
}

/// Parse a single OpenSSH public-key line body (`ssh-ed25519 <base64>`) into
/// a raw 32-byte verifying key. Returns the raw key or a reason to skip it.
fn parse_public_key(body: &str) -> Result<[u8; 32], String> {
    let mut parts = body.split_whitespace();
    let kind = parts.next().unwrap_or("");
    let b64 = parts.next().unwrap_or("");
    if kind != "ssh-ed25519" {
        return Err("not an ssh-ed25519 key".into());
    }
    let payload = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
        .map_err(|_| "bad base64".to_string())?;
    // OpenSSH blob: len(11) "ssh-ed25519" len(32) <32-byte raw key>
    if payload.len() != 4 + 11 + 4 + 32 || &payload[4..15] != b"ssh-ed25519" {
        return Err("bad key blob".into());
    }
    Ok(payload[19..51].try_into().unwrap())
}

/// Does `src` (IPv4 or IPv6, no port) fall inside `cidr` ("a.b.c.d/N" or
/// "2001:db8::/48")? "any" matches everything. Malformed input -> false.
fn ip_in_cidr(src: &str, cidr: &str) -> bool {
    if cidr.eq_ignore_ascii_case("any") {
        return true;
    }
    let (net, bits) = match cidr.split_once('/') {
        Some((a, b)) => match b.parse::<u32>() {
            Ok(n) => (a, n),
            Err(_) => return false,
        },
        None => (cidr, 0), // bare address == /prefix of its own size
    };
    let (Ok(src_ip), Ok(net_ip)) = (IpAddr::from_str(src), IpAddr::from_str(net)) else {
        return false;
    };
    let (src_b, net_b): (Vec<u8>, Vec<u8>) = match (src_ip, net_ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => (a.octets().to_vec(), b.octets().to_vec()),
        (IpAddr::V6(a), IpAddr::V6(b)) => (a.octets().to_vec(), b.octets().to_vec()),
        _ => return false, // v4 vs v6 mismatch
    };
    let max = src_b.len() as u32 * 8;
    if bits > max {
        return false;
    }
    let bits = if cidr.contains('/') { bits } else { max };
    let full = (bits / 8) as usize;
    if src_b[..full] != net_b[..full] {
        return false;
    }
    let rem = bits % 8;
    if rem == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem);
    src_b[full] & mask == net_b[full] & mask
}

fn sha256_hex(data: &[u8]) -> String {
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Build auth state from config. No rules -> None (disabled). A rule naming
/// an undefined client is a loud exit (never run with the wrong allowlist).
pub fn load_auth_state(cfg: &AuthConfig) -> Option<Arc<AuthState>> {
    if cfg.rules.is_empty() {
        eprintln!("mademind: auth disabled (no [auth] rules)");
        return None;
    }
    let mut keys = HashMap::new();
    for c in &cfg.clients {
        match parse_public_key(&c.public_key) {
            Ok(raw) => match ed25519_dalek::VerifyingKey::from_bytes(&raw) {
                Ok(k) => {
                    keys.insert(c.id.clone(), k);
                }
                Err(e) => eprintln!("mademind: auth: bad key for client {}: {e}", c.id),
            },
            Err(e) => eprintln!("mademind: auth: skipping client {}: {e}", c.id),
        }
    }
    let known: HashSet<&str> = keys.keys().map(|s| s.as_str()).collect();
    for r in &cfg.rules {
        for id in &r.clients {
            if !known.contains(id.as_str()) {
                eprintln!(
                    "mademind: auth: rule references undefined client {id:?} — refusing to start"
                );
                std::process::exit(3);
            }
        }
    }
    eprintln!(
        "mademind: auth on — {} client(s): {}, {} rule(s)",
        keys.len(),
        keys.keys()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        cfg.rules.len()
    );
    Some(Arc::new(AuthState {
        keys,
        tolerance: cfg.tolerance_secs,
        seen: Mutex::new(HashMap::new()),
        rules: cfg.rules.clone(),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

    pub fn test_keypair(client_id: &str) -> (SigningKey, VerifyingKey, String) {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let vk = sk.verifying_key();
        // authorized_keys line: ssh-ed25519 <base64(len(11)+algo+len(32)+raw)> <comment>
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(vk.as_bytes());
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &blob);
        (sk, vk, format!("ssh-ed25519 {b64} {client_id}"))
    }

    pub fn signed_headers(
        sk: &SigningKey,
        client: &str,
        ts: i64,
        method: &str,
        url: &str,
        body: &[u8],
    ) -> (String, String, String) {
        let message = format!(
            "{AUTH_VERSION}\n{client}\n{ts}\n{method} {url}\n{}",
            sha256_hex(body)
        );
        let sig = sk.sign(message.as_bytes());
        let b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sig.to_bytes());
        (client.to_string(), ts.to_string(), b64)
    }

    pub fn auth_state_for(vk: VerifyingKey, client: &str, tolerance: i64) -> AuthState {
        AuthState {
            keys: HashMap::from([(client.to_string(), vk)]),
            tolerance,
            seen: Mutex::new(HashMap::new()),
            rules: vec![AuthRule {
                ips: vec!["any".into()],
                required: true,
                clients: vec![client.to_string()],
            }],
        }
    }

    #[test]
    fn ip_in_cidr_matches_v4_and_v6() {
        assert!(ip_in_cidr("127.0.0.1", "127.0.0.1/32"));
        assert!(!ip_in_cidr("127.0.0.2", "127.0.0.1/32"));
        assert!(ip_in_cidr("192.168.1.100", "192.168.1.0/24"));
        assert!(!ip_in_cidr("192.168.2.1", "192.168.1.0/24"));
        assert!(ip_in_cidr("10.0.0.1", "10.0.0.0/8"));
        assert!(!ip_in_cidr("11.0.0.1", "10.0.0.0/8"));
        assert!(ip_in_cidr("10.0.0.130", "10.0.0.128/25"));
        assert!(!ip_in_cidr("10.0.0.127", "10.0.0.128/25"));
        assert!(ip_in_cidr("8.8.8.8", "0.0.0.0/0"));
        // bare address = exact match
        assert!(ip_in_cidr("10.1.2.3", "10.1.2.3"));
        assert!(!ip_in_cidr("10.1.2.4", "10.1.2.3"));
        // v6
        assert!(ip_in_cidr("::1", "::1/128"));
        assert!(ip_in_cidr("2001:db8::42", "2001:db8::/32"));
        assert!(!ip_in_cidr("2001:db9::1", "2001:db8::/32"));
        // mismatched families never match
        assert!(!ip_in_cidr("::1", "127.0.0.1/32"));
        assert!(!ip_in_cidr("127.0.0.1", "::1/128"));
        // any + malformed
        assert!(ip_in_cidr("1.2.3.4", "any"));
        assert!(!ip_in_cidr("not-an-ip", "10.0.0.0/8"));
        assert!(!ip_in_cidr("10.0.0.1", "10.0.0.0/33"));
    }

    #[test]
    fn rule_for_picks_first_matching_cidr() {
        let (_sk, vk, _line) = test_keypair("laptop");
        let a = AuthState {
            keys: HashMap::from([("laptop".to_string(), vk)]),
            tolerance: 300,
            seen: Mutex::new(HashMap::new()),
            rules: vec![
                AuthRule {
                    ips: vec!["127.0.0.1/32".into(), "::1/128".into()],
                    required: false,
                    clients: vec![],
                },
                AuthRule {
                    ips: vec!["192.168.1.0/24".into()],
                    required: true,
                    clients: vec!["laptop".into()],
                },
                AuthRule {
                    ips: vec!["any".into()],
                    required: true,
                    clients: vec!["laptop".into()],
                },
            ],
        };
        assert!(!a.rule_for("127.0.0.1").unwrap().required);
        assert!(a.rule_for("192.168.1.55").unwrap().required);
        assert!(a.rule_for("8.8.8.8").unwrap().required); // falls to any
    }

    #[test]
    fn public_key_line_parses_back() {
        let (_, vk, _) = test_keypair("x");
        let line = public_key_line(vk.as_bytes());
        assert_eq!(parse_public_key(&line).unwrap(), *vk.as_bytes());
    }

    #[test]
    fn parse_public_key_roundtrips_ed25519() {
        let (_sk, _vk, line) = test_keypair("laptop");
        let body = line
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(parse_public_key(&body).expect("should parse").len(), 32);
        assert!(parse_public_key("ssh-rsa AAAA...").is_err());
        assert!(parse_public_key("ssh-ed25519 !!!not-base64!!!").is_err());
    }

    #[test]
    fn verify_accepts_valid_signature() {
        let (sk, vk, _) = test_keypair("laptop");
        let a = auth_state_for(vk, "laptop", 300);
        let (c, t, s) = signed_headers(&sk, "laptop", 1_000_000, "POST", "/query", b"{}");
        assert!(a
            .verify(&c, &t, &s, "POST", "/query", b"{}", 1_000_100)
            .is_ok());
    }

    #[test]
    fn verify_rejects_replay_of_same_signature() {
        let (sk, vk, _) = test_keypair("laptop");
        let a = auth_state_for(vk, "laptop", 300);
        let (c, t, s) = signed_headers(&sk, "laptop", 1_000_000, "POST", "/query", b"{}");
        assert!(a
            .verify(&c, &t, &s, "POST", "/query", b"{}", 1_000_100)
            .is_ok());
        assert_eq!(
            a.verify(&c, &t, &s, "POST", "/query", b"{}", 1_000_100),
            Err(AuthError::Replay)
        );
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let (sk, vk, _) = test_keypair("laptop");
        let a = auth_state_for(vk, "laptop", 300);
        let (c, t, s) = signed_headers(&sk, "laptop", 1_000_000, "POST", "/query", b"{}");
        assert_eq!(
            a.verify(&c, &t, &s, "POST", "/query", b"CHANGED", 1_000_100),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_unknown_client_and_expired() {
        let (sk, vk, _) = test_keypair("laptop");
        let a = auth_state_for(vk, "laptop", 300);
        let (c, t, s) = signed_headers(&sk, "laptop", 1_000_000, "GET", "/file", b"");
        assert_eq!(
            a.verify("other", &t, &s, "GET", "/file", b"", 1_000_100),
            Err(AuthError::UnknownClient)
        );
        let (c2, t2, s2) = signed_headers(&sk, "laptop", 100_000, "GET", "/file", b"");
        assert_eq!(
            a.verify(&c2, &t2, &s2, "GET", "/file", b"", 1_000_000),
            Err(AuthError::Expired)
        );
        assert_eq!(
            a.verify(&c, "not-a-number", &s, "GET", "/file", b"", 1_000_100),
            Err(AuthError::BadTimestamp)
        );
    }
}
