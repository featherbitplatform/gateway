# Upstream mTLS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `upstream` node can present a client certificate (and trust a private CA) when connecting to TLS backends, for both HTTPS proxying and wss WebSocket relays.

**Architecture:** A new `UpstreamTls` identity type (`src/outbound/tls.rs`) loads and validates cert materials at policy-compile time and exposes a content-hash cache key. `OutboundClient` and the wss connector keep identity-keyed caches of built clients/connectors; a process-wide registry lets the WebSocket relay (which only sees JSON context values) resolve an identity from its key. Spec: `docs/superpowers/specs/2026-07-25-upstream-mtls-design.md`.

**Tech Stack:** Rust, rustls 0.23 (ring provider), hyper-rustls, tokio-rustls, rustls-pemfile 2, rcgen 0.13 (dev-dep, already present).

## Global Constraints

- Conventional Commits; **no** Co-Authored-By trailer (CLAUDE.md).
- Work happens on the `feature/mtls` branch (gitflow).
- rustls provider is pinned to ring via `crate::server::tls::install_crypto_provider()` — call it before building any rustls config.
- `None`/absent mTLS config must preserve today's behavior byte-for-byte (existing tests must keep passing).
- Comment density/idiom: match existing files (`src/outbound/mod.rs` doc-comment style).
- Run `graphify update .` after code changes (CLAUDE.md).

---

### Task 1: `UpstreamTls` — load, validate, hash, registry

**Files:**
- Create: `src/outbound/tls.rs`
- Modify: `src/outbound/mod.rs` (add `pub mod tls;` after the imports; change `struct NoVerification` to `pub(crate) struct NoVerification(pub(crate) rustls::crypto::CryptoProvider);`)
- Test: inline `#[cfg(test)]` in `src/outbound/tls.rs`

**Interfaces:**
- Consumes: `crate::server::tls::install_crypto_provider()`, `super::NoVerification`.
- Produces (used by Tasks 2–4):
  - `UpstreamTls::load(client_paths: Option<(&str, &str)>, ca_path: Option<&str>, verify: bool) -> Result<Arc<UpstreamTls>, String>`
  - `UpstreamTls::cache_key(&self) -> u64`
  - `UpstreamTls::client_config(&self) -> Result<rustls::ClientConfig, String>` (no ALPN set — callers set it)
  - `UpstreamTls::register(this: &Arc<UpstreamTls>)` / `UpstreamTls::lookup(key: u64) -> Option<Arc<UpstreamTls>>`

- [ ] **Step 1: Write the failing tests**

