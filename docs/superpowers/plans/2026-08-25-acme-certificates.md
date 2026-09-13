# Automatic Certificates (ACME) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The gateway obtains and renews its own TLS certificates from any ACME CA (Let's Encrypt, ZeroSSL, step-ca, Pebble) using the TLS-ALPN-01 challenge, with filesystem or redis storage, placeholder-cert bootstrap, lease-coordinated renewal, Admin API + UI visibility, and Prometheus metrics.

**Architecture:** A new `src/acme/` subsystem *produces* certificates (protocol client behind an `AcmeClient` trait, order state machine, `CertStorage` trait with fs/redis backends, renewal `Manager`, metrics) and publishes them into a shared `ManagedCerts` `ArcSwap` map. `src/server/tls.rs` only *consumes*: a cert slot is `File` or `Managed(cert_id)`, and a ClientHello with ALPN `acme-tls/1` is answered from a `ChallengeSolver`. Renewals swap a map entry, never rebuild the `ServerConfig`.

**Tech Stack:** Rust (tokio, rustls 0.23/ring, `instant-acme` 0.8, `rcgen` 0.14, `x509-parser`, `redis` behind `redis-store`), axum Admin API, React/TypeScript UI (vitest), Playwright e2e, Pebble (Let's Encrypt's test CA) in CI.

**Spec:** `docs/superpowers/specs/2026-08-25-acme-certificates-design.md`

## Global Constraints

- The dependency tree stays **ring-only**: every new crypto-touching dep uses `default-features = false` + a `ring` feature; never pull `aws-lc-rs`.
- Conventional Commits (`feat(acme): …`, `test(acme): …`, `docs: …`), **no** `Co-Authored-By` trailer.
- Before every commit: `cargo fmt`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test` (rustfmt runs before tests in CI). The headless build must keep compiling: `cargo check --no-default-features` and `cargo check --no-default-features --features redis-store`.
- `key_type` accepts only `ecdsa-p256` (default) and `ecdsa-p384`; RSA is refused at load.
- `admin.tls` never accepts `acme`; the admin listener never advertises `acme-tls/1`.
- Private keys and account credentials never appear in the Admin API, UI, logs, or traces; in redis they are sealed with `CookieSealer`.
- Every `${ENV}` in `system.yaml` resolves at load (`load_yaml_with_env`) — no extra interpolation code needed for ACME fields.
- Existing file-based TLS behavior (loading, SNI, mTLS, hot-reload) must remain byte-for-byte identical; all existing tests keep passing with only the `Option<String>` / `acme: None` literal changes.
- Live tests are env-gated and self-skip: `FEATHERBIT_TEST_REDIS_URL` (redis), `FEATHERBIT_TEST_PEBBLE_URL` (+ `FEATHERBIT_TEST_PEBBLE_CA`, `FEATHERBIT_TEST_ACME_DOMAIN` default `localhost`, `FEATHERBIT_TEST_ACME_PORT` default `18443`).
- Do not commit the user's local files (`tests/oidc-test.yml`, `*.png`, the unrelated untracked SAST spec).
- After the last task: `graphify update .`.

## File map

| Path | Responsibility |
|---|---|
| `Cargo.toml` | `instant-acme`, `rcgen` 0.14 (runtime dep), `time` |
| `src/config/system.rs` | `AcmeConfig`, `AcmeEabConfig`, `AcmeStorageConfig`, `AcmeSlot`; `TlsConfig`/`SniCert` optional file paths + `acme`; `parse_duration`; `SystemConfig::validate`, `validate_against_gateway` |
| `src/main.rs` | `mod acme;`, call `system.validate()` and `validate_against_gateway` fail-fast |
| `src/acme/mod.rs` | `AcmeError`, `CertId`, `CertState`, `CertMeta`, `ManagedCert`, `ManagedCerts`, `StoredCert`, cert parsing helpers, placeholder cert, `AcmeRuntime`, `start()` |
| `src/acme/storage/mod.rs` | `CertStorage` trait, `contract` test suite |
| `src/acme/storage/fs.rs` | filesystem backend |
| `src/acme/storage/redis.rs` | `stores:` backend (feature `redis-store`), sealed keys, SET NX leases |
| `src/acme/challenge.rs` | `ChallengeSolver` trait, `TlsAlpnSolver`, challenge cert builder |
| `src/acme/client.rs` | `AcmeClient`/`AcmeOrder`/`AcmeClientFactory` traits, `InstantAcme*` impls, `MockAcmeClient` (test) |
| `src/acme/order.rs` | `KeyType`, key/CSR generation, chain verification, `issue()` |
| `src/acme/metrics.rs` | `AcmeMetrics` |
| `src/acme/manager.rs` | `when_to_renew`, `backoff_secs`, `Manager` (scheduler, lease, peers, force-renew) |
| `src/acme/live_tests.rs` | Pebble-gated end-to-end test |
| `src/server/tls.rs` | `CertSlot`, `AcmeHooks`, ALPN branch, `acme-tls/1` advertising, `negotiated_acme_challenge` |
| `src/server/listener.rs` | ACME startup wiring, close `acme-tls/1` connections |
| `src/state.rs` | `SharedState.acme: ArcSwapOption<AcmeRuntime>` |
| `src/stores/mod.rs` | `StoreRegistry::client(name)` |
| `src/admin/status.rs` | `/readyz` placeholder check |
| `src/admin/acme.rs` | `GET /api/acme/certs`, `POST /api/acme/certs/{id}/renew` |
| `src/admin/stores.rs` | `acme.storage` referrer on delete |
| `ui/src/types/index.ts`, `ui/src/api/client.ts` | `AcmeCert*` types, `listAcmeCerts`, `renewAcmeCert` |
| `ui/src/certs.ts` (+ `.test.ts`) | `expiryTone`, `formatExpiresIn` |
| `ui/src/components/CertificatesPanel.tsx` | the panel |
| `ui/src/components/Sidebar.tsx`, `ui/src/App.tsx` | footer button + wiring |
| `dev/pebble/pebble-config.json`, `dev/pebble/docker-compose.yml` | local Pebble |
| `.github/workflows/ci.yml` | `acme-live` job; Pebble step + env on the `e2e` job |
| `e2e/tests/acme.spec.ts`, `e2e/fixtures/acme/{system,gateway}.yaml`, `e2e/E2E_TESTBOOK.md` | `E2E-ACME-*` |
| `website/docs/guides/tls.md`, `website/docs/reference/roadmap.md`, `CLAUDE.md`, `config/system.yaml` | docs |

---

### Task 1: Dependencies — `instant-acme`, `rcgen` 0.14 as a runtime dep, `time`

**Files:**
- Modify: `Cargo.toml` (deps block after `x509-parser = "0.18"`; remove `rcgen = "0.13"` from `[dev-dependencies]`)
- Modify: `src/server/tls.rs` tests (`key_pair` → `signing_key`; `signed_by` API), `src/server/listener.rs:684`

**Interfaces:**
- Produces: crates `instant_acme`, `rcgen` 0.14, `time` available to `src/`.

- [ ] **Step 1: Edit `Cargo.toml`**

Add after the `x509-parser = "0.18"` line in `[dependencies]`:

```toml
# ACME (RFC 8555) client for automatic certificates (src/acme). Ring-only like
# the rest of the tree: default features would pull aws-lc-rs.
instant-acme = { version = "0.8", default-features = false, features = ["ring", "hyper-rustls"] }
# CSRs, TLS-ALPN-01 challenge certs and placeholder certs (runtime), plus the
# self-signed certs the TLS tests use. `x509-parser` lets the test-only mock CA
# sign a CSR. Ring-only.
rcgen = { version = "0.14", default-features = false, features = ["crypto", "pem", "ring", "x509-parser"] }
# ARI (renewal-info) windows from instant-acme are `time::OffsetDateTime`s.
time = "0.3"
```

Delete these two lines from `[dev-dependencies]`:

```toml
# Self-signed certs for TLS listener tests (ring-based, emits PEM).
rcgen = "0.13"
```

- [ ] **Step 2: Build and collect the breakage**

Run: `cargo build 2>&1 | grep -E "^error|-->" | head -40`
Expected: errors only in `src/server/tls.rs` tests and `src/server/listener.rs:684` about `key_pair` (renamed to `signing_key` in rcgen 0.14) and `signed_by` (new signature). No errors outside tests.

- [ ] **Step 3: Fix the rcgen 0.14 API changes in tests**

In `src/server/tls.rs` and `src/server/listener.rs`, replace every `certified.key_pair` with `certified.signing_key` (sites: `self_signed`, `write_fresh_cert`, `write_named_cert` in tls.rs; `tls_echo_ws_server` in listener.rs).

Replace `gen_signed` in `src/server/tls.rs` (the mTLS helpers around line 811) with the 0.14 signing API:

```rust
    fn gen_signed(
        cn: &str,
        ca_cert: &rcgen::Certificate,
        ca_key: &rcgen::KeyPair,
    ) -> (rcgen::Certificate, rcgen::KeyPair) {
        // SAN = [cn], and an explicit subject CN so identity extraction has both.
        let mut params = rcgen::CertificateParams::new(vec![cn.to_string()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        let key = rcgen::KeyPair::generate().unwrap();
        // rcgen 0.14: signing goes through an `Issuer` built from the CA cert + key.
        let issuer = rcgen::Issuer::from_ca_cert_der(ca_cert.der(), ca_key).unwrap();
        let cert = params.signed_by(&key, &issuer).unwrap();
        (cert, key)
    }
```

`gen_ca` is unchanged (`self_signed` still exists). If `Issuer::from_ca_cert_der` needs an owned key, use `rcgen::Issuer::new(params, key)` with the CA's params kept alongside — prefer `from_ca_cert_der` first.

- [ ] **Step 4: Verify everything still passes**

Run: `cargo test 2>&1 | tail -5` and `cargo clippy --all-targets --locked -- -D warnings`
Expected: all tests pass (same count as before), clippy clean.

- [ ] **Step 5: Verify the tree is still ring-only**

Run: `cargo tree -i aws-lc-rs 2>&1 | head -3`
Expected: `error: package ID specification `aws-lc-rs` did not match any packages` (or the same set of matches as on `develop` — compare with `git stash; cargo tree -i aws-lc-rs; git stash pop` if unsure). If `cargo deny` is installed: `cargo deny check` reports no new advisories/licenses.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/server/tls.rs src/server/listener.rs
git commit -m "chore(deps): add instant-acme, promote rcgen 0.14 to a runtime dep"
```

---

### Task 2: Config types and fail-fast validation

**Files:**
- Modify: `src/config/system.rs` (`TlsConfig`, `SniCert`, new `AcmeConfig` family, `parse_duration`, `SystemConfig::validate`, `validate_against_gateway`)
- Modify: `src/config/mod.rs` re-exports
- Modify: `src/server/tls.rs` (`build_server_config`, `spawn_cert_watcher` adapt to `Option<String>` paths; test literals)
- Modify: `src/server/listener.rs:686` test literal
- Modify: `src/main.rs` (call validation)

**Interfaces:**
- Produces:
  - `pub struct AcmeConfig { directory_url: String, directory_ca_path: Option<String>, contact: Vec<String>, terms_of_service_agreed: bool, eab: Option<AcmeEabConfig>, key_type: String, renew_before: String, storage: AcmeStorageConfig }`
  - `pub struct AcmeEabConfig { key_id: String, hmac_key: String }`
  - `pub enum AcmeStorageConfig { Filesystem { dir: String }, Store { store: String, encryption_key: String } }`
  - `pub struct AcmeSlot { domains: Vec<String> }`
  - `TlsConfig { cert_path: Option<String>, key_path: Option<String>, acme: Option<AcmeSlot>, … }`, `SniCert { server_name, cert_path: Option<String>, key_path: Option<String>, acme: Option<AcmeSlot> }`
  - `impl TlsConfig { pub fn managed_domains(&self) -> Vec<Vec<String>>; pub fn validate(&self, acme_enabled: bool, label: &str) -> Result<(), String> }`
  - `impl AcmeConfig { pub fn renew_before_duration(&self) -> Result<Duration, String>; pub fn validate(&self) -> Result<(), String> }`
  - `pub fn parse_duration(s: &str) -> Result<Duration, String>`
  - `pub fn normalize_domain(d: &str) -> Result<String, String>`
  - `impl SystemConfig { pub fn validate(&self) -> Result<(), String>; pub fn validate_against_gateway(&self, gw: &GatewayConfig) -> Result<(), String> }`

- [ ] **Step 1: Write failing config tests**

Append to the `#[cfg(test)] mod tests` in `src/config/system.rs` (create the module if absent):

```rust
#[cfg(test)]
mod acme_config_tests {
    use super::*;

    fn sys(yaml: &str) -> SystemConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30d").unwrap().as_secs(), 30 * 86_400);
        assert_eq!(parse_duration("12h").unwrap().as_secs(), 12 * 3_600);
        assert_eq!(parse_duration("5m").unwrap().as_secs(), 300);
        assert_eq!(parse_duration("90s").unwrap().as_secs(), 90);
        assert_eq!(parse_duration("42").unwrap().as_secs(), 42);
        assert!(parse_duration("").is_err());
        assert!(parse_duration("3w").is_err());
        assert!(parse_duration("-1d").is_err());
    }

    #[test]
    fn normalize_domain_rules() {
        assert_eq!(normalize_domain("API.Example.com").unwrap(), "api.example.com");
        assert!(normalize_domain("*.example.com").unwrap_err().contains("wildcard"));
        assert!(normalize_domain("10.0.0.1").is_err());
        assert!(normalize_domain("::1").is_err());
        assert!(normalize_domain("").is_err());
        assert!(normalize_domain("bad_host.example.com").is_err());
    }

    #[test]
    fn file_tls_without_acme_block_is_valid_and_has_no_managed_domains() {
        let s = sys("tls:\n  cert_path: a.pem\n  key_path: a.key\n");
        s.validate().unwrap();
        assert!(s.tls.as_ref().unwrap().managed_domains().is_empty());
    }

    #[test]
    fn half_file_pair_is_rejected() {
        let s = sys("tls:\n  cert_path: a.pem\n");
        assert!(s.validate().unwrap_err().contains("cert_path"));
    }

    #[test]
    fn acme_slot_requires_top_level_block() {
        let s = sys("tls:\n  acme:\n    domains: [api.example.com]\n");
        assert!(s.validate().unwrap_err().contains("acme:"));
    }

    #[test]
    fn acme_slot_and_file_pair_are_mutually_exclusive() {
        let s = sys(
            "acme:\n  terms_of_service_agreed: true\ntls:\n  cert_path: a.pem\n  key_path: a.key\n  acme:\n    domains: [api.example.com]\n",
        );
        assert!(s.validate().unwrap_err().contains("exactly one"));
    }

    #[test]
    fn managed_default_needs_explicit_domains_and_sni_defaults_to_server_name() {
        let s = sys("acme:\n  terms_of_service_agreed: true\ntls:\n  acme: {}\n");
        assert!(s.validate().unwrap_err().contains("domains"));

        let s = sys(
            "acme:\n  terms_of_service_agreed: true\ntls:\n  acme:\n    domains: [B.example.com, a.example.com]\n  sni_certs:\n    - server_name: Tenant.example.com\n      acme: {}\n",
        );
        s.validate().unwrap();
        assert_eq!(
            s.tls.as_ref().unwrap().managed_domains(),
            vec![
                vec!["b.example.com".to_string(), "a.example.com".to_string()],
                vec!["tenant.example.com".to_string()]
            ]
        );
    }

    #[test]
    fn wildcard_acme_domains_are_rejected() {
        let s = sys(
            "acme:\n  terms_of_service_agreed: true\ntls:\n  acme:\n    domains: [\"*.example.com\"]\n",
        );
        assert!(s.validate().unwrap_err().contains("wildcard"));
    }

    #[test]
    fn tos_must_be_agreed() {
        let s = sys("acme:\n  terms_of_service_agreed: false\ntls:\n  acme:\n    domains: [a.example.com]\n");
        assert!(s.validate().unwrap_err().contains("terms_of_service_agreed"));
    }

    #[test]
    fn directory_must_be_https_and_key_type_ecdsa() {
        let s = sys("acme:\n  terms_of_service_agreed: true\n  directory_url: http://ca.local/dir\n");
        assert!(s.validate().unwrap_err().contains("https"));
        let s = sys("acme:\n  terms_of_service_agreed: true\n  key_type: rsa-2048\n");
        let err = s.validate().unwrap_err();
        assert!(err.contains("ecdsa-p256") && err.contains("ecdsa-p384"), "{err}");
    }

    #[test]
    fn admin_tls_rejects_acme() {
        let s = sys(
            "acme:\n  terms_of_service_agreed: true\nadmin:\n  username: a\n  password: b\n  tls:\n    acme:\n      domains: [admin.example.com]\n",
        );
        assert!(s.validate().unwrap_err().contains("admin.tls"));
    }

    #[test]
    fn store_storage_requires_encryption_key_and_declared_store() {
        let s = sys(
            "acme:\n  terms_of_service_agreed: true\n  storage:\n    type: store\n    store: r\n",
        );
        let err = s.validate().unwrap_err();
        #[cfg(feature = "redis-store")]
        assert!(err.contains("encryption_key"), "{err}");
        #[cfg(not(feature = "redis-store"))]
        assert!(err.contains("redis-store"), "{err}");

        #[cfg(feature = "redis-store")]
        {
            let s = sys(
                "acme:\n  terms_of_service_agreed: true\n  storage:\n    type: store\n    store: r\n    encryption_key: k\n",
            );
            s.validate().unwrap();
            let gw: crate::config::GatewayConfig = serde_yaml::from_str("{}").unwrap();
            assert!(s.validate_against_gateway(&gw).unwrap_err().contains("'r'"));
            let gw: crate::config::GatewayConfig = serde_yaml::from_str(
                "stores:\n  - name: r\n    type: redis\n    url: redis://127.0.0.1:6379\n",
            )
            .unwrap();
            s.validate_against_gateway(&gw).unwrap();
        }
    }

    #[test]
    fn defaults() {
        let s = sys("acme:\n  terms_of_service_agreed: true\n");
        let a = s.acme.unwrap();
        assert_eq!(a.directory_url, "https://acme-v02.api.letsencrypt.org/directory");
        assert_eq!(a.key_type, "ecdsa-p256");
        assert_eq!(a.renew_before_duration().unwrap().as_secs(), 30 * 86_400);
        assert!(matches!(a.storage, AcmeStorageConfig::Filesystem { ref dir } if dir == "/var/lib/featherbit/acme"));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test acme_config_tests 2>&1 | grep -E "^error" | head -5`
Expected: compile errors (`AcmeStorageConfig`, `parse_duration`, `validate` not found).

- [ ] **Step 3: Add the types to `src/config/system.rs`**

Add to `SystemConfig` (after `debug`):

```rust
    /// Automatic certificates via ACME (RFC 8555, TLS-ALPN-01). `None` (the
    /// default) disables the feature; any `tls.acme` / `sni_certs[].acme` slot
    /// then fails validation.
    #[serde(default)]
    pub acme: Option<AcmeConfig>,
```

Change `TlsConfig`'s two path fields and add `acme`:

```rust
    /// Path to the PEM certificate chain. Required unless this slot is
    /// ACME-managed (`acme`).
    #[serde(default)]
    pub cert_path: Option<String>,
    /// Path to the PEM private key. Required unless this slot is ACME-managed.
    #[serde(default)]
    pub key_path: Option<String>,
    /// Obtain this certificate automatically via ACME instead of files.
    /// Mutually exclusive with `cert_path`/`key_path`; requires the top-level
    /// `acme:` block. `domains` is mandatory here (a default cert has no
    /// server name to infer from).
    #[serde(default)]
    pub acme: Option<AcmeSlot>,
```

Same on `SniCert` (`cert_path`, `key_path` become `Option<String>` with `#[serde(default)]`; add `pub acme: Option<AcmeSlot>` whose doc says `domains` defaults to `[server_name]`).

Add the new types (after `SniCert`):

```rust
/// One ACME-managed certificate slot.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct AcmeSlot {
    /// DNS names on the certificate. On an `sni_certs` entry an empty list
    /// means `[server_name]`. Wildcards are rejected (TLS-ALPN-01 cannot
    /// issue them).
    #[serde(default)]
    pub domains: Vec<String>,
}

/// Top-level ACME settings (`acme:` in `system.yaml`).
#[derive(Debug, Deserialize, Clone)]
pub struct AcmeConfig {
    /// ACME directory URL; defaults to Let's Encrypt production. Must be `https://`.
    #[serde(default = "default_acme_directory")]
    pub directory_url: String,
    /// PEM trust root(s) for the CA's own HTTPS endpoint (private CAs, Pebble).
    /// Replaces the system roots for that connection only.
    #[serde(default)]
    pub directory_ca_path: Option<String>,
    /// Account contacts, e.g. `mailto:ops@example.com`.
    #[serde(default)]
    pub contact: Vec<String>,
    /// Must be `true`: registering an account asserts agreement to the CA's terms.
    #[serde(default)]
    pub terms_of_service_agreed: bool,
    /// External Account Binding (ZeroSSL, Google Trust Services, step-ca).
    #[serde(default)]
    pub eab: Option<AcmeEabConfig>,
    /// Certificate key type: `ecdsa-p256` (default) or `ecdsa-p384`.
    #[serde(default = "default_acme_key_type")]
    pub key_type: String,
    /// Renew when less than this remains before `not_after` (`30d`, `12h`,
    /// `90s`, or bare seconds); the CA's ARI window, when offered, may renew earlier.
    #[serde(default = "default_acme_renew_before")]
    pub renew_before: String,
    /// Where account, keys, certificates, challenges and leases live.
    #[serde(default)]
    pub storage: AcmeStorageConfig,
}

/// External Account Binding credentials issued by the CA.
#[derive(Debug, Deserialize, Clone)]
pub struct AcmeEabConfig {
    pub key_id: String,
    /// base64url (or standard base64) HMAC key as handed out by the CA.
    pub hmac_key: String,
}

/// ACME state storage backend (`acme.storage.type`).
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AcmeStorageConfig {
    /// A directory on local disk (default `/var/lib/featherbit/acme`).
    Filesystem {
        #[serde(default = "default_acme_dir")]
        dir: String,
    },
    /// A declared `stores:` entry from `gateway.yaml`; keys are sealed with
    /// `encryption_key` (AES-256-GCM, key derived by SHA-256) before storage.
    Store {
        store: String,
        #[serde(default)]
        encryption_key: String,
    },
}

impl Default for AcmeStorageConfig {
    fn default() -> Self {
        Self::Filesystem {
            dir: default_acme_dir(),
        }
    }
}

fn default_acme_directory() -> String {
    "https://acme-v02.api.letsencrypt.org/directory".to_string()
}
fn default_acme_key_type() -> String {
    "ecdsa-p256".to_string()
}
fn default_acme_renew_before() -> String {
    "30d".to_string()
}
fn default_acme_dir() -> String {
    "/var/lib/featherbit/acme".to_string()
}

/// Parses `30d` / `12h` / `5m` / `90s` / bare seconds into a [`Duration`].
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("duration must not be empty".to_string());
    }
    let (num, mult) = match s.chars().last().unwrap() {
        'd' => (&s[..s.len() - 1], 86_400u64),
        'h' => (&s[..s.len() - 1], 3_600),
        'm' => (&s[..s.len() - 1], 60),
        's' => (&s[..s.len() - 1], 1),
        c if c.is_ascii_digit() => (s, 1),
        other => return Err(format!("unknown duration unit '{other}' in '{s}' (use d/h/m/s)")),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("invalid duration '{s}'"))?;
    Ok(std::time::Duration::from_secs(n * mult))
}

/// Lowercases and validates one ACME DNS identifier: no wildcards, no IPs, only
/// `[a-z0-9.-]`, non-empty labels.
pub fn normalize_domain(d: &str) -> Result<String, String> {
    let d = d.trim().to_ascii_lowercase();
    if d.is_empty() {
        return Err("domain must not be empty".to_string());
    }
    if d.contains('*') {
        return Err(format!(
            "'{d}': TLS-ALPN-01 cannot issue wildcard certificates; use a file-based cert"
        ));
    }
    if d.parse::<std::net::IpAddr>().is_ok() || d.contains(':') {
        return Err(format!("'{d}': IP addresses are not supported ACME identifiers"));
    }
    if !d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        || d.split('.').any(|label| label.is_empty())
    {
        return Err(format!("'{d}' is not a valid DNS name"));
    }
    Ok(d)
}

impl SniCert {
    /// Normalized ACME domains for this entry, or `None` when file-based.
    /// An empty `acme.domains` means `[server_name]`.
    pub fn acme_domains(&self) -> Result<Option<Vec<String>>, String> {
        match &self.acme {
            None => Ok(None),
            Some(slot) if slot.domains.is_empty() => {
                Ok(Some(vec![normalize_domain(&self.server_name)?]))
            }
            Some(slot) => Ok(Some(
                slot.domains
                    .iter()
                    .map(|d| normalize_domain(d))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
        }
    }
}

impl TlsConfig {
    /// Normalized domain lists of every ACME-managed slot, default cert first,
    /// then `sni_certs` in order. Empty when nothing is managed. Assumes
    /// [`TlsConfig::validate`] passed.
    pub fn managed_domains(&self) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        if let Some(slot) = &self.acme {
            out.push(
                slot.domains
                    .iter()
                    .filter_map(|d| normalize_domain(d).ok())
                    .collect(),
            );
        }
        for sc in &self.sni_certs {
            if let Ok(Some(domains)) = sc.acme_domains() {
                out.push(domains);
            }
        }
        out
    }

    /// Structural validation of every cert slot. `acme_enabled` is whether the
    /// top-level `acme:` block exists; `label` names the block in errors
    /// (`"tls"` / `"admin.tls"`).
    pub fn validate(&self, acme_enabled: bool, label: &str) -> Result<(), String> {
        fn check_slot(
            what: &str,
            cert: &Option<String>,
            key: &Option<String>,
            acme: &Option<AcmeSlot>,
            acme_enabled: bool,
        ) -> Result<(), String> {
            match (cert, key, acme) {
                (Some(_), Some(_), None) => Ok(()),
                (None, None, Some(_)) if acme_enabled => Ok(()),
                (None, None, Some(_)) => Err(format!(
                    "{what}: acme slot requires the top-level `acme:` block in system.yaml"
                )),
                (None, None, None) => Err(format!(
                    "{what}: set cert_path + key_path, or acme"
                )),
                (Some(_), None, _) | (None, Some(_), _) => Err(format!(
                    "{what}: cert_path and key_path must be set together"
                )),
                (Some(_), Some(_), Some(_)) => Err(format!(
                    "{what}: set exactly one of cert_path/key_path or acme"
                )),
            }
        }
        check_slot(label, &self.cert_path, &self.key_path, &self.acme, acme_enabled)?;
        if let Some(slot) = &self.acme {
            if slot.domains.is_empty() {
                return Err(format!(
                    "{label}.acme: a managed default certificate needs explicit `domains`"
                ));
            }
            for d in &slot.domains {
                normalize_domain(d).map_err(|e| format!("{label}.acme.domains: {e}"))?;
            }
        }
        for (i, sc) in self.sni_certs.iter().enumerate() {
            let what = format!("{label}.sni_certs[{i}] ({})", sc.server_name);
            check_slot(&what, &sc.cert_path, &sc.key_path, &sc.acme, acme_enabled)?;
            sc.acme_domains().map_err(|e| format!("{what}: {e}"))?;
        }
        Ok(())
    }
}

impl AcmeConfig {
    pub fn renew_before_duration(&self) -> Result<std::time::Duration, String> {
        parse_duration(&self.renew_before).map_err(|e| format!("acme.renew_before: {e}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.terms_of_service_agreed {
            return Err("acme.terms_of_service_agreed must be true to register an account".into());
        }
        if !self.directory_url.starts_with("https://") {
            return Err(format!(
                "acme.directory_url must be https:// (got '{}')",
                self.directory_url
            ));
        }
        if let Some(p) = &self.directory_ca_path {
            std::fs::metadata(p)
                .map_err(|e| format!("acme.directory_ca_path '{p}' is not readable: {e}"))?;
        }
        if !matches!(self.key_type.as_str(), "ecdsa-p256" | "ecdsa-p384") {
            return Err(format!(
                "acme.key_type '{}' is not supported; use ecdsa-p256 or ecdsa-p384 (RSA needs a non-ring crypto backend)",
                self.key_type
            ));
        }
        self.renew_before_duration()?;
        if let Some(eab) = &self.eab {
            if eab.key_id.is_empty() || eab.hmac_key.is_empty() {
                return Err("acme.eab: key_id and hmac_key must both be set".into());
            }
        }
        match &self.storage {
            AcmeStorageConfig::Filesystem { dir } if dir.trim().is_empty() => {
                Err("acme.storage.dir must not be empty".into())
            }
            AcmeStorageConfig::Filesystem { .. } => Ok(()),
            #[cfg(not(feature = "redis-store"))]
            AcmeStorageConfig::Store { .. } => Err(
                "acme.storage.type: store — this binary was built without the redis-store feature"
                    .into(),
            ),
            #[cfg(feature = "redis-store")]
            AcmeStorageConfig::Store { encryption_key, .. } if encryption_key.trim().is_empty() => {
                Err("acme.storage.encryption_key is required for type: store".into())
            }
            #[cfg(feature = "redis-store")]
            AcmeStorageConfig::Store { .. } => Ok(()),
        }
    }
}

impl SystemConfig {
    /// Fail-fast structural validation, run once after loading `system.yaml`.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(acme) = &self.acme {
            acme.validate()?;
        }
        if let Some(tls) = &self.tls {
            tls.validate(self.acme.is_some(), "tls")?;
        }
        if let Some(admin) = &self.admin {
            if let Some(tls) = &admin.tls {
                if tls.acme.is_some() || tls.sni_certs.iter().any(|s| s.acme.is_some()) {
                    return Err(
                        "admin.tls does not support acme (the admin listener is not validated by the CA); use cert_path/key_path"
                            .into(),
                    );
                }
                tls.validate(false, "admin.tls")?;
            }
        }
        Ok(())
    }

