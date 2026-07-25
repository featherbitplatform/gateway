//! Encrypted client-side session cookies.
//!
//! The primitive that lets stateless SSO plugins (`openid-connect`,
//! `cas-auth`, `authz-casdoor`) run interactive login flows without any
//! server-side session store: the session payload (tokens, claims, or the
//! transient auth-flow state) is sealed into an authenticated, encrypted
//! cookie with an embedded expiry. Any gateway instance sharing the signing
//! secret can open any cookie, so this works across a horizontally-scaled
//! deployment with no coordination.
//!
//! Sealing uses AES-256-GCM (via `ring`) with a per-message random nonce; the
//! 256-bit key is derived from the configured secret by SHA-256, so a secret
//! of any length is accepted. The expiry is part of the authenticated
//! plaintext, so it cannot be tampered with.
//!
//! Trade-offs inherent to client-side sessions (shared with APISIX's default
//! cookie sessions): there is no cheap server-side revocation before expiry
//! (use short lifetimes), and cookies are capped near 4 KB, so only essential
//! data should be stored.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};

/// Failure opening a sealed cookie. Every variant means "treat the request as
/// unauthenticated" — callers should never distinguish these to a client.
#[derive(Debug, PartialEq)]
pub enum CookieError {
    /// The value is not valid base64url or is too short to contain a nonce.
    Malformed,
    /// Authentication/decryption failed (wrong key or tampered value).
    BadSeal,
    /// The sealed payload's expiry is in the past.
    Expired,
}

impl std::fmt::Display for CookieError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Malformed => "malformed cookie",
            Self::BadSeal => "cookie failed authentication",
            Self::Expired => "cookie expired",
        };
        f.write_str(s)
    }
}

/// Seals and opens encrypted session cookies with a fixed derived key.
pub struct CookieSealer {
    key_bytes: [u8; 32],
    rng: SystemRandom,
}

impl CookieSealer {
    /// Derives the AES-256 key from `secret` (SHA-256), so any-length secrets
    /// work. The same secret must be configured on every gateway instance.
    pub fn new(secret: &str) -> Self {
        let d = digest(&SHA256, secret.as_bytes());
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(d.as_ref());
        Self {
            key_bytes,
            rng: SystemRandom::new(),
        }
    }

    fn key(&self) -> LessSafeKey {
        LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &self.key_bytes)
                .expect("AES-256-GCM key from 32 bytes is always valid"),
        )
    }

    /// Seals `payload` into a cookie value that expires after `ttl`.
    ///
    /// Layout of the returned base64url string:
    /// `nonce(12) || AES-256-GCM(expiry_be_u64(8) || payload)`. The expiry is
    /// authenticated, so an attacker cannot extend a session.
    pub fn seal(&self, payload: &[u8], ttl: Duration) -> String {
        let expiry = now_unix().saturating_add(ttl.as_secs());

        let mut plaintext = Vec::with_capacity(8 + payload.len());
        plaintext.extend_from_slice(&expiry.to_be_bytes());
        plaintext.extend_from_slice(payload);

        let mut nonce_bytes = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce_bytes)
            .expect("system RNG must produce a nonce");
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        self.key()
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut plaintext)
            .expect("sealing never fails for a valid key/nonce");

        let mut out = Vec::with_capacity(NONCE_LEN + plaintext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&plaintext);
        URL_SAFE_NO_PAD.encode(out)
    }

    /// Opens a sealed cookie value, returning the original payload.
    ///
    /// Fails with [`CookieError`] if the value is malformed, authentication
    /// fails (wrong key or tampering), or the embedded expiry has passed.
    pub fn open(&self, value: &str) -> Result<Vec<u8>, CookieError> {
        let raw = URL_SAFE_NO_PAD
            .decode(value.as_bytes())
            .map_err(|_| CookieError::Malformed)?;
        // Need at least nonce + the GCM tag (16) + 8-byte expiry.
        if raw.len() < NONCE_LEN + 16 + 8 {
            return Err(CookieError::Malformed);
        }
        let (nonce_bytes, sealed) = raw.split_at(NONCE_LEN);
        let mut in_out = sealed.to_vec();
        let nonce =
            Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| CookieError::Malformed)?;

        let plaintext = self
            .key()
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| CookieError::BadSeal)?;

        if plaintext.len() < 8 {
            return Err(CookieError::BadSeal);
        }
        let mut expiry_bytes = [0u8; 8];
        expiry_bytes.copy_from_slice(&plaintext[..8]);
        let expiry = u64::from_be_bytes(expiry_bytes);
        if now_unix() > expiry {
            return Err(CookieError::Expired);
        }
        Ok(plaintext[8..].to_vec())
    }
}

/// `SameSite` attribute for a `Set-Cookie` header.
// Full attribute set for completeness; the SSO flows currently only emit `Lax`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

/// Attributes for building a `Set-Cookie` header value.
pub struct CookieAttrs<'a> {
    pub path: &'a str,
    /// `Max-Age` in seconds; `None` omits it (session cookie), `Some(0)`
    /// deletes the cookie.
    pub max_age: Option<u64>,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: SameSite,
}

