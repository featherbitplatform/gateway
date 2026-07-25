//! Shared outbound HTTP client for plugin callouts and upstream proxying.
//!
//! One pooled hyper client pair lives in `PluginResources` for the process
//! lifetime: plugins (forward-auth, opa, loggers, upstream, ...) reuse its
//! connection pool instead of constructing a client per node or per request.
//! Supports `http` and `https` (rustls, native roots); `ssl_verify: false`
//! selects a lazily-built client with certificate verification disabled.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

type PooledClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// A single outbound request. `timeout` covers the whole call: connect,
/// request write, and response body collection.
pub struct OutboundRequest {
    pub method: http::Method,
    pub url: String,
    /// Header name/value pairs; names may repeat for multi-value headers.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// Whole-call deadline. Callers should default to 3s for callouts.
    pub timeout: Duration,
    /// When false, TLS certificate verification is disabled (matching
    /// APISIX's `ssl_verify: false`). Ignored for plain-http URLs.
    pub ssl_verify: bool,
}

impl OutboundRequest {
    /// A GET with the callout default timeout (3s) and TLS verification on.
    #[allow(dead_code)] // convenience ctor; callers currently build the struct literally
    pub fn new(method: http::Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: Bytes::new(),
            timeout: Duration::from_secs(3),
            ssl_verify: true,
        }
    }
}

/// A fully-buffered outbound response.
pub struct OutboundResponse {
    pub status: u16,
    pub headers: HashMap<String, Vec<String>>,
    pub body: Bytes,
}

/// Outbound call failure, distinguishing timeouts from transport errors so
/// callers can map them to distinct gateway error codes.
#[derive(Debug)]
pub enum OutboundError {
    Timeout(Duration),
    /// Request could not be built (bad URL/header) — a config-shaped error.
    InvalidRequest(String),
    /// Connect/transport/body failure.
    Transport(String),
}

impl std::fmt::Display for OutboundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(d) => write!(f, "outbound request timed out after {:?}", d),
            Self::InvalidRequest(m) => write!(f, "invalid outbound request: {}", m),
            Self::Transport(m) => write!(f, "outbound transport error: {}", m),
        }
    }
}

/// Process-wide pooled HTTP client.
pub struct OutboundClient {
    verified: PooledClient,
    /// Built on first `ssl_verify: false` request; never constructed on the
    /// common path.
    insecure: OnceLock<PooledClient>,
}

impl OutboundClient {
    /// Builds the shared client. TLS uses the platform's native root store.
    /// Advertises both HTTP/1.1 and HTTP/2 via ALPN, so TLS upstreams that
    /// support h2 (e.g. gRPC backends) are served over HTTP/2 while plain-`http`
    /// upstreams stay HTTP/1.1.
    pub fn new() -> Self {
        // Pin the process-level rustls provider to ring (both backends compile
        // in through the dependency tree). Shared with the listeners.
        crate::server::tls::install_crypto_provider();

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("failed to load native TLS roots")
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        Self {
            verified: Client::builder(TokioExecutor::new()).build(https),
            insecure: OnceLock::new(),
        }
    }

    /// Performs the request, honoring `timeout` and `ssl_verify`.
    pub async fn request(&self, req: OutboundRequest) -> Result<OutboundResponse, OutboundError> {
        let mut builder = http::Request::builder().method(req.method).uri(&req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder
            .body(Full::new(req.body))
            .map_err(|e| OutboundError::InvalidRequest(e.to_string()))?;

        let client = if req.ssl_verify {
            &self.verified
        } else {
            self.insecure.get_or_init(build_insecure_client)
        };

        let deadline = req.timeout;
        let call = async {
            let response = client
                .request(request)
                .await
                .map_err(|e| OutboundError::Transport(e.to_string()))?;

            let status = response.status().as_u16();
            let mut headers: HashMap<String, Vec<String>> = HashMap::new();
            for (name, value) in response.headers() {
                headers
                    .entry(name.as_str().to_string())
                    .or_default()
                    .push(value.to_str().unwrap_or("").to_string());
            }
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(|e| OutboundError::Transport(e.to_string()))?
                .to_bytes();

            Ok(OutboundResponse {
                status,
                headers,
                body,
            })
        };

        tokio::time::timeout(deadline, call)
            .await
            .map_err(|_| OutboundError::Timeout(deadline))?
    }
}

impl Default for OutboundClient {
    fn default() -> Self {
        Self::new()
    }
}

fn build_insecure_client() -> PooledClient {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerification(
            rustls::crypto::ring::default_provider(),
        )))
        .with_no_client_auth();
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .build();
    Client::builder(TokioExecutor::new()).build(https)
}

/// Builds a client TLS connector for a raw upstream WebSocket (`wss`) handshake.
///
/// ALPN is pinned to `http/1.1` (the WebSocket upgrade is HTTP/1.1). When
/// `verify` is false, certificate verification is disabled (matching
/// `ssl_verify: false`). The verified connector is cached, since loading the
/// platform's native root store on every connection is wasteful; the insecure
/// connector is cheap and built on demand.
pub fn client_tls_connector(verify: bool) -> Result<tokio_rustls::TlsConnector, String> {
    crate::server::tls::install_crypto_provider();

    if !verify {
        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification(
                rustls::crypto::ring::default_provider(),
            )))
            .with_no_client_auth();
        let mut config = config;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        return Ok(tokio_rustls::TlsConnector::from(Arc::new(config)));
    }

    static VERIFIED: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
    if let Some(c) = VERIFIED.get() {
        return Ok(c.clone());
    }

    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return Err(format!(
            "no usable native root certificates ({} load error(s))",
            loaded.errors.len()
        ));
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    // Race-safe: whoever loses just uses the winner's connector.
    Ok(VERIFIED.get_or_init(|| connector).clone())
}

/// Certificate verifier that accepts everything — only reachable via an
/// explicit `ssl_verify: false` in plugin config.
#[derive(Debug)]
struct NoVerification(rustls::crypto::CryptoProvider);

impl rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
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
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_tls_connector_insecure_builds() {
        // Insecure connector never touches the native store — fast/deterministic.
        assert!(client_tls_connector(false).is_ok());
    }

    #[test]
    fn test_client_tls_connector_verified_builds() {
        // On a normal dev/CI host the native root store loads; if an
        // environment has no parsable roots the helper returns Err gracefully.
        assert!(client_tls_connector(true).is_ok());
    }
}