    /// Cross-file checks that need `gateway.yaml`: an ACME `store` must be declared.
    pub fn validate_against_gateway(&self, gw: &crate::config::GatewayConfig) -> Result<(), String> {
        if let Some(AcmeConfig {
            storage: AcmeStorageConfig::Store { store, .. },
            ..
        }) = &self.acme
        {
            if !gw.stores.iter().any(|s| &s.name == store) {
                return Err(format!(
                    "acme.storage.store references unknown store '{store}' (declare it under `stores:` in gateway.yaml)"
                ));
            }
        }
        Ok(())
    }
}
```

Export in `src/config/mod.rs`'s `pub use system::{…}` list: `AcmeConfig, AcmeEabConfig, AcmeSlot, AcmeStorageConfig, parse_duration, normalize_domain`.

- [ ] **Step 4: Make `tls.rs` compile with optional paths (behavior unchanged)**

In `src/server/tls.rs` add a variant to `TlsError`:

```rust
    #[error("TLS slot '{0}' has no certificate source (set cert_path/key_path, or acme)")]
    MissingCertSource(String),
```

and a helper:

```rust
/// The file pair of a file-based slot. ACME-managed slots are handled by the
/// resolver (see `CertSlot`); calling this on one is a wiring bug surfaced as
/// `MissingCertSource`.
fn file_pair<'a>(
    cert: &'a Option<String>,
    key: &'a Option<String>,
    what: &str,
) -> Result<(&'a str, &'a str), TlsError> {
    match (cert, key) {
        (Some(c), Some(k)) => Ok((c.as_str(), k.as_str())),
        _ => Err(TlsError::MissingCertSource(what.to_string())),
    }
}
```

In `build_server_config`: replace `load_cert_chain(&tls.cert_path)?` / `load_private_key(&tls.key_path)?` with

```rust
    let (cert_path, key_path) = file_pair(&tls.cert_path, &tls.key_path, "default")?;
    let chain = load_cert_chain(cert_path)?;
    let key = load_private_key(key_path)?;
```

and in the SNI loop `let (c_path, k_path) = file_pair(&sc.cert_path, &sc.key_path, &sc.server_name)?;`.

In `spawn_cert_watcher`, build the path list from `Some` paths only:

```rust
    let mut paths: Vec<&String> = Vec::new();
    paths.extend(tls.cert_path.iter());
    paths.extend(tls.key_path.iter());
    for sc in &tls.sni_certs {
        paths.extend(sc.cert_path.iter());
        paths.extend(sc.key_path.iter());
    }
    if paths.is_empty() {
        info!("{} has no file-based certificates; cert watcher not started", label);
        return;
    }
```

Update every `TlsConfig { … }` / `SniCert { … }` literal in tests (`src/server/tls.rs`: `self_signed`, `test_cert_hot_reload_swaps_served_cert`, `test_cert_hot_reload_via_file_watcher`, `test_sni_multicert_selects_by_hostname`, the mTLS helper; `src/server/listener.rs` `tls_echo_ws_server`): wrap paths in `Some(...)` and add `acme: None,`.

- [ ] **Step 5: Wire fail-fast validation into `main.rs`**

After the `system` load block (before `init_logging`):

```rust
    if let Err(e) = system.validate() {
        eprintln!("Invalid system config: {}", e);
        std::process::exit(1);
    }
```

After the gateway config is loaded (right after the `let (config_store, gateway, config_path) = match … ;` block):

```rust
    if let Err(e) = system.validate_against_gateway(&gateway) {
        eprintln!("Invalid config: {}", e);
        std::process::exit(1);
    }
```

- [ ] **Step 6: Run tests**

Run: `cargo test 2>&1 | tail -5 && cargo test --no-default-features --features redis-store acme_config 2>&1 | tail -3 && cargo check --no-default-features`
Expected: all pass (new `acme_config_tests` included), headless check compiles.

- [ ] **Step 7: Commit**

```bash
git add src/config/system.rs src/config/mod.rs src/server/tls.rs src/server/listener.rs src/main.rs
git commit -m "feat(config): acme block, optional cert paths and fail-fast TLS slot validation"
```

---

### Task 3: `src/acme/mod.rs` — core types, cert parsing, placeholder cert

**Files:**
- Create: `src/acme/mod.rs`, stub `src/acme/storage/mod.rs`
- Modify: `src/main.rs` (`mod acme;` before `mod admin;`)

**Interfaces:**
- Produces (all `pub` in `crate::acme`):
  - `enum AcmeError { Config(String), Storage(String), Protocol(String), Certificate(String), Crypto(String) }` (thiserror)
  - `struct CertId(String)`; `CertId::from_domains(&[String]) -> Result<(CertId, Vec<String>), AcmeError>` (lowercase, sorted, deduped; id = joined with `,`); `as_str()`, `Display`
  - `enum CertState { Placeholder, Issued, Renewing, Failed }` + `as_str()`, `const ALL: [CertState; 4]`
  - `struct CertMeta { not_before: i64, not_after: i64, issuer: String, serial: String, next_renewal_at: Option<i64>, last_attempt_at: Option<i64>, last_error: Option<String> }` (serde, Default)
  - `struct ManagedCert { key: Arc<CertifiedKey>, leaf_der: Vec<u8>, state: CertState, meta: CertMeta, domains: Vec<String> }`
  - `type ManagedCerts = Arc<ArcSwap<HashMap<String, ManagedCert>>>`; `new_managed_certs()`, `publish(&ManagedCerts, &CertId, ManagedCert)`, `update(&ManagedCerts, &CertId, impl FnOnce(&mut ManagedCert))`
  - `struct StoredCert { chain_pem: String, key_pem: String, issued_at: i64 }` (serde, PartialEq)
  - `fn now_unix() -> i64`
  - `fn placeholder_cert(domains: &[String]) -> Result<(Arc<CertifiedKey>, Vec<u8>), AcmeError>`
  - `fn load_certified_key(chain_pem: &str, key_pem: &str) -> Result<(Arc<CertifiedKey>, Vec<u8>), AcmeError>` (returns leaf DER; errors if key ≠ cert)
  - `fn parse_cert_meta(leaf_der: &[u8]) -> Result<CertMeta, AcmeError>`, `fn leaf_dns_sans(&[u8]) -> Result<Vec<String>, AcmeError>`, `fn leaf_spki(&[u8]) -> Result<Vec<u8>, AcmeError>`

- [ ] **Step 1: Write the failing tests** (bottom of the new `src/acme/mod.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn cert_id_normalizes_sorts_and_dedups() {
        let (id, domains) =
            CertId::from_domains(&s(&["B.example.com", "a.example.com", "a.EXAMPLE.com"])).unwrap();
        assert_eq!(id.as_str(), "a.example.com,b.example.com");
        assert_eq!(domains, s(&["a.example.com", "b.example.com"]));
        assert!(CertId::from_domains(&[]).is_err());
        assert!(CertId::from_domains(&s(&["*.example.com"])).is_err());
    }

    #[test]
    fn placeholder_is_self_signed_for_domains_and_short_lived() {
        let (key, leaf) = placeholder_cert(&s(&["api.example.com", "www.example.com"])).unwrap();
        assert!(key.end_entity_cert().is_ok());
        let sans = leaf_dns_sans(&leaf).unwrap();
        assert!(sans.contains(&"api.example.com".to_string()));
        let meta = parse_cert_meta(&leaf).unwrap();
        let now = now_unix();
        assert!(meta.not_after > now && meta.not_after <= now + 3_700, "{:?}", meta);
        assert!(meta.not_before <= now);
    }

    #[test]
    fn load_certified_key_round_trips_and_rejects_foreign_key() {
        let a = rcgen::generate_simple_self_signed(s(&["a.example.com"])).unwrap();
        let b = rcgen::generate_simple_self_signed(s(&["b.example.com"])).unwrap();
        let (ck, leaf) =
            load_certified_key(&a.cert.pem(), &a.signing_key.serialize_pem()).unwrap();
        assert_eq!(ck.end_entity_cert().unwrap().as_ref(), leaf.as_slice());
        assert!(!leaf_spki(&leaf).unwrap().is_empty());
        let err = load_certified_key(&a.cert.pem(), &b.signing_key.serialize_pem()).unwrap_err();
        assert!(matches!(err, AcmeError::Certificate(_)), "{err}");
        assert!(load_certified_key("not pem", &a.signing_key.serialize_pem()).is_err());
    }

    #[test]
    fn parse_cert_meta_reads_validity_issuer_serial() {
        let a = rcgen::generate_simple_self_signed(s(&["a.example.com"])).unwrap();
        let meta = parse_cert_meta(a.cert.der()).unwrap();
        assert!(meta.not_after > meta.not_before);
        assert!(!meta.serial.is_empty());
        assert!(meta.issuer.contains("rcgen"), "{}", meta.issuer);
    }

    #[test]
    fn publish_and_update_swap_map_entries() {
        let certs = new_managed_certs();
        let (id, domains) = CertId::from_domains(&s(&["a.example.com"])).unwrap();
        let (key, leaf) = placeholder_cert(&domains).unwrap();
        publish(
            &certs,
            &id,
            ManagedCert { key, leaf_der: leaf, state: CertState::Placeholder, meta: CertMeta::default(), domains },
        );
        assert_eq!(certs.load().get(id.as_str()).unwrap().state, CertState::Placeholder);
        update(&certs, &id, |c| c.state = CertState::Failed);
        assert_eq!(certs.load().get(id.as_str()).unwrap().state, CertState::Failed);
        assert_eq!(CertState::Failed.as_str(), "failed");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test acme::tests 2>&1 | grep -E "^error" | head -3`
Expected: unresolved module / missing items.

- [ ] **Step 3: Implement `src/acme/mod.rs`**

```rust
//! Automatic certificates via ACME (RFC 8555) with the TLS-ALPN-01 challenge
//! (RFC 8737).
//!
//! This module *produces* certificates; `server::tls` only consumes them. The
//! contract between the two is [`ManagedCerts`] — an atomically swappable map
//! from [`CertId`] to the current [`ManagedCert`] — plus the
//! `challenge::ChallengeSolver` the TLS resolver asks when a ClientHello
//! carries ALPN `acme-tls/1`. Renewals swap a map entry; they never rebuild
//! the rustls `ServerConfig`.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};

pub mod storage;

#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error("acme config: {0}")]
    Config(String),
    #[error("acme storage: {0}")]
    Storage(String),
    #[error("acme protocol: {0}")]
    Protocol(String),
    #[error("acme certificate: {0}")]
    Certificate(String),
    #[error("acme crypto: {0}")]
    Crypto(String),
}

/// Stable identity of one managed certificate: its normalized domain set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CertId(String);

impl CertId {
    /// Normalizes (lowercase, sorted, deduplicated) and validates the domains;
    /// returns the id and the normalized list.
    pub fn from_domains(domains: &[String]) -> Result<(CertId, Vec<String>), AcmeError> {
        let mut norm = domains
            .iter()
            .map(|d| crate::config::normalize_domain(d).map_err(AcmeError::Config))
            .collect::<Result<Vec<_>, _>>()?;
        norm.sort();
        norm.dedup();
        if norm.is_empty() {
            return Err(AcmeError::Config("a certificate needs at least one domain".into()));
        }
        Ok((CertId(norm.join(",")), norm))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CertId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CertState {
    /// Self-signed stand-in served until the first issuance succeeds.
    Placeholder,
    Issued,
    Renewing,
    /// Last attempt failed; the previous cert (or placeholder) keeps serving.
    Failed,
}

impl CertState {
    pub const ALL: [CertState; 4] =
        [CertState::Placeholder, CertState::Issued, CertState::Renewing, CertState::Failed];

    pub fn as_str(&self) -> &'static str {
        match self {
            CertState::Placeholder => "placeholder",
            CertState::Issued => "issued",
            CertState::Renewing => "renewing",
            CertState::Failed => "failed",
        }
    }
}

/// Operator-facing facts about a managed certificate (never the key).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CertMeta {
    pub not_before: i64,
    pub not_after: i64,
    pub issuer: String,
    pub serial: String,
    #[serde(default)]
    pub next_renewal_at: Option<i64>,
    #[serde(default)]
    pub last_attempt_at: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// The current certificate for one [`CertId`], as served by the TLS resolver.
#[derive(Debug, Clone)]
pub struct ManagedCert {
    pub key: Arc<CertifiedKey>,
    pub leaf_der: Vec<u8>,
    pub state: CertState,
    pub meta: CertMeta,
    pub domains: Vec<String>,
}

/// Keyed by `CertId::as_str()`.
pub type ManagedCerts = Arc<ArcSwap<HashMap<String, ManagedCert>>>;

pub fn new_managed_certs() -> ManagedCerts {
    Arc::new(ArcSwap::from_pointee(HashMap::new()))
}

/// Replaces (or inserts) the entry for `id`. New TLS connections see it on
/// their next `resolve`; in-flight ones are unaffected.
pub fn publish(certs: &ManagedCerts, id: &CertId, cert: ManagedCert) {
    let mut map: HashMap<String, ManagedCert> = (**certs.load()).clone();
    map.insert(id.as_str().to_string(), cert);
    certs.store(Arc::new(map));
}

/// Mutates the entry for `id` in place (no-op when absent).
pub fn update(certs: &ManagedCerts, id: &CertId, f: impl FnOnce(&mut ManagedCert)) {
    let mut map: HashMap<String, ManagedCert> = (**certs.load()).clone();
    if let Some(c) = map.get_mut(id.as_str()) {
        f(c);
        certs.store(Arc::new(map));
    }
}

/// What storage persists per certificate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCert {
    pub chain_pem: String,
    pub key_pem: String,
    pub issued_at: i64,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// A self-signed, 1-hour placeholder (SAN = all domains, CN = the first) served
/// while no real certificate exists yet. Never persisted.
pub fn placeholder_cert(domains: &[String]) -> Result<(Arc<CertifiedKey>, Vec<u8>), AcmeError> {
    let first = domains
        .first()
        .ok_or_else(|| AcmeError::Config("placeholder needs a domain".into()))?;
    let mut params = rcgen::CertificateParams::new(domains.to_vec())
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, first.as_str());
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::hours(1);
    let key = rcgen::KeyPair::generate().map_err(|e| AcmeError::Crypto(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    load_certified_key(&cert.pem(), &key.serialize_pem())
}

/// Parses a PEM chain + PEM key into a rustls [`CertifiedKey`], verifying the
/// key matches the leaf. Returns the leaf DER alongside for metadata parsing.
pub fn load_certified_key(
    chain_pem: &str,
    key_pem: &str,
) -> Result<(Arc<CertifiedKey>, Vec<u8>), AcmeError> {
    let chain: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(chain_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AcmeError::Certificate(format!("chain PEM: {e}")))?;
    if chain.is_empty() {
        return Err(AcmeError::Certificate("chain PEM contains no certificates".into()));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| AcmeError::Certificate(format!("key PEM: {e}")))?;
    let leaf = chain[0].as_ref().to_vec();
    let ck = CertifiedKey::from_der(chain, key, &provider())
        .map_err(|e| AcmeError::Certificate(format!("certificate/key: {e}")))?;
    ck.keys_match().map_err(|e| {
        AcmeError::Certificate(format!("private key does not match certificate: {e}"))
    })?;
    Ok((Arc::new(ck), leaf))
}

fn parse_leaf(leaf_der: &[u8]) -> Result<x509_parser::prelude::X509Certificate<'_>, AcmeError> {
    use x509_parser::prelude::*;
    X509Certificate::from_der(leaf_der)
        .map(|(_, c)| c)
        .map_err(|e| AcmeError::Certificate(format!("leaf DER: {e}")))
}

pub fn parse_cert_meta(leaf_der: &[u8]) -> Result<CertMeta, AcmeError> {
    let cert = parse_leaf(leaf_der)?;
    Ok(CertMeta {
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        issuer: cert.issuer().to_string(),
        serial: cert.raw_serial_as_string(),
        next_renewal_at: None,
        last_attempt_at: None,
        last_error: None,
    })
}

/// DNS SANs of the leaf, lowercased.
pub fn leaf_dns_sans(leaf_der: &[u8]) -> Result<Vec<String>, AcmeError> {
    use x509_parser::prelude::*;
    let cert = parse_leaf(leaf_der)?;
    let mut out = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            if let GeneralName::DNSName(d) = name {
                out.push(d.to_ascii_lowercase());
            }
        }
    }
    Ok(out)
}

/// The leaf's SubjectPublicKeyInfo DER (for "does this chain belong to my key").
pub fn leaf_spki(leaf_der: &[u8]) -> Result<Vec<u8>, AcmeError> {
    let cert = parse_leaf(leaf_der)?;
    Ok(cert.tbs_certificate.subject_pki.raw.to_vec())
}
```

Add `mod acme;` to `src/main.rs`. Create `src/acme/storage/mod.rs` containing only `//! ACME state storage backends (filled in by the next task).` so `pub mod storage;` compiles.

- [ ] **Step 4: Run tests**

Run: `cargo test acme::tests 2>&1 | tail -8`
Expected: 5 passed. If `subject_pki.raw` is not public in x509-parser 0.18, use `cert.public_key().raw` instead.

- [ ] **Step 5: Commit**

```bash
git add src/acme src/main.rs
git commit -m "feat(acme): core types, managed-cert map, placeholder and chain parsing"
```

---

### Task 4: `CertStorage` trait, contract tests, filesystem backend

**Files:**
- Create: `src/acme/storage/mod.rs` (replace the stub), `src/acme/storage/fs.rs`

**Interfaces:**
- Produces:
  ```rust
  #[async_trait]
  pub trait CertStorage: Send + Sync {
      fn label(&self) -> String;                                   // "filesystem" | "store:<name>"
      async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError>;
      async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError>;
      async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError>;
      async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<(), AcmeError>;
      async fn put_challenge(&self, domain: &str, key_auth: &str, ttl: Duration) -> Result<(), AcmeError>;
      async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError>;
      async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError>;
      async fn try_acquire_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError>;
      async fn renew_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError>;
      async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError>;
  }
  pub struct FsCertStorage; impl FsCertStorage { pub fn new(dir: impl Into<PathBuf>) -> Self }
  #[cfg(test)] pub(crate) mod contract { pub async fn run_all(storage: Arc<dyn CertStorage>) }
  ```

- [ ] **Step 1: Write the contract suite and the fs tests**

`src/acme/storage/mod.rs`:

```rust
//! ACME state storage: account credentials, per-certificate key+chain, pending
//! TLS-ALPN-01 key authorizations, and the renewal lease that keeps N gateway
//! instances from ordering the same certificate at once.
//!
//! Backends: [`fs::FsCertStorage`] (single node, or a shared volume for certs —
//! not for challenges) and, behind the `redis-store` feature,
//! `redis::RedisCertStorage` (cluster-ready; keys sealed at rest).

use std::time::Duration;

use async_trait::async_trait;

use super::{AcmeError, CertId, StoredCert};

pub mod fs;
#[cfg(feature = "redis-store")]
pub mod redis;

#[async_trait]
pub trait CertStorage: Send + Sync {
    /// Operator-facing name shown in the Admin API (`filesystem`, `store:<name>`).
    fn label(&self) -> String;
    async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError>;
    async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError>;
    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError>;
    async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<(), AcmeError>;
    /// Registers `key_auth` for `domain`; expires after `ttl`.
    async fn put_challenge(&self, domain: &str, key_auth: &str, ttl: Duration) -> Result<(), AcmeError>;
    async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError>;
    async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError>;
    /// `true` when this `owner` now holds the lease for `id` (fresh or already
    /// its own); `false` when another live owner holds it.
    async fn try_acquire_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError>;
    /// Extends the lease iff `owner` holds it.
    async fn renew_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError>;
    /// Releases iff `owner` holds it (a no-op otherwise).
    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError>;
}

/// Backend-agnostic behavior every `CertStorage` must satisfy. Each backend's
/// tests call `run_all`. TTL checks sleep ≥1 s because backends may keep
/// second-resolution expiries.
#[cfg(test)]
pub(crate) mod contract {
    use super::*;
    use std::sync::Arc;

    pub async fn run_all(storage: Arc<dyn CertStorage>) {
        account_round_trip(&*storage).await;
        cert_round_trip(&*storage).await;
        challenge_ttl(&*storage).await;
        lease_semantics(&*storage).await;
    }

    async fn account_round_trip(s: &dyn CertStorage) {
        assert_eq!(s.load_account().await.unwrap(), None);
        s.save_account(b"{\"id\":\"acct\"}").await.unwrap();
        assert_eq!(s.load_account().await.unwrap().unwrap(), b"{\"id\":\"acct\"}");
        s.save_account(b"v2").await.unwrap();
        assert_eq!(s.load_account().await.unwrap().unwrap(), b"v2");
    }

    async fn cert_round_trip(s: &dyn CertStorage) {
        let (id, _) =
            CertId::from_domains(&["a.example.com".into(), "b.example.com".into()]).unwrap();
        assert!(s.load_cert(&id).await.unwrap().is_none());
        let cert = StoredCert { chain_pem: "CHAIN".into(), key_pem: "KEY".into(), issued_at: 1_700_000_000 };
        s.save_cert(&id, &cert).await.unwrap();
        assert_eq!(s.load_cert(&id).await.unwrap().unwrap(), cert);
        let (other, _) = CertId::from_domains(&["c.example.com".into()]).unwrap();
        assert!(s.load_cert(&other).await.unwrap().is_none());
    }

    async fn challenge_ttl(s: &dyn CertStorage) {
        assert!(s.get_challenge("x.example.com").await.unwrap().is_none());
        s.put_challenge("x.example.com", "tok.thumb", Duration::from_secs(60)).await.unwrap();
        assert_eq!(s.get_challenge("x.example.com").await.unwrap().unwrap(), "tok.thumb");
        s.remove_challenge("x.example.com").await.unwrap();
        assert!(s.get_challenge("x.example.com").await.unwrap().is_none());
        s.put_challenge("y.example.com", "old", Duration::from_millis(50)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(s.get_challenge("y.example.com").await.unwrap().is_none(), "expired reads as absent");
    }

    async fn lease_semantics(s: &dyn CertStorage) {
        let (id, _) = CertId::from_domains(&["lease.example.com".into()]).unwrap();
        let ttl = Duration::from_secs(30);
        assert!(s.try_acquire_lease(&id, "me", ttl).await.unwrap());
        assert!(!s.try_acquire_lease(&id, "peer", ttl).await.unwrap());
        assert!(s.try_acquire_lease(&id, "me", ttl).await.unwrap(), "re-entrant for the owner");
        assert!(s.renew_lease(&id, "me", ttl).await.unwrap());
        assert!(!s.renew_lease(&id, "peer", ttl).await.unwrap());
        s.release_lease(&id, "peer").await.unwrap(); // not the owner: no-op
        assert!(!s.try_acquire_lease(&id, "peer", ttl).await.unwrap());
        s.release_lease(&id, "me").await.unwrap();
        assert!(s.try_acquire_lease(&id, "peer", Duration::from_millis(50)).await.unwrap());
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(s.try_acquire_lease(&id, "me", ttl).await.unwrap(), "expired lease is free");
        s.release_lease(&id, "me").await.unwrap();
    }
}
```

Bottom of the new `src/acme/storage/fs.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fb_acme_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[tokio::test]
    async fn fs_storage_satisfies_contract() {
        let dir = temp_dir("contract");
        let storage = Arc::new(FsCertStorage::new(dir.clone()));
        assert_eq!(storage.label(), "filesystem");
        crate::acme::storage::contract::run_all(storage).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fs_layout_and_atomic_write() {
        let dir = temp_dir("layout");
        let storage = FsCertStorage::new(dir.clone());
        let (id, _) = CertId::from_domains(&["a.example.com".into()]).unwrap();
        storage
            .save_cert(&id, &StoredCert { chain_pem: "C".into(), key_pem: "K".into(), issued_at: 1 })
            .await
            .unwrap();
        let cert_dir = dir.join("certs").join("a.example.com");
        assert!(cert_dir.join("chain.pem").exists());
        assert!(cert_dir.join("key.pem").exists());
        assert!(cert_dir.join("meta.json").exists());
        assert!(!cert_dir.join("key.pem.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cert_dir.join("key.pem")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test acme::storage 2>&1 | grep -E "^error" | head -3`
Expected: `FsCertStorage` not found / `mod fs` missing.

- [ ] **Step 3: Implement `src/acme/storage/fs.rs`**