impl Default for CookieAttrs<'_> {
    fn default() -> Self {
        Self {
            path: "/",
            max_age: None,
            http_only: true,
            secure: true,
            same_site: SameSite::Lax,
        }
    }
}

/// Builds a `Set-Cookie` header value for `name=value` with `attrs`.
pub fn build_set_cookie(name: &str, value: &str, attrs: &CookieAttrs) -> String {
    let mut s = format!("{}={}; Path={}", name, value, attrs.path);
    if let Some(max_age) = attrs.max_age {
        s.push_str(&format!("; Max-Age={}", max_age));
    }
    if attrs.http_only {
        s.push_str("; HttpOnly");
    }
    if attrs.secure {
        s.push_str("; Secure");
    }
    s.push_str(&format!("; SameSite={}", attrs.same_site.as_str()));
    s
}

/// Whether a cookie scoped to `cookie_path` is sent by the browser on a request
/// to `request_path` (RFC 6265 §5.1.4 path-match): an exact match, or
/// `cookie_path` is a prefix ending at a `/` boundary. `/` (or empty) covers
/// everything.
///
/// Interactive-login plugins use this to reject a `session.cookie.path` that
/// would starve their OAuth callback of the session/flow cookie (which loops
/// login forever).
pub fn path_covers(cookie_path: &str, request_path: &str) -> bool {
    let cp = cookie_path.trim_end_matches('/');
    if cp.is_empty() {
        return true;
    }
    request_path == cp || request_path.starts_with(&format!("{}/", cp))
}

/// Reads a named cookie from a `Cookie` header value (`a=1; b=2`).
pub fn read_cookie<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(v);
            }
        }
    }
    None
}

/// A `Max-Age=0` deletion cookie for `name`.
pub fn delete_cookie(name: &str, path: &str) -> String {
    build_set_cookie(
        name,
        "",
        &CookieAttrs {
            path,
            max_age: Some(0),
            ..Default::default()
        },
    )
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seal_open_round_trip() {
        let sealer = CookieSealer::new("my-signing-secret");
        let payload = br#"{"sub":"alice","tok":"xyz"}"#;
        let cookie = sealer.seal(payload, Duration::from_secs(3600));
        assert_eq!(sealer.open(&cookie).unwrap(), payload);
    }

    #[test]
    fn test_wrong_key_fails() {
        let a = CookieSealer::new("secret-a");
        let b = CookieSealer::new("secret-b");
        let cookie = a.seal(b"hello", Duration::from_secs(60));
        assert_eq!(b.open(&cookie), Err(CookieError::BadSeal));
    }

    #[test]
    fn test_tamper_fails() {
        let sealer = CookieSealer::new("k");
        let mut cookie = sealer.seal(b"hello", Duration::from_secs(60));
        // Flip a character in the middle of the ciphertext.
        let mid = cookie.len() / 2;
        let ch = cookie.as_bytes()[mid];
        let replacement = if ch == b'A' { 'B' } else { 'A' };
        cookie.replace_range(mid..mid + 1, &replacement.to_string());
        assert!(matches!(
            sealer.open(&cookie),
            Err(CookieError::BadSeal) | Err(CookieError::Malformed)
        ));
    }

    #[test]
    fn test_expiry_enforced() {
        let sealer = CookieSealer::new("k");
        let cookie = sealer.seal(b"hello", Duration::from_secs(0));
        // ttl 0 → expiry == now; a moment later it is in the past.
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(sealer.open(&cookie), Err(CookieError::Expired));
    }

    #[test]
    fn test_malformed() {
        let sealer = CookieSealer::new("k");
        assert_eq!(sealer.open("not base64!!!"), Err(CookieError::Malformed));
        assert_eq!(sealer.open("aGVsbG8"), Err(CookieError::Malformed)); // too short
    }

    #[test]
    fn test_path_covers() {
        assert!(path_covers("/", "/anything"));
        assert!(path_covers("", "/anything"));
        assert!(path_covers("/app_a", "/app_a"));
        assert!(path_covers("/app_a", "/app_a/callback"));
        assert!(path_covers("/app_a/", "/app_a/callback"));
        // Prefix that is not a path boundary must NOT match.
        assert!(!path_covers("/app_a", "/app_ab"));
        assert!(!path_covers("/app_a", "/app_b/callback"));
    }

    #[test]
    fn test_read_cookie() {
        assert_eq!(read_cookie("a=1; session=xyz; b=2", "session"), Some("xyz"));
        assert_eq!(read_cookie("a=1", "session"), None);
        assert_eq!(read_cookie("session=abc", "session"), Some("abc"));
    }

    #[test]
    fn test_build_set_cookie() {
        let c = build_set_cookie(
            "session",
            "val",
            &CookieAttrs {
                path: "/app",
                max_age: Some(3600),
                http_only: true,
                secure: true,
                same_site: SameSite::Lax,
            },
        );
        assert_eq!(
            c,
            "session=val; Path=/app; Max-Age=3600; HttpOnly; Secure; SameSite=Lax"
        );
        assert_eq!(
            delete_cookie("session", "/"),
            "session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax"
        );
    }
}
