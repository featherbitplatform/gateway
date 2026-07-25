//! Trace records and context snapshots.
//!
//! A [`Trace`] is one policy execution — a live request or a sandbox run —
//! recorded as an initial [`ContextSnapshot`] plus one [`NodeStep`] per node
//! the engine walked.
//!
//! # Why one snapshot per step, not before/after pairs
//!
//! The [`Context`] flows strictly linearly: every plugin takes it by value and
//! hands it back in both the `Ok` and `Err` arms of [`PluginResult`], so
//! `before(step N) == after(step N - 1)` and `before(step 0) == Trace::initial`
//! are guaranteed by the plugin contract itself. Storing explicit pairs would
//! double memory for exactly zero information.
//!
//! The "what did this plugin change" view is therefore *derived*, not stored:
//! see [`crate::debug::diff`], which is computed at read time so the traced
//! request path stays cheap.
//!
//! [`PluginResult`]: crate::plugins::PluginResult

use std::collections::{BTreeMap, HashSet};

use bytes::Bytes;
use serde::Serialize;

use crate::context::{Context, GatewayError};

/// Placeholder substituted for any value the redaction policy matches.
pub const REDACTED: &str = "<redacted>";

/// Header names always redacted, regardless of configuration.
const DEFAULT_REDACT_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "apikey",
    "x-auth-token",
    "x-access-token",
    "x-csrf-token",
    "x-amz-security-token",
    "x-forwarded-client-cert",
];

/// Query parameters always redacted. OAuth/OIDC put codes and tokens in the
/// query string, so omitting these would leak on every `openid-connect` trace.
const DEFAULT_REDACT_QUERY: &[&str] = &[
    "access_token",
    "id_token",
    "refresh_token",
    "token",
    "api_key",
    "apikey",
    "code",
    "client_secret",
    "state",
];

/// Substrings that mark a `context.message` key as secret-bearing.
///
/// `message` is free-form (plugins and Lua scripts write whatever they like),
/// so substring matching is the only defence that scales. Deliberately **not**
/// bare `key`: that would redact `consumer.key_id`, which is exactly what a
/// developer needs to see.
const DEFAULT_REDACT_MESSAGE_SUBSTRINGS: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "token",
    "credential",
    "private_key",
    "authorization",
    "jwt",
];

/// Case-insensitive denylists used when capturing a snapshot.
///
/// Built-ins are always applied; configured names *extend* them rather than
/// replacing them, so an operator cannot accidentally widen exposure by
/// setting a short custom list.
#[derive(Debug, Clone)]
pub struct RedactionPolicy {
    headers: HashSet<String>,
    query_params: HashSet<String>,
    message_keys: HashSet<String>,
    message_substrings: Vec<String>,
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self::new(&[], &[], &[])
    }
}

impl RedactionPolicy {
    /// Builds the policy from the configured extra names.
    pub fn new(headers: &[String], query_params: &[String], message_keys: &[String]) -> Self {
        let lower = |extra: &[String], builtin: &[&str]| -> HashSet<String> {
            builtin
                .iter()
                .map(|s| s.to_string())
                .chain(extra.iter().map(|s| s.to_lowercase()))
                .collect()
        };
        Self {
            headers: lower(headers, DEFAULT_REDACT_HEADERS),
            query_params: lower(query_params, DEFAULT_REDACT_QUERY),
            message_keys: message_keys.iter().map(|s| s.to_lowercase()).collect(),
            message_substrings: DEFAULT_REDACT_MESSAGE_SUBSTRINGS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    fn header_is_secret(&self, name: &str) -> bool {
        self.headers.contains(&name.to_lowercase())
    }

    fn query_is_secret(&self, name: &str) -> bool {
        self.query_params.contains(&name.to_lowercase())
    }

    fn message_is_secret(&self, key: &str) -> bool {
        let lower = key.to_lowercase();
        self.message_keys.contains(&lower)
            || self
                .message_substrings
                .iter()
                .any(|s| lower.contains(s.as_str()))
    }
}

/// How a captured body was handled.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct BodyCapture {
    /// Byte length of the real body. Always recorded — it is free, and answers
    /// "did the body change size?" without capturing the content.
    pub len: usize,
    /// The body text, present only when body capture is enabled, the body
    /// changed since the previous step, and the content is valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// True when `text` was clipped to the configured limit.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub truncated: bool,
    /// True when the body is byte-identical to the previous step's, so the
    /// text was deliberately not repeated.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub unchanged: bool,
    /// True when the body is not valid UTF-8 (an image, blob, …). `text` is
    /// omitted rather than rendered as replacement-character mojibake; `len`
    /// still reports the true size.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub binary: bool,
}

/// Redacted view of `Context.request`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RequestSnapshot {
    pub method: String,
    pub path: String,
    pub host: String,
    pub scheme: String,
    pub headers: BTreeMap<String, Vec<String>>,
    pub query_params: BTreeMap<String, Vec<String>>,
    pub body: BodyCapture,
}