```rust
//! Filesystem `CertStorage`.
//!
//! Layout under `dir`: `account.json`, `certs/<cert_id>/{chain.pem,key.pem,meta.json}`,
//! `challenges/<domain>` (`{key_auth, expires_at}`), `leases/<cert_id>`
//! (`{owner, expires_at}`). Every write is temp-file + rename; secret files are
//! `0600` on unix. Expired challenge/lease files read as absent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::CertStorage;
use crate::acme::{now_unix, AcmeError, CertId, StoredCert};

pub struct FsCertStorage {
    dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct ChallengeFile {
    key_auth: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct LeaseFile {
    owner: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct MetaFile {
    issued_at: i64,
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> AcmeError {
    AcmeError::Storage(format!("{what} {}: {e}", path.display()))
}

/// Writes `bytes` to `path` atomically (temp file beside it, then rename).
/// `secret` files get `0600` on unix before the rename.
fn atomic_write(path: &Path, bytes: &[u8], secret: bool) -> Result<(), AcmeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err("create dir", parent, e))?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes).map_err(|e| io_err("write", &tmp, e))?;
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io_err("chmod", &tmp, e))?;
    }
    #[cfg(not(unix))]
    let _ = secret;
    if let Err(first) = std::fs::rename(&tmp, path) {
        // Windows refuses to rename over an existing file.
        std::fs::remove_file(path).map_err(|e| io_err("replace", path, e))?;
        std::fs::rename(&tmp, path).map_err(|_| io_err("rename", path, first))?;
    }
    Ok(())
}

fn read_opt(path: &Path) -> Result<Option<Vec<u8>>, AcmeError> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("read", path, e)),
    }
}

fn remove_opt(path: &Path) -> Result<(), AcmeError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove", path, e)),
    }
}

fn expires_at(ttl: Duration) -> i64 {
    now_unix() + ttl.as_secs().max(1) as i64
}

impl FsCertStorage {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn cert_dir(&self, id: &CertId) -> PathBuf {
        self.dir.join("certs").join(id.as_str())
    }
    fn challenge_path(&self, domain: &str) -> PathBuf {
        self.dir.join("challenges").join(domain)
    }
    fn lease_path(&self, id: &CertId) -> PathBuf {
        self.dir.join("leases").join(id.as_str())
    }

    fn read_lease(&self, id: &CertId) -> Result<Option<LeaseFile>, AcmeError> {
        let Some(bytes) = read_opt(&self.lease_path(id))? else {
            return Ok(None);
        };
        let lease: LeaseFile = match serde_json::from_slice(&bytes) {
            Ok(l) => l,
            Err(_) => return Ok(None), // a corrupt lease is no lease
        };
        if lease.expires_at <= now_unix() {
            return Ok(None);
        }
        Ok(Some(lease))
    }

    fn write_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<(), AcmeError> {
        let lease = LeaseFile { owner: owner.to_string(), expires_at: expires_at(ttl) };
        atomic_write(&self.lease_path(id), &serde_json::to_vec(&lease).unwrap(), false)
    }
}

#[async_trait]
impl CertStorage for FsCertStorage {
    fn label(&self) -> String {
        "filesystem".to_string()
    }

    async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError> {
        read_opt(&self.dir.join("account.json"))
    }

    async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError> {
        atomic_write(&self.dir.join("account.json"), creds, true)
    }

    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError> {
        let dir = self.cert_dir(id);
        let (Some(chain), Some(key)) =
            (read_opt(&dir.join("chain.pem"))?, read_opt(&dir.join("key.pem"))?)
        else {
            return Ok(None);
        };
        let issued_at = read_opt(&dir.join("meta.json"))?
            .and_then(|b| serde_json::from_slice::<MetaFile>(&b).ok())
            .map(|m| m.issued_at)
            .unwrap_or(0);
        Ok(Some(StoredCert {
            chain_pem: String::from_utf8_lossy(&chain).into_owned(),
            key_pem: String::from_utf8_lossy(&key).into_owned(),
            issued_at,
        }))
    }

    async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<(), AcmeError> {
        let dir = self.cert_dir(id);
        // Key first, chain last: a reader that sees a chain always finds its key.
        atomic_write(&dir.join("key.pem"), cert.key_pem.as_bytes(), true)?;
        atomic_write(
            &dir.join("meta.json"),
            &serde_json::to_vec(&MetaFile { issued_at: cert.issued_at }).unwrap(),
            false,
        )?;
        atomic_write(&dir.join("chain.pem"), cert.chain_pem.as_bytes(), false)
    }

    async fn put_challenge(&self, domain: &str, key_auth: &str, ttl: Duration) -> Result<(), AcmeError> {
        let file = ChallengeFile { key_auth: key_auth.to_string(), expires_at: expires_at(ttl) };
        atomic_write(&self.challenge_path(domain), &serde_json::to_vec(&file).unwrap(), false)
    }

    async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError> {
        let Some(bytes) = read_opt(&self.challenge_path(domain))? else {
            return Ok(None);
        };
        let file: ChallengeFile = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        if file.expires_at <= now_unix() {
            return Ok(None);
        }
        Ok(Some(file.key_auth))
    }

    async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError> {
        remove_opt(&self.challenge_path(domain))
    }

    async fn try_acquire_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError> {
        match self.read_lease(id)? {
            Some(l) if l.owner != owner => Ok(false),
            _ => {
                self.write_lease(id, owner, ttl)?;
                Ok(true)
            }
        }
    }

    async fn renew_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError> {
        match self.read_lease(id)? {
            Some(l) if l.owner == owner => {
                self.write_lease(id, owner, ttl)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError> {
        match self.read_lease(id)? {
            Some(l) if l.owner == owner => remove_opt(&self.lease_path(id)),
            _ => Ok(()),
        }
    }
}
```

Until Task 5 exists, comment out the `#[cfg(feature = "redis-store")] pub mod redis;` line (Task 5 restores it).

- [ ] **Step 4: Run tests**

Run: `cargo test acme::storage 2>&1 | tail -6`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add src/acme/storage
git commit -m "feat(acme): CertStorage trait, contract suite and filesystem backend"
```

---

### Task 5: Redis `CertStorage` (sealed keys, SET NX leases) + `StoreRegistry::client`

**Files:**
- Create: `src/acme/storage/redis.rs`
- Modify: `src/acme/storage/mod.rs` (restore `pub mod redis;`), `src/stores/mod.rs` (`client()` accessor)

**Interfaces:**
- Consumes: `crate::stores::redis_store::RedisStoreClient` (`conn().await -> Result<ConnectionManager, String>`, `key_prefix()`, `name()`), `crate::plugins::util::cookie_session::CookieSealer` (`new(secret)`, `seal(&[u8], Duration) -> String`, `open(&str) -> Result<Vec<u8>, CookieError>`).
- Produces:
  - `pub struct RedisCertStorage; RedisCertStorage::new(client: Arc<RedisStoreClient>, encryption_key: &str) -> Self` implementing `CertStorage` (`label()` = `"store:<name>"`)
  - `impl StoreRegistry { pub fn client(&self, name: &str) -> Result<Arc<RedisStoreClient>, String> }` (headless variant returns the "built without redis-store" error)

- [ ] **Step 1: Add the `client()` accessor to `src/stores/mod.rs`**

Next to `counter_store`:

```rust
    /// The raw client for `name` (ACME storage borrows it; sessions/counters have
    /// their own typed accessors).
    #[cfg(feature = "redis-store")]
    pub fn client(&self, name: &str) -> Result<Arc<redis_store::RedisStoreClient>, String> {
        self.clients.get(name).cloned().ok_or_else(|| {
            let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
            names.sort_unstable();
            format!(
                "unknown store '{}' — declared stores: {}",
                name,
                if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
            )
        })
    }
```

(No headless variant is needed: the only caller is behind `#[cfg(feature = "redis-store")]` too.)

