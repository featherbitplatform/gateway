//! SNI-based routing for TCP TLS passthrough.
//!
//! [`extract_sni`] peeks the server name out of a TLS ClientHello **without
//! terminating TLS**, so the [`SniRouter`] can pick a backend pool by hostname
//! and the raw bytes are relayed on to it. The parser operates on a possibly
//! partial, fully untrusted buffer: it is bounds-checked at every step and
//! **never panics** — a short buffer yields [`SniResult::Incomplete`] (read
//! more), and anything malformed or non-ClientHello yields
//! [`SniResult::NotPresent`] (fall back to the default pool).

use std::sync::Arc;

use crate::balancer::Balancer;

/// Outcome of parsing a (possibly partial) TLS record for the SNI hostname.
#[derive(Debug, PartialEq)]
pub enum SniResult {
    /// SNI hostname found (lowercased).
    Found(String),
    /// The ClientHello parsed fully but carried no SNI, or the bytes are not a
    /// TLS ClientHello at all — route to the default pool.
    NotPresent,
    /// The buffer is shorter than a declared length — read more and retry.
    Incomplete,
}

/// Big-endian `u16` at `buf[p..p+2]`, or `None` if out of bounds.
fn be16(buf: &[u8], p: usize) -> Option<usize> {
    let hi = *buf.get(p)? as usize;
    let lo = *buf.get(p + 1)? as usize;
    Some((hi << 8) | lo)
}

/// Big-endian `u24` at `buf[p..p+3]`, or `None` if out of bounds.
fn be24(buf: &[u8], p: usize) -> Option<usize> {
    let a = *buf.get(p)? as usize;
    let b = *buf.get(p + 1)? as usize;
    let c = *buf.get(p + 2)? as usize;
    Some((a << 16) | (b << 8) | c)
}

/// Max TLS record length per RFC (2^14 + 256 headroom); a larger declared
/// length is treated as malformed.
const MAX_RECORD_LEN: usize = 16_640;

/// Extracts the SNI hostname from the start of a TLS stream. Bounds-safe and
/// panic-free; see the module docs for the [`SniResult`] contract.
pub fn extract_sni(buf: &[u8]) -> SniResult {
    // --- TLS record header (5 bytes) ---
    if buf.len() < 5 {
        return SniResult::Incomplete;
    }
    if buf[0] != 0x16 {
        return SniResult::NotPresent; // not a handshake record
    }
    if buf[1] != 0x03 {
        return SniResult::NotPresent; // not TLS 1.x
    }
    let record_len = match be16(buf, 3) {
        Some(n) if n <= MAX_RECORD_LEN => n,
        Some(_) => return SniResult::NotPresent, // absurd length → malformed
        None => return SniResult::Incomplete,
    };
    let record_end = (5 + record_len).min(buf.len());

    // --- Handshake header (4 bytes) ---
    if record_end < 9 {
        return SniResult::Incomplete;
    }
    if buf[5] != 0x01 {
        return SniResult::NotPresent; // not a ClientHello
    }
    let hs_len = match be24(buf, 6) {
        Some(n) => n,
        None => return SniResult::Incomplete,
    };
    let hs_end = (9 + hs_len).min(record_end);

    // --- ClientHello body ---
    let mut p = 9;
    // legacy_version (2) + random (32)
    p = match advance(p, 2 + 32, hs_end) {
        Some(p) => p,
        None => return SniResult::Incomplete,
    };
    // session_id: 1-byte length + data
    p = match skip_vec(buf, p, 1, hs_end) {
        Skip::Ok(p) => p,
        Skip::Short => return SniResult::Incomplete,
    };
    // cipher_suites: 2-byte length + data
    p = match skip_vec(buf, p, 2, hs_end) {
        Skip::Ok(p) => p,
        Skip::Short => return SniResult::Incomplete,
    };
    // compression_methods: 1-byte length + data
    p = match skip_vec(buf, p, 1, hs_end) {
        Skip::Ok(p) => p,
        Skip::Short => return SniResult::Incomplete,
    };

    // --- extensions block: 2-byte total length ---
    if p == hs_end {
        return SniResult::NotPresent; // ClientHello with no extensions
    }
    let ext_total = match be16(buf, p) {
        Some(n) => n,
        None => return SniResult::Incomplete,
    };
    p += 2;
    let ext_end = (p + ext_total).min(hs_end);

    // --- iterate extensions ---
    while p + 4 <= ext_end {
        let ext_type = match be16(buf, p) {
            Some(n) => n,
            None => return SniResult::Incomplete,
        };
        let ext_len = match be16(buf, p + 2) {
            Some(n) => n,
            None => return SniResult::Incomplete,
        };
        let body_start = p + 4;
        let body_end = body_start + ext_len;
        if body_end > buf.len() {
            return SniResult::Incomplete; // extension body not fully arrived
        }
        if body_end > ext_end {
            return SniResult::NotPresent; // declared length overruns the block
        }
        if ext_type == 0x0000 {
            return parse_server_name(buf, body_start, body_end);
        }
        p = body_end;
    }

    SniResult::NotPresent
}

