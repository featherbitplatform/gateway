//! Schema for `system.yaml`: process-level settings (data-plane listener,
//! TLS, HTTP/2, timeouts, logging, admin API). Loaded once at startup and
//! never hot-reloaded; every top-level field has a serde default, so any
//! section may be omitted.

use serde::Deserialize;

/// Root of `system.yaml`.
///
/// ```yaml
/// listener: { bind: 0.0.0.0, port: 8080 }
/// admin:
///   port: 9090
///   username: ${ADMIN_USER:-admin}
///   password: ${ADMIN_PASS}
/// logging: { level: info, format: json }
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct SystemConfig {
    /// Data-plane listener; defaults to `0.0.0.0:8080` when the section is omitted.
    #[serde(default = "default_listener")]
    pub listener: ListenerConfig,
    /// TLS termination settings; `None` (the default) serves plain HTTP.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// HTTP/2 toggle; enabled by default. When on, the listener serves HTTP/2
    /// alongside HTTP/1.1 (ALPN-negotiated over TLS, h2c prior-knowledge over
    /// plaintext).
    #[serde(default)]
    pub http2: Http2Config,
    /// Connection/read/write/idle timeouts, in seconds.
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    /// Log level and output format; defaults to `info` / `json`.
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Admin REST API settings; `None` (the default) disables the admin server entirely.
    #[serde(default)]
    pub admin: Option<AdminConfig>,
    /// Where gateway config (routes/policies/consumers) is loaded from and
    /// where Admin API writes are persisted. Defaults to the local file.
    #[serde(default)]
    pub config: ConfigSourceConfig,
    /// L4 (TCP/UDP) stream listeners. Each binds a port at startup and proxies
    /// raw bytes to an upstream pool, independent of the HTTP data plane.
    #[serde(default)]
    pub stream: Vec<StreamListenerConfig>,
    /// Policy-execution tracing and the plugin sandbox; disabled by default.
    #[serde(default)]
    pub debug: DebugConfig,
    /// Automatic certificates via ACME (RFC 8555, TLS-ALPN-01). `None` (the
    /// default) disables the feature; any `tls.acme` / `sni_certs[].acme` slot
    /// then fails validation.
    #[serde(default)]
    pub acme: Option<AcmeConfig>,
}

/// Debug mode: per-request policy-execution tracing plus the plugin sandbox.
///
/// Off by default. Because `system.yaml` is read once at startup and never
/// hot-reloaded, **toggling debug mode requires a restart** — which is also the
/// safety property that keeps a compromised Admin API credential from switching
/// on request-context capture.
///
/// ```yaml
/// debug:
///   enabled: ${FEATHERBIT_DEBUG:-false}
///   capture_bodies: ${FEATHERBIT_DEBUG_BODIES:-false}
/// ```
///
/// Always keep the `:-` default in interpolated values: `${FEATHERBIT_DEBUG}`
/// with the variable unset expands to empty text, which YAML parses as null and
/// serde then rejects for a `bool`.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct DebugConfig {
    /// Master switch. When false nothing is traced and every `/api/debug/*`
    /// route except `GET /api/debug/config` responds `404`.
    pub enabled: bool,
    /// Allows `POST /api/debug/sandbox` (only meaningful while `enabled`), so a
    /// deployment can trace requests without exposing plugin execution.
    pub sandbox: bool,
    /// Request header whose presence opts a single request into tracing.
    /// Lowercased when the settings are resolved.
    pub trigger_header: String,
    /// Trace every request instead of waiting for `trigger_header`. A firehose:
    /// it snapshots the context once per node for all traffic.
    pub trace_all: bool,
    /// Capture request/response bodies in snapshots. Off by default because it
    /// is the expensive part; bodies are also the one thing redaction cannot
    /// clean.
    pub capture_bodies: bool,
    /// Per-body truncation limit, in bytes, when `capture_bodies` is on.
    pub max_body_bytes: usize,
    /// Ring-buffer capacity. `0` disables storage.
    pub max_traces: usize,
    /// Maximum steps recorded per trace, bounding a runaway policy's trace.
    pub max_steps: usize,
    /// Deadline for one sandbox run, in seconds.
    pub sandbox_timeout_seconds: u64,
    /// Header names to redact **in addition to** the built-in denylist.
    pub redact_headers: Vec<String>,
    /// Query parameter names to redact in addition to the built-in denylist.
    pub redact_query_params: Vec<String>,
    /// `context.message` keys to redact in addition to the built-in denylist.
    pub redact_message_keys: Vec<String>,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sandbox: true,
            trigger_header: default_trigger_header(),
            trace_all: false,
            capture_bodies: false,
            max_body_bytes: 8192,
            max_traces: 50,
            max_steps: 200,
            sandbox_timeout_seconds: 30,
            redact_headers: Vec::new(),
            redact_query_params: Vec::new(),
            redact_message_keys: Vec::new(),
        }
    }
}