- [ ] **Step 2: Write the gated live test** (bottom of the new `src/acme/storage/redis.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use redis::AsyncCommands;

    fn live_client(tag: &str) -> Option<Arc<RedisStoreClient>> {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping: FEATHERBIT_TEST_REDIS_URL not set");
            return None;
        };
        let cfg: crate::config::StoreConfig = serde_yaml::from_str(&format!(
            "name: acme-live\ntype: redis\nurl: {url}\nkey_prefix: fbacme{tag}{}\n",
            std::process::id()
        ))
        .unwrap();
        Some(Arc::new(RedisStoreClient::build(&cfg).unwrap()))
    }

    #[tokio::test]
    async fn redis_storage_satisfies_contract() {
        let Some(client) = live_client("c") else { return };
        let storage = Arc::new(RedisCertStorage::new(client, "test-secret"));
        assert_eq!(storage.label(), "store:acme-live");
        crate::acme::storage::contract::run_all(storage).await;
    }

    #[tokio::test]
    async fn redis_storage_seals_private_material() {
        let Some(client) = live_client("s") else { return };
        let storage = RedisCertStorage::new(client.clone(), "test-secret");
        let (id, _) = CertId::from_domains(&["sealed.example.com".into()]).unwrap();
        let cert = StoredCert {
            chain_pem: "-----BEGIN CERTIFICATE-----\nAAA\n".into(),
            key_pem: "-----BEGIN PRIVATE KEY-----\nSECRET-KEY-BYTES\n".into(),
            issued_at: 42,
        };
        storage.save_cert(&id, &cert).await.unwrap();
        storage.save_account(b"ACCOUNT-SECRET").await.unwrap();

        let mut conn = client.conn().await.unwrap();
        let raw_cert: String = conn.get(cert_key(client.key_prefix(), &id)).await.unwrap();
        assert!(raw_cert.contains("BEGIN CERTIFICATE"), "chain is stored in clear");
        assert!(!raw_cert.contains("SECRET-KEY-BYTES"), "key must be sealed: {raw_cert}");
        let raw_acct: String = conn.get(account_key(client.key_prefix())).await.unwrap();
        assert!(!raw_acct.contains("ACCOUNT-SECRET"));

        // A different secret cannot open it.
        let other = RedisCertStorage::new(client.clone(), "wrong");
        assert!(other.load_cert(&id).await.is_err());
        assert_eq!(storage.load_cert(&id).await.unwrap().unwrap(), cert);
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test acme::storage::redis 2>&1 | grep -E "^error" | head -3`
Expected: `RedisCertStorage` not found.

- [ ] **Step 4: Implement `src/acme/storage/redis.rs`**

```rust
//! `stores:`-backed `CertStorage` (redis/valkey; `redis-store` feature).
//!
//! Keys under the store's `key_prefix`: `acme:account`, `acme:cert:{<id>}`,
//! `acme:challenge:<domain>` (with TTL), `acme:lease:{<id>}` (`SET NX PX`,
//! owner-checked renew/release via Lua). Account credentials and certificate
//! private keys are sealed with [`CookieSealer`] (AES-256-GCM, key = SHA-256 of
//! `acme.storage.encryption_key`) before they are written; chains are stored
//! in clear. Hash tags keep one certificate's keys on a single Cluster slot.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

use super::CertStorage;
use crate::acme::{AcmeError, CertId, StoredCert};
use crate::plugins::util::cookie_session::CookieSealer;
use crate::stores::redis_store::RedisStoreClient;

/// Sealed blobs never expire on their own; storage TTLs govern lifetime.
const SEAL_TTL: Duration = Duration::from_secs(100 * 365 * 86_400);

const RENEW_LEASE_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('PEXPIRE', KEYS[1], ARGV[2])
else
  return 0
end"#;

const RELEASE_LEASE_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
else
  return 0
end"#;

pub(super) fn account_key(prefix: &str) -> String {
    format!("{prefix}:acme:account")
}
pub(super) fn cert_key(prefix: &str, id: &CertId) -> String {
    format!("{prefix}:acme:cert:{{{}}}", id.as_str())
}
fn challenge_key(prefix: &str, domain: &str) -> String {
    format!("{prefix}:acme:challenge:{domain}")
}
fn lease_key(prefix: &str, id: &CertId) -> String {
    format!("{prefix}:acme:lease:{{{}}}", id.as_str())
}

#[derive(Serialize, Deserialize)]
struct CertRecord {
    chain_pem: String,
    /// `CookieSealer::seal(key_pem)`.
    key_sealed: String,
    issued_at: i64,
}

pub struct RedisCertStorage {
    client: Arc<RedisStoreClient>,
    sealer: CookieSealer,
    renew_script: redis::Script,
    release_script: redis::Script,
}

impl RedisCertStorage {
    pub fn new(client: Arc<RedisStoreClient>, encryption_key: &str) -> Self {
        Self {
            client,
            sealer: CookieSealer::new(encryption_key),
            renew_script: redis::Script::new(RENEW_LEASE_SCRIPT),
            release_script: redis::Script::new(RELEASE_LEASE_SCRIPT),
        }
    }

    async fn conn(&self) -> Result<redis::aio::ConnectionManager, AcmeError> {
        self.client.conn().await.map_err(AcmeError::Storage)
    }

    fn err(&self, what: &str, e: impl std::fmt::Display) -> AcmeError {
        AcmeError::Storage(format!("store '{}': {what}: {e}", self.client.name()))
    }

    fn open(&self, sealed: &str, what: &str) -> Result<Vec<u8>, AcmeError> {
        self.sealer.open(sealed).map_err(|e| {
            AcmeError::Storage(format!(
                "store '{}': cannot unseal {what} ({e}) — was acme.storage.encryption_key changed?",
                self.client.name()
            ))
        })
    }

    fn prefix(&self) -> &str {
        self.client.key_prefix()
    }
}

#[async_trait]
impl CertStorage for RedisCertStorage {
    fn label(&self) -> String {
        format!("store:{}", self.client.name())
    }

    async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError> {
        let mut conn = self.conn().await?;
        let sealed: Option<String> = conn
            .get(account_key(self.prefix()))
            .await
            .map_err(|e| self.err("load_account", e))?;
        sealed.map(|s| self.open(&s, "account credentials")).transpose()
    }

    async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        conn.set::<_, _, ()>(account_key(self.prefix()), self.sealer.seal(creds, SEAL_TTL))
            .await
            .map_err(|e| self.err("save_account", e))
    }

    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn
            .get(cert_key(self.prefix(), id))
            .await
            .map_err(|e| self.err("load_cert", e))?;
        let Some(raw) = raw else { return Ok(None) };
        let rec: CertRecord =
            serde_json::from_str(&raw).map_err(|e| self.err("load_cert: corrupt record", e))?;
        let key = self.open(&rec.key_sealed, "certificate private key")?;
        Ok(Some(StoredCert {
            chain_pem: rec.chain_pem,
            key_pem: String::from_utf8(key).map_err(|e| self.err("load_cert: key utf8", e))?,
            issued_at: rec.issued_at,
        }))
    }

    async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<(), AcmeError> {
        let rec = CertRecord {
            chain_pem: cert.chain_pem.clone(),
            key_sealed: self.sealer.seal(cert.key_pem.as_bytes(), SEAL_TTL),
            issued_at: cert.issued_at,
        };
        let mut conn = self.conn().await?;
        conn.set::<_, _, ()>(cert_key(self.prefix(), id), serde_json::to_string(&rec).unwrap())
            .await
            .map_err(|e| self.err("save_cert", e))
    }

    async fn put_challenge(&self, domain: &str, key_auth: &str, ttl: Duration) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        redis::cmd("SET")
            .arg(challenge_key(self.prefix(), domain))
            .arg(key_auth)
            .arg("PX")
            .arg(ttl.as_millis().max(1) as u64)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| self.err("put_challenge", e))
    }

    async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError> {
        let mut conn = self.conn().await?;
        conn.get(challenge_key(self.prefix(), domain))
            .await
            .map_err(|e| self.err("get_challenge", e))
    }

    async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        conn.del::<_, ()>(challenge_key(self.prefix(), domain))
            .await
            .map_err(|e| self.err("remove_challenge", e))
    }

    async fn try_acquire_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError> {
        let mut conn = self.conn().await?;
        let key = lease_key(self.prefix(), id);
        let acquired: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg(owner)
            .arg("NX")
            .arg("PX")
            .arg(ttl.as_millis().max(1) as u64)
            .query_async(&mut conn)
            .await
            .map_err(|e| self.err("try_acquire_lease", e))?;
        if acquired.is_some() {
            return Ok(true);
        }
        // Re-entrant for the current owner (and refreshes its TTL).
        self.renew_lease(id, owner, ttl).await
    }

    async fn renew_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool, AcmeError> {
        let mut conn = self.conn().await?;
        let n: i64 = self
            .renew_script
            .key(lease_key(self.prefix(), id))
            .arg(owner)
            .arg(ttl.as_millis().max(1) as u64)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| self.err("renew_lease", e))?;
        Ok(n == 1)
    }

    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        let _: i64 = self
            .release_script
            .key(lease_key(self.prefix(), id))
            .arg(owner)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| self.err("release_lease", e))?;
        Ok(())
    }
}
```

Restore `#[cfg(feature = "redis-store")] pub mod redis;` in `src/acme/storage/mod.rs`. If `CookieSealer`'s module path is not reachable, check `src/plugins/util/mod.rs` declares `pub mod cookie_session;` (it does today) and that `CookieError` implements `Display` (it does).

- [ ] **Step 5: Run tests (with and without a live redis)**

Run: `cargo test acme::storage 2>&1 | tail -6` — expected: redis tests print the skip line and pass.
If you have redis locally: `FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test acme::storage::redis 2>&1 | tail -6` — expected: 2 passed.
Run: `cargo check --no-default-features` — expected: compiles (module gated out).

- [ ] **Step 6: Commit**

```bash
git add src/acme/storage src/stores/mod.rs
git commit -m "feat(acme): redis CertStorage with sealed keys and SET NX leases"
```

---

### Task 6: TLS-ALPN-01 challenge solver

**Files:**
- Create: `src/acme/challenge.rs`
- Modify: `src/acme/mod.rs` (`pub mod challenge;`)

**Interfaces:**
- Consumes: `CertStorage` (Task 4).
- Produces:
  ```rust
  pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";
  pub const CHALLENGE_TTL: Duration = Duration::from_secs(600);
  pub trait ChallengeSolver: Send + Sync + std::fmt::Debug {
      /// Sync: called from rustls' `ResolvesServerCert` on the connection task.
      fn challenge_cert(&self, server_name: &str) -> Option<Arc<CertifiedKey>>;
  }
  pub fn key_authorization_digest(key_auth: &str) -> Vec<u8>;              // SHA-256
  pub fn build_challenge_cert(domain: &str, key_auth: &str) -> Result<Arc<CertifiedKey>, AcmeError>;
  pub struct TlsAlpnSolver;  // Debug
  impl TlsAlpnSolver {
      pub fn new(storage: Arc<dyn CertStorage>) -> Arc<Self>;
      pub async fn register(&self, domain: &str, key_auth: &str) -> Result<(), AcmeError>;  // storage + local cache
      pub async fn clear(&self, domain: &str) -> Result<(), AcmeError>;
      pub async fn refresh_from_storage(&self, domains: &[String]) -> Result<(), AcmeError>;  // peers
      pub fn cached_domains(&self) -> Vec<String>;
  }
  impl ChallengeSolver for TlsAlpnSolver
  ```

- [ ] **Step 1: Write the failing tests** (bottom of `src/acme/challenge.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::storage::fs::FsCertStorage;

    /// The `acmeIdentifier` extension (RFC 8737 §3): OID 1.3.6.1.5.5.7.1.31,
    /// critical, value = DER OCTET STRING of the SHA-256 key-auth digest.
    fn acme_identifier(leaf_der: &[u8]) -> Option<(bool, Vec<u8>)> {
        use x509_parser::prelude::*;
        let (_, cert) = X509Certificate::from_der(leaf_der).unwrap();
        cert.extensions()
            .iter()
            .find(|e| e.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
            .map(|e| (e.critical, e.value.to_vec()))
    }

    fn temp_storage(tag: &str) -> Arc<dyn CertStorage> {
        let d = std::env::temp_dir().join(format!("fb_acme_ch_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Arc::new(FsCertStorage::new(d))
    }

    #[test]
    fn challenge_cert_carries_critical_acme_identifier_with_digest() {
        let ck = build_challenge_cert("x.example.com", "token.thumbprint").unwrap();
        let leaf = ck.end_entity_cert().unwrap();
        let (critical, value) = acme_identifier(leaf.as_ref()).expect("extension present");
        assert!(critical);
        let digest = key_authorization_digest("token.thumbprint");
        assert_eq!(value.len(), 34);
        assert_eq!(&value[..2], &[0x04, 0x20]);
        assert_eq!(&value[2..], digest.as_slice());
        assert_eq!(crate::acme::leaf_dns_sans(leaf.as_ref()).unwrap(), vec!["x.example.com".to_string()]);
    }

    #[tokio::test]
    async fn register_serves_then_clear_stops() {
        let solver = TlsAlpnSolver::new(temp_storage("reg"));
        assert!(solver.challenge_cert("a.example.com").is_none());
        solver.register("a.example.com", "ka").await.unwrap();
        assert!(solver.challenge_cert("a.example.com").is_some());
        assert!(solver.challenge_cert("A.EXAMPLE.COM").is_some(), "SNI is case-insensitive");
        assert!(solver.challenge_cert("b.example.com").is_none());
        assert_eq!(solver.cached_domains(), vec!["a.example.com".to_string()]);
        solver.clear("a.example.com").await.unwrap();
        assert!(solver.challenge_cert("a.example.com").is_none());
    }

    #[tokio::test]
    async fn refresh_from_storage_picks_up_a_peer_registration() {
        let storage = temp_storage("peer");
        let peer = TlsAlpnSolver::new(storage.clone());
        let me = TlsAlpnSolver::new(storage);
        peer.register("p.example.com", "peer-ka").await.unwrap();
        assert!(me.challenge_cert("p.example.com").is_none());
        me.refresh_from_storage(&["p.example.com".to_string(), "none.example.com".to_string()]).await.unwrap();
        let ck = me.challenge_cert("p.example.com").expect("adopted from storage");
        let (_, value) = acme_identifier(ck.end_entity_cert().unwrap().as_ref()).unwrap();
        assert_eq!(&value[2..], key_authorization_digest("peer-ka").as_slice());
        peer.clear("p.example.com").await.unwrap();
        me.refresh_from_storage(&["p.example.com".to_string()]).await.unwrap();
        assert!(me.challenge_cert("p.example.com").is_none(), "cleared upstream ⇒ evicted");
    }

    #[tokio::test]
    async fn cache_entries_expire() {
        let solver = TlsAlpnSolver::with_ttl(temp_storage("ttl"), Duration::from_millis(30));
        solver.register("t.example.com", "ka").await.unwrap();
        assert!(solver.challenge_cert("t.example.com").is_some());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(solver.challenge_cert("t.example.com").is_none());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test acme::challenge 2>&1 | grep -E "^error" | head -3`
Expected: module not found.

- [ ] **Step 3: Implement `src/acme/challenge.rs`**

```rust
//! TLS-ALPN-01 (RFC 8737) challenge solving.
//!
//! The CA opens a TLS connection to the domain on 443 with ALPN `acme-tls/1`
//! and expects a self-signed certificate whose SAN is the domain and which
//! carries a critical `acmeIdentifier` extension holding the SHA-256 of the key
//! authorization. `server::tls`' resolver asks a [`ChallengeSolver`] for that
//! certificate; [`TlsAlpnSolver`] builds it from the key authorization the
//! order state machine registered.
//!
//! The resolver call is synchronous, so the solver keeps an in-process cache.
//! Registrations also go to [`CertStorage`] so a *peer* instance (behind a TCP
//! load balancer) can adopt them via [`TlsAlpnSolver::refresh_from_storage`].

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use rustls::sign::CertifiedKey;

use super::storage::CertStorage;
use super::{load_certified_key, AcmeError};

pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";
/// A pending challenge that outlives this is stale; storage and cache both drop it.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(600);

pub trait ChallengeSolver: Send + Sync + std::fmt::Debug {
    /// The challenge certificate for `server_name` (SNI, any case), if a
    /// validation is pending for it.
    fn challenge_cert(&self, server_name: &str) -> Option<Arc<CertifiedKey>>;
}

pub fn key_authorization_digest(key_auth: &str) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, key_auth.as_bytes())
        .as_ref()
        .to_vec()
}

/// Self-signed challenge certificate: SAN = `domain`, critical `acmeIdentifier`
/// = SHA-256(`key_auth`), fresh key, valid for the challenge TTL.
pub fn build_challenge_cert(domain: &str, key_auth: &str) -> Result<Arc<CertifiedKey>, AcmeError> {
    let mut params = rcgen::CertificateParams::new(vec![domain.to_string()])
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    let mut ext = rcgen::CustomExtension::new_acme_identifier(&key_authorization_digest(key_auth));
    ext.set_criticality(true);
    params.custom_extensions = vec![ext];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::seconds(CHALLENGE_TTL.as_secs() as i64 + 300);
    let key = rcgen::KeyPair::generate().map_err(|e| AcmeError::Crypto(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    load_certified_key(&cert.pem(), &key.serialize_pem()).map(|(ck, _)| ck)
}

struct Cached {
    key_auth: String,
    cert: Arc<CertifiedKey>,
    expires_at: Instant,
}

pub struct TlsAlpnSolver {
    storage: Arc<dyn CertStorage>,
    cache: RwLock<HashMap<String, Cached>>,
    ttl: Duration,
}

impl std::fmt::Debug for TlsAlpnSolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TlsAlpnSolver({} pending)", self.cache.read().map(|c| c.len()).unwrap_or(0))
    }
}

impl TlsAlpnSolver {
    pub fn new(storage: Arc<dyn CertStorage>) -> Arc<Self> {
        Self::with_ttl(storage, CHALLENGE_TTL)
    }

    pub fn with_ttl(storage: Arc<dyn CertStorage>, ttl: Duration) -> Arc<Self> {
        Arc::new(Self { storage, cache: RwLock::new(HashMap::new()), ttl })
    }

    fn cache_insert(&self, domain: &str, key_auth: &str) -> Result<(), AcmeError> {
        let cert = build_challenge_cert(domain, key_auth)?;
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            domain.to_ascii_lowercase(),
            Cached { key_auth: key_auth.to_string(), cert, expires_at: Instant::now() + self.ttl },
        );
        Ok(())
    }

    fn cache_remove(&self, domain: &str) {
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.remove(&domain.to_ascii_lowercase());
    }

    /// Registers a pending validation: persisted (for peers) and cached (for
    /// this instance's resolver).
    pub async fn register(&self, domain: &str, key_auth: &str) -> Result<(), AcmeError> {
        self.storage.put_challenge(domain, key_auth, self.ttl).await?;
        self.cache_insert(domain, key_auth)
    }

    pub async fn clear(&self, domain: &str) -> Result<(), AcmeError> {
        self.cache_remove(domain);
        self.storage.remove_challenge(domain).await
    }

    /// Adopts (or evicts) challenges another instance registered in storage.
    pub async fn refresh_from_storage(&self, domains: &[String]) -> Result<(), AcmeError> {
        for domain in domains {
            match self.storage.get_challenge(domain).await? {
                Some(key_auth) => {
                    let already = {
                        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
                        cache
                            .get(&domain.to_ascii_lowercase())
                            .is_some_and(|c| c.key_auth == key_auth)
                    };
                    if !already {
                        self.cache_insert(domain, &key_auth)?;
                    }
                }
                None => self.cache_remove(domain),
            }
        }
        Ok(())
    }

    pub fn cached_domains(&self) -> Vec<String> {
        let now = Instant::now();
        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<String> =
            cache.iter().filter(|(_, c)| c.expires_at > now).map(|(d, _)| d.clone()).collect();
        v.sort();
        v
    }
}

impl ChallengeSolver for TlsAlpnSolver {
    fn challenge_cert(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let name = server_name.to_ascii_lowercase();
        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
        match cache.get(&name) {
            Some(c) if c.expires_at > Instant::now() => Some(c.cert.clone()),
            _ => None,
        }
    }
}
```

Add `pub mod challenge;` to `src/acme/mod.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test acme::challenge 2>&1 | tail -8`
Expected: 4 passed. If `new_acme_identifier` already marks the extension critical, `set_criticality(true)` is harmless.

- [ ] **Step 5: Commit**

```bash
git add src/acme
git commit -m "feat(acme): TLS-ALPN-01 challenge solver with storage-backed peer refresh"
```

---

### Task 7: `tls.rs` — managed cert slots, `acme-tls/1` resolver branch, connection close

**Files:**
- Modify: `src/server/tls.rs` (resolver, `build_server_config`, `build_reloadable`, `spawn_cert_watcher`, new `negotiated_acme_challenge`, tests)
- Modify: `src/server/listener.rs` (pass `None` for now; close `acme-tls/1` connections), `src/admin/mod.rs` (pass `None`)

**Interfaces:**
- Consumes: `crate::acme::{ManagedCerts, CertId}`, `crate::acme::challenge::{ChallengeSolver, ACME_TLS_ALPN}`.
- Produces:
  ```rust
  #[derive(Clone, Debug)]
  pub struct AcmeHooks { pub certs: ManagedCerts, pub solver: Arc<dyn ChallengeSolver> }
  pub fn build_server_config(tls: &TlsConfig, http2_enabled: bool, acme: Option<&AcmeHooks>) -> Result<Arc<ServerConfig>, TlsError>
  pub fn build_reloadable(tls: &TlsConfig, http2_enabled: bool, acme: Option<&AcmeHooks>) -> Result<SharedTlsConfig, TlsError>
  pub fn spawn_cert_watcher(tls: TlsConfig, http2_enabled: bool, shared: SharedTlsConfig, label: &'static str, acme: Option<AcmeHooks>)
  pub fn negotiated_acme_challenge<IO>(stream: &tokio_rustls::server::TlsStream<IO>) -> bool
  ```
  `build_acceptor(tls, http2)` keeps its signature (passes `None`).

- [ ] **Step 1: Write the failing tests** (in `src/server/tls.rs`'s test module, after `test_sni_multicert_selects_by_hostname`)

```rust
    // ---- ACME: managed slots + TLS-ALPN-01 ----

    #[derive(Debug)]
    struct FakeSolver(std::sync::Mutex<std::collections::HashMap<String, Arc<CertifiedKey>>>);

    impl crate::acme::challenge::ChallengeSolver for FakeSolver {
        fn challenge_cert(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
            self.0.lock().unwrap().get(server_name).cloned()
        }
    }

    fn managed_tls(domains: &[&str]) -> TlsConfig {
        TlsConfig {
            cert_path: None,
            key_path: None,
            acme: Some(crate::config::AcmeSlot { domains: domains.iter().map(|d| d.to_string()).collect() }),
            min_version: "1.2".to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: Vec::new(),
        }
    }

    fn hooks_with_placeholder(domains: &[&str]) -> (crate::server::tls::AcmeHooks, crate::acme::CertId, Vec<u8>) {
        let domains: Vec<String> = domains.iter().map(|d| d.to_string()).collect();
        let (id, norm) = crate::acme::CertId::from_domains(&domains).unwrap();
        let (key, leaf) = crate::acme::placeholder_cert(&norm).unwrap();
        let certs = crate::acme::new_managed_certs();
        crate::acme::publish(
            &certs,
            &id,
            crate::acme::ManagedCert {
                key,
                leaf_der: leaf.clone(),
                state: crate::acme::CertState::Placeholder,
                meta: crate::acme::CertMeta::default(),
                domains: norm,
            },
        );
        let solver: Arc<dyn crate::acme::challenge::ChallengeSolver> =
            Arc::new(FakeSolver(std::sync::Mutex::new(Default::default())));
        (AcmeHooks { certs, solver }, id, leaf)
    }

    /// Like `served_leaf_cert_sni` but offering the given ALPN list; `Err` when
    /// the handshake is refused.
    async fn handshake_with_alpn(
        addr: std::net::SocketAddr,
        sni: &str,
        alpn: Vec<Vec<u8>>,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
        use tokio::net::TcpStream;
        install_crypto_provider();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let verifier = std::sync::Arc::new(CapturingVerifier {
            captured: captured.clone(),
            provider: rustls::crypto::ring::default_provider(),
        });
        let mut config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = alpn;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from(sni.to_string()).unwrap();
        let stream = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
        let negotiated = stream.get_ref().1.alpn_protocol().map(|p| p.to_vec());
        let leaf = captured.lock().unwrap().clone().ok_or("no cert")?;
        Ok((leaf, negotiated))
    }

    #[tokio::test]
    async fn test_managed_slot_serves_placeholder_then_swapped_cert_without_rebuild() {
        let (hooks, id, placeholder_leaf) = hooks_with_placeholder(&["m.example.com"]);
        let tls = managed_tls(&["m.example.com"]);
        let shared = build_reloadable(&tls, false, Some(&hooks)).unwrap();
        let addr = spawn_reload_server(shared).await;

        assert_eq!(served_leaf_cert_sni(addr, "m.example.com").await, placeholder_leaf);
        // No SNI / unknown SNI falls back to the managed default too.
        assert_eq!(served_leaf_cert_sni(addr, "other.example.com").await, placeholder_leaf);

        // "Issue": publish a new cert into the map — no ServerConfig rebuild.
        let issued = rcgen::generate_simple_self_signed(vec!["m.example.com".to_string()]).unwrap();
        let (key, leaf) =
            crate::acme::load_certified_key(&issued.cert.pem(), &issued.signing_key.serialize_pem()).unwrap();
        crate::acme::update(&hooks.certs, &id, |c| {
            c.key = key;
            c.leaf_der = leaf.clone();
            c.state = crate::acme::CertState::Issued;
        });
        assert_eq!(served_leaf_cert_sni(addr, "m.example.com").await, leaf);
    }

    #[tokio::test]
    async fn test_acme_tls_alpn_serves_challenge_cert_and_refuses_without_one() {
        // `hooks_with_placeholder` installs an empty FakeSolver; build a second
        // hooks value sharing the same cert map but with a pending challenge.
        let (empty_hooks, _, placeholder_leaf) = hooks_with_placeholder(&["m.example.com"]);
        let challenge = crate::acme::challenge::build_challenge_cert("m.example.com", "ka").unwrap();
        let challenge_leaf = challenge.end_entity_cert().unwrap().as_ref().to_vec();
        let pending = Arc::new(FakeSolver(std::sync::Mutex::new(Default::default())));
        pending.0.lock().unwrap().insert("m.example.com".to_string(), challenge.clone());
        let pending_hooks = AcmeHooks { certs: empty_hooks.certs.clone(), solver: pending };
        let tls = managed_tls(&["m.example.com"]);

        let addr_pending = spawn_reload_server(build_reloadable(&tls, true, Some(&pending_hooks)).unwrap()).await;
        let addr_empty = spawn_reload_server(build_reloadable(&tls, true, Some(&empty_hooks)).unwrap()).await;

        // Validator: gets the challenge cert and negotiates acme-tls/1.
        let (leaf, alpn) =
            handshake_with_alpn(addr_pending, "m.example.com", vec![b"acme-tls/1".to_vec()]).await.unwrap();
        assert_eq!(leaf, challenge_leaf);
        assert_eq!(alpn.as_deref(), Some(&b"acme-tls/1"[..]));

        // A normal client on the same listener is unaffected.
        let (leaf, alpn) =
            handshake_with_alpn(addr_pending, "m.example.com", vec![b"h2".to_vec(), b"http/1.1".to_vec()])
                .await
                .unwrap();
        assert_eq!(leaf, placeholder_leaf);
        assert_eq!(alpn.as_deref(), Some(&b"h2"[..]));

        // No pending challenge for the name ⇒ handshake refused (RFC 8737 §3).
        assert!(handshake_with_alpn(addr_empty, "m.example.com", vec![b"acme-tls/1".to_vec()]).await.is_err());
    }

    #[tokio::test]
    async fn test_acme_alpn_not_advertised_without_acme() {
        let (tls, cert, key) = self_signed("noacme", "1.2");
        let shared = build_reloadable(&tls, true, None).unwrap();
        let addr = spawn_reload_server(shared).await;
        // Client offering only acme-tls/1 against a non-ACME listener: rustls
        // refuses (no overlap), which is the pre-existing behavior.
        assert!(handshake_with_alpn(addr, "localhost", vec![b"acme-tls/1".to_vec()]).await.is_err());
        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
    }

    #[test]
    fn test_managed_slot_without_hooks_is_a_wiring_error() {
        let tls = managed_tls(&["m.example.com"]);
        let err = build_server_config(&tls, true, None).unwrap_err();
        assert!(matches!(err, TlsError::AcmeNotWired(_)), "{err}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_managed_slot test_acme 2>&1 | grep -E "^error" | head -5`
Expected: `AcmeHooks`, third argument, `AcmeNotWired` not found.

- [ ] **Step 3: Implement the resolver changes**

Replace the `SniCertResolver` definition + impl at the top of `src/server/tls.rs`:

```rust
use crate::acme::challenge::{ChallengeSolver, ACME_TLS_ALPN};
use crate::acme::ManagedCerts;

/// What the ACME subsystem hands the TLS layer: the live managed-cert map and
/// the TLS-ALPN-01 challenge solver. `None` everywhere ACME is not configured.
#[derive(Clone, Debug)]
pub struct AcmeHooks {
    pub certs: ManagedCerts,
    pub solver: Arc<dyn ChallengeSolver>,
}

/// Where one certificate slot's `CertifiedKey` comes from.
#[derive(Debug)]
enum CertSlot {
    /// Loaded from `cert_path`/`key_path` at (re)build time.
    File(Arc<CertifiedKey>),
    /// Looked up in `AcmeHooks::certs` on every ClientHello (so a renewal is a
    /// map swap, not a `ServerConfig` rebuild).
    Managed(String),
}

/// Resolves the server certificate: a ClientHello offering exactly ALPN
/// `acme-tls/1` is a CA validation and gets the pending challenge cert (or is
/// refused); otherwise SNI hostname (exact or single-label wildcard) selects a
/// slot, falling back to the default.
#[derive(Debug)]
struct SniCertResolver {
    certs: Vec<(SniPattern, CertSlot)>,
    default: CertSlot,
    acme: Option<AcmeHooks>,
}

impl SniCertResolver {
    fn slot_key(&self, slot: &CertSlot) -> Option<Arc<CertifiedKey>> {
        match slot {
            CertSlot::File(ck) => Some(ck.clone()),
            CertSlot::Managed(id) => self.acme.as_ref()?.certs.load().get(id).map(|c| c.key.clone()),
        }
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(hooks) = &self.acme {
            let only_acme = client_hello
                .alpn()
                .map(|mut alpn| alpn.next() == Some(ACME_TLS_ALPN) && alpn.next().is_none())
                .unwrap_or(false);
            if only_acme {
                // RFC 8737 §3: no pending challenge for this name ⇒ abort.
                return hooks.solver.challenge_cert(client_hello.server_name()?);
            }
        }
        if let Some(name) = client_hello.server_name() {
            for (pattern, slot) in &self.certs {
                if pattern.matches(name) {
                    return self.slot_key(slot);
                }
            }
        }
        self.slot_key(&self.default)
    }
}
```

Add to `TlsError`:

```rust
    #[error("TLS slot '{0}' is ACME-managed but no ACME runtime was provided (is `acme:` configured?)")]
    AcmeNotWired(String),
```

Rewrite the certificate-selection part of `build_server_config` (signature gains `acme: Option<&AcmeHooks>`):

```rust
    fn slot(
        what: &str,
        cert: &Option<String>,
        key: &Option<String>,
        acme_slot: &Option<crate::config::AcmeSlot>,
        domains_for_id: Vec<String>,
        acme: Option<&AcmeHooks>,
        provider: &Arc<rustls::crypto::CryptoProvider>,
    ) -> Result<CertSlot, TlsError> {
        if acme_slot.is_some() {
            if acme.is_none() {
                return Err(TlsError::AcmeNotWired(what.to_string()));
            }
            let (id, _) = crate::acme::CertId::from_domains(&domains_for_id)
                .map_err(|e| TlsError::RustlsConfig(e.to_string()))?;
            return Ok(CertSlot::Managed(id.as_str().to_string()));
        }
        let (c, k) = file_pair(cert, key, what)?;
        Ok(CertSlot::File(Arc::new(certified_key(load_cert_chain(c)?, load_private_key(k)?, provider)?)))
    }

    let default = slot(
        "default",
        &tls.cert_path,
        &tls.key_path,
        &tls.acme,
        tls.acme.as_ref().map(|s| s.domains.clone()).unwrap_or_default(),
        acme,
        &provider,
    )?;
    let mut certs = Vec::with_capacity(tls.sni_certs.len());
    for sc in &tls.sni_certs {
        let domains = sc
            .acme_domains()
            .map_err(TlsError::RustlsConfig)?
            .unwrap_or_default();
        certs.push((
            SniPattern::parse(&sc.server_name),
            slot(&sc.server_name, &sc.cert_path, &sc.key_path, &sc.acme, domains, acme, &provider)?,
        ));
    }

    // Always the resolver — it is what `with_single_cert` builds internally
    // (`AlwaysResolvesChain`), so a single file-based cert behaves identically.
    let mut config = builder.with_cert_resolver(Arc::new(SniCertResolver {
        certs,
        default,
        acme: acme.cloned(),
    }));
```

Delete the old `let chain = …; let key = …;` lines and the old `if tls.sni_certs.is_empty() { with_single_cert … } else { … }` block they fed; `certified_key` is still used by `slot()`.

ALPN: after the existing `config.alpn_protocols = …` assignment add

```rust
    if acme.is_some() {
        // Advertised last: browsers offering h2/http1.1 never pick it, and
        // rustls would otherwise abort a validator's acme-tls/1-only hello with
        // no_application_protocol before the resolver could answer.
        config.alpn_protocols.push(ACME_TLS_ALPN.to_vec());
    }
```

`build_acceptor` passes `None`. `build_reloadable(tls, http2_enabled, acme: Option<&AcmeHooks>)` forwards it. `spawn_cert_watcher(..., acme: Option<AcmeHooks>)` forwards `acme.as_ref()` into the rebuild call. Add:

```rust
/// True when the finished handshake negotiated `acme-tls/1`: the connection
/// was a CA validation and must be closed without serving anything.
pub fn negotiated_acme_challenge<IO>(stream: &tokio_rustls::server::TlsStream<IO>) -> bool {
    stream.get_ref().1.alpn_protocol() == Some(ACME_TLS_ALPN)
}
```

- [ ] **Step 4: Update callers**

`src/server/listener.rs`: `tls::build_reloadable(tls_cfg, http2_enabled, None)?; tls::spawn_cert_watcher(tls_cfg.clone(), http2_enabled, shared.clone(), "data-plane", None);` (Task 12 replaces `None`). After `Ok(tls_stream) => {` insert:

```rust
                                if tls::negotiated_acme_challenge(&tls_stream) {
                                    tracing::debug!("acme-tls/1 validation handshake from {}; closing", remote_addr);
                                    return;
                                }
```

`src/admin/mod.rs`: `tls::build_reloadable(tls_cfg, true, None)?; tls::spawn_cert_watcher(tls_cfg.clone(), true, shared.clone(), "admin", None);`.

Existing tests: add `, None` to every `build_reloadable(&tls, …)` / `build_server_config(&tls, …)` / `spawn_cert_watcher(…)` call.

- [ ] **Step 5: Run tests**

Run: `cargo test server::tls 2>&1 | tail -6 && cargo test 2>&1 | tail -3`
Expected: all TLS tests pass including the 4 new ones; full suite green.

- [ ] **Step 6: Commit**

```bash
git add src/server/tls.rs src/server/listener.rs src/admin/mod.rs
git commit -m "feat(tls): managed certificate slots and TLS-ALPN-01 resolver branch"
```

---

### Task 8: `AcmeClient` traits, `instant-acme` implementation, mock CA for tests

**Files:**
- Create: `src/acme/client.rs`
- Modify: `src/acme/mod.rs` (`pub mod client;`)

**Interfaces:**
- Consumes: `AcmeConfig`, `AcmeEabConfig` (Task 2), `CertStorage` (Task 4).
- Produces:
  ```rust
  pub struct PendingChallenge { pub domain: String, pub key_auth: String }
  #[async_trait] pub trait AcmeOrder: Send {
      async fn pending_challenges(&mut self) -> Result<Vec<PendingChallenge>, AcmeError>;
      async fn mark_ready(&mut self, domain: &str) -> Result<(), AcmeError>;
      async fn wait_ready(&mut self) -> Result<(), AcmeError>;          // poll until Ready; Err on Invalid/timeout
      async fn finalize(&mut self, csr_der: &[u8]) -> Result<String, AcmeError>;  // chain PEM
  }
  #[async_trait] pub trait AcmeClient: Send + Sync {
      async fn new_order(&self, domains: &[String]) -> Result<Box<dyn AcmeOrder>, AcmeError>;
      /// ARI suggested window (start, end) as unix seconds; `Ok(None)` when the CA offers none.
      async fn renewal_window(&self, leaf_der: &[u8]) -> Result<Option<(i64, i64)>, AcmeError>;
  }
  #[async_trait] pub trait AcmeClientFactory: Send + Sync {
      async fn connect(&self) -> Result<Arc<dyn AcmeClient>, AcmeError>;
  }
  pub struct InstantAcmeFactory; InstantAcmeFactory::new(cfg: AcmeConfig, storage: Arc<dyn CertStorage>) -> Self
  pub fn decode_hmac_key(s: &str) -> Result<Vec<u8>, AcmeError>
  #[cfg(test)] pub(crate) mod mock { pub struct MockAcmeClient; pub struct MockBehavior { fail_step: Option<MockStep>, ari: Option<(i64,i64)>, wrong_key_chain: bool, validity_secs: i64 }; pub enum MockStep { NewOrder, WaitReady, Finalize }; pub struct MockFactory }
  ```

- [ ] **Step 1: Write the failing tests** (bottom of `src/acme/client.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_hmac_key_accepts_url_safe_and_standard_base64() {
        assert_eq!(decode_hmac_key("AQID").unwrap(), vec![1, 2, 3]);
        assert_eq!(decode_hmac_key("_-8").unwrap(), vec![0xff, 0xef]);
        assert_eq!(decode_hmac_key("/+8=").unwrap(), vec![0xff, 0xef]);
        assert!(decode_hmac_key("not base64!").is_err());
    }

    #[tokio::test]
    async fn mock_client_issues_a_chain_for_the_csr_key() {
        let client = mock::MockAcmeClient::new(mock::MockBehavior::default());
        let mut order = client.new_order(&["a.example.com".into(), "b.example.com".into()]).await.unwrap();
        let pending = order.pending_challenges().await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(order.wait_ready().await.is_err(), "not all challenges marked ready");
        for p in &pending {
            order.mark_ready(&p.domain).await.unwrap();
        }
        order.wait_ready().await.unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let csr = rcgen::CertificateParams::new(vec!["a.example.com".to_string(), "b.example.com".to_string()])
            .unwrap()
            .serialize_request(&key)
            .unwrap();
        let chain = order.finalize(csr.der()).await.unwrap();
        let (ck, leaf) = crate::acme::load_certified_key(&chain, &key.serialize_pem()).unwrap();
        assert!(ck.end_entity_cert().is_ok());
        let mut sans = crate::acme::leaf_dns_sans(&leaf).unwrap();
        sans.sort();
        assert_eq!(sans, vec!["a.example.com".to_string(), "b.example.com".to_string()]);
        assert!(crate::acme::parse_cert_meta(&leaf).unwrap().issuer.contains("Mock ACME CA"));
        assert_eq!(client.orders(), 1);
    }

    #[tokio::test]
    async fn mock_client_honors_failure_script_and_ari() {
        let client = mock::MockAcmeClient::new(mock::MockBehavior {
            fail_step: Some(mock::MockStep::Finalize),
            ari: Some((100, 200)),
            ..Default::default()
        });
        let mut order = client.new_order(&["a.example.com".into()]).await.unwrap();
        for p in order.pending_challenges().await.unwrap() {
            order.mark_ready(&p.domain).await.unwrap();
        }
        order.wait_ready().await.unwrap();
        assert!(order.finalize(b"irrelevant").await.is_err());
        assert_eq!(client.renewal_window(b"any").await.unwrap(), Some((100, 200)));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test acme::client 2>&1 | grep -E "^error" | head -3`
Expected: module not found.

- [ ] **Step 3: Implement `src/acme/client.rs`**

```rust
//! The ACME protocol boundary.
//!
//! [`AcmeClient`]/[`AcmeOrder`] are the *only* surface the order state machine
//! (`order.rs`) and the scheduler (`manager.rs`) see, so both are unit-tested
//! against [`mock::MockAcmeClient`] with no network. [`InstantAcmeClient`]
//! implements them over `instant-acme` (JWS, directory, account/EAB, orders,
//! ARI). [`AcmeClientFactory`] exists because the CA may be unreachable at
//! startup: the manager connects lazily and retries.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, ExternalAccountKey,
    Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use tracing::{debug, info};

use super::storage::CertStorage;
use super::AcmeError;
use crate::config::AcmeConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChallenge {
    pub domain: String,
    /// `<token>.<account-key-thumbprint>`; the solver hashes it.
    pub key_auth: String,
}

#[async_trait]
pub trait AcmeOrder: Send {
    /// One entry per authorization still `pending` (already-valid ones are skipped).
    async fn pending_challenges(&mut self) -> Result<Vec<PendingChallenge>, AcmeError>;
    /// Tells the CA the TLS-ALPN-01 challenge for `domain` may be validated now.
    async fn mark_ready(&mut self, domain: &str) -> Result<(), AcmeError>;
    /// Polls until the order is `ready`; `Err` on `invalid` or timeout.
    async fn wait_ready(&mut self) -> Result<(), AcmeError>;
    /// Submits the CSR and downloads the PEM chain.
    async fn finalize(&mut self, csr_der: &[u8]) -> Result<String, AcmeError>;
}

#[async_trait]
pub trait AcmeClient: Send + Sync {
    async fn new_order(&self, domains: &[String]) -> Result<Box<dyn AcmeOrder>, AcmeError>;
    /// RFC 9773 renewal-info window for the certificate, when the CA offers it.
    async fn renewal_window(&self, leaf_der: &[u8]) -> Result<Option<(i64, i64)>, AcmeError>;
}

#[async_trait]
pub trait AcmeClientFactory: Send + Sync {
    async fn connect(&self) -> Result<Arc<dyn AcmeClient>, AcmeError>;
}

/// EAB HMAC keys are handed out base64url; some CAs paste standard base64.
pub fn decode_hmac_key(s: &str) -> Result<Vec<u8>, AcmeError> {
    let s = s.trim();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(s))
        .map_err(|e| AcmeError::Config(format!("acme.eab.hmac_key is not base64: {e}")))
}

fn proto(e: instant_acme::Error) -> AcmeError {
    AcmeError::Protocol(e.to_string())
}

pub struct InstantAcmeFactory {
    cfg: AcmeConfig,
    storage: Arc<dyn CertStorage>,
}

impl InstantAcmeFactory {
    pub fn new(cfg: AcmeConfig, storage: Arc<dyn CertStorage>) -> Self {
        Self { cfg, storage }
    }

    fn builder(&self) -> Result<instant_acme::AccountBuilder, AcmeError> {
        match &self.cfg.directory_ca_path {
            Some(path) => Account::builder_with_root(path).map_err(proto),
            None => Account::builder().map_err(proto),
        }
    }
}

#[async_trait]
impl AcmeClientFactory for InstantAcmeFactory {
    /// Loads the stored account or registers a new one (persisting its credentials).
    async fn connect(&self) -> Result<Arc<dyn AcmeClient>, AcmeError> {
        if let Some(bytes) = self.storage.load_account().await? {
            let creds: AccountCredentials = serde_json::from_slice(&bytes)
                .map_err(|e| AcmeError::Storage(format!("stored account credentials are corrupt: {e}")))?;
            let account = self.builder()?.from_credentials(creds).await.map_err(proto)?;
            debug!("acme: using stored account {}", account.id());
            return Ok(Arc::new(InstantAcmeClient { account }));
        }
        let contacts: Vec<&str> = self.cfg.contact.iter().map(String::as_str).collect();
        let eab = match &self.cfg.eab {
            Some(e) => Some(ExternalAccountKey::new(e.key_id.clone(), &decode_hmac_key(&e.hmac_key)?)),
            None => None,
        };
        let (account, creds) = self
            .builder()?
            .create(
                &NewAccount {
                    contact: &contacts,
                    terms_of_service_agreed: self.cfg.terms_of_service_agreed,
                    only_return_existing: false,
                },
                self.cfg.directory_url.clone(),
                eab.as_ref(),
            )
            .await
            .map_err(proto)?;
        let bytes = serde_json::to_vec(&creds)
            .map_err(|e| AcmeError::Storage(format!("serialize account credentials: {e}")))?;
        self.storage.save_account(&bytes).await?;
        info!("acme: registered account {} at {}", account.id(), self.cfg.directory_url);
        Ok(Arc::new(InstantAcmeClient { account }))
    }
}

pub struct InstantAcmeClient {
    account: Account,
}

#[async_trait]
impl AcmeClient for InstantAcmeClient {
    async fn new_order(&self, domains: &[String]) -> Result<Box<dyn AcmeOrder>, AcmeError> {
        let identifiers: Vec<Identifier> = domains.iter().cloned().map(Identifier::Dns).collect();
        let order = self
            .account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(proto)?;
        Ok(Box::new(InstantAcmeOrder { order }))
    }

    async fn renewal_window(&self, leaf_der: &[u8]) -> Result<Option<(i64, i64)>, AcmeError> {
        let der = rustls::pki_types::CertificateDer::from(leaf_der.to_vec());
        let id = match instant_acme::CertificateIdentifier::try_from(&der) {
            Ok(id) => id,
            Err(e) => {
                debug!("acme: no ARI identifier for certificate: {e}");
                return Ok(None);
            }
        };
        match self.account.renewal_info(&id).await {
            Ok((info, _retry_after)) => Ok(Some((
                info.suggested_window.start.unix_timestamp(),
                info.suggested_window.end.unix_timestamp(),
            ))),
            // ARI is advisory: a CA without it (or a transient error) just means
            // "use renew_before".
            Err(e) => {
                debug!("acme: renewal_info unavailable: {e}");
                Ok(None)
            }
        }
    }
}

struct InstantAcmeOrder {
    order: instant_acme::Order,
}

fn dns_name(ident: &Identifier) -> Result<String, AcmeError> {
    match ident {
        Identifier::Dns(d) => Ok(d.clone()),
        other => Err(AcmeError::Protocol(format!("unsupported identifier {other:?}"))),
    }
}

#[async_trait]
impl AcmeOrder for InstantAcmeOrder {
    async fn pending_challenges(&mut self) -> Result<Vec<PendingChallenge>, AcmeError> {
        let mut out = Vec::new();
        let mut authorizations = self.order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(proto)?;
            let domain = dns_name(authz.identifier().identifier)?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => {
                    return Err(AcmeError::Protocol(format!(
                        "authorization for {domain} is {other:?}"
                    )))
                }
            }
            let challenge = authz.challenge(ChallengeType::TlsAlpn01).ok_or_else(|| {
                AcmeError::Protocol(format!("CA offers no tls-alpn-01 challenge for {domain}"))
            })?;
            out.push(PendingChallenge {
                domain,
                key_auth: challenge.key_authorization().as_str().to_string(),
            });
        }
        Ok(out)
    }

    async fn mark_ready(&mut self, domain: &str) -> Result<(), AcmeError> {
        let mut authorizations = self.order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(proto)?;
            if dns_name(authz.identifier().identifier)? != domain {
                continue;
            }
            let mut challenge = authz.challenge(ChallengeType::TlsAlpn01).ok_or_else(|| {
                AcmeError::Protocol(format!("CA offers no tls-alpn-01 challenge for {domain}"))
            })?;
            return challenge.set_ready().await.map_err(proto);
        }
        Err(AcmeError::Protocol(format!("order has no authorization for {domain}")))
    }

    async fn wait_ready(&mut self) -> Result<(), AcmeError> {
        let status = self.order.poll_ready(&RetryPolicy::default()).await.map_err(proto)?;
        if matches!(status, OrderStatus::Ready | OrderStatus::Valid) {
            Ok(())
        } else {
            let detail = self
                .order
                .state()
                .error
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_default();
            Err(AcmeError::Protocol(format!("order is {status:?}: {detail}")))
        }
    }

    async fn finalize(&mut self, csr_der: &[u8]) -> Result<String, AcmeError> {
        self.order.finalize_csr(csr_der).await.map_err(proto)?;
        self.order
            .poll_certificate(&RetryPolicy::default())
            .await
            .map_err(proto)
    }
}

/// A scripted in-process CA for unit tests: hands out deterministic key
/// authorizations, requires every challenge to be marked ready, and signs the
/// CSR's public key with its own root so `load_certified_key` accepts the
/// chain. `MockBehavior` injects failures.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MockStep {
        NewOrder,
        WaitReady,
        Finalize,
    }

    #[derive(Debug, Clone)]
    pub struct MockBehavior {
        pub fail_step: Option<MockStep>,
        pub ari: Option<(i64, i64)>,
        /// Sign a random key instead of the CSR's (verification must reject it).
        pub wrong_key_chain: bool,
        pub validity_secs: i64,
    }

    impl Default for MockBehavior {
        fn default() -> Self {
            Self { fail_step: None, ari: None, wrong_key_chain: false, validity_secs: 90 * 86_400 }
        }
    }

    pub struct MockAcmeClient {
        ca_params: rcgen::CertificateParams,
        ca_key: rcgen::KeyPair,
        ca_pem: String,
        pub behavior: Mutex<MockBehavior>,
        orders: AtomicUsize,
    }

    impl MockAcmeClient {
        pub fn new(behavior: MockBehavior) -> Arc<Self> {
            let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            ca_params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "Mock ACME CA");
            let ca_key = rcgen::KeyPair::generate().unwrap();
            let ca_pem = ca_params.self_signed(&ca_key).unwrap().pem();
            Arc::new(Self { ca_params, ca_key, ca_pem, behavior: Mutex::new(behavior), orders: AtomicUsize::new(0) })
        }

        pub fn orders(&self) -> usize {
            self.orders.load(Ordering::SeqCst)
        }

        pub fn set_behavior(&self, b: MockBehavior) {
            *self.behavior.lock().unwrap() = b;
        }

        fn behavior(&self) -> MockBehavior {
            self.behavior.lock().unwrap().clone()
        }

        fn sign(&self, csr_der: &[u8], domains: &[String]) -> Result<String, AcmeError> {
            let b = self.behavior();
            let csr = rcgen::CertificateSigningRequestParams::from_der(
                &rustls::pki_types::CertificateSigningRequestDer::from(csr_der.to_vec()),
            )
            .map_err(|e| AcmeError::Protocol(format!("mock: bad CSR: {e}")))?;
            let mut params = rcgen::CertificateParams::new(domains.to_vec()).unwrap();
            let now = time::OffsetDateTime::now_utc();
            params.not_before = now - time::Duration::minutes(1);
            params.not_after = now + time::Duration::seconds(b.validity_secs);
            let issuer = rcgen::Issuer::from_params(&self.ca_params, &self.ca_key);
            let leaf = if b.wrong_key_chain {
                let other = rcgen::KeyPair::generate().unwrap();
                params.signed_by(&other, &issuer).unwrap()
            } else {
                // Same params (SANs, validity) but the CSR's public key.
                rcgen::CertificateSigningRequestParams { params, public_key: csr.public_key }
                    .signed_by(&issuer)
                    .unwrap()
            };
            Ok(format!("{}{}", leaf.pem(), self.ca_pem))
        }
    }

    pub struct MockOrder {
        client: Arc<MockAcmeClient>,
        domains: Vec<String>,
        ready: Vec<String>,
    }

    /// Implemented on `Arc<MockAcmeClient>` (not `MockAcmeClient`) so an order
    /// can call back into the CA; tests always hold the `Arc` `new()` returns.
    #[async_trait]
    impl AcmeClient for Arc<MockAcmeClient> {
        async fn new_order(&self, domains: &[String]) -> Result<Box<dyn AcmeOrder>, AcmeError> {
            self.orders.fetch_add(1, Ordering::SeqCst);
            if self.behavior().fail_step == Some(MockStep::NewOrder) {
                return Err(AcmeError::Protocol("mock: newOrder rejected (rateLimited)".into()));
            }
            Ok(Box::new(MockOrder { client: self.clone(), domains: domains.to_vec(), ready: Vec::new() }))
        }

        async fn renewal_window(&self, _leaf_der: &[u8]) -> Result<Option<(i64, i64)>, AcmeError> {
            Ok(self.behavior().ari)
        }
    }

    #[async_trait]
    impl AcmeOrder for MockOrder {
        async fn pending_challenges(&mut self) -> Result<Vec<PendingChallenge>, AcmeError> {
            Ok(self
                .domains
                .iter()
                .map(|d| PendingChallenge { domain: d.clone(), key_auth: format!("tok-{d}.mockthumb") })
                .collect())
        }

        async fn mark_ready(&mut self, domain: &str) -> Result<(), AcmeError> {
            if !self.domains.iter().any(|d| d == domain) {
                return Err(AcmeError::Protocol(format!("mock: no authorization for {domain}")));
            }
            self.ready.push(domain.to_string());
            Ok(())
        }

        async fn wait_ready(&mut self) -> Result<(), AcmeError> {
            if self.client.behavior().fail_step == Some(MockStep::WaitReady) {
                return Err(AcmeError::Protocol("mock: order is Invalid: challenge failed".into()));
            }
            if self.domains.iter().all(|d| self.ready.contains(d)) {
                Ok(())
            } else {
                Err(AcmeError::Protocol("mock: order is Invalid: authorization pending".into()))
            }
        }

        async fn finalize(&mut self, csr_der: &[u8]) -> Result<String, AcmeError> {
            if self.client.behavior().fail_step == Some(MockStep::Finalize) {
                return Err(AcmeError::Protocol("mock: finalize rejected (badCSR)".into()));
            }
            self.client.sign(csr_der, &self.domains)
        }
    }

    /// Factory returning the same mock every time (or failing `fail_connects` times first).
    pub struct MockFactory {
        pub client: Arc<MockAcmeClient>,
        pub fail_connects: AtomicUsize,
    }

    impl MockFactory {
        pub fn new(client: Arc<MockAcmeClient>) -> Arc<Self> {
            Arc::new(Self { client, fail_connects: AtomicUsize::new(0) })
        }
    }

    #[async_trait]
    impl AcmeClientFactory for MockFactory {
        async fn connect(&self) -> Result<Arc<dyn AcmeClient>, AcmeError> {
            if self.fail_connects.load(Ordering::SeqCst) > 0 {
                self.fail_connects.fetch_sub(1, Ordering::SeqCst);
                return Err(AcmeError::Protocol("mock: directory unreachable".into()));
            }
            Ok(Arc::new(self.client.clone()) as Arc<dyn AcmeClient>)
        }
    }
}
```

In the tests, `client.new_order(…)` resolves through the `Arc<MockAcmeClient>` impl because `client` is the `Arc` that `new()` returns. If `rcgen::Issuer::from_params(&params, &key)` does not accept a `&KeyPair` as `S: SigningKey`, store the CA as an `rcgen::Issuer<'static, rcgen::KeyPair>` built once with `Issuer::new(ca_params, ca_key)` in `new()` and sign with `&self.issuer`.

Add `pub mod client;` to `src/acme/mod.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test acme::client 2>&1 | tail -8`
Expected: 3 passed. (`OrderState.error` is `Option<Problem>`; if the field is private in 0.8.5, drop the `detail` and use `format!("order is {status:?}")`.)

- [ ] **Step 5: Commit**

```bash
git add src/acme
git commit -m "feat(acme): AcmeClient boundary over instant-acme with a scripted mock CA"
```

---

### Task 9: `order.rs` — key generation, CSR, chain verification, `issue()`

**Files:**
- Create: `src/acme/order.rs`
- Modify: `src/acme/mod.rs` (`pub mod order;`)

**Interfaces:**
- Consumes: `AcmeClient`/`AcmeOrder` (Task 8), `TlsAlpnSolver::{register, clear}` (Task 6), `load_certified_key`, `leaf_dns_sans`, `leaf_spki`, `parse_cert_meta`, `now_unix`, `StoredCert` (Task 3).
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum KeyType { EcdsaP256, EcdsaP384 }
  impl KeyType { pub fn parse(s: &str) -> Result<Self, AcmeError> }
  pub fn generate_key(kt: KeyType) -> Result<rcgen::KeyPair, AcmeError>
  pub fn build_csr(domains: &[String], key: &rcgen::KeyPair) -> Result<Vec<u8>, AcmeError>
  pub fn verify_chain(chain_pem: &str, key: &rcgen::KeyPair, domains: &[String], now: i64) -> Result<(), AcmeError>
  pub async fn issue(client: &dyn AcmeClient, solver: &TlsAlpnSolver, domains: &[String], key_type: KeyType) -> Result<StoredCert, AcmeError>
  ```

- [ ] **Step 1: Write the failing tests** (bottom of `src/acme/order.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::client::mock::{MockAcmeClient, MockBehavior, MockStep};
    use crate::acme::client::AcmeClient;
    use crate::acme::storage::fs::FsCertStorage;
    use std::sync::Arc;

    fn solver(tag: &str) -> Arc<TlsAlpnSolver> {
        let d = std::env::temp_dir().join(format!("fb_acme_order_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        TlsAlpnSolver::new(Arc::new(FsCertStorage::new(d)))
    }

    fn doms() -> Vec<String> {
        vec!["a.example.com".to_string(), "b.example.com".to_string()]
    }

    #[test]
    fn key_type_parse_and_generate() {
        assert_eq!(KeyType::parse("ecdsa-p256").unwrap(), KeyType::EcdsaP256);
        assert_eq!(KeyType::parse("ecdsa-p384").unwrap(), KeyType::EcdsaP384);
        assert!(KeyType::parse("rsa-2048").is_err());
        let k = generate_key(KeyType::EcdsaP384).unwrap();
        assert!(k.is_compatible(&rcgen::PKCS_ECDSA_P384_SHA384));
        let csr = build_csr(&doms(), &k).unwrap();
        assert!(!csr.is_empty());
    }

    #[tokio::test]
    async fn issue_happy_path_returns_verified_cert_and_clears_challenges() {
        let client = MockAcmeClient::new(MockBehavior::default());
        let solver = solver("happy");
        let stored = issue(&client, &solver, &doms(), KeyType::EcdsaP256).await.unwrap();
        let (_, leaf) = crate::acme::load_certified_key(&stored.chain_pem, &stored.key_pem).unwrap();
        let mut sans = crate::acme::leaf_dns_sans(&leaf).unwrap();
        sans.sort();
        assert_eq!(sans, doms());
        assert!(stored.issued_at > 0);
        assert!(solver.cached_domains().is_empty(), "challenges cleared after success");
        assert_eq!(client.orders(), 1);
    }

    #[tokio::test]
    async fn issue_rejects_chain_for_a_foreign_key_and_clears_challenges() {
        let client = MockAcmeClient::new(MockBehavior { wrong_key_chain: true, ..Default::default() });
        let solver = solver("wrongkey");
        let err = issue(&client, &solver, &doms(), KeyType::EcdsaP256).await.unwrap_err();
        assert!(matches!(err, AcmeError::Certificate(_)), "{err}");
        assert!(solver.cached_domains().is_empty());
    }

    #[tokio::test]
    async fn issue_surfaces_ca_failures_and_clears_challenges() {
        for step in [MockStep::NewOrder, MockStep::WaitReady, MockStep::Finalize] {
            let client = MockAcmeClient::new(MockBehavior { fail_step: Some(step), ..Default::default() });
            let solver = solver(&format!("fail{step:?}"));
            let err = issue(&client, &solver, &doms(), KeyType::EcdsaP256).await.unwrap_err();
            assert!(matches!(err, AcmeError::Protocol(_)), "{step:?}: {err}");
            assert!(solver.cached_domains().is_empty(), "{step:?}: challenges must be cleared");
        }
    }

    #[test]
    fn verify_chain_checks_sans_validity_and_key() {
        let key = generate_key(KeyType::EcdsaP256).unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["a.example.com".to_string()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(30);
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();
        let t = crate::acme::now_unix();
        verify_chain(&pem, &key, &["a.example.com".to_string()], t).unwrap();
        // Missing SAN.
        assert!(verify_chain(&pem, &key, &doms(), t).is_err());
        // Expired (evaluate "now" after not_after).
        assert!(verify_chain(&pem, &key, &["a.example.com".to_string()], t + 31 * 86_400).is_err());
        // Foreign key.
        let other = generate_key(KeyType::EcdsaP256).unwrap();
        assert!(verify_chain(&pem, &other, &["a.example.com".to_string()], t).is_err());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test acme::order 2>&1 | grep -E "^error" | head -3`
Expected: module not found.

- [ ] **Step 3: Implement `src/acme/order.rs`**

```rust
//! One certificate issuance, start to finish: fresh key → newOrder → register
//! every TLS-ALPN-01 key authorization with the solver → tell the CA "ready" →
//! wait → CSR → finalize → download → **verify** → hand back a [`StoredCert`].
//! Challenges are cleared on every exit path so a stuck order never leaves a
//! validatable challenge cert behind. Verification runs before anything is
//! persisted: a CA returning garbage never evicts a working certificate.

use rcgen::PublicKeyData;

use super::challenge::TlsAlpnSolver;
use super::client::{AcmeClient, PendingChallenge};
use super::{leaf_dns_sans, leaf_spki, load_certified_key, now_unix, parse_cert_meta, AcmeError, StoredCert};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    EcdsaP256,
    EcdsaP384,
}

impl KeyType {
    pub fn parse(s: &str) -> Result<Self, AcmeError> {
        match s {
            "ecdsa-p256" => Ok(KeyType::EcdsaP256),
            "ecdsa-p384" => Ok(KeyType::EcdsaP384),
            other => Err(AcmeError::Config(format!(
                "key_type '{other}' is not supported (ecdsa-p256 | ecdsa-p384)"
            ))),
        }
    }

    fn alg(self) -> &'static rcgen::SignatureAlgorithm {
        match self {
            KeyType::EcdsaP256 => &rcgen::PKCS_ECDSA_P256_SHA256,
            KeyType::EcdsaP384 => &rcgen::PKCS_ECDSA_P384_SHA384,
        }
    }
}

/// A fresh private key for one issuance (never reused across renewals).
pub fn generate_key(kt: KeyType) -> Result<rcgen::KeyPair, AcmeError> {
    rcgen::KeyPair::generate_for(kt.alg()).map_err(|e| AcmeError::Crypto(e.to_string()))
}

/// DER CSR with SAN = `domains`, CN = the first domain.
pub fn build_csr(domains: &[String], key: &rcgen::KeyPair) -> Result<Vec<u8>, AcmeError> {
    let mut params = rcgen::CertificateParams::new(domains.to_vec())
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    if let Some(first) = domains.first() {
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, first.as_str());
    }
    let csr = params
        .serialize_request(key)
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    Ok(csr.der().as_ref().to_vec())
}

/// Clock-skew tolerance for a just-issued `not_before`.
const NOT_BEFORE_SKEW_SECS: i64 = 300;

/// The chain parses, the leaf's public key is `key`, it is valid at `now`
/// (±skew), and its DNS SANs cover every domain.
pub fn verify_chain(chain_pem: &str, key: &rcgen::KeyPair, domains: &[String], now: i64) -> Result<(), AcmeError> {
    let (_, leaf) = load_certified_key(chain_pem, &key.serialize_pem())?;
    if leaf_spki(&leaf)? != key.subject_public_key_info() {
        return Err(AcmeError::Certificate("leaf public key does not match the generated key".into()));
    }
    let meta = parse_cert_meta(&leaf)?;
    if meta.not_after <= now {
        return Err(AcmeError::Certificate(format!(
            "issued certificate is already expired (not_after={})",
            meta.not_after
        )));
    }
    if meta.not_before > now + NOT_BEFORE_SKEW_SECS {
        return Err(AcmeError::Certificate(format!(
            "issued certificate is not yet valid (not_before={})",
            meta.not_before
        )));
    }
    let sans = leaf_dns_sans(&leaf)?;
    for d in domains {
        if !sans.iter().any(|s| s == &d.to_ascii_lowercase()) {
            return Err(AcmeError::Certificate(format!(
                "issued certificate lacks SAN {d} (has {sans:?})"
            )));
        }
    }
    Ok(())
}

/// Runs one order. On success the certificate is verified but **not** stored —
/// the caller persists and publishes it.
pub async fn issue(
    client: &dyn AcmeClient,
    solver: &TlsAlpnSolver,
    domains: &[String],
    key_type: KeyType,
) -> Result<StoredCert, AcmeError> {
    let mut registered: Vec<String> = Vec::new();
    let result = run(client, solver, domains, key_type, &mut registered).await;
    for domain in &registered {
        if let Err(e) = solver.clear(domain).await {
            tracing::warn!("acme: failed to clear challenge for {domain}: {e}");
        }
    }
    result
}

async fn run(
    client: &dyn AcmeClient,
    solver: &TlsAlpnSolver,
    domains: &[String],
    key_type: KeyType,
    registered: &mut Vec<String>,
) -> Result<StoredCert, AcmeError> {
    let key = generate_key(key_type)?;
    let mut order = client.new_order(domains).await?;
    let pending: Vec<PendingChallenge> = order.pending_challenges().await?;
    for p in &pending {
        solver.register(&p.domain, &p.key_auth).await?;
        registered.push(p.domain.clone());
    }
    for p in &pending {
        order.mark_ready(&p.domain).await?;
    }
    order.wait_ready().await?;
    let csr = build_csr(domains, &key)?;
    let chain_pem = order.finalize(&csr).await?;
    let now = now_unix();
    verify_chain(&chain_pem, &key, domains, now)?;
    Ok(StoredCert { chain_pem, key_pem: key.serialize_pem(), issued_at: now })
}
```

Add `pub mod order;` to `src/acme/mod.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test acme::order 2>&1 | tail -8`
Expected: 5 passed. If `subject_public_key_info()` is not on `PublicKeyData` in rcgen 0.14, use `key.public_key_der()`; if the SPKI encodings differ (e.g. x509-parser's `raw` excludes the outer SEQUENCE), compare `leaf_spki` against the SPKI extracted from `rcgen::Certificate` of a self-signed cert built from the same key in a unit test and pick the matching accessor.

- [ ] **Step 5: Commit**

```bash
git add src/acme
git commit -m "feat(acme): order state machine with pre-persist chain verification"
```

---

### Task 10: Prometheus metrics

**Files:**
- Create: `src/acme/metrics.rs`
- Modify: `src/acme/mod.rs` (`pub mod metrics;`)

**Interfaces:**
- Consumes: `prometheus::Registry` (`GatewayMetrics.registry`, `pub`).
- Produces:
  ```rust
  pub struct AcmeMetrics { pub not_after: IntGaugeVec, pub state: IntGaugeVec, pub renewals: IntCounterVec, pub last_attempt: IntGaugeVec }
  impl AcmeMetrics {
      pub fn register(registry: &prometheus::Registry) -> Arc<Self>;   // idempotent-safe: ignores AlreadyReg
      pub fn observe(&self, cert_id: &str, state: CertState, not_after: i64);
      pub fn attempt(&self, cert_id: &str, success: bool, at: i64);
  }
  ```

- [ ] **Step 1: Write the failing test** (bottom of `src/acme/metrics.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::{Encoder, TextEncoder};

    fn render(r: &prometheus::Registry) -> String {
        let mut buf = Vec::new();
        TextEncoder::new().encode(&r.gather(), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn series_render_with_expected_names_and_labels() {
        let registry = prometheus::Registry::new();
        let m = AcmeMetrics::register(&registry);
        m.observe("a.example.com", CertState::Issued, 1_800_000_000);
        m.attempt("a.example.com", true, 1_700_000_000);
        m.attempt("a.example.com", false, 1_700_000_100);
        let out = render(&registry);
        assert!(out.contains("featherbit_acme_cert_not_after_timestamp_seconds{cert_id=\"a.example.com\"} 1800000000"), "{out}");
        assert!(out.contains("featherbit_acme_cert_state{cert_id=\"a.example.com\",state=\"issued\"} 1"));
        assert!(out.contains("featherbit_acme_cert_state{cert_id=\"a.example.com\",state=\"placeholder\"} 0"));
        assert!(out.contains("featherbit_acme_renewals_total{cert_id=\"a.example.com\",result=\"success\"} 1"));
        assert!(out.contains("featherbit_acme_renewals_total{cert_id=\"a.example.com\",result=\"failure\"} 1"));
        assert!(out.contains("featherbit_acme_last_renewal_attempt_timestamp_seconds{cert_id=\"a.example.com\"} 1700000100"));
        // Placeholder reports 0 for not_after.
        m.observe("b.example.com", CertState::Placeholder, 0);
        assert!(render(&registry).contains("featherbit_acme_cert_not_after_timestamp_seconds{cert_id=\"b.example.com\"} 0"));
        // Registering twice on the same registry must not panic.
        let _ = AcmeMetrics::register(&registry);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test acme::metrics 2>&1 | grep -E "^error" | head -3`

- [ ] **Step 3: Implement `src/acme/metrics.rs`**

```rust
//! Prometheus series for managed certificates — enough for the standard
//! "expires in < 7 d" and "renewal failing" alerts.

use std::sync::Arc;

use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry};

use super::CertState;

pub struct AcmeMetrics {
    /// `featherbit_acme_cert_not_after_timestamp_seconds{cert_id}` (0 for placeholders).
    pub not_after: IntGaugeVec,
    /// `featherbit_acme_cert_state{cert_id,state}` — 1 for the current state, 0 otherwise.
    pub state: IntGaugeVec,
    /// `featherbit_acme_renewals_total{cert_id,result="success"|"failure"}`.
    pub renewals: IntCounterVec,
    /// `featherbit_acme_last_renewal_attempt_timestamp_seconds{cert_id}`.
    pub last_attempt: IntGaugeVec,
}

impl AcmeMetrics {
    /// Creates the collectors and registers them on `registry`. A collector
    /// already registered (same process, second call) is reused silently.
    pub fn register(registry: &Registry) -> Arc<Self> {
        let not_after = IntGaugeVec::new(
            Opts::new("featherbit_acme_cert_not_after_timestamp_seconds", "Expiry of the served managed certificate (unix seconds; 0 = placeholder)"),
            &["cert_id"],
        )
        .unwrap();
        let state = IntGaugeVec::new(
            Opts::new("featherbit_acme_cert_state", "Current state of a managed certificate (1 = current)"),
            &["cert_id", "state"],
        )
        .unwrap();
        let renewals = IntCounterVec::new(
            Opts::new("featherbit_acme_renewals_total", "ACME issuance/renewal attempts by outcome"),
            &["cert_id", "result"],
        )
        .unwrap();
        let last_attempt = IntGaugeVec::new(
            Opts::new("featherbit_acme_last_renewal_attempt_timestamp_seconds", "Unix time of the last issuance attempt"),
            &["cert_id"],
        )
        .unwrap();
        for c in [
            Box::new(not_after.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(state.clone()),
            Box::new(renewals.clone()),
            Box::new(last_attempt.clone()),
        ] {
            match registry.register(c) {
                Ok(()) | Err(prometheus::Error::AlreadyReg) => {}
                Err(e) => panic!("acme metrics registration: {e}"),
            }
        }
        Arc::new(Self { not_after, state, renewals, last_attempt })
    }

    pub fn observe(&self, cert_id: &str, state: CertState, not_after: i64) {
        self.not_after.with_label_values(&[cert_id]).set(not_after);
        for s in CertState::ALL {
            self.state
                .with_label_values(&[cert_id, s.as_str()])
                .set(i64::from(s == state));
        }
    }

    pub fn attempt(&self, cert_id: &str, success: bool, at: i64) {
        self.renewals
            .with_label_values(&[cert_id, if success { "success" } else { "failure" }])
            .inc();
        self.last_attempt.with_label_values(&[cert_id]).set(at);
    }
}
```

Add `pub mod metrics;` to `src/acme/mod.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test acme::metrics 2>&1 | tail -5`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add src/acme
git commit -m "feat(acme): prometheus series for certificate state and renewals"
```

---

### Task 11: `manager.rs` — renewal scheduler, lease, peers, force-renew

**Files:**
- Create: `src/acme/manager.rs`
- Modify: `src/acme/mod.rs` (`pub mod manager;`)

**Interfaces:**
- Consumes: `AcmeClientFactory`/`AcmeClient` (Task 8), `order::{issue, KeyType}` (Task 9), `TlsAlpnSolver` (Task 6), `CertStorage` (Task 4), `ManagedCerts`/`publish`/`update`/`load_certified_key`/`parse_cert_meta` (Task 3), `AcmeMetrics` (Task 10).
- Produces:
  ```rust
  pub const LEASE_TTL: Duration = 5 min;
  pub fn when_to_renew(not_after: i64, ari: Option<(i64, i64)>, renew_before_secs: i64) -> i64;
  pub fn backoff_secs(failures: u32) -> u64;                       // 60·2^(n−1), capped 3600; 0 for n = 0
  #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)] #[serde(rename_all="snake_case")]
  pub enum RenewOutcome { Scheduled, NotDue, InProgress, Unknown }
  #[derive(Debug, Clone)] pub struct ManagedSlot { pub id: CertId, pub domains: Vec<String> }
  #[derive(Debug, Clone)] pub struct ManagerConfig { pub key_type: KeyType, pub renew_before: Duration, pub poll_interval: Duration, pub lease_ttl: Duration, pub peer_poll: Duration, pub challenge_refresh: Duration }
  impl Default for ManagerConfig  // p256, 30 d, poll 60 s, lease 5 min, peer_poll 60 s, refresh 2 s
  pub struct Manager;
  impl Manager {
      pub fn new(cfg: ManagerConfig, factory: Arc<dyn AcmeClientFactory>, storage: Arc<dyn CertStorage>, solver: Arc<TlsAlpnSolver>, certs: ManagedCerts, slots: Vec<ManagedSlot>, metrics: Option<Arc<AcmeMetrics>>) -> Arc<Self>;
      pub fn slots(&self) -> &[ManagedSlot];
      pub fn renew_now(&self, id: &str, force: bool) -> RenewOutcome;
      pub fn in_flight(&self, id: &str) -> bool;
      pub async fn run(self: Arc<Self>);          // spawns one task per slot; returns immediately
  }
  ```

- [ ] **Step 1: Write the failing tests** (bottom of `src/acme/manager.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::client::mock::{MockAcmeClient, MockBehavior, MockFactory, MockStep};
    use crate::acme::storage::fs::FsCertStorage;
    use crate::acme::{new_managed_certs, placeholder_cert, publish, CertMeta, CertState, ManagedCert};

    #[test]
    fn when_to_renew_takes_the_earlier_of_renew_before_and_ari() {
        let not_after = 1_000_000;
        assert_eq!(when_to_renew(not_after, None, 100), 999_900);
        assert_eq!(when_to_renew(not_after, Some((999_000, 999_500)), 100), 999_000);
        assert_eq!(when_to_renew(not_after, Some((999_950, 999_990)), 100), 999_900);
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_an_hour() {
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(6), 1_920);
        assert_eq!(backoff_secs(7), 3_600);
        assert_eq!(backoff_secs(40), 3_600);
    }

    struct Harness {
        client: Arc<MockAcmeClient>,
        factory: Arc<MockFactory>,
        storage: Arc<dyn CertStorage>,
        certs: ManagedCerts,
        slot: ManagedSlot,
    }

    fn harness(tag: &str, behavior: MockBehavior) -> Harness {
        let d = std::env::temp_dir().join(format!("fb_acme_mgr_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let storage: Arc<dyn CertStorage> = Arc::new(FsCertStorage::new(d));
        let client = MockAcmeClient::new(behavior);
        let factory = MockFactory::new(client.clone());
        let (id, domains) = CertId::from_domains(&["m.example.com".into()]).unwrap();
        let certs = new_managed_certs();
        let (key, leaf) = placeholder_cert(&domains).unwrap();
        publish(&certs, &id, ManagedCert { key, leaf_der: leaf, state: CertState::Placeholder, meta: CertMeta::default(), domains: domains.clone() });
        Harness { client, factory, storage, certs, slot: ManagedSlot { id, domains } }
    }

    fn fast_cfg() -> ManagerConfig {
        ManagerConfig {
            poll_interval: Duration::from_millis(50),
            peer_poll: Duration::from_millis(50),
            challenge_refresh: Duration::from_millis(20),
            ..Default::default()
        }
    }

    fn manager(h: &Harness, cfg: ManagerConfig) -> Arc<Manager> {
        Manager::new(
            cfg,
            h.factory.clone(),
            h.storage.clone(),
            TlsAlpnSolver::new(h.storage.clone()),
            h.certs.clone(),
            vec![h.slot.clone()],
            None,
        )
    }

    async fn wait_state(certs: &ManagedCerts, id: &CertId, want: CertState) -> ManagedCert {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(c) = certs.load().get(id.as_str()) {
                if c.state == want {
                    return c.clone();
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {want:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn placeholder_is_issued_on_start_and_persisted() {
        let h = harness("issue", MockBehavior::default());
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        let issued = wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
        assert!(issued.meta.issuer.contains("Mock ACME CA"));
        assert!(issued.meta.next_renewal_at.is_some());
        assert!(h.storage.load_cert(&h.slot.id).await.unwrap().is_some());
        assert_eq!(h.client.orders(), 1);
        // Not due: renew_now without force is refused, with force re-issues.
        assert_eq!(m.renew_now(h.slot.id.as_str(), false), RenewOutcome::NotDue);
        assert_eq!(m.renew_now("nope", true), RenewOutcome::Unknown);
        assert_eq!(m.renew_now(h.slot.id.as_str(), true), RenewOutcome::Scheduled);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while h.client.orders() < 2 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let renewed = wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
        assert_ne!(renewed.meta.serial, issued.meta.serial);
    }

    #[tokio::test]
    async fn failure_keeps_placeholder_records_error_and_backs_off() {
        let h = harness("fail", MockBehavior { fail_step: Some(MockStep::Finalize), ..Default::default() });
        let before = h.certs.load().get(h.slot.id.as_str()).unwrap().leaf_der.clone();
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        let failed = wait_state(&h.certs, &h.slot.id, CertState::Failed).await;
        assert!(failed.meta.last_error.as_deref().unwrap_or("").contains("badCSR"));
        assert!(failed.meta.last_attempt_at.is_some());
        assert_eq!(failed.leaf_der, before, "placeholder keeps serving");
        let n = h.client.orders();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.client.orders(), n, "backoff (≥60 s) prevents a hot retry loop");
        // Let the CA recover and force a retry immediately.
        h.client.set_behavior(MockBehavior::default());
        assert_eq!(m.renew_now(h.slot.id.as_str(), false), RenewOutcome::Scheduled, "a placeholder is always due");
        wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
    }

    #[tokio::test]
    async fn unreachable_directory_at_start_is_retried() {
        let h = harness("connect", MockBehavior::default());
        h.factory.fail_connects.store(2, std::sync::atomic::Ordering::SeqCst);
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        // Two failed connects → Failed twice with backoff; force-renew skips the wait.
        wait_state(&h.certs, &h.slot.id, CertState::Failed).await;
        m.renew_now(h.slot.id.as_str(), true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        m.renew_now(h.slot.id.as_str(), true);
        wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
    }

    #[tokio::test]
    async fn peer_holding_the_lease_makes_us_adopt_its_certificate() {
        let h = harness("peer", MockBehavior::default());
        // "Peer" holds the lease and (later) writes the cert.
        assert!(h.storage.try_acquire_lease(&h.slot.id, "peer", Duration::from_secs(30)).await.unwrap());
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.client.orders(), 0, "must not order while a peer holds the lease");
        let solver = TlsAlpnSolver::new(h.storage.clone());
        let stored = crate::acme::order::issue(&h.client, &solver, &h.slot.domains, KeyType::EcdsaP256).await.unwrap();
        h.storage.save_cert(&h.slot.id, &stored).await.unwrap();
        let adopted = wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
        assert!(adopted.meta.issuer.contains("Mock ACME CA"));
        assert_eq!(h.client.orders(), 1, "only the peer's order happened");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test acme::manager 2>&1 | grep -E "^error" | head -3`

- [ ] **Step 3: Implement `src/acme/manager.rs`**

```rust
//! The renewal scheduler: one task per managed certificate that decides when
//! to (re)issue, takes the storage lease so only one instance orders, runs
//! `order::issue`, persists, publishes into [`ManagedCerts`], and backs off on
//! failure. Instances that lose the lease poll storage and adopt whatever the
//! leaseholder wrote, refreshing challenge certs meanwhile so the CA may
//! validate through any of them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};

use super::challenge::TlsAlpnSolver;
use super::client::{AcmeClient, AcmeClientFactory};
use super::metrics::AcmeMetrics;
use super::order::{issue, KeyType};
use super::storage::CertStorage;
use super::{load_certified_key, now_unix, parse_cert_meta, publish, update, AcmeError, CertId, CertState, ManagedCert, ManagedCerts};

pub const LEASE_TTL: Duration = Duration::from_secs(300);
/// ARI is re-queried at most this often per certificate.
const ARI_INTERVAL_SECS: i64 = 3_600;

/// Unix time at which the certificate should be renewed: `renew_before` ahead of
/// expiry, or the start of the CA's ARI window if that comes first.
pub fn when_to_renew(not_after: i64, ari: Option<(i64, i64)>, renew_before_secs: i64) -> i64 {
    let by_window = not_after - renew_before_secs;
    match ari {
        Some((start, _)) => by_window.min(start),
        None => by_window,
    }
}

/// `60·2^(failures−1)` seconds, capped at one hour; `0` when there are no failures.
pub fn backoff_secs(failures: u32) -> u64 {
    if failures == 0 {
        return 0;
    }
    60u64.saturating_mul(1u64 << (failures - 1).min(10)).min(3_600)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenewOutcome {
    Scheduled,
    NotDue,
    InProgress,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ManagedSlot {
    pub id: CertId,
    pub domains: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ManagerConfig {
    pub key_type: KeyType,
    pub renew_before: Duration,
    /// How often an idle slot re-evaluates (ARI changes, peers' writes).
    pub poll_interval: Duration,
    pub lease_ttl: Duration,
    /// How often a non-leaseholder checks storage for a peer's certificate.
    pub peer_poll: Duration,
    /// How often a non-leaseholder refreshes challenge certs from storage.
    pub challenge_refresh: Duration,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            key_type: KeyType::EcdsaP256,
            renew_before: Duration::from_secs(30 * 86_400),
            poll_interval: Duration::from_secs(60),
            lease_ttl: LEASE_TTL,
            peer_poll: Duration::from_secs(60),
            challenge_refresh: Duration::from_secs(2),
        }
    }
}

struct SlotControl {
    waker: Notify,
    force: AtomicBool,
    in_flight: AtomicBool,
}

pub struct Manager {
    cfg: ManagerConfig,
    factory: Arc<dyn AcmeClientFactory>,
    client: Mutex<Option<Arc<dyn AcmeClient>>>,
    storage: Arc<dyn CertStorage>,
    solver: Arc<TlsAlpnSolver>,
    certs: ManagedCerts,
    slots: Vec<ManagedSlot>,
    controls: HashMap<String, SlotControl>,
    metrics: Option<Arc<AcmeMetrics>>,
    owner: String,
}

impl Manager {
    pub fn new(
        cfg: ManagerConfig,
        factory: Arc<dyn AcmeClientFactory>,
        storage: Arc<dyn CertStorage>,
        solver: Arc<TlsAlpnSolver>,
        certs: ManagedCerts,
        slots: Vec<ManagedSlot>,
        metrics: Option<Arc<AcmeMetrics>>,
    ) -> Arc<Self> {
        let controls = slots
            .iter()
            .map(|s| {
                (
                    s.id.as_str().to_string(),
                    SlotControl { waker: Notify::new(), force: AtomicBool::new(false), in_flight: AtomicBool::new(false) },
                )
            })
            .collect();
        let owner = format!(
            "{}:{}:{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "gateway".into()),
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        Arc::new(Self { cfg, factory, client: Mutex::new(None), storage, solver, certs, slots, controls, metrics, owner })
    }

    pub fn slots(&self) -> &[ManagedSlot] {
        &self.slots
    }

    pub fn in_flight(&self, id: &str) -> bool {
        self.controls.get(id).is_some_and(|c| c.in_flight.load(Ordering::SeqCst))
    }

    /// Wakes the slot's task. Without `force`, a valid certificate outside its
    /// renewal window is left alone (`NotDue`) — Let's Encrypt's duplicate-cert
    /// limits are the classic footgun.
    pub fn renew_now(&self, id: &str, force: bool) -> RenewOutcome {
        let Some(ctl) = self.controls.get(id) else { return RenewOutcome::Unknown };
        if ctl.in_flight.load(Ordering::SeqCst) {
            return RenewOutcome::InProgress;
        }
        let due = self
            .certs
            .load()
            .get(id)
            .map(|c| c.state != CertState::Issued || c.meta.next_renewal_at.is_some_and(|t| t <= now_unix()))
            .unwrap_or(true);
        if !due && !force {
            return RenewOutcome::NotDue;
        }
        ctl.force.store(true, Ordering::SeqCst);
        ctl.waker.notify_one();
        RenewOutcome::Scheduled
    }

    /// Spawns the per-slot loops and returns.
    pub async fn run(self: Arc<Self>) {
        for slot in self.slots.clone() {
            let me = self.clone();
            tokio::spawn(async move { me.run_slot(slot).await });
        }
    }

    async fn client(&self) -> Result<Arc<dyn AcmeClient>, AcmeError> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let c = self.factory.connect().await?;
        *guard = Some(c.clone());
        Ok(c)
    }

    fn snapshot(&self, id: &CertId) -> Option<ManagedCert> {
        self.certs.load().get(id.as_str()).cloned()
    }

    fn observe(&self, id: &CertId) {
        if let (Some(m), Some(c)) = (&self.metrics, self.snapshot(id)) {
            let not_after = if c.state == CertState::Placeholder { 0 } else { c.meta.not_after };
            m.observe(id.as_str(), c.state, not_after);
        }
    }

    async fn run_slot(self: Arc<Self>, slot: ManagedSlot) {
        let ctl = &self.controls[slot.id.as_str()];
        let mut failures: u32 = 0;
        let mut ari: Option<(i64, i64)> = None;
        let mut ari_checked_at: i64 = 0;
        self.observe(&slot.id);
        loop {
            let Some(current) = self.snapshot(&slot.id) else { return };
            let forced = ctl.force.swap(false, Ordering::SeqCst);
            let now = now_unix();

            // Decide when this cert is due. Order matters: a forced renewal skips
            // the backoff; a failed attempt (placeholder or not) waits it out; a
            // placeholder with no failures yet is due immediately.
            let due_at = if forced {
                now
            } else if failures > 0 {
                current.meta.last_attempt_at.unwrap_or(now) + backoff_secs(failures) as i64
            } else if current.state == CertState::Placeholder {
                now
            } else {
                if now - ari_checked_at >= ARI_INTERVAL_SECS {
                    if let Ok(client) = self.client().await {
                        ari = client.renewal_window(&current.leaf_der).await.unwrap_or(None);
                    }
                    ari_checked_at = now;
                }
                let t = when_to_renew(current.meta.not_after, ari, self.cfg.renew_before.as_secs() as i64);
                update(&self.certs, &slot.id, |c| c.meta.next_renewal_at = Some(t));
                t
            };

            if now < due_at {
                let wait = Duration::from_secs((due_at - now) as u64).min(self.cfg.poll_interval);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = ctl.waker.notified() => {}
                }
                continue;
            }

            // Due: try to become the leaseholder.
            ctl.in_flight.store(true, Ordering::SeqCst);
            let outcome = self.attempt(&slot).await;
            ctl.in_flight.store(false, Ordering::SeqCst);
            match outcome {
                Ok(true) => {
                    failures = 0;
                    ari_checked_at = 0; // re-query ARI for the new cert
                }
                Ok(false) => {
                    // A peer holds the lease: adopt its result, keep challenges fresh.
                    self.follow_peer(&slot, ctl).await;
                }
                Err(e) => {
                    failures += 1;
                    error!("acme: issuance for {} failed (attempt {}): {}", slot.id, failures, e);
                    let now = now_unix();
                    // A placeholder that fails stays `Placeholder` (that is what
                    // /readyz keys on); a real cert that fails to renew is `Failed`
                    // but keeps serving.
                    update(&self.certs, &slot.id, |c| {
                        if c.state != CertState::Placeholder {
                            c.state = CertState::Failed;
                        }
                        c.meta.last_attempt_at = Some(now);
                        c.meta.last_error = Some(e.to_string());
                        c.meta.next_renewal_at = Some(now + backoff_secs(failures) as i64);
                    });
                    if let Some(m) = &self.metrics {
                        m.attempt(slot.id.as_str(), false, now);
                    }
                    if self.snapshot(&slot.id).is_some_and(|c| c.state == CertState::Placeholder) {
                        warn!("acme: {} is still serving a self-signed placeholder", slot.id);
                    }
                }
            }
            self.observe(&slot.id);
        }
    }

    /// One issuance under the lease. `Ok(false)` = a peer holds the lease.
    async fn attempt(&self, slot: &ManagedSlot) -> Result<bool, AcmeError> {
        if !self.storage.try_acquire_lease(&slot.id, &self.owner, self.cfg.lease_ttl).await? {
            return Ok(false);
        }
        update(&self.certs, &slot.id, |c| {
            if c.state == CertState::Issued || c.state == CertState::Failed {
                c.state = CertState::Renewing;
            }
        });
        self.observe(&slot.id);
        let result = async {
            let client = self.client().await?;
            // Keep the lease alive while the (possibly slow) order runs.
            let keepalive = {
                let storage = self.storage.clone();
                let id = slot.id.clone();
                let owner = self.owner.clone();
                let ttl = self.cfg.lease_ttl;
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(ttl / 4).await;
                        let _ = storage.renew_lease(&id, &owner, ttl).await;
                    }
                })
            };
            let issued = issue(client.as_ref(), &self.solver, &slot.domains, self.cfg.key_type).await;
            keepalive.abort();
            let stored = issued?;
            self.storage.save_cert(&slot.id, &stored).await?;
            let (key, leaf) = load_certified_key(&stored.chain_pem, &stored.key_pem)?;
            let meta = parse_cert_meta(&leaf)?;
            let now = now_unix();
            info!("acme: issued certificate for {} (serial {}, expires {})", slot.id, meta.serial, meta.not_after);
            publish(
                &self.certs,
                &slot.id,
                ManagedCert {
                    key,
                    leaf_der: leaf,
                    state: CertState::Issued,
                    meta: super::CertMeta { last_attempt_at: Some(now), ..meta },
                    domains: slot.domains.clone(),
                },
            );
            if let Some(m) = &self.metrics {
                m.attempt(slot.id.as_str(), true, now);
            }
            Ok::<(), AcmeError>(())
        }
        .await;
        if let Err(e) = self.storage.release_lease(&slot.id, &self.owner).await {
            warn!("acme: releasing lease for {} failed: {}", slot.id, e);
        }
        // On failure, restore the served state (the placeholder/old cert is untouched).
        if result.is_err() {
            update(&self.certs, &slot.id, |c| {
                if c.state == CertState::Renewing {
                    c.state = CertState::Failed;
                }
            });
        }
        result.map(|_| true)
    }

    /// Non-leaseholder path: refresh challenge certs from storage every
    /// `challenge_refresh` and look for the peer's certificate every `peer_poll`,
    /// for at most one lease TTL (then the outer loop re-evaluates).
    async fn follow_peer(&self, slot: &ManagedSlot, ctl: &SlotControl) {
        let deadline = tokio::time::Instant::now() + self.cfg.lease_ttl;
        let mut next_adopt = tokio::time::Instant::now();
        while tokio::time::Instant::now() < deadline {
            if let Err(e) = self.solver.refresh_from_storage(&slot.domains).await {
                warn!("acme: challenge refresh for {} failed: {}", slot.id, e);
            }
            if tokio::time::Instant::now() >= next_adopt {
                next_adopt = tokio::time::Instant::now() + self.cfg.peer_poll;
                match self.adopt_from_storage(slot).await {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(e) => warn!("acme: reading peer certificate for {} failed: {}", slot.id, e),
                }
            }
            if ctl.force.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(self.cfg.challenge_refresh).await;
        }
    }

    /// Publishes the stored certificate when it is newer than what we serve.
    async fn adopt_from_storage(&self, slot: &ManagedSlot) -> Result<bool, AcmeError> {
        let Some(stored) = self.storage.load_cert(&slot.id).await? else { return Ok(false) };
        let (key, leaf) = load_certified_key(&stored.chain_pem, &stored.key_pem)?;
        let meta = parse_cert_meta(&leaf)?;
        let current = self.snapshot(&slot.id);
        let newer = current
            .as_ref()
            .map(|c| c.state == CertState::Placeholder || meta.not_after > c.meta.not_after)
            .unwrap_or(true);
        if !newer || meta.not_after <= now_unix() {
            return Ok(false);
        }
        publish(
            &self.certs,
            &slot.id,
            ManagedCert { key, leaf_der: leaf, state: CertState::Issued, meta, domains: slot.domains.clone() },
        );
        info!("acme: adopted certificate for {} issued by a peer", slot.id);
        self.observe(&slot.id);
        Ok(true)
    }
}
```

In `attempt()`, the `Renewing → Failed` restore must also leave a `Placeholder` alone: `if c.state == CertState::Renewing { c.state = CertState::Failed; }` is correct because a placeholder is never moved to `Renewing` (see the `update` at the top of `attempt`).

Add `pub mod manager;` to `src/acme/mod.rs`.

- [ ] **Step 4: Run tests**

Run: `cargo test acme::manager 2>&1 | tail -10`
Expected: 6 passed (two pure, four async). The async tests take ~1–3 s each; if `unreachable_directory_at_start_is_retried` is flaky, increase the sleep between the two `renew_now` calls to 200 ms.

- [ ] **Step 5: Commit**

```bash
git add src/acme
git commit -m "feat(acme): lease-coordinated renewal manager with backoff and peer adoption"
```

---

### Task 12: Startup wiring (`AcmeRuntime`, `SharedState.acme`) and `/readyz`

**Files:**
- Modify: `src/acme/mod.rs` (`AcmeRuntime`, `build_storage`, `start`, test helper)
- Modify: `src/state.rs` (`acme` field), `src/server/listener.rs` (start + hooks), `src/admin/status.rs` (`readyz`)

**Interfaces:**
- Consumes: everything from Tasks 2–11; `StoreRegistry::client` (Task 5); `GatewayMetrics.registry`.
- Produces:
  ```rust
  pub struct AcmeRuntime { pub certs: ManagedCerts, pub solver: Arc<TlsAlpnSolver>, pub manager: Arc<Manager>, pub storage_label: String }
  impl AcmeRuntime {
      pub fn hooks(&self) -> crate::server::tls::AcmeHooks;
      pub fn placeholder_ids(&self) -> Vec<String>;          // sorted
  }
  pub fn build_storage(cfg: &AcmeConfig, stores: &StoreRegistry) -> Result<Arc<dyn CertStorage>, AcmeError>
  pub async fn start(cfg: &AcmeConfig, tls: &TlsConfig, stores: &StoreRegistry, metrics: &GatewayMetrics) -> Result<Arc<AcmeRuntime>, AcmeError>
  #[cfg(test)] pub(crate) mod testing { pub fn runtime_with(certs: ManagedCerts, slots: Vec<ManagedSlot>) -> Arc<AcmeRuntime>; pub fn placeholder_runtime(domains: &[&str]) -> Arc<AcmeRuntime>; pub fn issued_runtime(domains: &[&str]) -> Arc<AcmeRuntime> }
  SharedState { …, pub acme: arc_swap::ArcSwapOption<crate::acme::AcmeRuntime> }
  ```

- [ ] **Step 1: Write the failing tests**

In `src/acme/mod.rs` tests:

```rust
    #[tokio::test]
    async fn start_seeds_placeholders_then_reuses_a_stored_cert() {
        let dir = std::env::temp_dir().join(format!("fb_acme_start_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let system: crate::config::SystemConfig = serde_yaml::from_str(&format!(
            "acme:\n  terms_of_service_agreed: true\n  storage:\n    type: filesystem\n    dir: {}\ntls:\n  acme:\n    domains: [s.example.com]\n",
            dir.display().to_string().replace('\\', "/")
        ))
        .unwrap();
        let cfg = system.acme.as_ref().unwrap();
        let tls = system.tls.as_ref().unwrap();
        let stores = crate::stores::StoreRegistry::default();
        let metrics = crate::metrics::GatewayMetrics::new();

        let rt = start(cfg, tls, &stores, &metrics).await.unwrap();
        assert_eq!(rt.placeholder_ids(), vec!["s.example.com".to_string()]);
        assert_eq!(rt.storage_label, "filesystem");
        assert!(metrics.render().contains("featherbit_acme_cert_state{cert_id=\"s.example.com\",state=\"placeholder\"} 1"));

        // A valid stored cert is adopted at start (no placeholder, no order).
        let issued = rcgen::generate_simple_self_signed(vec!["s.example.com".to_string()]).unwrap();
        let storage = build_storage(cfg, &stores).unwrap();
        let (id, _) = CertId::from_domains(&["s.example.com".into()]).unwrap();
        storage
            .save_cert(&id, &StoredCert { chain_pem: issued.cert.pem(), key_pem: issued.signing_key.serialize_pem(), issued_at: now_unix() })
            .await
            .unwrap();
        let rt2 = start(cfg, tls, &stores, &crate::metrics::GatewayMetrics::new()).await.unwrap();
        assert!(rt2.placeholder_ids().is_empty());
        assert_eq!(rt2.certs.load().get("s.example.com").unwrap().state, CertState::Issued);
        let _ = std::fs::remove_dir_all(dir);
    }
```

In `src/admin/status.rs` tests (create the module if there is none; reuse the `test_state` pattern from `src/admin/stores.rs`):

```rust
#[cfg(test)]
mod acme_readyz_tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state() -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(
            "routes:\n  - name: r\n    match:\n      path: /x\n    policy: p\npolicies:\n  - name: p\n    nodes:\n      - id: in\n        type: listener\n      - id: out\n        type: client\n    edges:\n      - { from: in.out, to: out.in }\n",
        )
        .unwrap();
        Arc::new(SharedState::new(system, gateway, None, Arc::new(FileConfigStore::new("g.yaml".into()))).unwrap())
    }

    async fn readyz_status(state: Arc<SharedState>) -> (StatusCode, serde_json::Value) {
        let resp = router()
            .with_state(state)
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn readyz_is_503_while_a_managed_cert_is_a_placeholder() {
        let s = state();
        let (status, _) = readyz_status(s.clone()).await;
        assert_eq!(status, StatusCode::OK, "no acme ⇒ ready");

        s.acme.store(Some(crate::acme::testing::placeholder_runtime(&["p.example.com"])));
        let (status, body) = readyz_status(s.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["acme"]["placeholder"][0], "p.example.com");

        s.acme.store(Some(crate::acme::testing::issued_runtime(&["p.example.com"])));
        let (status, body) = readyz_status(s).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["acme"]["placeholder"].as_array().unwrap().len(), 0);
    }
}
```

If the policy YAML above does not compile in `SharedState::new` (route match key or node type names), copy the minimal route/policy from an existing admin test (e.g. `src/admin/routes.rs`) — the readiness check only needs `routes` non-empty.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test start_seeds readyz_is_503 2>&1 | grep -E "^error" | head -5`

- [ ] **Step 3: Implement the runtime in `src/acme/mod.rs`**

```rust
use tracing::{info, warn};

use crate::config::{AcmeConfig, AcmeStorageConfig, TlsConfig};
use crate::metrics::GatewayMetrics;
use crate::stores::StoreRegistry;
use challenge::TlsAlpnSolver;
use manager::{ManagedSlot, Manager, ManagerConfig};
use storage::CertStorage;

/// Everything the rest of the process needs from ACME once started.
pub struct AcmeRuntime {
    pub certs: ManagedCerts,
    pub solver: Arc<TlsAlpnSolver>,
    pub manager: Arc<Manager>,
    pub storage_label: String,
}

impl AcmeRuntime {
    pub fn hooks(&self) -> crate::server::tls::AcmeHooks {
        crate::server::tls::AcmeHooks { certs: self.certs.clone(), solver: self.solver.clone() }
    }

    /// Ids of managed certs still serving a placeholder (drives `/readyz`).
    pub fn placeholder_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .certs
            .load()
            .iter()
            .filter(|(_, c)| c.state == CertState::Placeholder)
            .map(|(id, _)| id.clone())
            .collect();
        v.sort();
        v
    }
}

pub fn build_storage(cfg: &AcmeConfig, stores: &StoreRegistry) -> Result<Arc<dyn CertStorage>, AcmeError> {
    match &cfg.storage {
        AcmeStorageConfig::Filesystem { dir } => Ok(Arc::new(storage::fs::FsCertStorage::new(dir))),
        #[cfg(feature = "redis-store")]
        AcmeStorageConfig::Store { store, encryption_key } => {
            let client = stores.client(store).map_err(AcmeError::Config)?;
            Ok(Arc::new(storage::redis::RedisCertStorage::new(client, encryption_key)))
        }
        #[cfg(not(feature = "redis-store"))]
        AcmeStorageConfig::Store { .. } => {
            let _ = stores;
            Err(AcmeError::Config("acme.storage.type: store needs the redis-store feature".into()))
        }
    }
}

/// Builds storage, seeds [`ManagedCerts`] from storage (or placeholders), and
/// spawns the renewal manager. The CA is **not** contacted here — the manager
/// connects lazily — so a CA outage never blocks the listener from starting.
pub async fn start(
    cfg: &AcmeConfig,
    tls: &TlsConfig,
    stores: &StoreRegistry,
    metrics: &GatewayMetrics,
) -> Result<Arc<AcmeRuntime>, AcmeError> {
    let storage = build_storage(cfg, stores)?;
    let solver = TlsAlpnSolver::new(storage.clone());
    let certs = new_managed_certs();
    let acme_metrics = metrics::AcmeMetrics::register(&metrics.registry);
    let now = now_unix();

    let mut slots = Vec::new();
    for domains in tls.managed_domains() {
        let (id, domains) = CertId::from_domains(&domains)?;
        if slots.iter().any(|s: &ManagedSlot| s.id == id) {
            continue; // two slots with the same domain set share one certificate
        }
        let adopted = match storage.load_cert(&id).await {
            Ok(Some(stored)) => match load_certified_key(&stored.chain_pem, &stored.key_pem)
                .and_then(|(key, leaf)| parse_cert_meta(&leaf).map(|meta| (key, leaf, meta)))
            {
                Ok((key, leaf, meta)) if meta.not_after > now => {
                    info!("acme: loaded stored certificate for {} (expires {})", id, meta.not_after);
                    Some(ManagedCert { key, leaf_der: leaf, state: CertState::Issued, meta, domains: domains.clone() })
                }
                Ok(_) => {
                    warn!("acme: stored certificate for {} is expired; serving a placeholder", id);
                    None
                }
                Err(e) => {
                    warn!("acme: stored certificate for {} is unusable ({}); serving a placeholder", id, e);
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                warn!("acme: reading stored certificate for {} failed ({}); serving a placeholder", id, e);
                None
            }
        };
        let cert = match adopted {
            Some(c) => c,
            None => {
                let (key, leaf) = placeholder_cert(&domains)?;
                warn!("acme: {} has no certificate yet — serving a self-signed placeholder until issuance succeeds", id);
                ManagedCert { key, leaf_der: leaf, state: CertState::Placeholder, meta: CertMeta::default(), domains: domains.clone() }
            }
        };
        publish(&certs, &id, cert);
        slots.push(ManagedSlot { id, domains });
    }

    let manager_cfg = ManagerConfig {
        key_type: order::KeyType::parse(&cfg.key_type)?,
        renew_before: cfg.renew_before_duration().map_err(AcmeError::Config)?,
        ..ManagerConfig::default()
    };
    let factory = Arc::new(client::InstantAcmeFactory::new(cfg.clone(), storage.clone()));
    let manager = Manager::new(manager_cfg, factory, storage.clone(), solver.clone(), certs.clone(), slots, Some(acme_metrics));
    tokio::spawn(manager.clone().run());

    Ok(Arc::new(AcmeRuntime { certs, solver, manager, storage_label: storage.label() }))
}

/// Runtimes for admin/readiness unit tests: a manager that never runs, over a
/// mock CA and a temp filesystem store.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use client::mock::{MockAcmeClient, MockBehavior, MockFactory};

    pub fn runtime_with(certs: ManagedCerts, slots: Vec<ManagedSlot>) -> Arc<AcmeRuntime> {
        let dir = std::env::temp_dir().join(format!("fb_acme_rt_{}_{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        let storage: Arc<dyn CertStorage> = Arc::new(storage::fs::FsCertStorage::new(dir));
        let solver = TlsAlpnSolver::new(storage.clone());
        let factory = MockFactory::new(MockAcmeClient::new(MockBehavior::default()));
        let manager = Manager::new(ManagerConfig::default(), factory, storage, solver.clone(), certs.clone(), slots, None);
        Arc::new(AcmeRuntime { certs, solver, manager, storage_label: "filesystem".into() })
    }

    fn seeded(domains: &[&str], state: CertState) -> Arc<AcmeRuntime> {
        let domains: Vec<String> = domains.iter().map(|d| d.to_string()).collect();
        let (id, domains) = CertId::from_domains(&domains).unwrap();
        let certs = new_managed_certs();
        let (key, leaf) = if state == CertState::Placeholder {
            placeholder_cert(&domains).unwrap()
        } else {
            let c = rcgen::generate_simple_self_signed(domains.clone()).unwrap();
            load_certified_key(&c.cert.pem(), &c.signing_key.serialize_pem()).unwrap()
        };
        let meta = if state == CertState::Placeholder { CertMeta::default() } else { parse_cert_meta(&leaf).unwrap() };
        publish(&certs, &id, ManagedCert { key, leaf_der: leaf, state, meta, domains: domains.clone() });
        runtime_with(certs, vec![ManagedSlot { id, domains }])
    }

    pub fn placeholder_runtime(domains: &[&str]) -> Arc<AcmeRuntime> {
        seeded(domains, CertState::Placeholder)
    }

    pub fn issued_runtime(domains: &[&str]) -> Arc<AcmeRuntime> {
        seeded(domains, CertState::Issued)
    }
}
```

- [ ] **Step 4: `SharedState`, listener, readyz**

`src/state.rs`: add `pub acme: arc_swap::ArcSwapOption<crate::acme::AcmeRuntime>,` to the struct and `acme: arc_swap::ArcSwapOption::empty(),` in `new`.

`src/server/listener.rs` `start_server`, replace the TLS block:

```rust
    let tls_config: Option<tls::SharedTlsConfig> = match &system.tls {
        Some(tls_cfg) => {
            // ACME: seed managed certs (stored or placeholder) before the config
            // is built, so the listener is up — and validatable — immediately.
            let hooks = match &system.acme {
                Some(acme_cfg) if !tls_cfg.managed_domains().is_empty() => {
                    let stores = state.resources.stores.load();
                    let rt = crate::acme::start(acme_cfg, tls_cfg, &stores, &state.metrics).await?;
                    state.acme.store(Some(rt.clone()));
                    Some(rt.hooks())
                }
                _ => None,
            };
            let shared = tls::build_reloadable(tls_cfg, http2_enabled, hooks.as_ref())?;
            tls::spawn_cert_watcher(tls_cfg.clone(), http2_enabled, shared.clone(), "data-plane", hooks);
            Some(shared)
        }
        None => None,
    };
```

`src/admin/status.rs` `readyz`:

```rust
async fn readyz(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let routes = state.routes.read().await;
    if routes.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status": "not_ready", "reason": "no routes loaded"})),
        );
    }
    let placeholders = state
        .acme
        .load()
        .as_ref()
        .map(|rt| rt.placeholder_ids())
        .unwrap_or_default();
    if !placeholders.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "reason": "acme placeholder certs",
                "acme": {"placeholder": placeholders},
            })),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ready",
            "routes": routes.len(),
            "acme": {"placeholder": placeholders},
        })),
    )
}
```

Update the doc comment above it: ready = routes loaded **and** no managed certificate is still a placeholder; renewal failures never affect readiness.

- [ ] **Step 5: Run tests**

Run: `cargo test 2>&1 | tail -4 && cargo check --no-default-features && cargo clippy --all-targets --locked -- -D warnings`
Expected: green; both new tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/acme/mod.rs src/state.rs src/server/listener.rs src/admin/status.rs
git commit -m "feat(acme): runtime startup wiring, placeholder bootstrap and readiness gate"
```

---

### Task 13: Admin API — `GET /api/acme/certs`, `POST /api/acme/certs/{id}/renew`, store referrer

**Files:**
- Create: `src/admin/acme.rs`
- Modify: `src/admin/mod.rs` (`mod acme;`, `.merge(acme::router())`), `src/admin/stores.rs` (delete guard)

**Interfaces:**
- Consumes: `SharedState.acme` (Task 12), `Manager::{renew_now, in_flight, slots}` (Task 11), `RenewOutcome`.
- Produces: `pub fn router() -> Router<Arc<SharedState>>`; JSON shapes:
  - `GET /api/acme/certs` → `200 {"enabled": bool, "storage": "filesystem"|"store:<name>" (only when enabled), "certs": [{ "id", "domains", "state", "not_before", "not_after", "issuer", "serial", "next_renewal_at", "last_attempt_at", "last_error" }]}`
  - `POST /api/acme/certs/{id}/renew[?force=true]` → `202 {"scheduled": true}` | `200 {"scheduled": false, "reason": "not_due"}` | `409 {"error": "in_progress"}` | `404 {"error": "not_found"}` | `501 {"error": "acme is not configured"}`

- [ ] **Step 1: Write the failing tests** (bottom of the new `src/admin/acme.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn state() -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        Arc::new(SharedState::new(system, gateway, None, Arc::new(FileConfigStore::new("g.yaml".into()))).unwrap())
    }

    async fn call(state: Arc<SharedState>, method: Method, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = router()
            .with_state(state)
            .oneshot(Request::builder().method(method).uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn list_reports_disabled_when_acme_is_absent() {
        let (status, body) = call(state(), Method::GET, "/api/acme/certs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false);
        assert_eq!(body["certs"].as_array().unwrap().len(), 0);
        let (status, _) = call(state(), Method::POST, "/api/acme/certs/x/renew").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn list_shows_managed_certs_without_key_material() {
        let s = state();
        s.acme.store(Some(crate::acme::testing::issued_runtime(&["B.example.com", "a.example.com"])));
        let (status, body) = call(s, Method::GET, "/api/acme/certs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], true);
        assert_eq!(body["storage"], "filesystem");
        let cert = &body["certs"][0];
        assert_eq!(cert["id"], "a.example.com,b.example.com");
        assert_eq!(cert["domains"], serde_json::json!(["a.example.com", "b.example.com"]));
        assert_eq!(cert["state"], "issued");
        assert!(cert["not_after"].as_i64().unwrap() > 0);
        assert!(cert.get("key_pem").is_none() && cert.get("chain_pem").is_none());
        assert!(!body.to_string().contains("PRIVATE KEY"));
    }

    #[tokio::test]
    async fn renew_semantics() {
        let s = state();
        s.acme.store(Some(crate::acme::testing::issued_runtime(&["a.example.com"])));
        let (status, _) = call(s.clone(), Method::POST, "/api/acme/certs/nope/renew").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Freshly issued, long-lived: not due.
        let (status, body) = call(s.clone(), Method::POST, "/api/acme/certs/a.example.com/renew").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["scheduled"], false);
        assert_eq!(body["reason"], "not_due");
        let (status, body) = call(s.clone(), Method::POST, "/api/acme/certs/a.example.com/renew?force=true").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["scheduled"], true);
        // A placeholder is always due.
        let p = state();
        p.acme.store(Some(crate::acme::testing::placeholder_runtime(&["p.example.com"])));
        let (status, _) = call(p, Method::POST, "/api/acme/certs/p.example.com/renew").await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
}
```

And in `src/admin/stores.rs` tests, next to `test_delete_referenced_store_is_409_with_referrers`:

```rust
    #[tokio::test]
    async fn test_delete_store_used_by_acme_storage_is_409() {
        let system: SystemConfig = serde_yaml::from_str(
            "acme:\n  terms_of_service_agreed: true\n  storage:\n    type: store\n    store: s1\n    encryption_key: k\n",
        )
        .unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(
            "stores:\n  - name: s1\n    type: redis\n    url: redis://127.0.0.1:6379\n",
        )
        .unwrap();
        let state = Arc::new(
            SharedState::new(system, gateway, None, Arc::new(FileConfigStore::new("gateway.yaml".into()))).unwrap(),
        );
        let req = Request::builder().method("DELETE").uri("/api/stores/s1").body(Body::empty()).unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(body["error"], "in_use");
        assert!(body["referrers"].as_array().unwrap().iter().any(|r| r.as_str().unwrap().contains("acme.storage")));
    }
```

(This test is `#[cfg(feature = "redis-store")]` — the headless build cannot construct a `Store` storage config that passes `SharedState::new`; `SharedState::new` does not call `validate()`, but keep the gate so the test never depends on that.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test admin::acme test_delete_store_used_by_acme 2>&1 | grep -E "^error" | head -5`

- [ ] **Step 3: Implement `src/admin/acme.rs`**

```rust
//! Admin surface for ACME-managed certificates: read-only status plus a
//! "renew now" nudge. Configuration stays in `system.yaml` (restart-gated like
//! every TLS setting), so there is deliberately no CRUD here. Responses never
//! include key material — only what `CertMeta` records.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::acme::manager::RenewOutcome;
use crate::state::SharedState;

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/acme/certs", get(list_certs))
        .route("/api/acme/certs/{id}/renew", post(renew_cert))
}

/// `GET /api/acme/certs` — every managed certificate with state and metadata.
/// `{"enabled": false, "certs": []}` when `acme:` is not configured, so the UI
/// can tell "not configured" from "nothing managed" without a status-code guess.
async fn list_certs(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let Some(rt) = state.acme.load_full() else {
        return Json(serde_json::json!({"enabled": false, "certs": []}));
    };
    let map = rt.certs.load();
    let mut certs: Vec<serde_json::Value> = rt
        .manager
        .slots()
        .iter()
        .filter_map(|slot| map.get(slot.id.as_str()).map(|c| (slot, c)))
        .map(|(slot, c)| {
            serde_json::json!({
                "id": slot.id.as_str(),
                "domains": c.domains,
                "state": if rt.manager.in_flight(slot.id.as_str()) && c.state != crate::acme::CertState::Placeholder {
                    crate::acme::CertState::Renewing
                } else {
                    c.state
                },
                "not_before": c.meta.not_before,
                "not_after": c.meta.not_after,
                "issuer": c.meta.issuer,
                "serial": c.meta.serial,
                "next_renewal_at": c.meta.next_renewal_at,
                "last_attempt_at": c.meta.last_attempt_at,
                "last_error": c.meta.last_error,
            })
        })
        .collect();
    certs.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(serde_json::json!({
        "enabled": true,
        "storage": rt.storage_label,
        "certs": certs,
    }))
}

/// `POST /api/acme/certs/{id}/renew[?force=true]` — nudges the renewal manager.
async fn renew_cert(
    State(state): State<Arc<SharedState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(rt) = state.acme.load_full() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error": "acme is not configured"})),
        );
    };
    let force = params.get("force").is_some_and(|v| v == "true" || v == "1");
    match rt.manager.renew_now(&id, force) {
        RenewOutcome::Scheduled => (StatusCode::ACCEPTED, Json(serde_json::json!({"scheduled": true}))),
        RenewOutcome::NotDue => (
            StatusCode::OK,
            Json(serde_json::json!({"scheduled": false, "reason": "not_due"})),
        ),
        RenewOutcome::InProgress => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "in_progress"})),
        ),
        RenewOutcome::Unknown => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        ),
    }
}
```

`src/admin/mod.rs`: add `mod acme;` (first in the list) and `.merge(acme::router())` after `.merge(routes::router())`.

`src/admin/stores.rs`, in the delete handler where `store_referrers(&gw, &name)` is computed, extend:

```rust
        let mut referrers = store_referrers(&gw, &name);
        if let Some(crate::config::AcmeConfig {
            storage: crate::config::AcmeStorageConfig::Store { store, .. },
            ..
        }) = &state.system.acme
        {
            if store == &name {
                referrers.push("acme.storage (system.yaml)".to_string());
            }
        }
