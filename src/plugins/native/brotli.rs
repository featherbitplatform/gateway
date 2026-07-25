//! The `brotli` node — compresses the response body with brotli when the
//! client accepts it and the response matches the configured content types
//! and minimum size. Port of APISIX's `brotli` plugin (response-phase: place
//! after `upstream`, before `client`).
//!
//! Shares its gating helpers (`Accept-Encoding` parsing, content-type
//! matching, already-encoded detection) with the [`gzip`](super::gzip) node;
//! featherbit performs the `Accept-Encoding` check explicitly since there is
//! no nginx below the gateway to do it.
//!
//! Deviations from APISIX:
//! - `mode`, `lgwin`, `lgblock`, and `http_version` are not supported —
//!   [`content_codec`] uses generic mode with its own window size, and
//!   responses are fully buffered.
//! - `min_length` is compared against the actual buffered body length, not
//!   the upstream `Content-Length` header.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::native::gzip::{accepts_encoding, response_already_encoded, ContentTypes};
use crate::plugins::util::content_codec::{self, ContentEncoding};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Compresses `Context.response.body` with brotli, sets
/// `content-encoding: br`, drops the stale `content-length`, and downgrades
/// a strong `ETag` to a weak one (as APISIX does — the compressed bytes are
/// not byte-identical to the original representation). Skipped (pure
/// passthrough) when the client does not accept `br`, the response is
/// already content-encoded, the content type does not match, or the body is
/// shorter than `min_length`. This plugin never fails at execution time — a
/// codec error logs a warning and leaves the response untouched.
pub struct BrotliPlugin {
    types: ContentTypes,
    min_length: usize,
    comp_level: u32,
    vary: bool,
}

/// Mirrors APISIX's `weak_etag_header`: a strong quoted etag (`"abc"`)
/// becomes weak (`W/"abc"`), a weak etag is kept, and a non-standard
/// (unquoted) etag is dropped since it cannot be weakened.
fn weaken_etag(ctx: &mut Context) {
    let Some(etag) = ctx
        .response
        .headers
        .get("etag")
        .and_then(|v| v.first())
        .cloned()
    else {
        return;
    };

    if etag.starts_with("W/") {
        return; // already weak
    }
    if etag.len() >= 2 && etag.starts_with('"') && etag.ends_with('"') {
        ctx.response
            .headers
            .insert("etag".to_string(), vec![format!("W/{}", etag)]);
    } else {
        ctx.response.headers.remove("etag");
    }
}