In a new `src/outbound/tls.rs`, write the module skeleton (docs + empty impl blocks are fine) and these tests. Test helpers use rcgen like `src/server/tls.rs` does; write PEMs under `std::env::temp_dir()` with pid-suffixed names.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// CA + a leaf signed by it, written as PEM files. Returns
    /// (cert_path, key_path, ca_path).
    fn write_identity(tag: &str) -> (String, String, String) {
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let leaf_params = rcgen::CertificateParams::new(vec!["client".to_string()]).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert = dir.join(format!("featherbit_utls_{}_{}.crt", tag, pid));
        let key = dir.join(format!("featherbit_utls_{}_{}.key", tag, pid));
        let ca = dir.join(format!("featherbit_utls_{}_{}.ca.crt", tag, pid));
        std::fs::write(&cert, leaf_cert.pem()).unwrap();
        std::fs::write(&key, leaf_key.serialize_pem()).unwrap();
        std::fs::write(&ca, ca_cert.pem()).unwrap();
        (
            cert.to_str().unwrap().to_string(),
            key.to_str().unwrap().to_string(),
            ca.to_str().unwrap().to_string(),
        )
    }

    #[test]
    fn test_upstream_tls_load_client_and_ca() {
        let (cert, key, ca) = write_identity("happy");
        let id = UpstreamTls::load(Some((&cert, &key)), Some(&ca), true).unwrap();
        assert!(id.verify);
        // Dry-built once in load(); building again also works.
        assert!(id.client_config().is_ok());
    }

    #[test]
    fn test_upstream_tls_load_ca_only() {
        let (_, _, ca) = write_identity("caonly");
        assert!(UpstreamTls::load(None, Some(&ca), true).is_ok());
    }

    #[test]
    fn test_upstream_tls_load_missing_file_errors() {
        let err = UpstreamTls::load(Some(("/nonexistent.crt", "/nonexistent.key")), None, true)
            .unwrap_err();
        assert!(err.contains("/nonexistent.crt"), "err was: {}", err);
    }

    #[test]
    fn test_upstream_tls_load_garbage_key_errors() {
        let (cert, key, _) = write_identity("garbage");
        std::fs::write(&key, "not a pem").unwrap();
        assert!(UpstreamTls::load(Some((&cert, &key)), None, true).is_err());
    }

    #[test]
    fn test_upstream_tls_load_mismatched_key_errors() {
        // Key from a *different* identity: rustls 0.23 rejects the pair
        // (InconsistentKeys) during the dry ClientConfig build.
        let (cert, _, _) = write_identity("mismatch_a");
        let (_, other_key, _) = write_identity("mismatch_b");
        assert!(UpstreamTls::load(Some((&cert, &other_key)), None, true).is_err());
    }

    #[test]
    fn test_upstream_tls_cache_key_content_hash() {
        let (cert, key, ca) = write_identity("hash");
        let a = UpstreamTls::load(Some((&cert, &key)), Some(&ca), true).unwrap();
        let b = UpstreamTls::load(Some((&cert, &key)), Some(&ca), true).unwrap();
        // Same bytes -> same key; verify flag flips the key; different
        // materials -> different key.
        assert_eq!(a.cache_key(), b.cache_key());
        let c = UpstreamTls::load(Some((&cert, &key)), Some(&ca), false).unwrap();
        assert_ne!(a.cache_key(), c.cache_key());
        let (cert2, key2, _) = write_identity("hash2");
        let d = UpstreamTls::load(Some((&cert2, &key2)), None, true).unwrap();
        assert_ne!(a.cache_key(), d.cache_key());
    }

    #[test]
    fn test_upstream_tls_registry_roundtrip() {
        let (cert, key, _) = write_identity("registry");
        let id = UpstreamTls::load(Some((&cert, &key)), None, true).unwrap();
        UpstreamTls::register(&id);
        let found = UpstreamTls::lookup(id.cache_key()).expect("registered identity");
        assert_eq!(found.cache_key(), id.cache_key());
        assert!(UpstreamTls::lookup(id.cache_key().wrapping_add(1)).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_upstream_tls_`
Expected: compile error (types/functions not defined yet). That counts as the failing state; add stubs only if you want to see assert failures instead.

- [ ] **Step 3: Implement `src/outbound/tls.rs`**

```rust
//! Per-upstream TLS identities — mTLS to upstream backends.
//!
//! An [`UpstreamTls`] bundles what an `upstream` node needs to talk to a
//! mutual-TLS or private-PKI backend: an optional client cert/key pair, an
//! optional CA bundle (which *replaces* the native roots for that upstream),
//! and the verify flag. Materials are read, parsed, and dry-built into a
//! rustls config once at policy-compile time, so bad files fail the policy
//! load rather than a live request.
//!
//! Consumers cache built clients/connectors keyed by [`UpstreamTls::cache_key`],
//! a hash of the PEM contents + flags: rotated cert files hash to a new key
//! and naturally get a fresh connection pool after a config reload. Cache and
//! registry entries are never evicted; the population is bounded by the number
//! of distinct identities ever configured, which is small in practice.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock, RwLock};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub struct UpstreamTls {
    client: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
    ca: Option<Vec<CertificateDer<'static>>>,
    /// Verify the upstream's certificate. `false` still presents the client
    /// cert — the handshake is mutual, verification of the peer is skipped.
    pub verify: bool,
    key: u64,
}

/// Process-wide identity registry, so code that only sees JSON context values
/// (the WebSocket relay) can resolve an identity from its cache key.
fn registry() -> &'static RwLock<HashMap<u64, Arc<UpstreamTls>>> {
    static REGISTRY: OnceLock<RwLock<HashMap<u64, Arc<UpstreamTls>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

impl UpstreamTls {
    /// Loads and validates an identity. `client_paths` is `(cert, key)` —
    /// pairing of the two YAML keys is enforced by the caller's config parse.
    /// Reads + PEM-parses every file and dry-builds the rustls config so
    /// unreadable files, empty bundles, and cert/key mismatches all fail here,
    /// at policy-compile time.
    pub fn load(
        client_paths: Option<(&str, &str)>,
        ca_path: Option<&str>,
        verify: bool,
    ) -> Result<Arc<Self>, String> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        verify.hash(&mut hasher);

        let client = match client_paths {
            Some((cert_path, key_path)) => {
                let cert_pem = std::fs::read(cert_path)
                    .map_err(|e| format!("client_cert_path '{}': {}", cert_path, e))?;
                let key_pem = std::fs::read(key_path)
                    .map_err(|e| format!("client_key_path '{}': {}", key_path, e))?;
                cert_pem.hash(&mut hasher);
                key_pem.hash(&mut hasher);
                let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(&cert_pem[..]))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("client_cert_path '{}': {}", cert_path, e))?;
                if certs.is_empty() {
                    return Err(format!(
                        "client_cert_path '{}': no certificates found",
                        cert_path
                    ));
                }
                let key =
                    rustls_pemfile::private_key(&mut std::io::BufReader::new(&key_pem[..]))
                        .map_err(|e| format!("client_key_path '{}': {}", key_path, e))?
                        .ok_or_else(|| {
                            format!("client_key_path '{}': no private key found", key_path)
                        })?;
                Some((certs, key))
            }
            None => None,
        };

        let ca = match ca_path {
            Some(path) => {
                let pem = std::fs::read(path)
                    .map_err(|e| format!("ca_cert_path '{}': {}", path, e))?;
                pem.hash(&mut hasher);
                let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(&pem[..]))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("ca_cert_path '{}': {}", path, e))?;
                if certs.is_empty() {
                    return Err(format!("ca_cert_path '{}': no certificates found", path));
                }
                Some(certs)
            }
            None => None,
        };

        let identity = Arc::new(Self {
            client,
            ca,
            verify,
            key: hasher.finish(),
        });
        identity.client_config()?; // dry build: catches cert/key mismatch now
        Ok(identity)
    }

    /// Content-hash key (PEM bytes + verify flag). Stable within a process —
    /// exactly the lifetime of the caches it keys.
    pub fn cache_key(&self) -> u64 {
        self.key
    }

    /// Builds a rustls client config from the loaded materials. ALPN is left
    /// unset — the HTTP client and the wss connector want different values.
    pub fn client_config(&self) -> Result<rustls::ClientConfig, String> {
        crate::server::tls::install_crypto_provider();

        let builder = if self.verify {
            let mut roots = rustls::RootCertStore::empty();
            match &self.ca {
                // A configured CA bundle *replaces* the native roots.
                Some(certs) => {
                    let (added, _ignored) =
                        roots.add_parsable_certificates(certs.iter().cloned());
                    if added == 0 {
                        return Err("ca_cert_path: no usable certificates".to_string());
                    }
                }
                None => {
                    let loaded = rustls_native_certs::load_native_certs();
                    let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
                    if added == 0 {
                        return Err(format!(
                            "no usable native root certificates ({} load error(s))",
                            loaded.errors.len()
                        ));
                    }
                }
            }
            rustls::ClientConfig::builder().with_root_certificates(roots)
        } else {
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(super::NoVerification(
                    rustls::crypto::ring::default_provider(),
                )))
        };

        match &self.client {
            Some((certs, key)) => builder
                .with_client_auth_cert(certs.clone(), key.clone_key())
                .map_err(|e| format!("client cert/key rejected: {}", e)),
            None => Ok(builder.with_no_client_auth()),
        }
    }

    /// Publishes the identity in the process-wide registry (idempotent).
    pub fn register(this: &Arc<Self>) {
        registry()
            .write()
            .unwrap()
            .entry(this.key)
            .or_insert_with(|| this.clone());
    }

    /// Resolves a previously [`register`](Self::register)ed identity.
    pub fn lookup(key: u64) -> Option<Arc<UpstreamTls>> {
        registry().read().unwrap().get(&key).cloned()
    }
}
```

In `src/outbound/mod.rs`: add `pub mod tls;` below the module doc-comment, and make the verifier reachable:

```rust
pub(crate) struct NoVerification(pub(crate) rustls::crypto::CryptoProvider);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_upstream_tls_`
Expected: all 7 PASS. If `test_upstream_tls_load_mismatched_key_errors` fails because this rustls point-release does not check key consistency, verify with `cargo tree -i rustls`; only if the check is genuinely absent, replace the assertion body with a mismatch check done in `load` itself is NOT required — instead delete that single test and note it in the commit body. Do not weaken `load`.

- [ ] **Step 5: Commit**

```bash
git add src/outbound/tls.rs src/outbound/mod.rs
git commit -m "feat(outbound): add UpstreamTls identity loading and registry"
```

---

### Task 2: identity-keyed client cache in `OutboundClient`

**Files:**
- Modify: `src/outbound/mod.rs` (`OutboundRequest` + `OutboundClient`)
- Modify (mechanical, add `tls: None,` to each `OutboundRequest { ... }` literal): `src/config_store/etcd.rs`, and in `src/plugins/native/`: `authz_casdoor.rs` (×2), `authz_keycloak.rs`, `aws_lambda.rs`, `azure_functions.rs`, `cas_auth.rs`, `clickhouse_logger.rs`, `dingtalk_auth.rs` (×2), `elasticsearch_logger.rs`, `feishu_auth.rs` (×2), `forward_auth.rs`, `google_cloud_logging.rs` (×2), `http_logger.rs`, `lago.rs`, `loggly.rs`, `loki_logger.rs`, `opa.rs`, `openfunction.rs`, `openid_connect.rs` (×3), `opentelemetry.rs`, `openwhisk.rs`, `proxy_mirror.rs`, `skywalking.rs`, `skywalking_logger.rs`, `sls_logger.rs`, `splunk_hec_logging.rs`, `tencent_cloud_cls.rs`, `traffic_split.rs`, `upstream.rs`, `wolf_rbac.rs`, `zipkin.rs` (let `cargo build` confirm the list is complete)
- Test: inline in `src/outbound/mod.rs`

**Interfaces:**
- Consumes: `tls::UpstreamTls` from Task 1.
- Produces: `OutboundRequest.tls: Option<Arc<tls::UpstreamTls>>` (default `None`); `OutboundClient::request` honors it. Task 3 sets the field.

- [ ] **Step 1: Write the failing integration test**

Append to `mod tests` in `src/outbound/mod.rs`. It spins a real mTLS-requiring HTTP server: correct identity → 200; CA-only identity (no client cert) → transport error.

```rust
    /// Minimal one-shot HTTPS server that requires a client certificate and
    /// answers any request with `HTTP/1.1 200 OK`. Returns its port.
    async fn spawn_mtls_server(
        server_config: Arc<rustls::ServerConfig>,
    ) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
            // Serve connections until the test ends; failed handshakes
            // (missing client cert) just drop the connection.
            loop {
                let Ok((tcp, _)) = listener.accept().await else { break };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut stream) = acceptor.accept(tcp).await {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
                            )
                            .await;
                        let _ = stream.shutdown().await;
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn test_request_with_client_identity_reaches_mtls_backend() {
        crate::server::tls::install_crypto_provider();

        // CA, a server cert for "localhost", and a client cert — one CA for both.
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let server_params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let server_key = rcgen::KeyPair::generate().unwrap();
        let server_cert = server_params.signed_by(&server_key, &ca_cert, &ca_key).unwrap();

        let client_params = rcgen::CertificateParams::new(vec!["gw".to_string()]).unwrap();
        let client_key = rcgen::KeyPair::generate().unwrap();
        let client_cert = client_params.signed_by(&client_key, &ca_cert, &ca_key).unwrap();

        // Server side: require a client cert signed by the CA.
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(ca_cert.der().to_vec()))
            .unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .unwrap();
        let server_config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(server_cert.der().to_vec())],
                rustls::pki_types::PrivateKeyDer::try_from(
                    server_key.serialize_der(),
                )
                .unwrap(),
            )
            .unwrap();
        let port = spawn_mtls_server(Arc::new(server_config)).await;

        // Gateway side: identity = client cert + key, CA bundle for the server.
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert_path = dir.join(format!("featherbit_ob_mtls_{}.crt", pid));
        let key_path = dir.join(format!("featherbit_ob_mtls_{}.key", pid));
        let ca_path = dir.join(format!("featherbit_ob_mtls_{}.ca.crt", pid));
        std::fs::write(&cert_path, client_cert.pem()).unwrap();
        std::fs::write(&key_path, client_key.serialize_pem()).unwrap();
        std::fs::write(&ca_path, ca_cert.pem()).unwrap();

        let identity = tls::UpstreamTls::load(
            Some((cert_path.to_str().unwrap(), key_path.to_str().unwrap())),
            Some(ca_path.to_str().unwrap()),
            true,
        )
        .unwrap();

        let client = OutboundClient::new();
        let ok = client
            .request(OutboundRequest {
                method: http::Method::GET,
                url: format!("https://localhost:{}/", port),
                headers: Vec::new(),
                body: Bytes::new(),
                timeout: Duration::from_secs(5),
                ssl_verify: true,
                tls: Some(identity),
            })
            .await
            .expect("mTLS request should succeed");
        assert_eq!(ok.status, 200);

        // Without a client cert (CA-only identity) the handshake is rejected.
        let ca_only = tls::UpstreamTls::load(None, Some(ca_path.to_str().unwrap()), true)
            .unwrap();
        let err = client
            .request(OutboundRequest {
                method: http::Method::GET,
                url: format!("https://localhost:{}/", port),
                headers: Vec::new(),
                body: Bytes::new(),
                timeout: Duration::from_secs(5),
                ssl_verify: true,
                tls: Some(ca_only),
            })
            .await;
        assert!(matches!(err, Err(OutboundError::Transport(_))), "got: {:?}", err.map(|r| r.status));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_request_with_client_identity -- --nocapture`
Expected: compile error — `OutboundRequest` has no field `tls`.

- [ ] **Step 3: Implement**

In `src/outbound/mod.rs`:

1. `OutboundRequest` — add the field and update the docs + ctor:

```rust
    /// When false, TLS certificate verification is disabled (matching
    /// APISIX's `ssl_verify: false`). Ignored for plain-http URLs and when
    /// `tls` is set (the identity carries its own verify flag).
    pub ssl_verify: bool,
    /// Per-upstream TLS identity (client cert / private CA). `None` uses the
    /// shared verified/insecure clients — today's behavior.
    pub tls: Option<Arc<tls::UpstreamTls>>,
