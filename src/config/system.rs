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
    /// Path to the PEM certificate chain. Required.
    pub cert_path: String,
    /// Path to the PEM private key. Required.
    pub key_path: String,
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
}

/// One SNI-selected certificate for multi-domain TLS termination: an exact or
/// single-label-wildcard server name mapped to its own cert/key.
#[derive(Debug, Deserialize, Clone)]
pub struct SniCert {
    /// SNI hostname to match: exact (`api.example.com`) or single-label
    /// wildcard (`*.example.com`). Case-insensitive.
    pub server_name: String,
    /// PEM certificate chain to present for this hostname.
    pub cert_path: String,
    /// PEM private key for this hostname's certificate.
    pub key_path: String,
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
}