/// Parses the `server_name` extension body (`buf[start..end]`).
fn parse_server_name(buf: &[u8], start: usize, end: usize) -> SniResult {
    // server_name_list: 2-byte length
    let list_len = match be16(buf, start) {
        Some(n) => n,
        None => return SniResult::Incomplete,
    };
    let mut q = start + 2;
    let list_end = q + list_len;
    if list_end > buf.len() {
        return SniResult::Incomplete;
    }
    if list_end > end {
        return SniResult::NotPresent;
    }

    // Scan entries for the first host_name (type 0x00).
    while q + 3 <= list_end {
        let name_type = buf[q];
        let name_len = match be16(buf, q + 1) {
            Some(n) => n,
            None => return SniResult::Incomplete,
        };
        q += 3;
        let name_end = q + name_len;
        if name_end > buf.len() {
            return SniResult::Incomplete;
        }
        if name_end > list_end {
            return SniResult::NotPresent;
        }
        if name_type == 0x00 {
            return match std::str::from_utf8(&buf[q..name_end]) {
                Ok(s) if !s.is_empty() => SniResult::Found(s.to_ascii_lowercase()),
                _ => SniResult::NotPresent,
            };
        }
        q = name_end; // skip non-host_name entry
    }

    SniResult::NotPresent
}

/// Advances `p` by `n`, or `None` if that would exceed `ceiling`.
fn advance(p: usize, n: usize, ceiling: usize) -> Option<usize> {
    let next = p + n;
    (next <= ceiling).then_some(next)
}

enum Skip {
    Ok(usize),
    Short,
}

/// Skips a length-prefixed vector: reads a `len_bytes`-wide (1 or 2) big-endian
/// length at `p`, then skips that many bytes, all bounded by `ceiling`.
fn skip_vec(buf: &[u8], p: usize, len_bytes: usize, ceiling: usize) -> Skip {
    let len = match len_bytes {
        1 => match buf.get(p) {
            Some(&b) if p < ceiling => b as usize,
            _ => return Skip::Short,
        },
        _ => match be16(buf, p) {
            Some(n) if p + 2 <= ceiling => n,
            _ => return Skip::Short,
        },
    };
    match advance(p + len_bytes, len, ceiling) {
        Some(next) => Skip::Ok(next),
        None => Skip::Short,
    }
}

/// An SNI match pattern: exact hostname or a single-label wildcard. Shared by
/// the L4 stream router and the TLS multi-cert resolver.
#[derive(Debug)]
pub(crate) enum SniPattern {
    Exact(String),
    /// The suffix after `*.`; matches exactly one leading label.
    Wildcard(String),
}

impl SniPattern {
    pub(crate) fn parse(s: &str) -> Self {
        let s = s.to_ascii_lowercase();
        match s.strip_prefix("*.") {
            Some(rest) => SniPattern::Wildcard(rest.to_string()),
            None => SniPattern::Exact(s),
        }
    }

    pub(crate) fn matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self {
            SniPattern::Exact(e) => *e == host,
            SniPattern::Wildcard(base) => match host.strip_suffix(base.as_str()) {
                // Exactly one leading label: the prefix ends with '.', is more
                // than just ".", and has no interior dot.
                Some(prefix) => {
                    prefix.ends_with('.')
                        && prefix.len() > 1
                        && !prefix[..prefix.len() - 1].contains('.')
                }
                None => false,
            },
        }
    }
}

/// Routes a TCP connection to a backend pool by its ClientHello SNI hostname,
/// falling back to a default pool.
pub struct SniRouter {
    routes: Vec<(SniPattern, Arc<Balancer>)>,
    default: Arc<Balancer>,
}

impl SniRouter {
    /// Builds a router from `(server_name, pool)` pairs plus a default pool.
    pub fn new(routes: Vec<(String, Arc<Balancer>)>, default: Arc<Balancer>) -> Self {
        Self {
            routes: routes
                .into_iter()
                .map(|(name, bal)| (SniPattern::parse(&name), bal))
                .collect(),
            default,
        }
    }

    /// Whether any SNI routes are configured (if not, callers skip the peek).
    pub fn has_sni_routes(&self) -> bool {
        !self.routes.is_empty()
    }