/// Redacted view of `Context.response`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponseSnapshot {
    pub status_code: u16,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: BodyCapture,
}

/// A redacted point-in-time view of the whole [`Context`].
///
/// `BTreeMap` throughout (not `HashMap`) so key order is deterministic: stable
/// UI rendering, stable diffs, stable test assertions.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ContextSnapshot {
    pub request: RequestSnapshot,
    pub response: ResponseSnapshot,
    pub message: BTreeMap<String, serde_json::Value>,
    pub errors: Vec<GatewayError>,
}

/// Knobs for capturing a snapshot, resolved from `DebugConfig`.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub capture_bodies: bool,
    pub max_body_bytes: usize,
    pub redaction: RedactionPolicy,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            capture_bodies: false,
            max_body_bytes: 8192,
            redaction: RedactionPolicy::default(),
        }
    }
}

/// The bodies seen at the previous step, used to avoid storing an unchanged
/// body once per node. Cloning `Bytes` is a refcount bump, not a copy.
#[derive(Debug, Clone, Default)]
pub struct PreviousBodies {
    pub request: Option<Bytes>,
    pub response: Option<Bytes>,
}

impl ContextSnapshot {
    /// Captures `ctx`, applying redaction **at capture time**.
    ///
    /// Redaction is never deferred to read time: a secret must not enter the
    /// trace buffer at all, because the buffer outlives the request.
    pub fn capture(ctx: &Context, opts: &CaptureOptions, prev: &PreviousBodies) -> Self {
        Self {
            request: RequestSnapshot {
                method: ctx.request.method.clone(),
                path: ctx.request.path.clone(),
                host: ctx.request.host.clone(),
                scheme: ctx.request.scheme.clone(),
                headers: redact_map(&ctx.request.headers, |k| opts.redaction.header_is_secret(k)),
                query_params: redact_map(&ctx.request.query_params, |k| {
                    opts.redaction.query_is_secret(k)
                }),
                body: capture_body(&ctx.request.body, prev.request.as_ref(), opts),
            },
            response: ResponseSnapshot {
                status_code: ctx.response.status_code,
                headers: redact_map(&ctx.response.headers, |k| {
                    opts.redaction.header_is_secret(k)
                }),
                body: capture_body(&ctx.response.body, prev.response.as_ref(), opts),
            },
            message: ctx
                .message
                .iter()
                .map(|(k, v)| {
                    let value = if opts.redaction.message_is_secret(k) {
                        serde_json::Value::String(REDACTED.to_string())
                    } else {
                        v.clone()
                    };
                    (k.clone(), value)
                })
                .collect(),
            errors: ctx.errors.clone(),
        }
    }
}

