//! Consumer identities and credentials.
//!
//! The featherbit analogue of APISIX's consumer concept: named API clients
//! declared in `gateway.yaml` (or via the Admin API), each carrying
//! per-auth-plugin credentials. Auth plugins configured with
//! `use_consumers: true` resolve the presented credential against the
//! [`ConsumerStore`] and, on a match, attach the consumer's identity to the
//! request via [`attach_consumer`] so downstream nodes (consumer-restriction,
//! loggers, rate limits keyed on `$consumer_name`) can act on it.
//!
//! The store is immutable once built; reloads build a fresh store and swap
//! it atomically (`ArcSwap` in `PluginResources`), so lookups on the request
//! path are lock-free.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::context::Context;

/// A consumer as declared in `gateway.yaml` under `consumers:`.
///
/// ```yaml
/// consumers:
///   - name: alice
///     group: partners
///     labels: { custom_id: "42", tier: gold }
///     credentials:
///       key-auth: { key: "s3cret" }
///       basic-auth: { username: alice, password: pw }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsumerConfig {
    /// Unique consumer name (the identity attached to matching requests).
    pub name: String,
    /// Optional consumer group, exposed as `consumer.group` / `$consumer_group_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Free-form labels; `custom_id` is special-cased into the
    /// `X-Consumer-Custom-ID` header (matching APISIX).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    /// Per-auth-plugin credentials, keyed by plugin type
    /// (e.g. `key-auth: {key}`, `basic-auth: {username, password}`).
    #[serde(default)]
    pub credentials: HashMap<String, serde_json::Value>,
}

/// A resolved consumer, shared read-only across requests.
#[derive(Debug)]
pub struct Consumer {
    pub name: String,
    pub group: Option<String>,
    pub labels: HashMap<String, String>,
    pub credentials: HashMap<String, serde_json::Value>,
}

/// Lock-free lookup structure over the declared consumers.
///
/// Indexes each auth type's primary credential value → consumer, so an auth
/// plugin resolves a presented credential in one hash lookup. The primary
/// credential field per auth type: `key-auth` → `key`, `basic-auth` →
/// `username`, `jwt-auth` → `key` (falling back to the consumer name). Auth
/// plugins that need more than the primary field (e.g. basic-auth's password
/// check) read the matched consumer's `credentials`.
#[derive(Default)]
pub struct ConsumerStore {
    by_name: HashMap<String, Arc<Consumer>>,
    /// auth type → credential value → consumer
    by_credential: HashMap<String, HashMap<String, Arc<Consumer>>>,
}

/// The credential field indexed per auth type.
fn primary_credential_field(auth_type: &str) -> &'static str {
    match auth_type {
        "basic-auth" => "username",
        "hmac-auth" => "access_key",
        _ => "key",
    }
}