```

Update the module doc at the top of `stores.rs` to mention the ACME referrer.

- [ ] **Step 4: Run tests**

Run: `cargo test admin:: 2>&1 | tail -4`
Expected: all admin tests pass including the 3 new ACME ones and the store-referrer one.

- [ ] **Step 5: Commit**

```bash
git add src/admin/acme.rs src/admin/mod.rs src/admin/stores.rs
git commit -m "feat(admin): ACME certificate status and renew endpoints"
```

---

### Task 14: UI — Certificates panel

**Files:**
- Modify: `ui/src/types/index.ts`, `ui/src/api/client.ts`, `ui/src/components/Sidebar.tsx`, `ui/src/App.tsx`
- Create: `ui/src/certs.ts`, `ui/src/certs.test.ts`, `ui/src/components/CertificatesPanel.tsx`

**Interfaces:**
- Consumes: the JSON shapes from Task 13; `Dialog`/`DialogButton` (`ui/src/components/Dialog.tsx`), `formatUnixTime` (`ui/src/format.ts`), `parseApiError` (`ui/src/apiError.ts`).
- Produces:
  ```ts
  export type AcmeCertState = 'placeholder' | 'issued' | 'renewing' | 'failed';
  export interface AcmeCert { id: string; domains: string[]; state: AcmeCertState; not_before: number; not_after: number; issuer: string; serial: string; next_renewal_at: number | null; last_attempt_at: number | null; last_error: string | null; }
  export interface AcmeCertsResponse { enabled: boolean; storage?: string; certs: AcmeCert[]; }
  export interface AcmeRenewResponse { scheduled: boolean; reason?: string; }
  api.listAcmeCerts(): Promise<AcmeCertsResponse>; api.renewAcmeCert(id: string, force: boolean): Promise<AcmeRenewResponse>
  export type ExpiryTone = 'none' | 'ok' | 'warn' | 'danger';
  export function expiryTone(notAfter: number, nowSecs: number): ExpiryTone;   // none for 0; danger < 7 d or expired; warn < 30 d
  export function formatExpiresIn(notAfter: number, nowSecs: number): string;  // '—', 'expired', 'in 3d', 'in 5h', 'in 12m'
  <CertificatesPanel open onClose onError />;  Sidebar prop onOpenCertificates: () => void
  ```

- [ ] **Step 1: Write the failing vitest** — `ui/src/certs.test.ts`

```ts
import { describe, expect, it } from 'vitest';
import { expiryTone, formatExpiresIn } from './certs';