fn default_trigger_header() -> String {
    "x-featherbit-debug".to_string()
}

/// Selects the gateway-config backend.
///
/// ```yaml
/// config:
///   source: etcd          # file (default) | etcd
///   etcd:
///     endpoints: ["http://etcd:2379"]
///     prefix: /featherbit
/// ```
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ConfigSourceConfig {
    /// `file` (default, single-node) or `etcd` (shared config for an HA cluster).
    #[serde(default)]
    pub source: ConfigSourceKind,
    /// etcd connection settings; required when `source: etcd`.
    #[serde(default)]
    pub etcd: Option<EtcdConfig>,
}

/// Which config backend to use.
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSourceKind {
    /// Load from and apply Admin edits to the local `gateway.yaml` (default).
    #[default]
    File,
    /// Load from and write to etcd; watch for cluster-wide changes.
    Etcd,
}

/// etcd connection settings (used when `config.source` is `etcd`).
#[derive(Debug, Deserialize, Clone)]
pub struct EtcdConfig {
    /// etcd endpoints, e.g. `["http://127.0.0.1:2379"]`. Required.
    pub endpoints: Vec<String>,
    /// Key prefix under which resources are stored; defaults to `/featherbit`.
    #[serde(default = "default_etcd_prefix")]
    pub prefix: String,
    /// Optional username for etcd authentication.
    #[serde(default)]
    pub user: Option<String>,
    /// Optional password for etcd authentication.
    #[serde(default)]
    pub password: Option<String>,
    /// Connect/operation timeout in milliseconds; defaults to `3000`.
    #[serde(default = "default_etcd_timeout")]
    pub timeout_ms: u64,
}

fn default_etcd_prefix() -> String {
    "/featherbit".to_string()
}

fn default_etcd_timeout() -> u64 {
    3000
}

/// Bind address and port for the data-plane HTTP listener.
#[derive(Debug, Deserialize, Clone)]
pub struct ListenerConfig {
    /// Interface to bind; defaults to `0.0.0.0`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// TCP port; defaults to `8080`.
    #[serde(default = "default_port")]
    pub port: u16,
}

/// An L4 stream listener: binds `bind:port` and proxies raw TCP or UDP to an
/// upstream pool. Bound once at startup (fail-fast), like the HTTP listener.
#[derive(Debug, Deserialize, Clone)]
pub struct StreamListenerConfig {
    /// Transport protocol; defaults to `tcp`.
    #[serde(default)]
    pub protocol: StreamProtocol,
    /// Interface to bind; defaults to `0.0.0.0`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// TCP/UDP port to listen on. Required.
    pub port: u16,
    /// Backend pool this listener forwards to. When `sni_routes` are set, this
    /// is the fallback for connections whose SNI matches no route (and for
    /// non-TLS / no-SNI connections).
    pub upstream: StreamUpstreamConfig,
    /// SNI-based passthrough routes (TCP only). Each maps a ClientHello SNI
    /// hostname to its own upstream pool without terminating TLS. Ignored (with
    /// a warning) for UDP listeners.
    #[serde(default)]
    pub sni_routes: Vec<SniRoute>,
}