impl BrotliPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all optional):
    /// - `types` (array of content types, or the string `"*"` for any;
    ///   default `["text/html"]`): response content types to compress. The
    ///   response `content-type` is compared with parameters (`;charset=...`)
    ///   stripped; a response without a content type is never compressed.
    /// - `min_length` (integer ≥ 1, default `20`): bodies shorter than this
    ///   are not compressed.
    /// - `comp_level` (integer 0-11, default `6`): brotli quality level
    ///   (matches ngx_brotli's `brotli_comp_level` default).
    /// - `vary` (bool, default `false`): when true, appends
    ///   `Vary: Accept-Encoding` to compressed responses.
    ///
    /// ```yaml
    /// type: brotli
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
            None => 6,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or("comp_level must be an integer".to_string())?;
                if n > 11 {
                    return Err(format!("comp_level must be within 0-11, got {}", n));
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
impl Plugin for BrotliPlugin {
    fn plugin_type(&self) -> &str {
        "brotli"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let skip = !accepts_encoding(&ctx, "br")
            || response_already_encoded(&ctx)
            || !self.types.matches(&ctx)
            || ctx.response.body.len() < self.min_length;

        if !skip {
            match content_codec::encode(
                &ContentEncoding::Brotli,
                &ctx.response.body,
                self.comp_level,
            ) {
                Ok(compressed) => {
                    ctx.response.body = compressed;
                    // Body-mutation convention: recompute length, declare the
                    // new encoding.
                    ctx.response.headers.remove("content-length");
                    ctx.response.headers.insert(
                        "content-encoding".to_string(),
                        vec![ContentEncoding::Brotli.header_value().to_string()],
                    );
                    weaken_etag(&mut ctx);
                    if self.vary {
                        ctx.response
                            .headers
                            .entry("vary".to_string())
                            .or_default()
                            .push("Accept-Encoding".to_string());
                    }
                }
                Err(e) => {
                    tracing::warn!("brotli: compression skipped: {}", e);
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

    const BODY: &[u8] = b"<html>a body long enough to clear the default min_length</html>";

    fn test_context(body: &[u8], content_type: &str, accept: &str) -> Context {
        let mut request_headers = HashMap::new();
        request_headers.insert("accept-encoding".to_string(), vec![accept.to_string()]);
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

    fn plugin(config: serde_json::Value) -> BrotliPlugin {
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(config).expect("test config must be an object");
        BrotliPlugin::from_config(&map).expect("config should be valid")
    }

    #[tokio::test]
    async fn test_brotli_compresses_matching_response() {
        let p = plugin(serde_json::json!({ "vary": true }));
        let ctx = test_context(BODY, "text/html; charset=utf-8", "gzip, br");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();

        let headers = &out.context.response.headers;
        assert_eq!(
            headers.get("content-encoding"),
            Some(&vec!["br".to_string()])
        );
        assert!(!headers.contains_key("content-length"));
        assert_eq!(
            headers.get("vary"),
            Some(&vec!["Accept-Encoding".to_string()])
        );

        let decoded =
            content_codec::decode(&ContentEncoding::Brotli, &out.context.response.body).unwrap();
        assert_eq!(decoded.as_ref(), BODY);
    }

    #[tokio::test]
    async fn test_brotli_skips_when_client_does_not_accept_br() {
        let p = plugin(serde_json::json!({}));
        let ctx = test_context(BODY, "text/html", "gzip, deflate");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.body.as_ref(), BODY);
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));

        // But a wildcard accept matches.
        let p = plugin(serde_json::json!({}));
        let ctx = test_context(BODY, "text/html", "*");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.response.headers.get("content-encoding"),
            Some(&vec!["br".to_string()])
        );
    }

    #[tokio::test]
    async fn test_brotli_skips_already_encoded_response() {
        let plain = Bytes::copy_from_slice(BODY);
        let pre_encoded = content_codec::encode(&ContentEncoding::Gzip, &plain, 6).unwrap();

        let p = plugin(serde_json::json!({}));
        let mut ctx = test_context(&pre_encoded, "text/html", "br");
        ctx.response
            .headers
            .insert("content-encoding".to_string(), vec!["gzip".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();

        assert_eq!(
            out.context.response.headers.get("content-encoding"),
            Some(&vec!["gzip".to_string()])
        );
        assert_eq!(out.context.response.body, pre_encoded);
        assert!(out.context.response.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_brotli_type_and_length_gates() {
        let p = plugin(serde_json::json!({}));
        let ctx = test_context(BODY, "application/json", "br");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));

        let p = plugin(serde_json::json!({ "min_length": 1000 }));
        let ctx = test_context(BODY, "text/html", "br");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));
    }

    #[tokio::test]
    async fn test_brotli_etag_handling() {
        // Strong etag downgraded to weak.
        let p = plugin(serde_json::json!({}));
        let mut ctx = test_context(BODY, "text/html", "br");
        ctx.response
            .headers
            .insert("etag".to_string(), vec!["\"abc123\"".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.response.headers.get("etag"),
            Some(&vec!["W/\"abc123\"".to_string()])
        );

        // Weak etag kept as-is.
        let p = plugin(serde_json::json!({}));
        let mut ctx = test_context(BODY, "text/html", "br");
        ctx.response
            .headers
            .insert("etag".to_string(), vec!["W/\"abc123\"".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.response.headers.get("etag"),
            Some(&vec!["W/\"abc123\"".to_string()])
        );

        // Non-standard (unquoted) etag dropped.
        let p = plugin(serde_json::json!({}));
        let mut ctx = test_context(BODY, "text/html", "br");
        ctx.response
            .headers
            .insert("etag".to_string(), vec!["abc123".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!out.context.response.headers.contains_key("etag"));
    }

    #[test]
    fn test_brotli_config_validation() {
        for bad in [
            serde_json::json!({ "comp_level": 12 }),
            serde_json::json!({ "min_length": 0 }),
            serde_json::json!({ "types": [] }),
        ] {
            let map: HashMap<String, serde_json::Value> = serde_json::from_value(bad).unwrap();
            assert!(BrotliPlugin::from_config(&map).is_err());
        }
        // comp_level 0 is valid for brotli (unlike gzip).
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({ "comp_level": 0 })).unwrap();
        assert!(BrotliPlugin::from_config(&map).is_ok());
    }
}