const NOW = 1_800_000_000;
const DAY = 86_400;

describe('expiryTone', () => {
  it('is none for an unset expiry (placeholder)', () => {
    expect(expiryTone(0, NOW)).toBe('none');
  });
  it('is ok beyond 30 days, warn under 30, danger under 7 or expired', () => {
    expect(expiryTone(NOW + 60 * DAY, NOW)).toBe('ok');
    expect(expiryTone(NOW + 29 * DAY, NOW)).toBe('warn');
    expect(expiryTone(NOW + 6 * DAY, NOW)).toBe('danger');
    expect(expiryTone(NOW - 1, NOW)).toBe('danger');
  });
});

describe('formatExpiresIn', () => {
  it('renders the coarsest sensible unit', () => {
    expect(formatExpiresIn(0, NOW)).toBe('—');
    expect(formatExpiresIn(NOW - 5, NOW)).toBe('expired');
    expect(formatExpiresIn(NOW + 45 * DAY + 3600, NOW)).toBe('in 45d');
    expect(formatExpiresIn(NOW + 5 * 3600 + 90, NOW)).toBe('in 5h');
    expect(formatExpiresIn(NOW + 12 * 60 + 5, NOW)).toBe('in 12m');
    expect(formatExpiresIn(NOW + 40, NOW)).toBe('in <1m');
  });
});
```

Run: `cd ui && npx vitest run certs` — expected: FAIL (module missing).

- [ ] **Step 2: Implement `ui/src/certs.ts`**

```ts
/**
 * Pure helpers for the Certificates panel (kept out of the component file so
 * react-refresh keeps working and vitest can import them without a DOM).
 *
 * @module certs
 */