```

   and `tls: None,` inside `OutboundRequest::new`.

2. `OutboundClient` — add the cache field and constructor init:

```rust
    /// Clients for per-upstream TLS identities (mTLS / private CA), keyed by
    /// the identity's content hash. Never evicted — bounded by the number of
    /// distinct identities ever configured.
    custom: std::sync::RwLock<HashMap<u64, PooledClient>>,
```

   (`custom: std::sync::RwLock::new(HashMap::new()),` in `new()`.)

3. Client selection in `request()` — replace the current `let client = if req.ssl_verify { ... }` with:

```rust
        let custom_client;
        let client = match &req.tls {
            Some(identity) => {
                custom_client = self.identity_client(identity)?;
                &custom_client
            }
            None if req.ssl_verify => &self.verified,
            None => self.insecure.get_or_init(build_insecure_client),
        };
```

4. The cache method on `OutboundClient`:

```rust
    /// Returns the pooled client for `identity`, building it on first use.
    /// ALPN advertises h1+h2 like the default client (hyper-rustls sets the
    /// protocols on the config in `enable_http1`/`enable_http2`).
    fn identity_client(
        &self,
        identity: &Arc<tls::UpstreamTls>,
    ) -> Result<PooledClient, OutboundError> {
        if let Some(c) = self.custom.read().unwrap().get(&identity.cache_key()) {
            return Ok(c.clone());
        }
        // Materials were validated at policy compile; failure here is
        // config-shaped (e.g. native roots unavailable), not transport.
        let config = identity
            .client_config()
            .map_err(OutboundError::InvalidRequest)?;
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(config)
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let client = Client::builder(TokioExecutor::new()).build(https);
        // Race-safe: whoever loses uses the winner's client.
        Ok(self
            .custom
            .write()
            .unwrap()
            .entry(identity.cache_key())
            .or_insert(client)
            .clone())
    }