/// Redacts a header/query map into a deterministic `BTreeMap`, preserving the
/// arity of multi-valued entries so the shape of the request is still visible.
fn redact_map(
    src: &std::collections::HashMap<String, Vec<String>>,
    is_secret: impl Fn(&str) -> bool,
) -> BTreeMap<String, Vec<String>> {
    src.iter()
        .map(|(k, values)| {
            let out = if is_secret(k) {
                values.iter().map(|_| REDACTED.to_string()).collect()
            } else {
                values.clone()
            };
            (k.clone(), out)
        })
        .collect()
}

/// Records the body length always, and the text only when capture is on and the
/// bytes differ from the previous step.
fn capture_body(body: &Bytes, prev: Option<&Bytes>, opts: &CaptureOptions) -> BodyCapture {
    let len = body.len();
    if !opts.capture_bodies {
        return BodyCapture {
            len,
            ..Default::default()
        };
    }
    if let Some(p) = prev {
        if p == body {
            return BodyCapture {
                len,
                unchanged: true,
                ..Default::default()
            };
        }
    }
    let truncated = len > opts.max_body_bytes;
    let slice = if truncated {
        &body[..opts.max_body_bytes]
    } else {
        &body[..]
    };

    // Classify the content. A valid-UTF-8 body is shown as text; a genuinely
    // binary one (image, blob, ...) is not rendered — mojibake helps no one —
    // only its size is reported. The subtlety: a *text* body clipped by the
    // limit can end mid-multibyte-char, which is not binary. `Utf8Error`
    // distinguishes the two: `error_len() == None` means "unexpected end" (our
    // truncation split a char), while `Some(_)` means an invalid byte in the
    // middle (real binary).
    match std::str::from_utf8(slice) {
        Ok(s) => BodyCapture {
            len,
            text: Some(s.to_string()),
            truncated,
            ..Default::default()
        },
        Err(e) if e.error_len().is_none() => {
            // Valid text, cut mid-char: keep the valid prefix.
            let valid = std::str::from_utf8(&slice[..e.valid_up_to()]).unwrap_or("");
            BodyCapture {
                len,
                text: Some(valid.to_string()),
                truncated: true,
                ..Default::default()
            }
        }
        Err(_) => BodyCapture {
            len,
            binary: true,
            truncated,
            ..Default::default()
        },
    }
}

/// Whether a node returned success or routed through its error port.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepOutcome {
    Success,
    Error { code: String, message: String },
}

/// Which edge the engine followed after a node — including the cases where
/// there was none, which is what makes an unwired `error` port visible.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Followed the node's success edge.
    Success,
    /// Followed the node's own `error` edge.
    Error,
    /// Fell through to the policy-level `error_handler`.
    CatchAll,
    /// Stopped at a terminal `client` node.
    Terminal,
    /// Succeeded but had no success edge to follow.
    EndOfChain,
    /// Errored with neither an error edge nor a catch-all — the engine wrote a
    /// generic 500 and stopped.
    Unhandled,
    /// The edge pointed at a node id that does not exist.
    NodeNotFound,
}

/// One node execution.
#[derive(Debug, Clone, Serialize)]
pub struct NodeStep {
    pub index: usize,
    pub node_id: String,
    pub node_type: String,
    pub outcome: StepOutcome,
    pub duration_us: u64,
    pub edge: EdgeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_node_id: Option<String>,
    /// The context as it stood *after* this node ran.
    pub after: ContextSnapshot,
}

/// Whether a trace came from live traffic or a sandbox run.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TraceSource {
    Request,
    Sandbox,
}

