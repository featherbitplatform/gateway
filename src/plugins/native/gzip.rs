//! The `gzip` node — compresses the response body with gzip when the client
//! accepts it and the response matches the configured content types and
//! minimum size. Port of APISIX's `gzip` plugin (response-phase: place after
//! `upstream`, before `client`).
//!
//! In APISIX the plugin only flips nginx's gzip switches and nginx does the
//! actual work — including checking the request's `Accept-Encoding` header
//! below APISIX. featherbit has no nginx underneath, so this node performs
//! the `Accept-Encoding` check and the compression itself (via
//! [`content_codec`]).
//!
//! Deviations from APISIX:
//! - `http_version` and `buffers` are not supported (nginx tuning knobs with
//!   no featherbit equivalent — responses are fully buffered).
//! - `min_length` is compared against the actual buffered body length, not
//!   the upstream `Content-Length` header (which APISIX/nginx skip checking
//!   when absent).

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::util::content_codec::{self, ContentEncoding};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Compresses `Context.response.body` with gzip, sets
/// `content-encoding: gzip`, and drops the stale `content-length` so the
/// server layer recomputes it. Skipped (pure passthrough) when the client
/// does not accept gzip, the response is already content-encoded, the
/// content type does not match, or the body is shorter than `min_length`.
/// This plugin never fails at execution time — a codec error logs a warning
/// and leaves the response untouched.
pub struct GzipPlugin {
    types: ContentTypes,
    min_length: usize,
    comp_level: u32,
    vary: bool,
}

/// The `types` config: either any content type (`"*"`) or an allowlist
/// compared against the response `content-type` stripped of parameters.
pub(crate) enum ContentTypes {
    Any,
    List(Vec<String>),
}

impl ContentTypes {
    /// Parses the APISIX `types` shape: `"*"` or a non-empty array of
    /// non-empty strings. `None` (key absent) yields the given default list.
    pub(crate) fn parse(raw: Option<&serde_json::Value>, default: &[&str]) -> Result<Self, String> {
        match raw {
            None => Ok(Self::List(default.iter().map(|s| s.to_string()).collect())),
            Some(serde_json::Value::String(s)) if s == "*" => Ok(Self::Any),
            Some(serde_json::Value::Array(items)) => {
                if items.is_empty() {
                    return Err("types must contain at least one content type".to_string());
                }
                let list = items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .ok_or("types entries must be non-empty strings".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self::List(list))
            }
            Some(_) => Err("types must be \"*\" or an array of content types".to_string()),
        }
    }

    /// Whether the response `content-type` matches. As in APISIX, a missing
    /// content type never matches, and the value is compared with anything
    /// from `;` onward (e.g. `;charset=utf-8`) stripped.
    pub(crate) fn matches(&self, ctx: &Context) -> bool {
        let Some(content_type) = ctx
            .response
            .headers
            .get("content-type")
            .and_then(|v| v.first())
        else {
            return false;
        };
        match self {
            Self::Any => true,
            Self::List(list) => {
                let base = content_type.split(';').next().unwrap_or("").trim();
                list.iter().any(|t| t == base)
            }
        }
    }
}

/// Whether the request's `Accept-Encoding` allows `token` (e.g. `"gzip"`,
/// `"br"`): the token (or `*`) must be listed with a non-zero `q` value.
/// A missing header means the client did not opt in — no compression.
pub(crate) fn accepts_encoding(ctx: &Context, token: &str) -> bool {
    let Some(values) = ctx.request.headers.get("accept-encoding") else {
        return false;
    };
    for value in values {
        for part in value.split(',') {
            let mut pieces = part.trim().splitn(2, ';');
            let name = pieces.next().unwrap_or("").trim().to_ascii_lowercase();
            if name != token && name != "*" {
                continue;
            }
            let q_is_zero = pieces
                .next()
                .and_then(|params| params.trim().strip_prefix("q="))
                .map(|q| q.trim().parse::<f32>() == Ok(0.0))
                .unwrap_or(false);
            if !q_is_zero {
                return true;
            }
        }
    }
    false
}