```

5. Run `cargo build`; add `tls: None,` to every `OutboundRequest { ... }` literal the compiler reports (the ~35 sites listed under **Files**).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_request_with_client_identity` then the full `cargo test`
Expected: new test PASS; zero regressions elsewhere.

- [ ] **Step 5: Commit**

```bash
git add -A src
git commit -m "feat(outbound): identity-keyed mTLS client cache on OutboundClient"
```

---

### Task 3: `upstream` node config keys + validation

**Files:**
- Modify: `src/plugins/native/upstream.rs`
- Test: inline in `src/plugins/native/upstream.rs`

**Interfaces:**
- Consumes: `UpstreamTls::{load, register, cache_key}` (Task 1), `OutboundRequest.tls` (Task 2).
- Produces: node config keys `client_cert_path` / `client_key_path` / `ca_cert_path`; context key `__ws_upstream_tls_key` (u64, JSON number) consumed by Task 4; field `tls_identity: Option<Arc<UpstreamTls>>` on `UpstreamPlugin`.

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod tests` in `src/plugins/native/upstream.rs` (reuse its existing `make_plugin`-style helpers if present; otherwise build the config `HashMap` inline as the surrounding tests do). The cert helper mirrors Task 1's `write_identity`; copy it locally.

```rust
    fn write_identity(tag: &str) -> (String, String, String) {
        // Same helper as src/outbound/tls.rs tests: CA + leaf, PEM files in
        // temp_dir, returns (cert_path, key_path, ca_path).
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let leaf_params = rcgen::CertificateParams::new(vec!["client".to_string()]).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert = dir.join(format!("featherbit_up_{}_{}.crt", tag, pid));
        let key = dir.join(format!("featherbit_up_{}_{}.key", tag, pid));
        let ca = dir.join(format!("featherbit_up_{}_{}.ca.crt", tag, pid));
        std::fs::write(&cert, leaf_cert.pem()).unwrap();
        std::fs::write(&key, leaf_key.serialize_pem()).unwrap();
        std::fs::write(&ca, ca_cert.pem()).unwrap();
        (
            cert.to_str().unwrap().to_string(),
            key.to_str().unwrap().to_string(),
            ca.to_str().unwrap().to_string(),
        )
    }

    /// Base config with one target; callers add TLS keys.
    fn mtls_config() -> HashMap<String, serde_json::Value> {
        let mut config = HashMap::new();
        config.insert(
            "targets".to_string(),
            serde_json::json!([{"host": "backend", "port": 443}]),
        );
        config
    }

    #[test]
    fn test_mtls_config_requires_tls_true() {
        let (cert, key, _) = write_identity("needstls");
        let mut config = mtls_config();
        config.insert("client_cert_path".to_string(), serde_json::json!(cert));
        config.insert("client_key_path".to_string(), serde_json::json!(key));
        // tls defaults to false -> config error.
        let err = UpstreamPlugin::from_config(&config, &test_resources()).unwrap_err();
        assert!(err.contains("tls"), "err was: {}", err);
    }

    #[test]
    fn test_mtls_config_cert_and_key_must_pair() {
        let (cert, _, _) = write_identity("pair");
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        config.insert("client_cert_path".to_string(), serde_json::json!(cert));
        let err = UpstreamPlugin::from_config(&config, &test_resources()).unwrap_err();
        assert!(err.contains("together"), "err was: {}", err);
    }

    #[test]
    fn test_mtls_config_ca_with_no_verify_rejected() {
        let (_, _, ca) = write_identity("contradiction");
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        config.insert("ssl_verify".to_string(), serde_json::json!(false));
        config.insert("ca_cert_path".to_string(), serde_json::json!(ca));
        assert!(UpstreamPlugin::from_config(&config, &test_resources()).is_err());
    }

    #[test]
    fn test_mtls_config_loads_identity_and_registers() {
        let (cert, key, ca) = write_identity("loads");
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        config.insert("client_cert_path".to_string(), serde_json::json!(cert));
        config.insert("client_key_path".to_string(), serde_json::json!(key));
        config.insert("ca_cert_path".to_string(), serde_json::json!(ca));
        let plugin = UpstreamPlugin::from_config(&config, &test_resources()).unwrap();
        let id = plugin.tls_identity.as_ref().expect("identity loaded");
        // Registered for the WebSocket relay to look up.
        assert!(crate::outbound::tls::UpstreamTls::lookup(id.cache_key()).is_some());
    }

    #[test]
    fn test_mtls_config_absent_means_no_identity() {
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        let plugin = UpstreamPlugin::from_config(&config, &test_resources()).unwrap();
        assert!(plugin.tls_identity.is_none());
    }
