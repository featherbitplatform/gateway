//! TLS termination and protocol (HTTP/1.1 vs HTTP/2) selection for the
//! listeners.
//!
//! This is the single place that knows how to load a PEM cert/key, build a
//! rustls [`ServerConfig`] (with ALPN and a `min_version` floor), turn it into
//! a [`tokio_rustls::TlsAcceptor`], and serve a connection over the negotiated
//! protocol. Both the data-plane listener (`server::listener`) and the optional
//! Admin API TLS reuse [`build_acceptor`] + [`serve_connection`], so cert
//! handling lives in exactly one module.
//!
//! Everything is pinned to the rustls **ring** provider, matching the rest of
//! the dependency tree (see [`install_crypto_provider`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use hyper::body::{Body, Incoming};
use hyper::rt::{Read, Write};
use hyper::service::Service;
use hyper::{Request, Response};
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

use crate::acme::challenge::{ChallengeSolver, ACME_TLS_ALPN};
use crate::acme::ManagedCerts;
use crate::config::TlsConfig;
use crate::stream::sni::SniPattern;

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
            CertSlot::Managed(id) => self
                .acme
                .as_ref()?
                .certs
                .load()
                .get(id)
                .map(|c| c.key.clone()),
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

/// Loads a cert chain + key into a validated [`CertifiedKey`] (same load +
/// key-match check `with_single_cert` performs).
fn certified_key(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    provider: &rustls::crypto::CryptoProvider,
) -> Result<CertifiedKey, TlsError> {
    CertifiedKey::from_der(chain, key, provider).map_err(|e| TlsError::RustlsConfig(e.to_string()))
}

/// A live TLS `ServerConfig` that can be atomically swapped for cert rotation.
/// New connections read the current config; in-flight ones are unaffected.
pub type SharedTlsConfig = Arc<ArcSwap<ServerConfig>>;

/// Failure loading a cert/key or building the rustls config. Every variant
/// carries enough detail (the offending path or message) for a fail-fast
/// startup error.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("failed to read TLS certificate '{0}': {1}")]
    CertRead(String, String),
    #[error("TLS certificate file '{0}' contained no certificates")]
    NoCerts(String),
    #[error("failed to read TLS private key '{0}': {1}")]
    KeyRead(String, String),
    #[error("TLS private key file '{0}' contained no private key")]
    NoKey(String),
    #[error("unsupported TLS min_version '{0}' (expected \"1.2\" or \"1.3\")")]
    BadMinVersion(String),
    #[error("failed to build TLS server config: {0}")]
    RustlsConfig(String),
    #[error("failed to read client-CA bundle '{0}': {1}")]
    ClientCaRead(String, String),
    #[error("client-CA bundle '{0}' contained no certificates")]
    NoClientCaCerts(String),
    #[error("failed to build client certificate verifier: {0}")]
    ClientVerifier(String),
    #[error("TLS slot '{0}' has no certificate source (set cert_path/key_path, or acme)")]
    MissingCertSource(String),
    #[error(
        "TLS slot '{0}' is ACME-managed but no ACME runtime was provided (is `acme:` configured?)"
    )]
    AcmeNotWired(String),
}

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

/// Installs the process-level rustls **ring** `CryptoProvider` exactly once.
///
/// Both the ring and aws-lc-rs backends end up compiled in through the
/// dependency tree; without a pinned default, rustls config builders panic on
/// the ambiguity. Calling this more than once (from the data plane, admin, and
/// the outbound client) is safe — `install_default` returning `Err` when a
/// provider is already installed is a harmless no-op.
pub fn install_crypto_provider() {
    static INSTALL_PROVIDER: std::sync::Once = std::sync::Once::new();
    INSTALL_PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Loads the PEM certificate chain at `path`. Errors if the file is missing or
/// contains no certificates.
pub fn load_cert_chain(path: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let data =
        std::fs::read(path).map_err(|e| TlsError::CertRead(path.to_string(), e.to_string()))?;
    let certs = CertificateDer::pem_slice_iter(&data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::CertRead(path.to_string(), e.to_string()))?;
    if certs.is_empty() {
        return Err(TlsError::NoCerts(path.to_string()));
    }
    Ok(certs)
}

/// Loads the PEM private key at `path` (PKCS#8, PKCS#1, or SEC1). Errors if the
/// file is missing or contains no private key.
pub fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, TlsError> {
    let data =
        std::fs::read(path).map_err(|e| TlsError::KeyRead(path.to_string(), e.to_string()))?;
    match PrivateKeyDer::pem_slice_iter(&data).next() {
        Some(Ok(key)) => Ok(key),
        Some(Err(e)) => Err(TlsError::KeyRead(path.to_string(), e.to_string())),
        None => Err(TlsError::NoKey(path.to_string())),
    }
}

/// Loads a PEM CA bundle at `path` into a [`RootCertStore`] for verifying
/// **client** certificates (mTLS). Errors if the file is missing or yields no
/// usable certificates.
fn load_client_ca_roots(path: &str) -> Result<rustls::RootCertStore, TlsError> {
    let data =
        std::fs::read(path).map_err(|e| TlsError::ClientCaRead(path.to_string(), e.to_string()))?;
    let certs = CertificateDer::pem_slice_iter(&data)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::ClientCaRead(path.to_string(), e.to_string()))?;
    let mut roots = rustls::RootCertStore::empty();
    let (added, _skipped) = roots.add_parsable_certificates(certs);
    if added == 0 {
        return Err(TlsError::NoClientCaCerts(path.to_string()));
    }
    Ok(roots)
}