/// Whether the response already carries a `content-encoding` other than
/// identity. Compressing twice (or compressing opaque encoded bytes) is
/// never wanted, whatever the encoding is — so an unknown value also counts
/// as encoded.
pub(crate) fn response_already_encoded(ctx: &Context) -> bool {
    match ctx
        .response
        .headers
        .get("content-encoding")
        .and_then(|v| v.first())
    {
        None => false,
        Some(value) => !matches!(ContentEncoding::parse(value), Ok(None)),
    }
}

impl GzipPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all optional):
    /// - `types` (array of content types, or the string `"*"` for any;
    ///   default `["text/html"]`): response content types to compress. The
    ///   response `content-type` is compared with parameters (`;charset=...`)
    ///   stripped; a response without a content type is never compressed.
    /// - `min_length` (integer ≥ 1, default `20`): bodies shorter than this
    ///   are not compressed.
    /// - `comp_level` (integer 1-9, default `1`): gzip compression level.
    /// - `vary` (bool, default `false`): when true, appends
    ///   `Vary: Accept-Encoding` to compressed responses.
    ///
    /// ```yaml
    /// type: gzip
    /// config:
    ///   types: ["text/html", "application/json"]
    ///   min_length: 20
    ///   comp_level: 6
    ///   vary: true
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let types = ContentTypes::parse(config.get("types"), &["text/html"])?;

        let min_length = match config.get("min_length") {
            None => 20,
            Some(v) => {
                let n = v
                    .as_u64()
                    .filter(|n| *n >= 1)
                    .ok_or("min_length must be an integer >= 1".to_string())?;
                n as usize
            }
        };

        let comp_level = match config.get("comp_level") {
            None => 1,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or("comp_level must be an integer".to_string())?;
                if !(1..=9).contains(&n) {
                    return Err(format!("comp_level must be within 1-9, got {}", n));
                }
                n as u32
            }
        };

        let vary = config
            .get("vary")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            types,
            min_length,
            comp_level,
            vary,
        })
    }
}

#[async_trait]
impl Plugin for GzipPlugin {
    fn plugin_type(&self) -> &str {
        "gzip"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let skip = !accepts_encoding(&ctx, "gzip")
            || response_already_encoded(&ctx)
            || !self.types.matches(&ctx)
            || ctx.response.body.len() < self.min_length;

        if !skip {
            match content_codec::encode(&ContentEncoding::Gzip, &ctx.response.body, self.comp_level)
            {
                Ok(compressed) => {
                    ctx.response.body = compressed;
                    // Body-mutation convention: recompute length, declare the
                    // new encoding.
                    ctx.response.headers.remove("content-length");
                    ctx.response.headers.insert(
                        "content-encoding".to_string(),
                        vec![ContentEncoding::Gzip.header_value().to_string()],
                    );
                    if self.vary {
                        ctx.response
                            .headers
                            .entry("vary".to_string())
                            .or_default()
                            .push("Accept-Encoding".to_string());
                    }
                }
                Err(e) => {
                    tracing::warn!("gzip: compression skipped: {}", e);
                }
            }
        }

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    pub(crate) fn compressible_context(body: &[u8], content_type: &str) -> Context {
        let mut request_headers = HashMap::new();
        request_headers.insert(
            "accept-encoding".to_string(),
            vec!["gzip, deflate, br".to_string()],
        );
        let mut response_headers = HashMap::new();
        response_headers.insert("content-type".to_string(), vec![content_type.to_string()]);
        response_headers.insert("content-length".to_string(), vec![body.len().to_string()]);
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/test".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: request_headers,
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:12345".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 200,
                headers: response_headers,
                body: Bytes::copy_from_slice(body),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    const BODY: &[u8] = b"<html>a body long enough to clear the default min_length</html>";

    fn plugin(config: serde_json::Value) -> GzipPlugin {
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(config).expect("test config must be an object");
        GzipPlugin::from_config(&map).expect("config should be valid")
    }

    #[tokio::test]
    async fn test_gzip_compresses_matching_response() {
        let p = plugin(serde_json::json!({ "vary": true }));
        let ctx = compressible_context(BODY, "text/html; charset=utf-8");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();

        let headers = &out.context.response.headers;
        assert_eq!(
            headers.get("content-encoding"),
            Some(&vec!["gzip".to_string()])
        );
        assert!(!headers.contains_key("content-length"));
        assert_eq!(
            headers.get("vary"),
            Some(&vec!["Accept-Encoding".to_string()])
        );

        let decoded =
            content_codec::decode(&ContentEncoding::Gzip, &out.context.response.body).unwrap();
        assert_eq!(decoded.as_ref(), BODY);
    }

    #[tokio::test]
    async fn test_gzip_skips_when_client_does_not_accept() {
        let p = plugin(serde_json::json!({}));
        let mut ctx = compressible_context(BODY, "text/html");
        ctx.request.headers.remove("accept-encoding");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.body.as_ref(), BODY);
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));