export type ExpiryTone = 'none' | 'ok' | 'warn' | 'danger';

const DAY = 86_400;

/** Colour band for a certificate expiry: amber under 30 days, red under 7 (or expired). */
export function expiryTone(notAfter: number, nowSecs: number): ExpiryTone {
  if (!notAfter) return 'none';
  const left = notAfter - nowSecs;
  if (left < 7 * DAY) return 'danger';
  if (left < 30 * DAY) return 'warn';
  return 'ok';
}

/** `in 45d` / `in 5h` / `in 12m` / `in <1m` / `expired` / `—` (unset). */
export function formatExpiresIn(notAfter: number, nowSecs: number): string {
  if (!notAfter) return '—';
  const left = notAfter - nowSecs;
  if (left <= 0) return 'expired';
  if (left >= DAY) return `in ${Math.floor(left / DAY)}d`;
  if (left >= 3600) return `in ${Math.floor(left / 3600)}h`;
  if (left >= 60) return `in ${Math.floor(left / 60)}m`;
  return 'in <1m';
}
```

Run: `cd ui && npx vitest run certs` — expected: PASS.

- [ ] **Step 3: Types and API client**

Append to `ui/src/types/index.ts`:

```ts
/** Lifecycle state of an ACME-managed certificate. @remarks Mirrors src/acme/mod.rs::CertState. */
export type AcmeCertState = 'placeholder' | 'issued' | 'renewing' | 'failed';

/** One managed certificate as served by `GET /api/acme/certs` (never includes key material). */
export interface AcmeCert {
  /** Normalized, comma-joined domain set; the renew endpoint's path parameter. */
  id: string;
  domains: string[];
  state: AcmeCertState;
  /** Unix seconds; 0 while a placeholder is served. */
  not_before: number;
  not_after: number;
  issuer: string;
  serial: string;
  next_renewal_at: number | null;
  last_attempt_at: number | null;
  last_error: string | null;
}

/** Envelope of `GET /api/acme/certs`. `enabled: false` = no `acme:` block in system.yaml. */
export interface AcmeCertsResponse {
  enabled: boolean;
  /** `filesystem` or `store:<name>`; absent when disabled. */
  storage?: string;
  certs: AcmeCert[];
}

/** Body of `POST /api/acme/certs/{id}/renew` (200 not_due or 202 scheduled). */
export interface AcmeRenewResponse {
  scheduled: boolean;
  reason?: string;
}
```

Add to the `api` object in `ui/src/api/client.ts` (after the Sessions block) and extend the type import:

```ts
  // ACME certificates
  /** `GET /api/acme/certs` — managed certificate states; `enabled: false` when acme is not configured. */
  listAcmeCerts: () => request<AcmeCertsResponse>('/api/acme/certs'),
  /** `POST /api/acme/certs/{id}/renew[?force=true]` — 202 scheduled, 200 not_due, 409 in_progress, 404 unknown. */
  renewAcmeCert: (id: string, force: boolean) =>
    request<AcmeRenewResponse>(
      `/api/acme/certs/${encodeURIComponent(id)}/renew${force ? '?force=true' : ''}`,
      { method: 'POST' },
    ),
```

- [ ] **Step 4: The panel — `ui/src/components/CertificatesPanel.tsx`**

```tsx
/**
 * Certificates panel: read-only status of ACME-managed certificates with a
 * per-row "Renew now". Mirrors SessionsPanel's dialog layout; unlike it, this
 * one polls (every 30 s while open) because issuance state changes on its own.
 *
 * @module components/CertificatesPanel
 */
import { useCallback, useEffect, useState } from 'react';
import type { CSSProperties } from 'react';
import { Dialog, DialogButton } from './Dialog';
import { formatUnixTime } from '../format';
import { expiryTone, formatExpiresIn } from '../certs';
import { api } from '../api/client';
import { parseApiError } from '../apiError';
import type { AcmeCert, AcmeCertsResponse } from '../types';

interface CertificatesPanelProps {
  open: boolean;
  onClose: () => void;
  onError: (title: string, message: string) => void;
}

const POLL_MS = 30_000;

const badgeColor: Record<AcmeCert['state'], string> = {
  placeholder: 'var(--text-muted)',
  issued: 'var(--success)',
  renewing: 'var(--accent)',
  failed: 'var(--error)',
};

const toneColor = {
  none: 'var(--text-muted)',
  ok: 'var(--text-primary)',
  warn: 'var(--warning)',
  danger: 'var(--error)',
} as const;

const cell: CSSProperties = {
  padding: '6px 8px',
  fontSize: 'var(--text-xs)',
  verticalAlign: 'top',
  borderBottom: '1px solid var(--border)',
};

function NotConfigured() {
  return (
    <p data-testid="acme-not-configured" style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
      ACME is not configured — add an <code>acme:</code> block to <code>system.yaml</code> and mark a
      certificate slot <code>acme: {'{ domains: [...] }'}</code>, then restart.
    </p>
  );
}

