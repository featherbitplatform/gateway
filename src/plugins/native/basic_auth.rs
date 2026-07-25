//! HTTP Basic authentication plugin (`basic-auth`).
//!
//! Validates the `Authorization: Basic ...` header against a static user map
//! and/or the shared consumer store, and rejects unauthenticated requests with
//! a 401 challenge so the graph engine routes through the node's error port.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

use crate::consumers::attach_consumer;
use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Authenticates requests using HTTP Basic credentials checked against a
/// configured username/password map and/or the consumer store.
///
/// With inline `users`, a matching `username:password` pair simply lets the
/// request continue. With `use_consumers: true`, the username is resolved
/// against the gateway's `consumers:` section (their
/// `basic-auth: {username, password}` credentials) and the presented password
/// is checked against the matched consumer's stored password; on success the
/// consumer's identity is attached to the request (`consumer.*` keys in
/// `context.message` plus `X-Consumer-*` headers) for downstream nodes. Both
/// sources may be enabled together — inline users are checked first. For
/// back-compat the authenticated username is always written to
/// `context.message["user"]`. On failure the request is rejected with a 401
/// response carrying a `WWW-Authenticate: Basic` challenge.
pub struct BasicAuthPlugin {
    /// Username -> plaintext password map the credentials are checked against.
    users: HashMap<String, String>,
    /// Realm advertised in the `WWW-Authenticate` challenge header.
    realm: String,
    /// When true, credentials are also resolved against the consumer store.
    use_consumers: bool,
    /// Consumer attached when no credential matches (instead of rejecting).
    anonymous_consumer: Option<String>,
    /// When true, the `Authorization` header is removed before proxying.
    hide_credentials: bool,
    resources: Arc<PluginResources>,
}

impl BasicAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `users` (object, optional): map of username to plaintext password.
    /// - `use_consumers` (bool, default `false`): also resolve credentials
    ///   against the gateway's `consumers:` section and attach the matched
    ///   consumer.
    /// - At least one of `users` / `use_consumers` must be provided.
    /// - `realm` (string, default `"gateway"`): realm used in the
    ///   `WWW-Authenticate` challenge.
    /// - `anonymous_consumer` (string, optional): consumer name attached when
    ///   no credential matches, instead of rejecting (APISIX semantics).
    /// - `hide_credentials` (bool, default `false`): strip the `Authorization`
    ///   header before proxying upstream.
    ///
    /// ```yaml
    /// type: basic-auth
    /// config:
    ///   use_consumers: true
    ///   realm: internal-api
    ///   hide_credentials: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let users = parse_users(config.get("users"))?;

        let use_consumers = config
            .get("use_consumers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if users.is_empty() && !use_consumers {
            return Err("basic-auth plugin requires 'users' or 'use_consumers: true'".to_string());
        }

        let realm = config
            .get("realm")
            .and_then(|v| v.as_str())
            .unwrap_or("gateway")
            .to_string();

        let anonymous_consumer = config
            .get("anonymous_consumer")
            .and_then(|v| v.as_str())
            .map(String::from);

        let hide_credentials = config
            .get("hide_credentials")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            users,
            realm,
            use_consumers,
            anonymous_consumer,
            hide_credentials,
            resources: resources.clone(),
        })
    }

    /// Builds the 401 rejection: sets a JSON error body plus the
    /// `WWW-Authenticate` challenge on the response and returns a
    /// `PluginExecutionError` (code `UNAUTHORIZED`) carrying the context so
    /// the graph engine routes through the error port.
    fn reject(&self, ctx: Context) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body =
            Bytes::from(r#"{"error": "unauthorized", "message": "Invalid credentials"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        ctx.response.headers.insert(
            "www-authenticate".to_string(),
            vec![format!("Basic realm=\"{}\"", self.realm)],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "UNAUTHORIZED".to_string(),
                message: "Invalid credentials".to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Removes the `Authorization` header (per `hide_credentials`).
    fn strip_credential(&self, ctx: &mut Context) {
        ctx.request.headers.remove("authorization");
    }
}