impl ConsumerStore {
    /// Builds the store, failing fast on duplicate consumer names or on two
    /// consumers sharing the same credential value for the same auth type.
    pub fn from_config(consumers: &[ConsumerConfig]) -> Result<Self, String> {
        let mut store = Self::default();

        for config in consumers {
            if config.name.trim().is_empty() {
                return Err("consumer with empty name".to_string());
            }
            let consumer = Arc::new(Consumer {
                name: config.name.clone(),
                group: config.group.clone(),
                labels: config.labels.clone(),
                credentials: config.credentials.clone(),
            });

            if store
                .by_name
                .insert(consumer.name.clone(), consumer.clone())
                .is_some()
            {
                return Err(format!("duplicate consumer name '{}'", consumer.name));
            }

            for (auth_type, credential) in &config.credentials {
                let field = primary_credential_field(auth_type);
                let value = credential
                    .get(field)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!(
                            "consumer '{}': credential '{}' is missing string field '{}'",
                            consumer.name, auth_type, field
                        )
                    })?;

                let index = store.by_credential.entry(auth_type.clone()).or_default();
                if let Some(existing) = index.insert(value.to_string(), consumer.clone()) {
                    return Err(format!(
                        "consumers '{}' and '{}' share the same {} credential",
                        existing.name, consumer.name, auth_type
                    ));
                }
            }
        }

        Ok(store)
    }

    /// Resolves a presented credential value for an auth type.
    pub fn find_by_credential(&self, auth_type: &str, value: &str) -> Option<Arc<Consumer>> {
        self.by_credential.get(auth_type)?.get(value).cloned()
    }

    /// Looks a consumer up by name (used for `anonymous_consumer` fallback).
    pub fn get(&self, name: &str) -> Option<Arc<Consumer>> {
        self.by_name.get(name).cloned()
    }

    /// Number of declared consumers.
    #[allow(dead_code)] // store accessors; kept for completeness
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// True when no consumers are declared.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Attaches a matched consumer's identity to the request.
///
/// Writes the `context.message` keys downstream nodes read —
/// `consumer.name`, `consumer.group` (when set), `consumer.labels`,
/// `consumer.custom_id` (from `labels.custom_id`), `consumer.auth_type` —
/// and sets the upstream request headers `X-Consumer-Username` and
/// `X-Consumer-Custom-ID` (APISIX's upstream contract).
pub fn attach_consumer(ctx: &mut Context, consumer: &Consumer, auth_type: &str) {
    ctx.message.insert(
        "consumer.name".to_string(),
        serde_json::json!(consumer.name),
    );
    if let Some(ref group) = consumer.group {
        ctx.message
            .insert("consumer.group".to_string(), serde_json::json!(group));
    }
    if !consumer.labels.is_empty() {
        ctx.message.insert(
            "consumer.labels".to_string(),
            serde_json::json!(consumer.labels),
        );
    }
    ctx.message.insert(
        "consumer.auth_type".to_string(),
        serde_json::json!(auth_type),
    );

    ctx.request.headers.insert(
        "x-consumer-username".to_string(),
        vec![consumer.name.clone()],
    );
    if let Some(custom_id) = consumer.labels.get("custom_id") {
        ctx.message.insert(
            "consumer.custom_id".to_string(),
            serde_json::json!(custom_id),
        );
        ctx.request
            .headers
            .insert("x-consumer-custom-id".to_string(), vec![custom_id.clone()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: serde_json::Value) -> Vec<ConsumerConfig> {
        serde_json::from_value(json).expect("valid consumer config")
    }

    #[test]
    fn test_store_lookup_and_attach() {
        let store = ConsumerStore::from_config(&config(serde_json::json!([
            {
                "name": "alice",
                "group": "partners",
                "labels": { "custom_id": "42", "tier": "gold" },
                "credentials": {
                    "key-auth": { "key": "alice-key" },
                    "basic-auth": { "username": "alice", "password": "pw" }
                }
            },
            { "name": "bob", "credentials": { "key-auth": { "key": "bob-key" } } }
        ])))
        .unwrap();

        assert_eq!(store.len(), 2);
        let alice = store.find_by_credential("key-auth", "alice-key").unwrap();
        assert_eq!(alice.name, "alice");
        assert!(store.find_by_credential("key-auth", "nope").is_none());
        assert!(store.find_by_credential("jwt-auth", "alice-key").is_none());
        assert_eq!(
            store
                .find_by_credential("basic-auth", "alice")
                .unwrap()
                .name,
            "alice"
        );
        assert_eq!(store.get("bob").unwrap().name, "bob");

        // attach writes message keys + headers
        let mut ctx = crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: "/".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers: HashMap::new(),
            query_params: HashMap::new(),
            body: bytes::Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        });
        attach_consumer(&mut ctx, &alice, "key-auth");
        assert_eq!(
            ctx.message.get("consumer.name"),
            Some(&serde_json::json!("alice"))
        );
        assert_eq!(
            ctx.message.get("consumer.group"),
            Some(&serde_json::json!("partners"))
        );
        assert_eq!(
            ctx.message.get("consumer.custom_id"),
            Some(&serde_json::json!("42"))
        );
        assert_eq!(
            ctx.message.get("consumer.auth_type"),
            Some(&serde_json::json!("key-auth"))
        );
        assert_eq!(
            ctx.request.headers.get("x-consumer-username"),
            Some(&vec!["alice".to_string()])
        );
        assert_eq!(
            ctx.request.headers.get("x-consumer-custom-id"),
            Some(&vec!["42".to_string()])
        );
    }

    #[test]
    fn test_store_rejects_duplicates_and_malformed() {
        // duplicate name
        assert!(ConsumerStore::from_config(&config(serde_json::json!([
            { "name": "a" }, { "name": "a" }
        ])))
        .is_err());

        // shared credential value
        assert!(ConsumerStore::from_config(&config(serde_json::json!([
            { "name": "a", "credentials": { "key-auth": { "key": "same" } } },
            { "name": "b", "credentials": { "key-auth": { "key": "same" } } }
        ])))
        .is_err());

        // credential missing its primary field
        assert!(ConsumerStore::from_config(&config(serde_json::json!([
            { "name": "a", "credentials": { "key-auth": { "wrong": "x" } } }
        ])))
        .is_err());
    }
}