export function CertificatesPanel({ open, onClose, onError }: CertificatesPanelProps) {
  const [data, setData] = useState<AcmeCertsResponse | null>(null);
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  // Per-row two-step confirm: 'arm' after the first click, 'force' when the API said not_due.
  const [pending, setPending] = useState<{ id: string; stage: 'arm' | 'force' } | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await api.listAcmeCerts());
      setNow(Math.floor(Date.now() / 1000));
    } catch (e) {
      onError('Certificates', parseApiError(e));
    }
  }, [onError]);

  useEffect(() => {
    if (!open) return;
    void load();
    const t = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(t);
  }, [open, load]);

  const renew = async (id: string, force: boolean) => {
    try {
      const res = await api.renewAcmeCert(id, force);
      if (!res.scheduled && res.reason === 'not_due') {
        setPending({ id, stage: 'force' });
        return;
      }
      setPending(null);
      await load();
    } catch (e) {
      setPending(null);
      onError('Renew certificate', parseApiError(e));
    }
  };

  return (
    <Dialog
      open={open}
      title="Certificates"
      width={760}
      onClose={onClose}
      footer={
        <>
          <DialogButton variant="ghost" onClick={() => void load()}>Refresh</DialogButton>
          <DialogButton onClick={onClose}>Close</DialogButton>
        </>
      }
    >
      {data === null ? (
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)' }}>Loading…</p>
      ) : !data.enabled ? (
        <NotConfigured />
      ) : (
        <>
          <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', margin: '0 0 8px' }}>
            Storage: <code>{data.storage}</code> · {data.certs.length} managed certificate
            {data.certs.length === 1 ? '' : 's'}
          </p>
          <div style={{ overflowX: 'auto' }}>
            <table style={{ width: '100%', borderCollapse: 'collapse' }}>
              <thead>
                <tr style={{ color: 'var(--text-secondary)', textAlign: 'left' }}>
                  <th style={cell}>Domains</th>
                  <th style={cell}>State</th>
                  <th style={cell}>Expires</th>
                  <th style={cell}>Issuer</th>
                  <th style={cell}>Next renewal</th>
                  <th style={cell}></th>
                </tr>
              </thead>
              <tbody>
                {data.certs.map((c) => {
                  const tone = expiryTone(c.not_after, now);
                  const isPending = pending?.id === c.id;
                  return (
                    <tr key={c.id} data-testid="acme-cert-row" data-cert-id={c.id} data-state={c.state}>
                      <td style={{ ...cell, fontFamily: 'var(--font-mono)' }}>{c.domains.join(', ')}</td>
                      <td style={cell}>
                        <span
                          data-testid="acme-cert-state"
                          style={{
                            padding: '1px 6px',
                            borderRadius: 'var(--radius-sm)',
                            border: `1px solid ${badgeColor[c.state]}`,
                            color: badgeColor[c.state],
                            textTransform: 'capitalize',
                          }}
                        >
                          {c.state}
                        </span>
                        {c.last_error && (
                          <button
                            onClick={() => setExpanded(expanded === c.id ? null : c.id)}
                            style={{ marginLeft: 6, fontSize: 'var(--text-xs)', color: 'var(--error)', background: 'none', border: 0, cursor: 'pointer' }}
                          >
                            {expanded === c.id ? 'hide error' : 'last error'}
                          </button>
                        )}
                        {expanded === c.id && c.last_error && (
                          <pre
                            data-testid="acme-cert-error"
                            style={{ margin: '6px 0 0', whiteSpace: 'pre-wrap', fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}
                          >
                            {c.last_error}
                          </pre>
                        )}
                      </td>
                      <td style={{ ...cell, color: toneColor[tone] }} title={c.not_after ? formatUnixTime(c.not_after) : ''}>
                        {formatExpiresIn(c.not_after, now)}
                      </td>
                      <td style={cell}>{c.issuer || '—'}</td>
                      <td style={cell}>{c.next_renewal_at ? formatUnixTime(c.next_renewal_at) : '—'}</td>
                      <td style={{ ...cell, whiteSpace: 'nowrap' }}>
                        {!isPending && (
                          <DialogButton variant="ghost" onClick={() => setPending({ id: c.id, stage: 'arm' })}>
                            Renew now
                          </DialogButton>
                        )}
                        {isPending && pending.stage === 'arm' && (
                          <>
                            <DialogButton variant="primary" onClick={() => void renew(c.id, false)}>Confirm</DialogButton>
                            <DialogButton variant="ghost" onClick={() => setPending(null)}>Cancel</DialogButton>
                          </>
                        )}
                        {isPending && pending.stage === 'force' && (
                          <>
                            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', marginRight: 6 }}>
                              Not due yet.
                            </span>
                            <DialogButton variant="danger" onClick={() => void renew(c.id, true)}>Force renew</DialogButton>
                            <DialogButton variant="ghost" onClick={() => setPending(null)}>Cancel</DialogButton>
                          </>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </>
      )}
    </Dialog>
  );
}
```

If `--success` / `--warning` CSS variables do not exist in `ui/src/index.css`, add them next to `--error` in both theme blocks (green/amber values consistent with the palette), or reuse `--accent` for success.

- [ ] **Step 5: Sidebar button and App wiring**

`ui/src/components/Sidebar.tsx`: add prop `onOpenCertificates: () => void` (doc: "Called when "Certificates" is clicked; the parent opens the certificates panel.") to `SidebarProps` and destructuring; render a button **before** the Sessions button in the footer, same styling, icon `ShieldCheck` from `lucide-react`, `aria-label="Certificates"`, `title="ACME-managed TLS certificates"`, text `Certificates`. It is always enabled (the panel itself explains when ACME is not configured).

`ui/src/App.tsx`: `const [certsOpen, setCertsOpen] = useState(false);` next to `sessionsOpen`; pass `onOpenCertificates={() => setCertsOpen(true)}` to `<Sidebar>`; render next to `<SessionsPanel>`:

```tsx
      <CertificatesPanel open={certsOpen} onClose={() => setCertsOpen(false)} onError={handlePanelError} />
```

with `import { CertificatesPanel } from './components/CertificatesPanel';`.

- [ ] **Step 6: Lint, test, build**

Run: `cd ui && npm run lint && npm test && npm run build`
Expected: clean; `ui/dist` rebuilt (the Rust binary embeds it).

- [ ] **Step 7: Commit**

```bash
git add ui/src
git commit -m "feat(ui): certificates panel for ACME-managed certs"
```

---

### Task 15: Pebble integration test, local Pebble, CI job

**Files:**
- Create: `src/acme/live_tests.rs`, `dev/pebble/pebble-config.json`, `dev/pebble/docker-compose.yml`
- Modify: `src/acme/mod.rs` (`#[cfg(test)] mod live_tests;`), `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `server::start_server`, `SharedState`, `AcmeRuntime` (Task 12), env vars `FEATHERBIT_TEST_PEBBLE_URL`, `FEATHERBIT_TEST_PEBBLE_CA`, `FEATHERBIT_TEST_ACME_DOMAIN` (default `localhost`), `FEATHERBIT_TEST_ACME_PORT` (default `18443`).

- [ ] **Step 1: Pebble config and compose**

`dev/pebble/pebble-config.json` — Pebble's stock test config with the validator's TLS-ALPN port pointed at the gateway (`18443`), and a short authz/order retry so tests are quick:

```json
{
  "pebble": {
    "listenAddress": "0.0.0.0:14000",
    "managementListenAddress": "0.0.0.0:15000",
    "certificate": "test/certs/localhost/cert.pem",
    "privateKey": "test/certs/localhost/key.pem",
    "httpPort": 18080,
    "tlsPort": 18443,
    "ocspResponderURL": "",
    "externalAccountBindingRequired": false,
    "domainBlocklist": ["blocked-domain.example"],
    "retryAfter": { "authz": 1, "order": 1 },
    "keyAlgorithm": "ecdsa",
    "profiles": {
      "default": { "description": "90-day test certificates", "validityPeriod": 7776000 }
    }
  }
}
```

`dev/pebble/docker-compose.yml`:

```yaml
# Pebble — Let's Encrypt's ACME test CA — for exercising the gateway's ACME
# support locally. Host networking so Pebble's validator can reach a gateway
# listening on 127.0.0.1:18443 (tlsPort in pebble-config.json).
#
#   docker compose -f dev/pebble/docker-compose.yml up -d
#   curl -sk https://localhost:14000/dir | head          # directory
#   docker compose -f dev/pebble/docker-compose.yml cp pebble:/test/certs/pebble.minica.pem /tmp/pebble.minica.pem
#   FEATHERBIT_TEST_PEBBLE_URL=https://localhost:14000/dir \
#   FEATHERBIT_TEST_PEBBLE_CA=/tmp/pebble.minica.pem cargo test acme::live_tests -- --nocapture
services:
  pebble:
    image: ghcr.io/letsencrypt/pebble:latest
    network_mode: host
    command: ["-config", "/test/config/pebble-config.json", "-strict"]
    environment:
      # Validate immediately instead of sleeping a random 0-15 s per challenge.
      PEBBLE_VA_NOSLEEP: "1"
    volumes:
      - ./pebble-config.json:/test/config/pebble-config.json:ro
```

- [ ] **Step 2: Write the live test** — `src/acme/live_tests.rs`

```rust
//! End-to-end issuance against Pebble (Let's Encrypt's test CA). Gated on
//! `FEATHERBIT_TEST_PEBBLE_URL`; see `dev/pebble/` for running it locally and
//! the `acme-live` CI job. Pebble's validator connects to
//! `<domain>:<FEATHERBIT_TEST_ACME_PORT>` (its `tlsPort`), so the data-plane
//! listener binds that exact port.

use std::sync::Arc;
use std::time::Duration;

use crate::acme::CertState;
use crate::config::{GatewayConfig, SystemConfig};
use crate::config_store::FileConfigStore;
use crate::state::SharedState;

struct Env {
    dir_url: String,
    ca_path: String,
    domain: String,
    port: u16,
}

fn env() -> Option<Env> {
    let Ok(dir_url) = std::env::var("FEATHERBIT_TEST_PEBBLE_URL") else {
        eprintln!("skipping acme live test: FEATHERBIT_TEST_PEBBLE_URL not set");
        return None;
    };
    Some(Env {
        dir_url,
        ca_path: std::env::var("FEATHERBIT_TEST_PEBBLE_CA").expect("FEATHERBIT_TEST_PEBBLE_CA (path to pebble.minica.pem)"),
        domain: std::env::var("FEATHERBIT_TEST_ACME_DOMAIN").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FEATHERBIT_TEST_ACME_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(18443),
    })
}

fn system_yaml(e: &Env, storage_dir: &std::path::Path) -> String {
    format!(
        r#"
listener: {{ bind: "127.0.0.1", port: {port} }}
tls:
  acme: {{ domains: ["{domain}"] }}
acme:
  directory_url: "{dir}"
  directory_ca_path: "{ca}"
  terms_of_service_agreed: true
  contact: ["mailto:e2e@example.com"]
  renew_before: 30d
  storage: {{ type: filesystem, dir: "{sdir}" }}
"#,
        port = e.port,
        domain = e.domain,
        dir = e.dir_url,
        ca = e.ca_path.replace('\\', "/"),
        sdir = storage_dir.display().to_string().replace('\\', "/"),
    )
}

async fn wait_for(certs: &crate::acme::ManagedCerts, id: &str, pred: impl Fn(&crate::acme::ManagedCert) -> bool, what: &str) -> crate::acme::ManagedCert {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(c) = certs.load().get(id) {
            if pred(c) {
                return c.clone();
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn pebble_issues_renews_and_restart_reuses_the_stored_cert() {
    let Some(e) = env() else { return };
    let storage_dir = std::env::temp_dir().join(format!("fb_acme_pebble_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&storage_dir);

    let system: SystemConfig = serde_yaml::from_str(&system_yaml(&e, &storage_dir)).unwrap();
    system.validate().unwrap();
    let gateway: GatewayConfig = serde_yaml::from_str("{}").unwrap();
    let state = Arc::new(SharedState::new(system.clone(), gateway, None, Arc::new(FileConfigStore::new("unused.yaml".into()))).unwrap());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = {
        let state = state.clone();
        let system = system.clone();
        tokio::spawn(async move { crate::server::start_server(&system, state, shutdown_rx).await.unwrap() })
    };

    // The runtime appears once the listener has bound; it starts as a placeholder.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let rt = loop {
        if let Some(rt) = state.acme.load_full() {
            break rt;
        }
        assert!(tokio::time::Instant::now() < deadline, "acme runtime never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let id = e.domain.to_ascii_lowercase();
    assert_eq!(rt.placeholder_ids(), vec![id.clone()]);

    // Issuance completes; the served cert is Pebble's.
    let issued = wait_for(&rt.certs, &id, |c| c.state == CertState::Issued, "first issuance").await;
    assert!(issued.meta.issuer.to_lowercase().contains("pebble"), "{}", issued.meta.issuer);
    assert!(rt.placeholder_ids().is_empty());

    // Not due ⇒ refused; force ⇒ a new serial.
    assert_eq!(rt.manager.renew_now(&id, false), crate::acme::manager::RenewOutcome::NotDue);
    assert_eq!(rt.manager.renew_now(&id, true), crate::acme::manager::RenewOutcome::Scheduled);
    let renewed = wait_for(&rt.certs, &id, |c| c.state == CertState::Issued && c.meta.serial != issued.meta.serial, "forced renewal").await;
    assert_ne!(renewed.meta.serial, issued.meta.serial);

    // A restart (new runtime over the same storage) adopts the stored cert: no
    // placeholder, no new order.
    let stores = crate::stores::StoreRegistry::default();
    let rt2 = crate::acme::start(system.acme.as_ref().unwrap(), system.tls.as_ref().unwrap(), &stores, &crate::metrics::GatewayMetrics::new()).await.unwrap();
    let adopted = rt2.certs.load().get(&id).cloned().unwrap();
    assert_eq!(adopted.state, CertState::Issued);
    assert_eq!(adopted.meta.serial, renewed.meta.serial);

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    let _ = std::fs::remove_dir_all(&storage_dir);
}
```

Add to `src/acme/mod.rs`: `#[cfg(test)] mod live_tests;`.

- [ ] **Step 3: Run locally (skips without Pebble; runs with it)**

Run: `cargo test acme::live_tests -- --nocapture 2>&1 | tail -3` — expected: the skip line, 1 passed.
With Pebble up per the compose header (Linux/macOS, or Docker Desktop on Windows — `network_mode: host` needs Docker ≥ 4.29 on Desktop): expected: 1 passed within ~30 s. If Pebble cannot reach the listener, check `docker logs pebble` for the dial error; the domain must resolve to the host from inside the container (`localhost` does under host networking).

- [ ] **Step 4: CI job** — add to `.github/workflows/ci.yml` after `redis-live`:

```yaml
  # Real ACME issuance against Pebble (Let's Encrypt's test CA). Host networking
  # lets Pebble's validator reach the test's listener on 127.0.0.1:18443; the
  # test self-skips everywhere FEATHERBIT_TEST_PEBBLE_URL is unset.
  acme-live:
    name: acme live tests (pebble)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - name: Start Pebble
        run: |
          curl -fsSL -o /tmp/pebble.minica.pem \
            https://raw.githubusercontent.com/letsencrypt/pebble/main/test/certs/pebble.minica.pem
          docker run -d --name pebble --network host \
            -e PEBBLE_VA_NOSLEEP=1 \
            -v "$PWD/dev/pebble/pebble-config.json:/test/config/pebble-config.json:ro" \
            ghcr.io/letsencrypt/pebble:latest -config /test/config/pebble-config.json -strict
          for i in $(seq 1 30); do
            curl -sk https://localhost:14000/dir >/dev/null && break
            sleep 1
          done
          curl -sk https://localhost:14000/dir

      - run: cargo test --locked --no-default-features --features redis-store acme:: -- --nocapture
        env:
          FEATHERBIT_TEST_PEBBLE_URL: https://localhost:14000/dir
          FEATHERBIT_TEST_PEBBLE_CA: /tmp/pebble.minica.pem
          FEATHERBIT_TEST_ACME_DOMAIN: localhost
          FEATHERBIT_TEST_ACME_PORT: "18443"

      - name: Pebble logs
        if: failure()
        run: docker logs pebble
```

Also add the same "Start Pebble" step and the four env vars to the existing `e2e` job (before "Run e2e"; env on the "Run e2e" step) — Task 16's gated scenario uses them.

- [ ] **Step 5: Commit**

```bash
git add src/acme dev/pebble .github/workflows/ci.yml
git commit -m "test(acme): Pebble-backed issuance test with local compose and CI job"
```

---

### Task 16: Playwright e2e — `E2E-ACME-*`

**Files:**
- Create: `e2e/tests/acme.spec.ts`, `e2e/fixtures/acme/system.yaml`, `e2e/fixtures/acme/gateway.yaml`
- Modify: `e2e/E2E_TESTBOOK.md`, `e2e/playwright.config.ts` (export `GATEWAY_BIN` is already exported — verify; export `repo` too)

**Interfaces:**
- Consumes: `adminApi()` (`e2e/helpers/admin.ts`), `ADMIN_URL`, `GATEWAY_BIN` from `playwright.config.ts`; the UI test ids `acme-not-configured`, `acme-cert-row`, `acme-cert-state` (Task 14); the Pebble env vars (Task 15).

- [ ] **Step 1: Fixtures for the second gateway** (the suite's main gateway is plaintext on 18081 and must stay so)

`e2e/fixtures/acme/system.yaml`:

```yaml
# Second gateway for the Pebble-gated ACME scenario: HTTPS on the port Pebble's
# validator dials (tlsPort in dev/pebble/pebble-config.json), admin on 19092.
listener:
  bind: "127.0.0.1"
  port: ${FEATHERBIT_TEST_ACME_PORT:-18443}

tls:
  acme:
    domains: ["${FEATHERBIT_TEST_ACME_DOMAIN:-localhost}"]

acme:
  directory_url: ${FEATHERBIT_TEST_PEBBLE_URL}
  directory_ca_path: ${FEATHERBIT_TEST_PEBBLE_CA}
  terms_of_service_agreed: true
  contact: ["mailto:e2e@example.com"]
  storage:
    type: filesystem
    dir: ${FEATHERBIT_TEST_ACME_DIR:-e2e/.tmp/acme}

logging:
  level: ${LOG_LEVEL:-warn}
  format: text

admin:
  bind: "127.0.0.1"
  port: 19092
  username: admin
  password: admin
```

`e2e/fixtures/acme/gateway.yaml` — one echo route so `/readyz` has routes:

```yaml
routes:
  - name: echo
    match:
      path: /echo
    policy: echo
policies:
  - name: echo
    nodes:
      - id: in
        type: listener
      - id: up
        type: upstream
        config:
          host: ${ECHO_HOST:-127.0.0.1}
          port: ${ECHO_PORT:-3010}
      - id: out
        type: client
    edges:
      - { from: in.out, to: up.in }
      - { from: up.out, to: out.in }
```

(Copy the `upstream` node's exact config keys from `e2e/fixtures/gateway.yaml`'s echo policy if they differ.)

- [ ] **Step 2: The spec** — `e2e/tests/acme.spec.ts`

```ts
/**
 * ACME scenarios. See E2E_TESTBOOK.md ("ACME certificates").
 *
 * E2E-ACME-01 is unconditional: the main fixture gateway has no `acme:` block,
 * so it proves the "not configured" surface. E2E-ACME-02 is gated on
 * FEATHERBIT_TEST_PEBBLE_URL: it spawns a SECOND gateway (HTTPS on the port
 * Pebble validates against) from e2e/fixtures/acme/ and drives its UI.
 */
import {spawn, type ChildProcess} from 'node:child_process';
import {mkdirSync, rmSync} from 'node:fs';
import {resolve} from 'node:path';
import {test, expect, request, type APIRequestContext} from '@playwright/test';

import {ADMIN_PASS, ADMIN_USER, GATEWAY_BIN} from '../playwright.config';
import {adminApi} from '../helpers/admin';

const ACME_ADMIN_URL = 'http://127.0.0.1:19092';
const repo = resolve(__dirname, '..', '..');

test.describe('ACME certificates', () => {
  test('E2E-ACME-01: without an acme: block the API and the panel say "not configured"', async ({page}) => {
    const api = await adminApi();
    const res = await api.get('/api/acme/certs');
    expect(res.status()).toBe(200);
    expect(await res.json()).toEqual({enabled: false, certs: []});
    const renew = await api.post('/api/acme/certs/x/renew');
    expect(renew.status()).toBe(501);
    await api.dispose();

    await page.goto('/');
    await page.getByRole('button', {name: 'Certificates'}).click();
    await expect(page.getByTestId('acme-not-configured')).toBeVisible();
  });

  test('E2E-ACME-02: a Pebble-backed gateway issues, shows and force-renews its certificate', async ({page}) => {
    test.skip(!process.env.FEATHERBIT_TEST_PEBBLE_URL, 'FEATHERBIT_TEST_PEBBLE_URL not set');
    test.setTimeout(180_000);

    const acmeDir = resolve(repo, 'e2e', '.tmp', 'acme');
    rmSync(acmeDir, {recursive: true, force: true});
    mkdirSync(acmeDir, {recursive: true});

    const child: ChildProcess = spawn(
      GATEWAY_BIN,
      ['--system-config', 'e2e/fixtures/acme/system.yaml', '--gateway-config', 'e2e/fixtures/acme/gateway.yaml'],
      {
        cwd: repo,
        env: {...process.env, ECHO_HOST: '127.0.0.1', ECHO_PORT: '3010', LOG_LEVEL: 'warn', FEATHERBIT_TEST_ACME_DIR: acmeDir},
        stdio: ['ignore', 'ignore', 'pipe'],
      },
    );
    let stderr = '';
    child.stderr?.on('data', (d) => (stderr += d.toString()));

    let api: APIRequestContext | undefined;
    try {
      api = await request.newContext({
        baseURL: ACME_ADMIN_URL,
        httpCredentials: {username: ADMIN_USER, password: ADMIN_PASS},
      });
      await waitFor(async () => (await api!.get('/healthz')).ok(), 15_000, `gateway did not start: ${stderr}`);

      // Placeholder first: /readyz is 503 naming the cert; then issuance lands.
      const first = (await (await api.get('/api/acme/certs')).json()) as {certs: {id: string; state: string}[]};
      expect(first.certs).toHaveLength(1);
      const id = first.certs[0].id;
      const issued = await waitFor(
        async () => {
          const body = (await (await api!.get('/api/acme/certs')).json()) as {certs: {state: string; serial: string; issuer: string}[]};
          return body.certs[0].state === 'issued' ? body.certs[0] : null;
        },
        90_000,
        `never issued: ${stderr}`,
      );
      expect(issued.issuer.toLowerCase()).toContain('pebble');
      expect((await api.get('/readyz')).status()).toBe(200);

      // UI: the row shows Issued; Renew now → not due → Force renew → new serial.
      await page.goto(`${ACME_ADMIN_URL}/`);
      await page.getByRole('button', {name: 'Certificates'}).click();
      const row = page.getByTestId('acme-cert-row').filter({has: page.getByText(id.split(',')[0])});
      await expect(row.getByTestId('acme-cert-state')).toHaveText(/issued/i);
      await row.getByRole('button', {name: 'Renew now'}).click();
      await row.getByRole('button', {name: 'Confirm'}).click();
      await row.getByRole('button', {name: 'Force renew'}).click();
      await waitFor(
        async () => {
          const body = (await (await api!.get('/api/acme/certs')).json()) as {certs: {state: string; serial: string}[]};
          return body.certs[0].state === 'issued' && body.certs[0].serial !== issued.serial;
        },
        90_000,
        `never renewed: ${stderr}`,
      );
    } finally {
      await api?.dispose();
      child.kill();
      await new Promise((r) => child.once('exit', r));
    }
  });
});

async function waitFor<T>(probe: () => Promise<T | null | false>, timeoutMs: number, message: string): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const v = await probe();
      if (v) return v as T;
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(message);
}
```

If `GATEWAY_BIN` is not exported from `playwright.config.ts`, export it (it is declared with `export const` today — confirm).

- [ ] **Step 3: Testbook** — append to `e2e/E2E_TESTBOOK.md` after the "Stores & sessions" section:

```markdown
## ACME certificates — `tests/acme.spec.ts`

The main fixture gateway has no `acme:` block, so `E2E-ACME-01` proves the
"not configured" surface unconditionally. `E2E-ACME-02` is gated on
`FEATHERBIT_TEST_PEBBLE_URL` (+ `FEATHERBIT_TEST_PEBBLE_CA`, and optionally
`FEATHERBIT_TEST_ACME_DOMAIN`/`FEATHERBIT_TEST_ACME_PORT`, defaults
`localhost`/`18443`) and spawns a **second** gateway from `fixtures/acme/`
(HTTPS on the port Pebble's validator dials, admin on `19092`) so the suite's
plaintext gateway on 18081 is untouched. Run Pebble with
`dev/pebble/docker-compose.yml`; the CI `e2e` job starts it the same way.

| ID | Scenario | Expected |
|---|---|---|
| E2E-ACME-01 | `GET /api/acme/certs`, `POST /api/acme/certs/x/renew`; **Browser.** click the sidebar's **Certificates** footer button | `200 {"enabled":false,"certs":[]}`; `501`; the panel shows the "ACME is not configured" notice (`data-testid="acme-not-configured"`) |
| E2E-ACME-02 | *Gated on `FEATHERBIT_TEST_PEBBLE_URL`.* Spawn the ACME gateway; poll `GET /api/acme/certs` until `issued`; **Browser.** open its Certificates panel, **Renew now → Confirm → Force renew** | The single cert goes placeholder → issued with a Pebble issuer and `/readyz` is `200`; the row shows `Issued`; the first renew answers `not_due` (the panel offers **Force renew**); after forcing, the serial changes |
```

- [ ] **Step 4: Run**

Run: `cargo build --release && cd e2e && npx playwright test -g E2E-ACME` — expected: ACME-01 passes, ACME-02 skipped (no Pebble). With Pebble up and the env exported: both pass.
Run the whole suite once (`npm test`) to confirm nothing else regressed (the Sidebar gained a button).

- [ ] **Step 5: Commit**

```bash
git add e2e/tests/acme.spec.ts e2e/fixtures/acme e2e/E2E_TESTBOOK.md e2e/playwright.config.ts
git commit -m "test(e2e): ACME certificate scenarios"
```

---

### Task 17: Documentation, config example, roadmap, knowledge graph

**Files:**
- Modify: `website/docs/guides/tls.md`, `website/docs/reference/roadmap.md`, `CLAUDE.md`, `config/system.yaml`

- [ ] **Step 1: TLS guide** — insert a new section in `website/docs/guides/tls.md` after "### Certificate hot-reload" and before "## HTTP/2":

````markdown
## Automatic certificates (ACME)

Instead of files, a certificate slot can be **ACME-managed**: the gateway
registers with an ACME CA (Let's Encrypt by default), proves control of each
domain with the **TLS-ALPN-01** challenge on its own 443 listener, installs the
certificate, and renews it before expiry — no certbot, no sidecar.

```yaml
acme:
  directory_url: https://acme-v02.api.letsencrypt.org/directory   # default
  contact: ["mailto:ops@example.com"]
  terms_of_service_agreed: true
  key_type: ecdsa-p256          # or ecdsa-p384
  renew_before: 30d
  storage:
    type: filesystem
    dir: /var/lib/featherbit/acme

tls:
  acme:
    domains: [api.example.com, www.example.com]   # replaces cert_path/key_path
  sni_certs:
    - server_name: tenant.example.com
      acme: {}                                     # domains defaults to [server_name]
    - server_name: "*.legacy.example.com"          # file-based entries mix freely
      cert_path: /etc/gateway/tls/legacy.crt
      key_path: /etc/gateway/tls/legacy.key
```

### How it behaves

- **Bootstrap.** The listener comes up immediately with a self-signed
  **placeholder** for each managed certificate (TLS-ALPN-01 needs the listener
  up to validate). `GET /readyz` returns `503` with
  `{"acme":{"placeholder":[…]}}` until every managed cert is real, so a load
  balancer or Kubernetes holds traffic without killing the process. Issuance
  normally completes within seconds.
- **Renewal.** Each certificate renews `renew_before` ahead of expiry, or earlier
  if the CA publishes an ARI (renewal-info) window. A new private key is
  generated for every issuance. Renewed certificates are served to **new**
  connections instantly — no `ServerConfig` rebuild, no restart.
- **Failures never drop TLS.** A failed renewal keeps the current certificate
  serving, logs the ACME problem detail, backs off (1 min → 1 h), and shows up
  as `state: failed` with `last_error` in the Admin API and UI. Only a
  placeholder affects readiness.
- **Restart-gated** like every other TLS setting: changing `acme:` or a slot's
  `acme` requires a restart. Stored certificates are reused across restarts.

### Requirements and limits

- The CA must reach the gateway's data-plane listener on **port 443** of each
  domain (TLS-ALPN-01 is port-fixed). Behind a load balancer this means TCP
  passthrough — a TLS-terminating LB cannot forward the challenge.
- `directory_url` must be `https://`. `directory_ca_path` trusts a private CA's
  own HTTPS endpoint (step-ca, Pebble). `eab: { key_id, hmac_key }` enables
  External Account Binding (ZeroSSL, Google Trust Services).
- **Not supported in this version:** wildcard domains (need DNS-01), HTTP-01,
  RSA keys (the ring crypto backend cannot generate them), and ACME on
  `admin.tls`. All are refused at startup with a pointed error.
- Experiment against the **staging** directory
  (`https://acme-staging-v02.api.letsencrypt.org/directory`) — production
  Let's Encrypt has strict duplicate-certificate limits. The gateway never
  re-issues a still-valid certificate outside its renewal window unless you
  force it.

### Storage and clusters

State (account key, certificate keys and chains, pending challenges, the
renewal lease) lives in `acme.storage`:

- `type: filesystem` (default) — a directory; keys are written `0600`. Right
  for a single instance.
- `type: store` — a declared redis/valkey [`stores:`](../reference/configuration.md)
  entry, with `encryption_key` (env-interpolated) sealing every private key
  and the account credentials at rest (AES-256-GCM). Right for N instances:
  one instance takes a **lease** and orders; the others adopt the certificate
  from the store within a minute and, while an order is in flight, refresh
  the challenge certificate every 2 s so the CA may validate through any
  instance behind a TCP load balancer.

```yaml
acme:
  storage:
    type: store
    store: sessions-redis
    encryption_key: ${ACME_STORAGE_KEY}
```

Deleting a store that ACME uses is refused (`409 in_use`, referrer
`acme.storage (system.yaml)`).

### Operating it

- `GET /api/acme/certs` — every managed certificate: `state`
  (`placeholder` | `issued` | `renewing` | `failed`), `not_after`, `issuer`,
  `serial`, `next_renewal_at`, `last_error`. Never includes key material.
- `POST /api/acme/certs/{id}/renew` — renew now (`202`); a still-valid cert
  outside its window answers `200 {"scheduled":false,"reason":"not_due"}`
  unless `?force=true`. `{id}` is the comma-joined, sorted domain list.
- The web UI's **Certificates** footer button shows the same table with a
  per-row **Renew now**.
- Prometheus: `featherbit_acme_cert_not_after_timestamp_seconds{cert_id}`,
  `featherbit_acme_cert_state{cert_id,state}`,
  `featherbit_acme_renewals_total{cert_id,result}`,
  `featherbit_acme_last_renewal_attempt_timestamp_seconds{cert_id}` — alert on
  `not_after - time() < 7*86400` or a rising `result="failure"`.

Test locally against Pebble (Let's Encrypt's test CA) with
`dev/pebble/docker-compose.yml`; see the header of that file.
````

Also update the "Not covered (yet)" section at the bottom of `tls.md`: remove any "automatic certificates" mention if present and add "HTTP-01 / DNS-01 challenges (wildcards), RSA ACME keys, ACME for the admin listener".

- [ ] **Step 2: Roadmap** — in `website/docs/reference/roadmap.md`, extend the TLS termination row (line 13) with:

`… and **automatic certificates via ACME** (TLS-ALPN-01; Let's Encrypt or any RFC 8555 CA incl. EAB; filesystem or redis `stores:` storage with sealed keys; placeholder bootstrap gating `/readyz`; lease-coordinated renewal with ARI; Admin API + Certificates panel + Prometheus series — see [TLS guide → Automatic certificates](../guides/tls.md#automatic-certificates-acme)). Follow-ups: CRL/OCSP revocation, HTTP-01 and DNS-01 (wildcards), RSA ACME keys, ACME on `admin.tls`, pub/sub cert propagation between instances.`

- [ ] **Step 3: `CLAUDE.md`** — in the paragraph starting "TLS termination, HTTP/2, WebSocket…", add one sentence after the SNI sentence:

`**Automatic certificates (ACME)** are implemented in `src/acme/` (TLS-ALPN-01 only; `acme:` block in `system.yaml` + `acme: { domains }` on a `tls`/`sni_certs` slot; `CertStorage` fs/redis backends; placeholder bootstrap gates `/readyz`; `GET/POST /api/acme/certs…`; UI Certificates panel; Pebble-gated live tests via `FEATHERBIT_TEST_PEBBLE_URL`) — `src/server/tls.rs` only consumes `ManagedCerts` through `AcmeHooks`.`

- [ ] **Step 4: `config/system.yaml`** — append a commented example:

```yaml
# Automatic TLS certificates via ACME (TLS-ALPN-01). Uncomment `acme:` and
# replace `tls.cert_path/key_path` with `tls.acme.domains`. Restart-gated.
# Experiment against Let's Encrypt STAGING first — production has strict
# duplicate-certificate rate limits.
# acme:
#   directory_url: https://acme-staging-v02.api.letsencrypt.org/directory
#   contact: ["mailto:ops@example.com"]
#   terms_of_service_agreed: true
#   key_type: ecdsa-p256
#   renew_before: 30d
#   storage:
#     type: filesystem
#     dir: /var/lib/featherbit/acme
# tls:
#   acme:
#     domains: [api.example.com]
```

- [ ] **Step 5: Build docs, refresh the graph, final full verification**

Run: `cd website && npm run build` — expected: builds (anchors resolve).
Run: `graphify update .`
Run: `cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test 2>&1 | tail -3 && cargo check --no-default-features && (cd ui && npm run lint && npm test)`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add website/docs/guides/tls.md website/docs/reference/roadmap.md CLAUDE.md config/system.yaml graphify-out
git commit -m "docs: automatic certificates (ACME) guide, roadmap and config example"
```

Then report results and **wait for the go-ahead** before pushing / opening the PR against `develop` (per the delivery workflow).