/// Builds a rustls [`ServerConfig`] from `tls`, enforcing `min_version` and
/// advertising ALPN `h2`+`http/1.1` when `http2_enabled` (else `http/1.1`
/// only). Uses an explicit ring provider so the version floor is honored
/// regardless of global-provider install ordering.
pub fn build_server_config(
    tls: &TlsConfig,
    http2_enabled: bool,
    acme: Option<&AcmeHooks>,
) -> Result<Arc<ServerConfig>, TlsError> {
    install_crypto_provider();

    let versions: &[&'static rustls::SupportedProtocolVersion] = match tls.min_version.as_str() {
        "1.2" => &[&rustls::version::TLS13, &rustls::version::TLS12],
        "1.3" => &[&rustls::version::TLS13],
        other => return Err(TlsError::BadMinVersion(other.to_string())),
    };

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .map_err(|e| TlsError::RustlsConfig(e.to_string()))?;

    // mTLS: when a client-CA bundle is configured, verify client certs against
    // it (required by default; optional when `client_cert_required` is false).
    let builder = match &tls.client_ca_path {
        Some(ca_path) => {
            let roots = load_client_ca_roots(ca_path)?;
            let vbuilder = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider.clone(),
            );
            let vbuilder = if tls.client_cert_required {
                vbuilder
            } else {
                vbuilder.allow_unauthenticated()
            };
            let verifier = vbuilder
                .build()
                .map_err(|e| TlsError::ClientVerifier(e.to_string()))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };

    // Certificate selection: each slot is either a file-based cert loaded now,
    // or (when ACME-managed) a lookup key resolved against `AcmeHooks::certs`
    // on every ClientHello.
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
        Ok(CertSlot::File(Arc::new(certified_key(
            load_cert_chain(c)?,
            load_private_key(k)?,
            provider,
        )?)))
    }

    let default = slot(
        "default",
        &tls.cert_path,
        &tls.key_path,
        &tls.acme,
        tls.acme
            .as_ref()
            .map(|s| s.domains.clone())
            .unwrap_or_default(),
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
            slot(
                &sc.server_name,
                &sc.cert_path,
                &sc.key_path,
                &sc.acme,
                domains,
                acme,
                &provider,
            )?,
        ));
    }

    // Always the resolver — it is what `with_single_cert` builds internally
    // (`AlwaysResolvesChain`), so a single file-based cert behaves identically.
    let mut config = builder.with_cert_resolver(Arc::new(SniCertResolver {
        certs,
        default,
        acme: acme.cloned(),
    }));

    config.alpn_protocols = if http2_enabled {
        // h2 first so a client offering both prefers HTTP/2.
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    if acme.is_some() {
        // Advertised last: browsers offering h2/http1.1 never pick it, and
        // rustls would otherwise abort a validator's acme-tls/1-only hello with
        // no_application_protocol before the resolver could answer.
        config.alpn_protocols.push(ACME_TLS_ALPN.to_vec());
    }

    Ok(Arc::new(config))
}

/// Builds a [`TlsAcceptor`] ready to wrap accepted TCP streams. Fail-fast: any
/// cert/key/config error surfaces here at startup.
///
/// The listeners use [`build_reloadable`] + [`current_acceptor`] so certs can
/// hot-reload; this one-shot form is kept for tests and simple embedding.
#[allow(dead_code)]
pub fn build_acceptor(tls: &TlsConfig, http2_enabled: bool) -> Result<TlsAcceptor, TlsError> {
    Ok(TlsAcceptor::from(build_server_config(
        tls,
        http2_enabled,
        None,
    )?))
}

/// Builds a hot-reloadable TLS config: the initial `ServerConfig` wrapped in an
/// [`ArcSwap`] so [`spawn_cert_watcher`] can swap it in on cert rotation.
/// Fail-fast: a bad cert/key at startup surfaces here.
pub fn build_reloadable(
    tls: &TlsConfig,
    http2_enabled: bool,
    acme: Option<&AcmeHooks>,
) -> Result<SharedTlsConfig, TlsError> {
    Ok(Arc::new(ArcSwap::new(build_server_config(
        tls,
        http2_enabled,
        acme,
    )?)))
}

/// A [`TlsAcceptor`] over the **current** config. Call this per connection (it's
/// an atomic load + `Arc` clone) so reloads take effect for new connections.
pub fn current_acceptor(shared: &SharedTlsConfig) -> TlsAcceptor {
    TlsAcceptor::from(shared.load_full())
}