    /// Selects the pool for `sni` (first matching route, else the default).
    pub fn select(&self, sni: Option<&str>) -> &Arc<Balancer> {
        if let Some(host) = sni {
            for (pattern, balancer) in &self.routes {
                if pattern.matches(host) {
                    return balancer;
                }
            }
        }
        &self.default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balancer::{Strategy, Target};

    /// Builds a minimal but well-formed TLS ClientHello record carrying `sni`.
    fn build_client_hello(sni: &str) -> Vec<u8> {
        build_client_hello_ext(Some(sni))
    }

    /// Builds a ClientHello with an optional SNI extension.
    fn build_client_hello_ext(sni: Option<&str>) -> Vec<u8> {
        let mut ext = Vec::new();
        if let Some(sni) = sni {
            let name = sni.as_bytes();
            let mut sni_body = Vec::new();
            let entry_len = 1 + 2 + name.len();
            sni_body.extend_from_slice(&(entry_len as u16).to_be_bytes()); // list len
            sni_body.push(0x00); // host_name
            sni_body.extend_from_slice(&(name.len() as u16).to_be_bytes());
            sni_body.extend_from_slice(name);
            ext.extend_from_slice(&0x0000u16.to_be_bytes()); // type: server_name
            ext.extend_from_slice(&(sni_body.len() as u16).to_be_bytes());
            ext.extend_from_slice(&sni_body);
        }

        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // legacy_version
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0x00); // session_id len 0
        body.extend_from_slice(&2u16.to_be_bytes()); // cipher_suites len
        body.extend_from_slice(&[0x00, 0x2f]); // one suite
        body.push(0x01); // compression len
        body.push(0x00); // null compression
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes()); // extensions len
        body.extend_from_slice(&ext);

        let mut hs = vec![0x01]; // ClientHello
        hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]); // 3-byte len
        hs.extend_from_slice(&body);

        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    #[test]
    fn test_extract_sni_found() {
        let hello = build_client_hello("example.com");
        assert_eq!(extract_sni(&hello), SniResult::Found("example.com".into()));
    }

    #[test]
    fn test_extract_sni_lowercases() {
        let hello = build_client_hello("Example.COM");
        assert_eq!(extract_sni(&hello), SniResult::Found("example.com".into()));
    }

    #[test]
    fn test_extract_sni_no_extension() {
        let hello = build_client_hello_ext(None);
        assert_eq!(extract_sni(&hello), SniResult::NotPresent);
    }

    #[test]
    fn test_extract_sni_not_tls() {
        assert_eq!(
            extract_sni(&[0x17, 0x03, 0x01, 0x00, 0x05]),
            SniResult::NotPresent
        );
        assert_eq!(extract_sni(&[0xff; 20]), SniResult::NotPresent);
    }

    #[test]
    fn test_extract_sni_empty_and_truncated() {
        assert_eq!(extract_sni(&[]), SniResult::Incomplete);
        assert_eq!(extract_sni(&[0x16, 0x03]), SniResult::Incomplete);
        let hello = build_client_hello("example.com");
        // Cut off the last 5 bytes (mid server name).
        assert_eq!(
            extract_sni(&hello[..hello.len() - 5]),
            SniResult::Incomplete
        );
    }

    #[test]
    fn test_extract_sni_never_panics_on_any_truncation() {
        // Fuzz: every prefix length must return a variant without panicking.
        let hello = build_client_hello("api.internal.example.com");
        for n in 0..=hello.len() {
            let _ = extract_sni(&hello[..n]);
        }
        // And full length still resolves to the SNI.
        assert_eq!(
            extract_sni(&hello),
            SniResult::Found("api.internal.example.com".into())
        );
    }

    #[test]
    fn test_extract_sni_absurd_record_len() {
        // Handshake byte + record length claiming 60000 bytes.
        let mut buf = vec![0x16, 0x03, 0x01];
        buf.extend_from_slice(&60000u16.to_be_bytes());
        buf.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
        assert_eq!(extract_sni(&buf), SniResult::NotPresent);
    }

    #[test]
    fn test_pattern_exact() {
        let p = SniPattern::parse("api.example.com");
        assert!(p.matches("api.example.com"));
        assert!(p.matches("API.EXAMPLE.COM"));
        assert!(!p.matches("x.example.com"));
        assert!(!p.matches("example.com"));
    }

    #[test]
    fn test_pattern_wildcard_one_label() {
        let p = SniPattern::parse("*.example.com");
        assert!(p.matches("a.example.com"));
        assert!(p.matches("A.Example.com"));
        assert!(!p.matches("example.com")); // needs a leading label
        assert!(!p.matches("a.b.example.com")); // exactly one label
        assert!(!p.matches("xexample.com")); // not a label boundary
    }

    fn balancer(host: &str) -> Arc<Balancer> {
        Arc::new(
            Balancer::new(
                vec![Target {
                    host: host.into(),
                    port: 443,
                }],
                Strategy::RoundRobin,
            )
            .unwrap(),
        )
    }

    #[test]
    fn test_router_select() {
        let router = SniRouter::new(
            vec![
                ("a.example.com".into(), balancer("a")),
                ("*.b.example.com".into(), balancer("b")),
            ],
            balancer("default"),
        );
        assert_eq!(router.select(Some("a.example.com")).target(0).host, "a");
        assert_eq!(router.select(Some("x.b.example.com")).target(0).host, "b");
        assert_eq!(
            router.select(Some("unmatched.com")).target(0).host,
            "default"
        );
        assert_eq!(router.select(None).target(0).host, "default");
        assert!(router.has_sni_routes());
    }
}