#[async_trait]
impl Plugin for BasicAuthPlugin {
    fn plugin_type(&self) -> &str {
        "basic-auth"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let auth_header = ctx
            .request
            .headers
            .get("authorization")
            .and_then(|v| v.first())
            .cloned();

        let credentials = match auth_header {
            Some(h) if h.starts_with("Basic ") => STANDARD
                .decode(&h[6..])
                .ok()
                .and_then(|b| String::from_utf8(b).ok()),
            _ => None,
        };

        let parsed = credentials
            .as_deref()
            .and_then(|c| c.split_once(':'))
            .map(|(u, p)| (u.to_string(), p.to_string()));

        // Inline users first.
        if let Some((ref username, ref password)) = parsed {
            if let Some(expected) = self.users.get(username) {
                if expected == password {
                    if self.hide_credentials {
                        self.strip_credential(&mut ctx);
                    }
                    ctx.message.insert(
                        "user".to_string(),
                        serde_json::Value::String(username.clone()),
                    );
                    return Ok(PluginOutput {
                        context: ctx,
                        named_outputs: HashMap::new(),
                    });
                }
            }
        }

        // Consumer store: resolve by username, then verify the password
        // against the matched consumer's stored basic-auth credential.
        if self.use_consumers {
            if let Some((ref username, ref password)) = parsed {
                let store = self.resources.consumers.load();
                if let Some(consumer) = store.find_by_credential("basic-auth", username) {
                    let expected = consumer
                        .credentials
                        .get("basic-auth")
                        .and_then(|c| c.get("password"))
                        .and_then(|v| v.as_str());
                    if expected == Some(password.as_str()) {
                        if self.hide_credentials {
                            self.strip_credential(&mut ctx);
                        }
                        attach_consumer(&mut ctx, &consumer, "basic-auth");
                        ctx.message.insert(
                            "user".to_string(),
                            serde_json::Value::String(username.clone()),
                        );
                        return Ok(PluginOutput {
                            context: ctx,
                            named_outputs: HashMap::new(),
                        });
                    }
                }
            }
        }

        // Anonymous fallback.
        if let Some(ref name) = self.anonymous_consumer {
            let store = self.resources.consumers.load();
            if let Some(consumer) = store.get(name) {
                attach_consumer(&mut ctx, &consumer, "basic-auth");
                ctx.message.insert(
                    "user".to_string(),
                    serde_json::Value::String(consumer.name.clone()),
                );
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
        }

        self.reject(ctx)
    }
}

/// Parses the `users` config into a `username -> password` map, accepting either
/// shape:
///
/// - a **map** (hand-written YAML): `users: { alice: s3cret, bob: hunter2 }`
/// - an **array of objects** (what the web UI's node editor emits for this
///   field): `users: [{ username: alice, password: s3cret }, ...]`
///
/// The web UI serializes repeated-object fields as arrays, so accepting only the
/// map shape means a `basic-auth` node configured with users in the editor is
/// rejected at save time. `proxy-rewrite`'s `add_headers` already handles both
/// shapes for the same reason; this mirrors it. Blank rows left in the editor
/// (empty username) are skipped.
fn parse_users(raw: Option<&serde_json::Value>) -> Result<HashMap<String, String>, String> {
    let Some(raw) = raw else {
        return Ok(HashMap::new());
    };

    match raw {
        serde_json::Value::Object(m) => Ok(m
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect()),
        serde_json::Value::Array(items) => {
            let mut users = HashMap::new();
            for item in items {
                let obj = item.as_object().ok_or(
                    "basic-auth: 'users' entries must be objects with 'username' and 'password'",
                )?;
                let username = obj.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if username.trim().is_empty() {
                    continue; // blank row left in the UI editor
                }
                let password = obj
                    .get("password")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                users.insert(username.to_string(), password);
            }
            Ok(users)
        }
        _ => Err(
            "basic-auth: 'users' must be a map or an array of {username, password} objects"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consumers::{ConsumerConfig, ConsumerStore};
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx_with_auth(header: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(h) = header {
            headers.insert("authorization".to_string(), vec![h.to_string()]);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers,
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
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// `Authorization: Basic <base64(user:pass)>` header value.
    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", STANDARD.encode(format!("{}:{}", user, pass)))
    }

    fn resources_with_consumers() -> Arc<PluginResources> {
        let resources = PluginResources::empty();
        let consumers: Vec<ConsumerConfig> = serde_json::from_value(serde_json::json!([
            {
                "name": "alice",
                "credentials": { "basic-auth": { "username": "alice", "password": "pw" } }
            },
            { "name": "guest" }
        ]))
        .unwrap();
        resources
            .consumers
            .store(Arc::new(ConsumerStore::from_config(&consumers).unwrap()));
        resources
    }

    fn inline_config() -> HashMap<String, serde_json::Value> {
        let mut config = HashMap::new();
        config.insert(
            "users".to_string(),
            serde_json::json!({ "alice": "s3cret", "bob": "hunter2" }),
        );
        config
    }

    #[test]
    fn test_requires_users_or_consumers() {
        assert!(BasicAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
    }

    /// The web UI's node editor serializes `users` as an array of
    /// `{username, password}` objects. Regression guard: this used to be rejected
    /// as empty (the parser only accepted a map), so a basic-auth node configured
    /// with users in the UI could not be saved.
    #[test]
    fn test_parse_users_accepts_both_shapes() {
        // Map shape (hand-written YAML).
        let map = parse_users(Some(
            &serde_json::json!({ "alice": "s3cret", "bob": "hunter2" }),
        ))
        .unwrap();
        assert_eq!(map.get("alice"), Some(&"s3cret".to_string()));
        assert_eq!(map.get("bob"), Some(&"hunter2".to_string()));

        // Array shape (the web UI editor).
        let arr = parse_users(Some(&serde_json::json!([
            { "username": "alice", "password": "s3cret" },
            { "username": "bob", "password": "hunter2" },
        ])))
        .unwrap();
        assert_eq!(arr, map);

        // Blank rows left in the editor are skipped, not treated as a user.
        let with_blank = parse_users(Some(&serde_json::json!([
            { "username": "alice", "password": "s3cret" },
            { "username": "", "password": "" },
        ])))
        .unwrap();
        assert_eq!(with_blank.len(), 1);
        assert!(with_blank.contains_key("alice"));

        // A wrong scalar shape is a clear error, not a silent empty map.
        assert!(parse_users(Some(&serde_json::json!("nope"))).is_err());
        assert!(parse_users(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_ui_array_users_authenticate() {
        // The exact shape the UI saves must produce a working plugin.
        let mut config = HashMap::new();
        config.insert(
            "users".to_string(),
            serde_json::json!([{ "username": "alice", "password": "s3cret" }]),
        );
        let plugin = BasicAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap();

        let ok = plugin
            .execute(
                ctx_with_auth(Some(&basic("alice", "s3cret"))),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            ok.context.message.get("user"),
            Some(&serde_json::json!("alice"))
        );

        assert!(plugin
            .execute(
                ctx_with_auth(Some(&basic("alice", "wrong"))),
                &HashMap::new()
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_inline_users_still_work() {
        let plugin =
            BasicAuthPlugin::from_config(&inline_config(), &PluginResources::empty()).unwrap();

        let ok = plugin
            .execute(
                ctx_with_auth(Some(&basic("alice", "s3cret"))),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            ok.context.message.get("user"),
            Some(&serde_json::json!("alice"))
        );

        // wrong password
        assert!(plugin
            .execute(
                ctx_with_auth(Some(&basic("alice", "nope"))),
                &HashMap::new()
            )
            .await
            .is_err());
        // missing header
        assert!(plugin
            .execute(ctx_with_auth(None), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_reject_sets_challenge() {
        let mut config = inline_config();
        config.insert("realm".to_string(), serde_json::json!("internal-api"));
        let plugin = BasicAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap();

        let err = plugin
            .execute(ctx_with_auth(None), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "UNAUTHORIZED");
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(
            err.context.response.headers.get("www-authenticate"),
            Some(&vec!["Basic realm=\"internal-api\"".to_string()])
        );
    }

    #[tokio::test]
    async fn test_consumer_credentials_attach_identity() {
        let resources = resources_with_consumers();
        let mut config = HashMap::new();
        config.insert("use_consumers".to_string(), serde_json::json!(true));
        config.insert("hide_credentials".to_string(), serde_json::json!(true));
        let plugin = BasicAuthPlugin::from_config(&config, &resources).unwrap();

        let out = plugin
            .execute(ctx_with_auth(Some(&basic("alice", "pw"))), &HashMap::new())
            .await
            .unwrap();
        let ctx = out.context;
        assert_eq!(
            ctx.message.get("consumer.name"),
            Some(&serde_json::json!("alice"))
        );
        assert_eq!(ctx.message.get("user"), Some(&serde_json::json!("alice")));
        assert_eq!(
            ctx.request.headers.get("x-consumer-username"),
            Some(&vec!["alice".to_string()])
        );
        // hide_credentials stripped the Authorization header
        assert!(!ctx.request.headers.contains_key("authorization"));

        // wrong password against a known consumer is rejected
        assert!(plugin
            .execute(
                ctx_with_auth(Some(&basic("alice", "wrong"))),
                &HashMap::new()
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_anonymous_consumer_fallback() {
        let resources = resources_with_consumers();
        let mut config = HashMap::new();
        config.insert("use_consumers".to_string(), serde_json::json!(true));
        config.insert("anonymous_consumer".to_string(), serde_json::json!("guest"));
        let plugin = BasicAuthPlugin::from_config(&config, &resources).unwrap();

        let out = plugin
            .execute(ctx_with_auth(None), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            out.context.message.get("consumer.name"),
            Some(&serde_json::json!("guest"))
        );
    }
}