/// True when the finished handshake negotiated `acme-tls/1`: the connection
/// was a CA validation and must be closed without serving anything.
pub fn negotiated_acme_challenge<IO>(stream: &tokio_rustls::server::TlsStream<IO>) -> bool {
    stream.get_ref().1.alpn_protocol() == Some(ACME_TLS_ALPN)
}

/// Verified identity of an mTLS client, read from its leaf certificate.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientCertIdentity {
    /// Lowercase-hex SHA-256 fingerprint of the leaf certificate (stable id).
    pub fingerprint: String,
    /// Subject Common Name, if present.
    pub subject_cn: Option<String>,
    /// Subject Alternative Name DNS entries.
    pub san_dns: Vec<String>,
}

/// The verified client identity on an mTLS connection, or `None` if the client
/// presented no certificate (anonymous client in optional mode, or mTLS not
/// enabled). Read after the handshake.
pub fn client_cert_identity<IO>(
    stream: &tokio_rustls::server::TlsStream<IO>,
) -> Option<ClientCertIdentity> {
    let leaf = stream.get_ref().1.peer_certificates()?.first()?;
    let digest = ring::digest::digest(&ring::digest::SHA256, leaf.as_ref());
    let fingerprint = digest
        .as_ref()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    let (subject_cn, san_dns) = parse_client_identity(leaf.as_ref());
    Some(ClientCertIdentity {
        fingerprint,
        subject_cn,
        san_dns,
    })
}