```

If the existing test module has no `test_resources()` helper, use whatever the surrounding tests pass as `&Arc<PluginResources>` (they construct one for `from_config` today — reuse that expression verbatim).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_mtls_config_`
Expected: compile error — no TLS keys parsed, no `tls_identity` field.

- [ ] **Step 3: Implement**

In `UpstreamPlugin`: add the field (after `ssl_verify`):

```rust
    /// Per-upstream TLS identity (client cert / private CA); None = shared
    /// clients, exactly the pre-mTLS behavior.
    tls_identity: Option<Arc<crate::outbound::tls::UpstreamTls>>,
```

In `from_config`, after the existing `ssl_verify` parse:

```rust
        let client_cert_path = config
            .get("client_cert_path")
            .and_then(|v| v.as_str())
            .map(String::from);
        let client_key_path = config
            .get("client_key_path")
            .and_then(|v| v.as_str())
            .map(String::from);
        let ca_cert_path = config
            .get("ca_cert_path")
            .and_then(|v| v.as_str())
            .map(String::from);

        let any_mtls_key =
            client_cert_path.is_some() || client_key_path.is_some() || ca_cert_path.is_some();
        if any_mtls_key && !tls {
            return Err(
                "client_cert_path/client_key_path/ca_cert_path require tls: true".to_string(),
            );
        }
        if client_cert_path.is_some() != client_key_path.is_some() {
            return Err(
                "client_cert_path and client_key_path must be set together".to_string(),
            );
        }
        // A CA bundle exists to *verify* the upstream; pairing it with
        // ssl_verify:false is contradictory, so reject rather than guess.
        if ca_cert_path.is_some() && !ssl_verify {
            return Err("ca_cert_path with ssl_verify: false is contradictory".to_string());
        }

        let tls_identity = if any_mtls_key {
            let client = client_cert_path.as_deref().zip(client_key_path.as_deref());
            let identity =
                crate::outbound::tls::UpstreamTls::load(client, ca_cert_path.as_deref(), ssl_verify)?;
            crate::outbound::tls::UpstreamTls::register(&identity);
            Some(identity)
        } else {
            None
        };
```