/// One complete policy execution.
#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    pub id: String,
    /// Monotonic sequence number, giving stable newest-first ordering
    /// independent of clock skew.
    pub seq: u64,
    pub source: TraceSource,
    pub started_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    pub policy: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_us: u64,
    pub captured_bodies: bool,
    pub initial: ContextSnapshot,
    pub steps: Vec<NodeStep>,
    /// Human-readable notes about the recording itself (limits hit, etc.).
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use std::collections::HashMap;

    fn ctx() -> Context {
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            vec!["Bearer supersecret".to_string()],
        );
        headers.insert("accept".to_string(), vec!["application/json".to_string()]);
        let mut query = HashMap::new();
        query.insert("code".to_string(), vec!["oauth-code-xyz".to_string()]);
        query.insert("page".to_string(), vec!["2".to_string()]);
        let mut message = HashMap::new();
        message.insert(
            "consumer.key_id".to_string(),
            serde_json::json!("alice-key"),
        );
        message.insert("access_token".to_string(), serde_json::json!("tok-123"));
        message.insert("user_id".to_string(), serde_json::json!("alice"));
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/api/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: query,
                body: Bytes::from_static(b"request-body"),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 200,
                headers: HashMap::new(),
                body: Bytes::from_static(b"response-body"),
            },
            message,
            errors: Vec::new(),
        }
    }

    fn opts(capture_bodies: bool) -> CaptureOptions {
        CaptureOptions {
            capture_bodies,
            ..Default::default()
        }
    }

    #[test]
    fn test_body_length_recorded_but_text_withheld_by_default() {
        let s = ContextSnapshot::capture(&ctx(), &opts(false), &PreviousBodies::default());
        // The size is always useful and always free; the content is not captured.
        assert_eq!(s.request.body.len, 12);
        assert_eq!(s.response.body.len, 13);
        assert!(s.request.body.text.is_none());
        assert!(s.response.body.text.is_none());
    }

    #[test]
    fn test_bodies_captured_when_enabled() {
        let s = ContextSnapshot::capture(&ctx(), &opts(true), &PreviousBodies::default());
        assert_eq!(s.request.body.text.as_deref(), Some("request-body"));
        assert_eq!(s.response.body.text.as_deref(), Some("response-body"));
        assert!(!s.request.body.truncated);
    }

    #[test]
    fn test_body_truncated_at_limit() {
        let o = CaptureOptions {
            capture_bodies: true,
            max_body_bytes: 4,
            ..Default::default()
        };
        let s = ContextSnapshot::capture(&ctx(), &o, &PreviousBodies::default());
        assert_eq!(s.request.body.text.as_deref(), Some("requ"));
        assert!(s.request.body.truncated);
        // The true length is still reported, not the truncated one.
        assert_eq!(s.request.body.len, 12);
    }

    /// A binary body (e.g. a PNG) must be labelled, never rendered as mojibake,
    /// and must never break capture.
    #[test]
    fn test_binary_body_is_flagged_not_rendered() {
        let mut c = ctx();
        // PNG magic bytes + a NUL — not valid UTF-8.
        c.response.body = Bytes::from_static(&[0x89, b'P', b'N', b'G', 0x0d, 0x00, 0xff, 0xfe]);
        let s = ContextSnapshot::capture(&c, &opts(true), &PreviousBodies::default());
        assert!(
            s.response.body.binary,
            "non-UTF-8 body should be flagged binary"
        );
        assert!(
            s.response.body.text.is_none(),
            "binary body must not be rendered as text"
        );
        assert_eq!(s.response.body.len, 8, "true size still reported");
    }

    /// Valid UTF-8 text that happens to be clipped mid-multibyte-char is text,
    /// not binary — the truncation must not be mistaken for corruption.
    #[test]
    fn test_multibyte_text_truncated_midchar_is_still_text() {
        let mut c = ctx();
        // "é" is two bytes (0xc3 0xa9); a 3-byte limit cuts the second one.
        c.request.body = Bytes::from_static("aéb".as_bytes()); // a, é(2 bytes), b = 4 bytes
        let o = CaptureOptions {
            capture_bodies: true,
            max_body_bytes: 2,
            ..Default::default()
        };
        let s = ContextSnapshot::capture(&c, &o, &PreviousBodies::default());
        // Byte 2 splits 'é', so only "a" is valid — kept as text, flagged truncated.
        assert!(
            !s.request.body.binary,
            "truncated text must not be called binary"
        );
        assert_eq!(s.request.body.text.as_deref(), Some("a"));
        assert!(s.request.body.truncated);
    }

    #[test]
    fn test_unchanged_body_is_not_repeated() {
        let c = ctx();
        let prev = PreviousBodies {
            request: Some(c.request.body.clone()),
            response: Some(Bytes::from_static(b"different")),
        };
        let s = ContextSnapshot::capture(&c, &opts(true), &prev);
        assert!(
            s.request.body.unchanged,
            "identical body should not be stored again"
        );
        assert!(s.request.body.text.is_none());
        assert_eq!(s.request.body.len, 12, "length still recorded when deduped");
        // The response body did change, so it is captured.
        assert_eq!(s.response.body.text.as_deref(), Some("response-body"));
    }

    #[test]
    fn test_default_denylists_redact() {
        let s = ContextSnapshot::capture(&ctx(), &opts(false), &PreviousBodies::default());
        assert_eq!(s.request.headers["authorization"], vec![REDACTED]);
        assert_eq!(s.request.headers["accept"], vec!["application/json"]);
        // OAuth codes live in the query string -- redacting headers alone is not enough.
        assert_eq!(s.request.query_params["code"], vec![REDACTED]);
        assert_eq!(s.request.query_params["page"], vec!["2"]);
        assert_eq!(s.message["access_token"], serde_json::json!(REDACTED));
    }

    /// The substring rule must not swallow `consumer.key_id` -- that is exactly
    /// the value a developer is trying to see.
    #[test]
    fn test_key_id_survives_redaction() {
        let s = ContextSnapshot::capture(&ctx(), &opts(false), &PreviousBodies::default());
        assert_eq!(s.message["consumer.key_id"], serde_json::json!("alice-key"));
        assert_eq!(s.message["user_id"], serde_json::json!("alice"));
    }

    #[test]
    fn test_redaction_is_case_insensitive() {
        let mut c = ctx();
        c.request.headers.clear();
        c.request
            .headers
            .insert("Authorization".to_string(), vec!["x".to_string()]);
        c.request
            .headers
            .insert("X-API-Key".to_string(), vec!["y".to_string()]);
        let s = ContextSnapshot::capture(&c, &opts(false), &PreviousBodies::default());
        assert_eq!(s.request.headers["Authorization"], vec![REDACTED]);
        assert_eq!(s.request.headers["X-API-Key"], vec![REDACTED]);
    }

    #[test]
    fn test_multi_valued_header_arity_preserved() {
        let mut c = ctx();
        c.response.headers.insert(
            "set-cookie".to_string(),
            vec!["a=1".to_string(), "b=2".to_string()],
        );
        let s = ContextSnapshot::capture(&c, &opts(false), &PreviousBodies::default());
        // Two cookies were set: that shape is visible, the values are not.
        assert_eq!(s.response.headers["set-cookie"], vec![REDACTED, REDACTED]);
    }

    /// Configured names must *extend* the built-ins, never replace them.
    #[test]
    fn test_config_extends_builtin_denylist() {
        let o = CaptureOptions {
            redaction: RedactionPolicy::new(
                &["x-custom-secret".to_string()],
                &[],
                &["tenant".to_string()],
            ),
            ..Default::default()
        };
        let mut c = ctx();
        c.request
            .headers
            .insert("x-custom-secret".to_string(), vec!["s".to_string()]);
        c.message
            .insert("tenant".to_string(), serde_json::json!("acme"));
        let s = ContextSnapshot::capture(&c, &o, &PreviousBodies::default());
        assert_eq!(s.request.headers["x-custom-secret"], vec![REDACTED]);
        assert_eq!(s.message["tenant"], serde_json::json!(REDACTED));
        // ...and the built-ins still apply.
        assert_eq!(s.request.headers["authorization"], vec![REDACTED]);
    }
}