/// Extracts the subject CN and SAN DNS names from a DER-encoded certificate.
/// Panic-free: any parse error yields `(None, empty)`.
fn parse_client_identity(der: &[u8]) -> (Option<String>, Vec<String>) {
    use x509_parser::prelude::*;
    let cert = match X509Certificate::from_der(der) {
        Ok((_rem, cert)) => cert,
        Err(_) => return (None, Vec::new()),
    };
    let subject_cn = cert
        .subject()
        .iter_common_name()
        .filter_map(|a| a.as_str().ok())
        .next()
        .map(String::from);
    let san_dns = match cert.subject_alternative_name() {
        Ok(Some(ext)) => ext
            .value
            .general_names
            .iter()
            .filter_map(|gn| match gn {
                GeneralName::DNSName(name) => Some((*name).to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    (subject_cn, san_dns)
}

/// Watches the cert/key files and hot-reloads `shared` when they change.
///
/// Mirrors [`crate::hot_reload::watch_config`]: an OS thread runs a `notify`
/// watcher on each unique parent directory of the cert/key paths (so
/// Kubernetes' atomic secret symlink swap is caught, not just direct writes),
/// forwarding events to a debounced (500 ms) async loop. On each change the
/// `ServerConfig` is rebuilt and atomically stored; a bad/partial cert during
/// rotation is logged and the current config is **kept** (never crash or drop
/// TLS mid-rotation). `label` names the listener in logs (e.g. `"data-plane"`).
pub fn spawn_cert_watcher(
    tls: TlsConfig,
    http2_enabled: bool,
    shared: SharedTlsConfig,
    label: &'static str,
    acme: Option<AcmeHooks>,
) {
    let (tx, mut rx) = mpsc::channel::<()>(1);

    // Unique parent directories of every cert/key file (default + per-SNI), so
    // rotating any of them triggers a reload. ACME-managed slots have no file
    // paths to watch.
    let mut paths: Vec<&String> = Vec::new();
    paths.extend(tls.cert_path.iter());
    paths.extend(tls.key_path.iter());
    for sc in &tls.sni_certs {
        paths.extend(sc.cert_path.iter());
        paths.extend(sc.key_path.iter());
    }
    if paths.is_empty() {
        info!(
            "{} has no file-based certificates; cert watcher not started",
            label
        );
        return;
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for path in paths {
        let dir = PathBuf::from(path)
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }

    std::thread::spawn(move || {
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                        let _ = tx.blocking_send(());
                    }
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    error!("{} cert watcher failed to start: {}", label, e);
                    return;
                }
            };

        for dir in &dirs {
            if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
                error!("{} cert watcher failed to watch {:?}: {}", label, dir, e);
            }
        }
        info!("{} TLS certificate watcher started on {:?}", label, dirs);

        // Keep the watcher alive for the process lifetime.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    });

    tokio::spawn(async move {
        loop {
            if rx.recv().await.is_none() {
                break;
            }
            // Debounce: coalesce a burst of filesystem events.
            tokio::time::sleep(Duration::from_millis(500)).await;
            while rx.try_recv().is_ok() {}

            match build_server_config(&tls, http2_enabled, acme.as_ref()) {
                Ok(config) => {
                    shared.store(config);
                    info!("{} TLS certificate reloaded", label);
                }
                Err(e) => warn!(
                    "{} TLS certificate reload failed (keeping current): {}",
                    label, e
                ),
            }
        }
    });
}

/// Serves a single (already-handshaked, `TokioIo`-wrapped) connection.
///
/// When `http2_enabled`, the hyper-util **auto** builder sniffs the first bytes
/// and dispatches to HTTP/1.1 or HTTP/2 — covering ALPN-negotiated h2 over TLS,
/// h2c prior-knowledge over plaintext, and HTTP/1.1, in one call. Otherwise it
/// serves HTTP/1.1 only. Connection-level errors are logged, not propagated, so
/// one bad connection never affects the accept loop.
///
/// Both paths enable connection **upgrades** (`with_upgrades` /
/// `serve_connection_with_upgrades`) so a handler that returns `101 Switching
/// Protocols` (HTTP/1.1) or `200` (HTTP/2 extended CONNECT) can hand the raw
/// stream to a WebSocket relay (see [`crate::server::websocket`]). The h2 arm
/// also enables the RFC 8441 extended CONNECT protocol
/// (`SETTINGS_ENABLE_CONNECT_PROTOCOL`) so clients can open WebSockets over
/// HTTP/2; h1/h2 auto-detection is preserved.
///
/// The listeners drive connections via [`build_connection`] + a graceful-shutdown
/// watcher; this await-and-log form is kept for tests and simple embedding.
#[allow(dead_code)]
pub async fn serve_connection<I, S, B>(io: I, service: S, http2_enabled: bool)
where
    I: Read + Write + Unpin + Send + 'static,
    S: Service<Request<Incoming>, Response = Response<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if let Err(err) = build_connection(io, service, http2_enabled).await {
        error!("Connection error: {}", err);
    }
}

/// Builds (but does not drive) a connection future for `io`, ready to be either
/// `.await`ed directly or handed to a graceful-shutdown watcher.
///
/// Both protocol paths go through the hyper-util **auto** builder so they share
/// one connection type that implements
/// [`GracefulConnection`](hyper_util::server::graceful::GracefulConnection):
/// `http1_only()` for strict HTTP/1.1, or h2 (auto-detected, extended CONNECT
/// enabled) otherwise. `.into_owned()` detaches the connection from the builder
/// so it is `'static` and can be spawned. See [`serve_connection`] for the
/// simple await-and-log path.
pub fn build_connection<I, S, B>(
    io: I,
    service: S,
    http2_enabled: bool,
) -> auto::UpgradeableConnection<'static, I, S, TokioExecutor>
where
    I: Read + Write + Unpin + Send + 'static,
    S: Service<Request<Incoming>, Response = Response<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if http2_enabled {
        auto::Builder::new(TokioExecutor::new())
            .http2()
            .enable_connect_protocol()
            .serve_connection_with_upgrades(io, service)
            .into_owned()
    } else {
        auto::Builder::new(TokioExecutor::new())
            .http1_only()
            .serve_connection_with_upgrades(io, service)
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TlsConfig;

    /// Writes a fresh self-signed cert+key to unique temp paths and returns a
    /// `TlsConfig` pointing at them.
    fn self_signed(
        tag: &str,
        min_version: &str,
    ) -> (TlsConfig, std::path::PathBuf, std::path::PathBuf) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert_path = dir.join(format!("featherbit_{}_{}.crt", tag, pid));
        let key_path = dir.join(format!("featherbit_{}_{}.key", tag, pid));
        std::fs::write(&cert_path, certified.cert.pem()).unwrap();
        std::fs::write(&key_path, certified.signing_key.serialize_pem()).unwrap();
        let tls = TlsConfig {
            cert_path: Some(cert_path.to_string_lossy().into_owned()),
            key_path: Some(key_path.to_string_lossy().into_owned()),
            min_version: min_version.to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: Vec::new(),
            acme: None,
        };
        (tls, cert_path, key_path)
    }

    #[test]
    fn test_install_crypto_provider_idempotent() {
        install_crypto_provider();
        install_crypto_provider();
    }

    #[test]
    fn test_load_cert_and_key() {
        let (tls, cert, key) = self_signed("load", "1.2");
        assert!(!load_cert_chain(tls.cert_path.as_deref().unwrap())
            .unwrap()
            .is_empty());
        load_private_key(tls.key_path.as_deref().unwrap()).unwrap();
        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
    }

    #[test]
    fn test_load_cert_missing_file() {
        let err = load_cert_chain("does-not-exist.pem").unwrap_err();
        assert!(matches!(err, TlsError::CertRead(_, _)));
    }

    #[test]
    fn test_load_key_no_key_in_pem() {
        // A cert-only file has no private key.
        let (tls, cert, key) = self_signed("nokey", "1.2");
        let err = load_private_key(tls.cert_path.as_deref().unwrap()).unwrap_err();
        assert!(matches!(err, TlsError::NoKey(_)));
        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
    }

    #[test]
    fn test_alpn_reflects_http2_flag() {
        let (tls, cert, key) = self_signed("alpn", "1.2");

        let with_h2 = build_server_config(&tls, true, None).unwrap();
        assert_eq!(
            with_h2.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );

        let without_h2 = build_server_config(&tls, false, None).unwrap();
        assert_eq!(without_h2.alpn_protocols, vec![b"http/1.1".to_vec()]);

        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
    }

    #[test]
    fn test_min_version_1_3_ok_and_bad_rejected() {
        let (mut tls, cert, key) = self_signed("minver", "1.3");
        build_server_config(&tls, true, None).unwrap();

        tls.min_version = "sslv3".to_string();
        let err = build_server_config(&tls, true, None).unwrap_err();
        assert!(matches!(err, TlsError::BadMinVersion(v) if v == "sslv3"));

        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
    }

    /// End-to-end over a real socket: build an acceptor, serve one connection
    /// with the auto builder, and hit it with an HTTPS client — exercising the
    /// full TLS handshake + protocol negotiation path.
    #[tokio::test]
    async fn test_tls_round_trip() {
        use bytes::Bytes;
        use http_body_util::Full;
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;
        use tokio::net::TcpListener;

        let (tls, cert, key) = self_signed("rt", "1.2");
        let acceptor = build_acceptor(&tls, true).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(_) => return,
            };
            let service = service_fn(|_req| async {
                Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::from_static(b"ok"))))
            });
            serve_connection(TokioIo::new(tls_stream), service, true).await;
        });

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .use_rustls_tls() // reliably offers the h2 ALPN protocol
            .build()
            .unwrap();
        let resp = client
            .get(format!("https://{}/", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // ALPN advertised h2 first, so a modern HTTPS client negotiates HTTP/2.
        assert_eq!(resp.version(), reqwest::Version::HTTP_2);
        assert_eq!(resp.text().await.unwrap(), "ok");

        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
    }

    /// Generates a fresh self-signed cert and writes it to the given paths
    /// (overwriting) — each call produces a distinct cert (new key/serial).
    fn write_fresh_cert(cert_path: &std::path::Path, key_path: &std::path::Path) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(cert_path, certified.cert.pem()).unwrap();
        std::fs::write(key_path, certified.signing_key.serialize_pem()).unwrap();
    }

    /// A client cert verifier that records the presented leaf certificate and
    /// accepts everything (test-only).
    #[derive(Debug)]
    struct CapturingVerifier {
        captured: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
        provider: rustls::crypto::CryptoProvider,
    }

    impl rustls::client::danger::ServerCertVerifier for CapturingVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            *self.captured.lock().unwrap() = Some(end_entity.as_ref().to_vec());
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            match rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            ) {
                Ok(v) => Ok(v),
                // rustls-webpki's `EndEntityCert` parse is strict about unknown
                // *critical* X.509 extensions and fails before the signature is
                // even checked — which is exactly what RFC 8737's critical
                // `acmeIdentifier` extension on a TLS-ALPN-01 challenge cert
                // triggers. Fall back to verifying against the raw
                // SubjectPublicKeyInfo (same key, same signature, no extension
                // gate); production code never verifies signatures over ACME
                // certificates, only this test client does.
                Err(_) => {
                    let spki = crate::acme::leaf_spki(cert.as_ref())
                        .map_err(|e| rustls::Error::General(e.to_string()))?;
                    rustls::crypto::verify_tls13_signature_with_raw_key(
                        message,
                        &rustls::pki_types::SubjectPublicKeyInfoDer::from(spki),
                        dss,
                        &self.provider.signature_verification_algorithms,
                    )
                }
            }
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Binds a listener that serves the *current* config per connection and
    /// returns its address.
    async fn spawn_reload_server(shared: SharedTlsConfig) -> std::net::SocketAddr {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = current_acceptor(&shared);
                tokio::spawn(async move {
                    let _ = acceptor.accept(stream).await; // complete handshake, then drop
                });
            }
        });
        addr
    }

    /// Connects to `addr` (SNI `localhost`) and returns the presented leaf cert.
    async fn served_leaf_cert(addr: std::net::SocketAddr) -> Vec<u8> {
        served_leaf_cert_sni(addr, "localhost").await
    }

    /// Connects to `addr` with the given SNI and returns the presented leaf cert.
    async fn served_leaf_cert_sni(addr: std::net::SocketAddr, sni: &str) -> Vec<u8> {
        use tokio::net::TcpStream;
        install_crypto_provider();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let verifier = std::sync::Arc::new(CapturingVerifier {
            captured: captured.clone(),
            provider: rustls::crypto::ring::default_provider(),
        });
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from(sni.to_string()).unwrap();
        let _ = connector.connect(name, tcp).await.unwrap();
        let leaf = captured.lock().unwrap().clone();
        leaf.expect("no server certificate captured")
    }

    #[tokio::test]
    async fn test_cert_hot_reload_swaps_served_cert() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert = dir.join(format!("featherbit_reload_{}.crt", pid));
        let key = dir.join(format!("featherbit_reload_{}.key", pid));
        write_fresh_cert(&cert, &key);
        let tls = TlsConfig {
            cert_path: Some(cert.to_string_lossy().into_owned()),
            key_path: Some(key.to_string_lossy().into_owned()),
            min_version: "1.2".to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: Vec::new(),
            acme: None,
        };

        let shared = build_reloadable(&tls, false, None).unwrap();
        let addr = spawn_reload_server(shared.clone()).await;

        let leaf_a = served_leaf_cert(addr).await;

        // Rotate: overwrite the files with a new cert and swap it in (the
        // deterministic path the watcher also takes).
        write_fresh_cert(&cert, &key);
        shared.store(build_server_config(&tls, false, None).unwrap());

        let leaf_b = served_leaf_cert(addr).await;

        assert_ne!(
            leaf_a, leaf_b,
            "served certificate should change after reload"
        );

        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }

    #[tokio::test]
    async fn test_cert_hot_reload_via_file_watcher() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert = dir.join(format!("featherbit_watch_{}.crt", pid));
        let key = dir.join(format!("featherbit_watch_{}.key", pid));
        write_fresh_cert(&cert, &key);
        let tls = TlsConfig {
            cert_path: Some(cert.to_string_lossy().into_owned()),
            key_path: Some(key.to_string_lossy().into_owned()),
            min_version: "1.2".to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: Vec::new(),
            acme: None,
        };

        let shared = build_reloadable(&tls, false, None).unwrap();
        spawn_cert_watcher(tls.clone(), false, shared.clone(), "test", None);
        let addr = spawn_reload_server(shared.clone()).await;

        let leaf_a = served_leaf_cert(addr).await;

        // Overwrite the cert files; the watcher should reload within a few
        // hundred ms (500ms debounce + notify latency).
        write_fresh_cert(&cert, &key);

        let mut leaf_b = leaf_a.clone();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            leaf_b = served_leaf_cert(addr).await;
            if leaf_b != leaf_a {
                break;
            }
        }
        assert_ne!(
            leaf_a, leaf_b,
            "file watcher should hot-reload the certificate"
        );

        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }

    // ---- mTLS (client-certificate authentication) ----

    fn gen_ca() -> (rcgen::Certificate, rcgen::KeyPair) {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert, key)
    }

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

    fn sha256_hex(bytes: &[u8]) -> String {
        ring::digest::digest(&ring::digest::SHA256, bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Writes a self-signed server cert + CA bundle to temp files and returns a
    /// `TlsConfig` with mTLS configured, plus the CA cert/key for signing
    /// clients and the temp paths for cleanup.
    fn mtls_config(
        tag: &str,
        required: bool,
    ) -> (
        TlsConfig,
        rcgen::Certificate,
        rcgen::KeyPair,
        Vec<std::path::PathBuf>,
    ) {
        let (ca_cert, ca_key) = gen_ca();
        let server = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let scert = dir.join(format!("fb_mtls_{}_{}.crt", tag, pid));
        let skey = dir.join(format!("fb_mtls_{}_{}.key", tag, pid));
        let ca = dir.join(format!("fb_mtls_{}_{}_ca.crt", tag, pid));
        std::fs::write(&scert, server.cert.pem()).unwrap();
        std::fs::write(&skey, server.signing_key.serialize_pem()).unwrap();
        std::fs::write(&ca, ca_cert.pem()).unwrap();
        let tls = TlsConfig {
            cert_path: Some(scert.to_string_lossy().into_owned()),
            key_path: Some(skey.to_string_lossy().into_owned()),
            min_version: "1.2".to_string(),
            client_ca_path: Some(ca.to_string_lossy().into_owned()),
            client_cert_required: required,
            sni_certs: Vec::new(),
            acme: None,
        };
        (tls, ca_cert, ca_key, vec![scert, skey, ca])
    }

    /// Spawns a TLS server that captures the client-cert identity of each
    /// accepted connection into the returned shared slot.
    async fn spawn_mtls_server(
        tls: &TlsConfig,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Option<ClientCertIdentity>>>,
    ) {
        use tokio::net::TcpListener;
        let acceptor = build_acceptor(tls, false).unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cap = captured.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let cap = cap.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls_stream) = acceptor.accept(stream).await {
                        *cap.lock().unwrap() = client_cert_identity(&tls_stream);
                        // Write a byte so an accepted client reads data (vs. a
                        // rejected one reading the handshake alert).
                        let _ = tls_stream.write_all(b"1").await;
                        let _ = tls_stream.flush().await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                });
            }
        });
        (addr, captured)
    }

    /// Connects a TLS client, optionally presenting `(chain, key)`. Returns
    /// whether the connection was **accepted** — not just that the client-side
    /// handshake future resolved. In TLS 1.3 a server rejecting a missing client
    /// cert lets the client's `connect()` resolve `Ok`, then surfaces the alert
    /// on the first read; so we do a post-handshake read and treat a read error
    /// (the alert) as rejection, and a clean EOF/read as acceptance.
    async fn mtls_client_connects(
        addr: std::net::SocketAddr,
        client: Option<(
            Vec<rustls::pki_types::CertificateDer<'static>>,
            rustls::pki_types::PrivateKeyDer<'static>,
        )>,
    ) -> bool {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpStream;
        install_crypto_provider();
        let verifier = std::sync::Arc::new(CapturingVerifier {
            captured: std::sync::Arc::new(std::sync::Mutex::new(None)),
            provider: rustls::crypto::ring::default_provider(),
        });
        let builder = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier);
        let config = match client {
            Some((chain, key)) => builder.with_client_auth_cert(chain, key).unwrap(),
            None => builder.with_no_client_auth(),
        };
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut tls = match connector.connect(name, tcp).await {
            Ok(t) => t,
            Err(_) => return false,
        };
        let mut buf = [0u8; 1];
        // Ok(0) = clean EOF (accepted, then server dropped); Ok(n) = data.
        // Err = the server's rejection alert.
        tls.read(&mut buf).await.is_ok()
    }

    fn client_material(
        cert: &rcgen::Certificate,
        key: &rcgen::KeyPair,
    ) -> (
        Vec<rustls::pki_types::CertificateDer<'static>>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ) {
        let chain = vec![cert.der().clone()];
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into());
        (chain, key_der)
    }

    #[test]
    fn test_parse_client_identity() {
        let mut params = rcgen::CertificateParams::new(vec![
            "a.example.com".to_string(),
            "b.example.com".to_string(),
        ])
        .unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "svc-a");
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();

        let (cn, san) = parse_client_identity(cert.der().as_ref());
        assert_eq!(cn.as_deref(), Some("svc-a"));
        assert_eq!(
            san,
            vec!["a.example.com".to_string(), "b.example.com".to_string()]
        );

        // Malformed DER never panics.
        let (cn, san) = parse_client_identity(&[0xff; 16]);
        assert_eq!(cn, None);
        assert!(san.is_empty());
    }

    #[test]
    fn test_mtls_config_builds_and_rejects_empty_ca() {
        let (tls, _ca_cert, _ca_key, paths) = mtls_config("cfg", true);
        // Required and optional both build.
        build_server_config(&tls, false, None).unwrap();
        let mut optional = tls.clone();
        optional.client_cert_required = false;
        build_server_config(&optional, false, None).unwrap();

        // A client-CA path with no certs is an error.
        let empty = std::env::temp_dir().join(format!("fb_mtls_empty_{}.pem", std::process::id()));
        std::fs::write(&empty, b"not a certificate").unwrap();
        let mut bad = tls;
        bad.client_ca_path = Some(empty.to_string_lossy().into_owned());
        assert!(matches!(
            build_server_config(&bad, false, None),
            Err(TlsError::NoClientCaCerts(_))
        ));

        let _ = std::fs::remove_file(&empty);
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn test_mtls_required_enforces_client_cert() {
        let (tls, ca_cert, ca_key, paths) = mtls_config("req", true);
        let (client_cert, client_key) = gen_signed("client-1", &ca_cert, &ca_key);
        let expected_fp = sha256_hex(client_cert.der().as_ref());

        let (addr, captured) = spawn_mtls_server(&tls).await;

        // Client presenting a CA-signed cert connects, and the server sees its
        // identity (fingerprint + CN + SAN).
        let ok = mtls_client_connects(addr, Some(client_material(&client_cert, &client_key))).await;
        assert!(ok, "client with a valid cert should connect");
        let mut id = None;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            id = captured.lock().unwrap().clone();
            if id.is_some() {
                break;
            }
        }
        let id = id.expect("server should capture client identity");
        assert_eq!(id.fingerprint, expected_fp);
        assert_eq!(id.subject_cn.as_deref(), Some("client-1"));
        assert!(id.san_dns.iter().any(|s| s == "client-1"));

        // Client without a cert is rejected at the handshake.
        let ok = mtls_client_connects(addr, None).await;
        assert!(!ok, "client without a cert must be rejected when required");

        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[tokio::test]
    async fn test_mtls_optional_allows_anonymous() {
        let (tls, _ca_cert, _ca_key, paths) = mtls_config("opt", false);
        let (addr, captured) = spawn_mtls_server(&tls).await;

        // With the cert optional, a client without one still connects.
        let ok = mtls_client_connects(addr, None).await;
        assert!(ok, "anonymous client should connect in optional mode");
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_eq!(captured.lock().unwrap().clone(), None);

        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    // ---- SNI multi-certificate termination ----

    /// Writes a self-signed cert+key (for `sans`) to temp files, returning
    /// `(cert_path, key_path, leaf_der)`.
    fn write_named_cert(tag: &str, sans: Vec<String>) -> (String, String, Vec<u8>) {
        let certified = rcgen::generate_simple_self_signed(sans).unwrap();
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert = dir.join(format!("fb_sni_{}_{}.crt", tag, pid));
        let key = dir.join(format!("fb_sni_{}_{}.key", tag, pid));
        std::fs::write(&cert, certified.cert.pem()).unwrap();
        std::fs::write(&key, certified.signing_key.serialize_pem()).unwrap();
        let leaf = certified.cert.der().as_ref().to_vec();
        (
            cert.to_string_lossy().into_owned(),
            key.to_string_lossy().into_owned(),
            leaf,
        )
    }

    #[tokio::test]
    async fn test_sni_multicert_selects_by_hostname() {
        use crate::config::SniCert;

        let (def_cert, def_key, def_leaf) = write_named_cert("def", vec!["localhost".to_string()]);
        let (a_cert, a_key, a_leaf) = write_named_cert("a", vec!["a.example.com".to_string()]);
        let (w_cert, w_key, w_leaf) =
            write_named_cert("wild", vec!["x.tenant.example.com".to_string()]);

        let tls = TlsConfig {
            cert_path: Some(def_cert.clone()),
            key_path: Some(def_key.clone()),
            min_version: "1.2".to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: vec![
                SniCert {
                    server_name: "a.example.com".to_string(),
                    cert_path: Some(a_cert.clone()),
                    key_path: Some(a_key.clone()),
                    acme: None,
                },
                SniCert {
                    server_name: "*.tenant.example.com".to_string(),
                    cert_path: Some(w_cert.clone()),
                    key_path: Some(w_key.clone()),
                    acme: None,
                },
            ],
            acme: None,
        };

        let shared = build_reloadable(&tls, false, None).unwrap();
        let addr = spawn_reload_server(shared).await;

        // Exact match, wildcard match, and default fallback each get their cert.
        assert_eq!(served_leaf_cert_sni(addr, "a.example.com").await, a_leaf);
        assert_eq!(
            served_leaf_cert_sni(addr, "x.tenant.example.com").await,
            w_leaf
        );
        assert_eq!(
            served_leaf_cert_sni(addr, "unmatched.example.com").await,
            def_leaf
        );
        // Distinct certs, so the assertions above are meaningful.
        assert_ne!(a_leaf, def_leaf);
        assert_ne!(w_leaf, def_leaf);

        for p in [def_cert, def_key, a_cert, a_key, w_cert, w_key] {
            let _ = std::fs::remove_file(p);
        }
    }

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
            acme: Some(crate::config::AcmeSlot {
                domains: domains.iter().map(|d| d.to_string()).collect(),
            }),
            min_version: "1.2".to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: Vec::new(),
        }
    }

    fn hooks_with_placeholder(
        domains: &[&str],
    ) -> (crate::server::tls::AcmeHooks, crate::acme::CertId, Vec<u8>) {
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
        let stream = connector
            .connect(name, tcp)
            .await
            .map_err(|e| e.to_string())?;
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

        assert_eq!(
            served_leaf_cert_sni(addr, "m.example.com").await,
            placeholder_leaf
        );
        // No SNI / unknown SNI falls back to the managed default too.
        assert_eq!(
            served_leaf_cert_sni(addr, "other.example.com").await,
            placeholder_leaf
        );

        // "Issue": publish a new cert into the map — no ServerConfig rebuild.
        let issued = rcgen::generate_simple_self_signed(vec!["m.example.com".to_string()]).unwrap();
        let (key, leaf) = crate::acme::load_certified_key(
            &issued.cert.pem(),
            &issued.signing_key.serialize_pem(),
        )
        .unwrap();
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
        let challenge =
            crate::acme::challenge::build_challenge_cert("m.example.com", "ka").unwrap();
        let challenge_leaf = challenge.end_entity_cert().unwrap().as_ref().to_vec();
        let pending = Arc::new(FakeSolver(std::sync::Mutex::new(Default::default())));
        pending
            .0
            .lock()
            .unwrap()
            .insert("m.example.com".to_string(), challenge.clone());
        let pending_hooks = AcmeHooks {
            certs: empty_hooks.certs.clone(),
            solver: pending,
        };
        let tls = managed_tls(&["m.example.com"]);

        let addr_pending =
            spawn_reload_server(build_reloadable(&tls, true, Some(&pending_hooks)).unwrap()).await;
        let addr_empty =
            spawn_reload_server(build_reloadable(&tls, true, Some(&empty_hooks)).unwrap()).await;

        // Validator: gets the challenge cert and negotiates acme-tls/1.
        let (leaf, alpn) =
            handshake_with_alpn(addr_pending, "m.example.com", vec![b"acme-tls/1".to_vec()])
                .await
                .unwrap();
        assert_eq!(leaf, challenge_leaf);
        assert_eq!(alpn.as_deref(), Some(&b"acme-tls/1"[..]));

        // A normal client on the same listener is unaffected.
        let (leaf, alpn) = handshake_with_alpn(
            addr_pending,
            "m.example.com",
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        )
        .await
        .unwrap();
        assert_eq!(leaf, placeholder_leaf);
        assert_eq!(alpn.as_deref(), Some(&b"h2"[..]));

        // No pending challenge for the name ⇒ handshake refused (RFC 8737 §3).
        assert!(
            handshake_with_alpn(addr_empty, "m.example.com", vec![b"acme-tls/1".to_vec()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_acme_alpn_not_advertised_without_acme() {
        let (tls, cert, key) = self_signed("noacme", "1.2");
        let shared = build_reloadable(&tls, true, None).unwrap();
        let addr = spawn_reload_server(shared).await;
        // Client offering only acme-tls/1 against a non-ACME listener: rustls
        // refuses (no overlap), which is the pre-existing behavior.
        assert!(
            handshake_with_alpn(addr, "localhost", vec![b"acme-tls/1".to_vec()])
                .await
                .is_err()
        );
        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
    }

    #[test]
    fn test_managed_slot_without_hooks_is_a_wiring_error() {
        let tls = managed_tls(&["m.example.com"]);
        let err = build_server_config(&tls, true, None).unwrap_err();
        assert!(matches!(err, TlsError::AcmeNotWired(_)), "{err}");
    }
}
