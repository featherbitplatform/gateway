//! Consumer-label header injection plugin (`attach-consumer-label`).
//!
//! Copies the attached consumer's labels into upstream request headers so
//! backends can see who is calling without re-reading credentials. It does not
//! authenticate — an upstream auth node (e.g. `key-auth`) must have attached
//! the consumer identity first, writing `consumer.labels` into
//! `context.message`. Each label `k=v` becomes a request header
//! `<header_prefix><k>: v`. With no consumer attached it is a pure passthrough.
//! It never rejects.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Injects the consumer's labels as upstream request headers.
///
/// For every entry in `consumer.labels`, sets the request header
/// `<header_prefix><label_key>` to the label value (header names are
/// lowercased, matching how featherbit stores request headers). When no
/// consumer/labels are present the request passes through untouched.
pub struct AttachConsumerLabelPlugin {
    /// Prefix prepended to each label key to form the header name.
    header_prefix: String,
}

impl AttachConsumerLabelPlugin {
    /// Builds the plugin from node config. Never fails.
    ///
    /// Accepted keys:
    /// - `header_prefix` (string, default `"X-Consumer-"`): prefix prepended to
    ///   each label key to form the upstream header name. The combined name is
    ///   lowercased, so a `tier` label with prefix `X-Consumer-` becomes the
    ///   header `x-consumer-tier`.
    ///
    /// ```yaml
    /// type: attach-consumer-label
    /// config:
    ///   header_prefix: "X-Consumer-"
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let header_prefix = config
            .get("header_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("X-Consumer-")
            .to_string();

        Ok(Self { header_prefix })
    }
}

#[async_trait]
impl Plugin for AttachConsumerLabelPlugin {
    fn plugin_type(&self) -> &str {
        "attach-consumer-label"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Only act when a consumer with labels is attached; otherwise passthrough.
        if let Some(labels) = ctx
            .message
            .get("consumer.labels")
            .and_then(|v| v.as_object())
            .cloned()
        {
            for (key, value) in labels {
                if let Some(value) = value.as_str() {
                    let header = format!("{}{}", self.header_prefix, key).to_lowercase();
                    ctx.request.headers.insert(header, vec![value.to_string()]);
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

    fn ctx(labels: Option<serde_json::Value>) -> Context {
        let mut message = HashMap::new();
        if let Some(labels) = labels {
            message.insert("consumer.name".to_string(), serde_json::json!("alice"));
            message.insert("consumer.labels".to_string(), labels);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message,
            errors: Vec::new(),
        }
    }

    fn plugin(config: serde_json::Value) -> AttachConsumerLabelPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        AttachConsumerLabelPlugin::from_config(&map).unwrap()
    }

    #[tokio::test]
    async fn test_attaches_labels_with_default_prefix() {
        let p = plugin(serde_json::json!({}));
        let out = p
            .execute(
                ctx(Some(serde_json::json!({ "tier": "gold", "region": "eu" }))),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let headers = &out.context.request.headers;
        assert_eq!(
            headers.get("x-consumer-tier"),
            Some(&vec!["gold".to_string()])
        );
        assert_eq!(
            headers.get("x-consumer-region"),
            Some(&vec!["eu".to_string()])
        );
    }

    #[tokio::test]
    async fn test_custom_prefix() {
        let p = plugin(serde_json::json!({ "header_prefix": "X-Label-" }));
        let out = p
            .execute(
                ctx(Some(serde_json::json!({ "tier": "gold" }))),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            out.context.request.headers.get("x-label-tier"),
            Some(&vec!["gold".to_string()])
        );
    }

    #[tokio::test]
    async fn test_no_consumer_is_passthrough() {
        let p = plugin(serde_json::json!({}));
        let out = p.execute(ctx(None), &HashMap::new()).await.unwrap();
        // no consumer -> no headers injected, no error
        assert!(out
            .context
            .request
            .headers
            .keys()
            .all(|k| !k.starts_with("x-consumer-")));
    }
}