Add `tls_identity,` to the `Ok(Self { ... })`. In `execute()`: the buffered path's `OutboundRequest` gets `tls: self.tls_identity.clone(),` (replacing the `tls: None,` added in Task 2), and the WebSocket branch gains, next to the other `__ws_upstream_*` inserts:

```rust
            if let Some(identity) = &self.tls_identity {
                ctx.message.insert(
                    "__ws_upstream_tls_key".to_string(),
                    serde_json::json!(identity.cache_key()),
                );
            }
```

Also extend the `from_config` doc-comment's "Accepted keys" list with the three new keys and the validation rules (match the existing doc style, including a YAML example with `client_cert_path`/`client_key_path`/`ca_cert_path`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_mtls_config_` then `cargo test upstream`
Expected: new tests PASS, existing upstream tests unaffected.

- [ ] **Step 5: Commit**

```bash
git add src/plugins/native/upstream.rs
git commit -m "feat(upstream): client_cert_path/client_key_path/ca_cert_path config keys"
```

---

### Task 4: wss relay presents the client certificate

**Files:**
- Modify: `src/outbound/mod.rs` (`client_tls_connector`)
- Modify: `src/server/websocket.rs` (`proxy_upgrade` signature, ~line 136)
- Modify: `src/server/listener.rs` (~lines 356–390, the `__ws_upstream_*` reads)
- Test: inline in `src/outbound/mod.rs`

