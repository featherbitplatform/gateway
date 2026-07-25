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

#[derive(Debug)]
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