/// One SNI passthrough route: an exact or single-label-wildcard server name
/// mapped to its own upstream pool.
#[derive(Debug, Deserialize, Clone)]
pub struct SniRoute {
    /// SNI hostname to match: exact (`api.example.com`) or single-label
    /// wildcard (`*.example.com`). Case-insensitive.
    pub server_name: String,
    /// Backend pool for connections whose SNI matches `server_name`.
    pub upstream: StreamUpstreamConfig,
}

/// Transport protocol for an L4 stream listener.
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StreamProtocol {
    #[default]
    Tcp,
    Udp,
}

/// Upstream pool for an L4 stream listener.
#[derive(Debug, Deserialize, Clone)]
pub struct StreamUpstreamConfig {
    /// Backend targets (`host`/`port`); at least one required.
    pub targets: Vec<crate::balancer::Target>,
    /// Load-balancing strategy: `round_robin` (default), `least_connections`,
    /// or `ip_hash`. Absent means round-robin.
    #[serde(default)]
    pub load_balancing: Option<String>,
}

/// TLS termination settings for a listener (data plane or admin).
#[derive(Debug, Deserialize, Clone)]
pub struct TlsConfig {
    /// Path to the PEM certificate chain. Required unless this slot is
    /// ACME-managed (`acme`).
    #[serde(default)]
    pub cert_path: Option<String>,
    /// Path to the PEM private key. Required unless this slot is ACME-managed.
    #[serde(default)]
    pub key_path: Option<String>,
    /// Minimum TLS protocol version, `"1.2"` or `"1.3"`; defaults to `"1.2"`.
    #[serde(default = "default_tls_min_version")]
    pub min_version: String,
    /// PEM CA bundle used to verify **client** certificates (mTLS). When set,
    /// the listener requests and validates a client cert during the handshake.
    #[serde(default)]
    pub client_ca_path: Option<String>,
    /// When mTLS is enabled (`client_ca_path` set), whether a valid client cert
    /// is **required** (default) — clients without one are rejected — or
    /// optional (`false`, anonymous clients allowed; presented certs are still
    /// validated). Ignored when `client_ca_path` is unset.
    #[serde(default = "default_true")]
    pub client_cert_required: bool,
    /// Additional certificates selected by the ClientHello SNI hostname. When
    /// none match (or no SNI is sent), `cert_path`/`key_path` above is the
    /// default/fallback.
    #[serde(default)]
    pub sni_certs: Vec<SniCert>,
    /// Obtain this certificate automatically via ACME instead of files.
    /// Mutually exclusive with `cert_path`/`key_path`; requires the top-level
    /// `acme:` block. `domains` is mandatory here (a default cert has no
    /// server name to infer from).
    #[serde(default)]
    pub acme: Option<AcmeSlot>,
}