**Interfaces:**
- Consumes: `UpstreamTls::{lookup, client_config, cache_key}`, `__ws_upstream_tls_key` (Task 3).
- Produces: `client_tls_connector(verify: bool, identity: Option<&Arc<tls::UpstreamTls>>) -> Result<tokio_rustls::TlsConnector, String>`; `proxy_upgrade(..., tls_identity: Option<Arc<tls::UpstreamTls>>, ...)`.

- [ ] **Step 1: Write the failing test**

In `src/outbound/mod.rs` tests:

```rust
    #[test]
    fn test_client_tls_connector_with_identity_builds_and_caches() {
        // Reuse the identity written by the Task 1 helper — write PEMs inline
        // here the same way (CA + leaf via rcgen, temp files).
        let (cert, key, ca) = {
            let mut ca_params =
                rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            let ca_key = rcgen::KeyPair::generate().unwrap();
            let ca_cert = ca_params.self_signed(&ca_key).unwrap();
            let leaf_params =
                rcgen::CertificateParams::new(vec!["client".to_string()]).unwrap();
            let leaf_key = rcgen::KeyPair::generate().unwrap();
            let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
            let dir = std::env::temp_dir();
            let pid = std::process::id();
            let cert = dir.join(format!("featherbit_wsid_{}.crt", pid));
            let key = dir.join(format!("featherbit_wsid_{}.key", pid));
            let ca = dir.join(format!("featherbit_wsid_{}.ca.crt", pid));
            std::fs::write(&cert, leaf_cert.pem()).unwrap();
            std::fs::write(&key, leaf_key.serialize_pem()).unwrap();
            std::fs::write(&ca, ca_cert.pem()).unwrap();
            (
                cert.to_str().unwrap().to_string(),
                key.to_str().unwrap().to_string(),
                ca.to_str().unwrap().to_string(),
            )
        };
        let identity =
            tls::UpstreamTls::load(Some((&cert, &key)), Some(&ca), true).unwrap();
        assert!(client_tls_connector(true, Some(&identity)).is_ok());
        // Second call hits the connector cache — still fine.
        assert!(client_tls_connector(true, Some(&identity)).is_ok());
        // No identity: existing behavior, both variants still build.
        assert!(client_tls_connector(true, None).is_ok());
        assert!(client_tls_connector(false, None).is_ok());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_client_tls_connector`
Expected: compile error — `client_tls_connector` takes 1 argument.

- [ ] **Step 3: Implement**

1. `src/outbound/mod.rs` — new signature and identity branch at the top of the function (existing body unchanged below it); update the function doc-comment to mention the identity path and its per-identity cache:

```rust
pub fn client_tls_connector(
    verify: bool,
    identity: Option<&Arc<tls::UpstreamTls>>,
) -> Result<tokio_rustls::TlsConnector, String> {
    crate::server::tls::install_crypto_provider();

    if let Some(id) = identity {
        static CUSTOM: OnceLock<
            std::sync::RwLock<HashMap<u64, tokio_rustls::TlsConnector>>,
        > = OnceLock::new();
        let cache = CUSTOM.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
        if let Some(c) = cache.read().unwrap().get(&id.cache_key()) {
            return Ok(c.clone());
        }
        let mut config = id.client_config()?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        return Ok(cache
            .write()
            .unwrap()
            .entry(id.cache_key())
            .or_insert(connector)
            .clone());
    }
    // ... existing verify / no-verify body unchanged ...
```

2. Fix the two existing connector tests to pass `None` as the second argument.

3. `src/server/websocket.rs` — add `tls_identity: Option<Arc<crate::outbound::tls::UpstreamTls>>` as a parameter of `proxy_upgrade` (after `verify`), document it in the doc-comment, and change line ~155 to:

```rust
        let connector = crate::outbound::client_tls_connector(verify, tls_identity.as_ref())
            .map_err(WsError::Tls)?;
```