        // Explicit q=0 also opts out.
        let p = plugin(serde_json::json!({}));
        let mut ctx = compressible_context(BODY, "text/html");
        ctx.request.headers.insert(
            "accept-encoding".to_string(),
            vec!["gzip;q=0, br".to_string()],
        );
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));
    }

    #[tokio::test]
    async fn test_gzip_skips_already_encoded_response() {
        let plain = Bytes::copy_from_slice(BODY);
        let pre_encoded = content_codec::encode(&ContentEncoding::Brotli, &plain, 6).unwrap();

        let p = plugin(serde_json::json!({}));
        let mut ctx = compressible_context(&pre_encoded, "text/html");
        ctx.response
            .headers
            .insert("content-encoding".to_string(), vec!["br".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();

        // Untouched: still brotli, not double-compressed.
        assert_eq!(
            out.context.response.headers.get("content-encoding"),
            Some(&vec!["br".to_string()])
        );
        assert_eq!(out.context.response.body, pre_encoded);
        assert!(out.context.response.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_gzip_type_and_length_gates() {
        // Non-matching content type.
        let p = plugin(serde_json::json!({}));
        let ctx = compressible_context(BODY, "application/json");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));

        // Missing content type never matches, even with types "*".
        let p = plugin(serde_json::json!({ "types": "*" }));
        let mut ctx = compressible_context(BODY, "text/html");
        ctx.response.headers.remove("content-type");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));

        // Wildcard matches any present content type.
        let p = plugin(serde_json::json!({ "types": "*" }));
        let ctx = compressible_context(BODY, "application/octet-stream");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(out
            .context
            .response
            .headers
            .contains_key("content-encoding"));

        // Body below min_length.
        let p = plugin(serde_json::json!({ "min_length": 1000 }));
        let ctx = compressible_context(BODY, "text/html");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));
    }

    #[test]
    fn test_gzip_config_validation() {
        for bad in [
            serde_json::json!({ "comp_level": 0 }),
            serde_json::json!({ "comp_level": 10 }),
            serde_json::json!({ "min_length": 0 }),
            serde_json::json!({ "types": [] }),
            serde_json::json!({ "types": [""] }),
            serde_json::json!({ "types": "text/html" }),
        ] {
            let map: HashMap<String, serde_json::Value> = serde_json::from_value(bad).unwrap();
            assert!(GzipPlugin::from_config(&map).is_err());
        }
    }

    #[test]
    fn test_accepts_encoding_parsing() {
        let mut ctx = compressible_context(BODY, "text/html");
        let cases = [
            ("gzip", true),
            ("*", true),
            ("br;q=0.8, gzip;q=0.5", true),
            ("GZIP", true),
            ("gzip;q=0", false),
            ("br", false),
            ("", false),
            ("*;q=0", false),
        ];
        for (header, expected) in cases {
            ctx.request
                .headers
                .insert("accept-encoding".to_string(), vec![header.to_string()]);
            assert_eq!(
                accepts_encoding(&ctx, "gzip"),
                expected,
                "header {header:?}"
            );
        }
    }
}