/// One SNI-selected certificate for multi-domain TLS termination: an exact or
/// single-label-wildcard server name mapped to its own cert/key.
#[derive(Debug, Deserialize, Clone)]
pub struct SniCert {
    /// SNI hostname to match: exact (`api.example.com`) or single-label
    /// wildcard (`*.example.com`). Case-insensitive.
    pub server_name: String,
    /// PEM certificate chain to present for this hostname. Required unless
    /// this slot is ACME-managed (`acme`).
    #[serde(default)]
    pub cert_path: Option<String>,
    /// PEM private key for this hostname's certificate. Required unless this
    /// slot is ACME-managed.
    #[serde(default)]
    pub key_path: Option<String>,
    /// Obtain this certificate automatically via ACME instead of files.
    /// Mutually exclusive with `cert_path`/`key_path`; requires the top-level
    /// `acme:` block. `domains` defaults to `[server_name]` when omitted.
    #[serde(default)]
    pub acme: Option<AcmeSlot>,
}

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
    // Accepted and documented in `system.yaml`, but not yet read by the ACME
    // client (account registration lands in a later task).
    #[allow(dead_code)]
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
        other => {
            return Err(format!(
                "unknown duration unit '{other}' in '{s}' (use d/h/m/s)"
            ))
        }
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
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
        return Err(format!(
            "'{d}': IP addresses are not supported ACME identifiers"
        ));
    }
    if !d
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
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
    // Exercised directly by `acme_config_tests`; consumed by the cert
    // resolver/renewal loop in a later task.
    #[allow(dead_code)]
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
                (None, None, None) => Err(format!("{what}: set cert_path + key_path, or acme")),
                (Some(_), None, _) | (None, Some(_), _) => Err(format!(
                    "{what}: cert_path and key_path must be set together"
                )),
                (Some(_), Some(_), Some(_)) => Err(format!(
                    "{what}: set exactly one of cert_path/key_path or acme"
                )),
            }
        }
        check_slot(
            label,
            &self.cert_path,
            &self.key_path,
            &self.acme,
            acme_enabled,
        )?;
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
    pub fn validate_against_gateway(
        &self,
        gw: &crate::config::GatewayConfig,
    ) -> Result<(), String> {
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

/// HTTP/2 support toggle; enabled by default.
#[derive(Debug, Deserialize, Clone)]
pub struct Http2Config {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Connection lifecycle timeouts in seconds.
///
/// `connection`/`read`/`write` default to 30s; `idle` defaults to 300s;
/// `shutdown` (the graceful-drain deadline) defaults to 30s.
#[derive(Debug, Deserialize, Clone)]
pub struct TimeoutConfig {
    #[serde(default = "default_timeout_30")]
    pub connection_seconds: u64,
    // Accepted and documented in `system.yaml`, but not yet enforced by the
    // data plane (see the roadmap). Kept so existing configs stay valid.
    #[allow(dead_code)]
    #[serde(default = "default_timeout_30")]
    pub read_seconds: u64,
    #[allow(dead_code)]
    #[serde(default = "default_timeout_30")]
    pub write_seconds: u64,
    #[serde(default = "default_timeout_300")]
    pub idle_seconds: u64,
    /// Max time to drain in-flight connections on graceful shutdown before
    /// forcing exit.
    #[serde(default = "default_timeout_30")]
    pub shutdown_timeout_seconds: u64,
}

/// Logging configuration for the `tracing` subscriber.
///
/// `RUST_LOG`, when set, overrides `level` at startup.
#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    /// Log level filter (`trace`..`error`); defaults to `info`.
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Output format: `json` (default) or any other value for plain text.
    #[serde(default = "default_log_format")]
    pub format: String,
}

/// Admin REST API settings; presence of this section enables the admin server
/// on a separate port from the data plane.
#[derive(Debug, Deserialize, Clone)]
pub struct AdminConfig {
    /// Interface to bind; defaults to `0.0.0.0`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// TCP port; defaults to `9090`.
    #[serde(default = "default_admin_port")]
    pub port: u16,
    /// Basic Auth username. Required (typically supplied via `${ENV_VAR}`).
    pub username: String,
    /// Basic Auth password. Required (typically supplied via `${ENV_VAR}`).
    pub password: String,
    /// Serve the embedded web UI (node-graph editor) as the unauthenticated
    /// fallback. `false` returns 404 for non-API paths. Restart-gated like
    /// the rest of this file; inert in binaries compiled without the `ui`
    /// feature (the headless image), which never serve the UI.
    #[serde(default = "default_true")]
    // Only read by `build_router` when compiled with the `ui` feature (see
    // src/admin/mod.rs); still parsed and stored either way so a
    // `system.yaml` with `ui_enabled` set doesn't fail to parse on a
    // headless build.
    #[cfg_attr(not(feature = "ui"), allow(dead_code))]
    pub ui_enabled: bool,
    /// TLS termination for the admin listener; `None` (the default) serves
    /// plain HTTP. Reuses the same [`TlsConfig`] as the data plane.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

fn default_listener() -> ListenerConfig {
    ListenerConfig {
        bind: default_bind(),
        port: default_port(),
    }
}

fn default_bind() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_admin_port() -> u16 {
    9090
}
fn default_true() -> bool {
    true
}
fn default_tls_min_version() -> String {
    "1.2".to_string()
}
fn default_timeout_30() -> u64 {
    30
}
fn default_timeout_300() -> u64 {
    300
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_log_format() -> String {
    "json".to_string()
}

impl Default for Http2Config {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connection_seconds: 30,
            read_seconds: 30,
            write_seconds: 30,
            idle_seconds: 300,
            shutdown_timeout_seconds: 30,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "json".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shutdown_timeout_default_and_parse() {
        // Absent -> 30 (via serde default) and matches the Default impl.
        let cfg: TimeoutConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg.shutdown_timeout_seconds, 30);
        assert_eq!(TimeoutConfig::default().shutdown_timeout_seconds, 30);

        // Explicit value is honored.
        let cfg: TimeoutConfig = serde_yaml::from_str("shutdown_timeout_seconds: 5").unwrap();
        assert_eq!(cfg.shutdown_timeout_seconds, 5);
    }

    /// Debug mode must be off unless explicitly switched on — an omitted
    /// section can never enable context capture.
    #[test]
    fn test_debug_defaults_to_disabled() {
        let cfg: DebugConfig = serde_yaml::from_str("{}").unwrap();
        assert!(!cfg.enabled);
        assert!(!cfg.trace_all);
        assert!(!cfg.capture_bodies);
        assert!(cfg.sandbox, "sandbox is allowed once debug itself is on");
        assert_eq!(cfg.trigger_header, "x-featherbit-debug");
        assert_eq!(cfg.max_traces, 50);
        assert_eq!(cfg.max_steps, 200);
        assert_eq!(cfg.max_body_bytes, 8192);
        assert_eq!(cfg.sandbox_timeout_seconds, 30);
        assert!(cfg.redact_headers.is_empty());
    }

    /// A `system.yaml` with no `debug:` section at all still parses, leaving
    /// debug off.
    #[test]
    fn test_system_config_without_debug_section() {
        let cfg: SystemConfig = serde_yaml::from_str("listener: { port: 8080 }").unwrap();
        assert!(!cfg.debug.enabled);
    }

    #[test]
    fn test_debug_explicit_block_parses() {
        let cfg: DebugConfig = serde_yaml::from_str(
            "enabled: true\ncapture_bodies: true\nmax_traces: 5\nredact_headers: [x-custom]\n",
        )
        .unwrap();
        assert!(cfg.enabled);
        assert!(cfg.capture_bodies);
        assert_eq!(cfg.max_traces, 5);
        assert_eq!(cfg.redact_headers, vec!["x-custom".to_string()]);
        // Unset fields still fall back to their defaults.
        assert_eq!(cfg.trigger_header, "x-featherbit-debug");
        assert_eq!(cfg.max_steps, 200);
    }

    #[test]
    fn test_admin_ui_enabled_defaults_true() {
        let cfg: AdminConfig = serde_yaml::from_str("username: u\npassword: p\n").unwrap();
        assert!(cfg.ui_enabled);
    }

    #[test]
    fn test_admin_ui_enabled_false_parses() {
        let cfg: AdminConfig =
            serde_yaml::from_str("username: u\npassword: p\nui_enabled: false\n").unwrap();
        assert!(!cfg.ui_enabled);
    }
}

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
        assert_eq!(
            normalize_domain("API.Example.com").unwrap(),
            "api.example.com"
        );
        assert!(normalize_domain("*.example.com")
            .unwrap_err()
            .contains("wildcard"));
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
        assert!(s
            .validate()
            .unwrap_err()
            .contains("terms_of_service_agreed"));
    }

    #[test]
    fn directory_must_be_https_and_key_type_ecdsa() {
        let s =
            sys("acme:\n  terms_of_service_agreed: true\n  directory_url: http://ca.local/dir\n");
        assert!(s.validate().unwrap_err().contains("https"));
        let s = sys("acme:\n  terms_of_service_agreed: true\n  key_type: rsa-2048\n");
        let err = s.validate().unwrap_err();
        assert!(
            err.contains("ecdsa-p256") && err.contains("ecdsa-p384"),
            "{err}"
        );
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
        assert_eq!(
            a.directory_url,
            "https://acme-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(a.key_type, "ecdsa-p256");
        assert_eq!(a.renew_before_duration().unwrap().as_secs(), 30 * 86_400);
        assert!(
            matches!(a.storage, AcmeStorageConfig::Filesystem { ref dir } if dir == "/var/lib/featherbit/acme")
        );
    }
}