(`use std::sync::Arc;` if not already imported.)

4. `src/server/listener.rs` — next to the existing `__ws_upstream_verify` read (~line 377):

```rust
            // Resolve the upstream TLS identity registered at policy compile.
            // A missing entry can only mean the key was hand-injected — fall
            // back to the plain connector (verification still applies).
            let tls_identity = result_ctx
                .message
                .get("__ws_upstream_tls_key")
                .and_then(|v| v.as_u64())
                .and_then(crate::outbound::tls::UpstreamTls::lookup);
```

and pass `tls_identity,` to the `websocket::proxy_upgrade(...)` call after `verify`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_client_tls_connector` then full `cargo test`
Expected: PASS; existing WebSocket tests still green.

- [ ] **Step 5: Commit**

```bash
git add src/outbound/mod.rs src/server/websocket.rs src/server/listener.rs
git commit -m "feat(websocket): wss relay presents per-upstream client certificate"
```

---

### Task 5: docs, parity notes, final verification

**Files:**
- Modify: `website/docs/reference/plugins/upstream.md`
- Modify: `docs/apisix-parity.md` (untracked internal note — edit the file, it will not appear in the commit)
- Modify: `CLAUDE.md` (transport paragraph)

**Interfaces:** none — documentation only.

- [ ] **Step 1: Update `website/docs/reference/plugins/upstream.md`**

In the config-keys section (match the page's existing table/list style), add:

```markdown
### Mutual TLS to the upstream

When `tls: true`, the node can present a client certificate and/or trust a
private CA:

| Key | Type | Description |
| --- | --- | --- |
| `client_cert_path` | string | PEM client certificate (chain) presented to the upstream. Requires `client_key_path`. |
| `client_key_path` | string | PEM private key for `client_cert_path`. Requires `client_cert_path`. |
| `ca_cert_path` | string | PEM CA bundle used to verify the upstream. **Replaces** the system trust store for this upstream. Incompatible with `ssl_verify: false`. |

Files are loaded and validated when the policy compiles: unreadable files,
cert/key mismatches, or contradictory combinations reject the policy. Rotating
certificates means touching `gateway.yaml` (hot-reload recompiles the policy)
or restarting the gateway. `ssl_verify: false` together with a client
certificate is allowed: the certificate is presented, the upstream's own
certificate is not verified.

```yaml
type: upstream
config:
  tls: true
  targets:
    - host: payments.internal
      port: 8443
  client_cert_path: /etc/featherbit/certs/gateway-client.crt
  client_key_path: /etc/featherbit/certs/gateway-client.key
  ca_cert_path: /etc/featherbit/certs/private-ca.crt
```

Both HTTPS proxying and `wss` WebSocket relays present the certificate.
```

- [ ] **Step 2: Update `docs/apisix-parity.md`**

Find the upstream/TLS rows and note that APISIX's `upstream.tls.client_cert` / `tls.client_key` (and `ssl_trusted_certificate`) map to the `upstream` node's `client_cert_path` / `client_key_path` / `ca_cert_path`.

- [ ] **Step 3: Update `CLAUDE.md`**

In the transport paragraph (the one covering TLS/mTLS/SNI), after the sentence about mTLS client-cert verification, add:

```
Outbound mTLS is supported per `upstream` node (`client_cert_path`/`client_key_path`, plus `ca_cert_path` for private CAs), applied to both HTTPS proxying and wss relays.
```

- [ ] **Step 4: Full verification**

Run: `cargo test` (all green), `cargo clippy -- -D warnings` if clippy is clean on `develop` (check with `git stash`-free `cargo clippy` first; don't fix pre-existing lints), and `graphify update .`
Expected: no failures, no new warnings.

- [ ] **Step 5: Commit**

```bash
git add website/docs/reference/plugins/upstream.md CLAUDE.md
git commit -m "docs: document upstream mTLS configuration"
```

---

## Self-review notes

- Spec coverage: config surface + validation (Task 3), `UpstreamTls`/caches/registry (Tasks 1–2), wss path (Task 4), error handling (compile-time in Tasks 1/3; runtime maps to existing `OutboundError::Transport` / `WsError::Tls` untouched), testing (unit in 1/3/4, integration mTLS server in 2), docs (Task 5). The spec's "rule 2: ca may be used alone" is exercised by `test_upstream_tls_load_ca_only` and the CA-only rejection half of the Task 2 integration test.
- The "mTLS keys require `tls: true`" rule is an explicit-error interpretation of the spec's "meaningful only with `tls: true`" — silently ignoring keys would hide misconfiguration.
- Type consistency: `tls_identity` (plugin field), `OutboundRequest.tls`, `cache_key()`, `__ws_upstream_tls_key` used identically across tasks.
