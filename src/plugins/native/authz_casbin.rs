//! Embedded Casbin authorization plugin (`authz-casbin`).
//!
//! Ports Apache APISIX's `authz-casbin` plugin: an in-process ABAC/RBAC
//! authorization gate backed by the [`casbin`] crate. No network calls are
//! made — the model and policy are loaded from files on the gateway host or
//! from inline strings, and every request is evaluated locally against the
//! compiled [`Enforcer`].
//!
//! For each request the plugin derives a Casbin request tuple
//! `(subject, object, action)` where:
//! - **subject** is the authenticated consumer (`consumer.name` in
//!   `context.message`) when present, otherwise the value of a configured
//!   header (`username_header`, default `x-user`), otherwise `"anonymous"` —
//!   mirroring APISIX's `headers[conf.username] or "anonymous"`;
//! - **object** is the request path;
//! - **action** is the request method.
//!
//! `enforcer.enforce((sub, obj, act))` decides the outcome: `true` lets the
//! request continue through the **success** port; `false` rejects it with a
//! `403` routed through the **error** port (code `AUTHZ_CASBIN_DENIED`).
//!
//! ## Enforcer construction (blocking at load)
//!
//! Casbin's [`Enforcer::new`] is async, but plugin `from_config` is sync and
//! runs at config-load time. We build the enforcer on a dedicated short-lived
//! thread that owns a current-thread Tokio runtime and `block_on`s the async
//! construction. Running it on its own thread (rather than
//! `Handle::block_on`/`futures::executor::block_on` on the current thread)
//! avoids the "cannot start a runtime from within a runtime" panic when config
//! is loaded from inside an existing Tokio context, and still gives Casbin a
//! real Tokio runtime for the file-adapter's I/O. A bad model/policy fails
//! fast here, at load, never at request time. The built enforcer is wrapped in
//! an [`Arc`] and shared read-only across requests (`enforce` takes `&self`).

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

use casbin::{CoreApi, DefaultModel, Enforcer, FileAdapter, StringAdapter};

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Where the model and policy come from.
enum EnforcerSource {
    /// `model_path` + `policy_path`: files on the gateway host.
    Files {
        model_path: String,
        policy_path: String,
    },
    /// `model` + `policy`: inline Casbin config / CSV policy strings.
    Inline { model: String, policy: String },
}

/// Evaluates each request against a compiled Casbin model + policy.
pub struct AuthzCasbinPlugin {
    /// The compiled enforcer, shared read-only across requests.
    enforcer: Arc<Enforcer>,
    /// Lowercased header the subject falls back to when no consumer identity
    /// is attached.
    username_header: String,
}

impl AuthzCasbinPlugin {
    /// Builds the plugin from node config, compiling the enforcer eagerly.
    ///
    /// Accepted keys (one of the two source pairs is required, matching
    /// APISIX's `oneOf`):
    /// - `model_path` (string) + `policy_path` (string): load the Casbin model
    ///   and policy from files on the gateway host.
    /// - `model` (string) + `policy` (string): inline Casbin model config and
    ///   CSV policy text (loaded via Casbin's in-memory string adapter).
    /// - `username_header` (string, default `"x-user"`): header the subject is
    ///   read from when no consumer identity (`consumer.name`) is present;
    ///   lowercased for the case-insensitive header map.
    ///
    /// Fails fast if neither source pair is fully provided or if Casbin rejects
    /// the model/policy.
    ///
    /// ```yaml
    /// # inline model + policy
    /// - id: authz
    ///   type: authz-casbin
    ///   config:
    ///     username_header: x-user
    ///     model: |
    ///       [request_definition]
    ///       r = sub, obj, act
    ///       [policy_definition]
    ///       p = sub, obj, act
    ///       [role_definition]
    ///       g = _, _
    ///       [policy_effect]
    ///       e = some(where (p.eft == allow))
    ///       [matchers]
    ///       m = g(r.sub, p.sub) && r.obj == p.obj && r.act == p.act
    ///     policy: |
    ///       p, admin, /data, GET
    ///       g, alice, admin
    /// ```
    ///
    /// ```yaml
    /// # model + policy files on the gateway host
    /// - id: authz
    ///   type: authz-casbin
    ///   config:
    ///     model_path: /etc/featherbit/model.conf
    ///     policy_path: /etc/featherbit/policy.csv
    ///     username_header: x-user
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        _resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let get_str = |key: &str| {
            config
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };

        let source = match (
            get_str("model_path"),
            get_str("policy_path"),
            get_str("model"),
            get_str("policy"),
        ) {
            (Some(model_path), Some(policy_path), _, _) => EnforcerSource::Files {
                model_path,
                policy_path,
            },
            (_, _, Some(model), Some(policy)) => EnforcerSource::Inline { model, policy },
            _ => {
                return Err(
                    "authz-casbin requires either 'model_path' + 'policy_path' or \
                     'model' + 'policy'"
                        .to_string(),
                )
            }
        };

        let username_header = config
            .get("username_header")
            .and_then(|v| v.as_str())
            .unwrap_or("x-user")
            .to_lowercase();

        let enforcer = build_enforcer(source)?;

        Ok(Self {
            enforcer: Arc::new(enforcer),
            username_header,
        })
    }

    /// Resolves the Casbin subject: the attached consumer identity if present,
    /// else the configured header, else `"anonymous"`.
    fn subject(&self, ctx: &Context) -> String {
        if let Some(name) = ctx.message.get("consumer.name").and_then(|v| v.as_str()) {
            return name.to_string();
        }
        ctx.request
            .headers
            .get(&self.username_header)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_else(|| "anonymous".to_string())
    }

    /// Builds the 403 denial carrying the context so the graph engine routes
    /// through the error port.
    fn deny(ctx: Context) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 403;
        ctx.response.body = Bytes::from(r#"{"message":"Access Denied"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "AUTHZ_CASBIN_DENIED".to_string(),
                message: "Access denied by Casbin policy".to_string(),
                metadata: HashMap::new(),
            },
        })
    }
}

/// Compiles the enforcer on a dedicated thread with its own Tokio runtime.
///
/// See the module docs for why this indirection exists. Any construction
/// failure (bad model config, missing/invalid policy file, malformed CSV) is
/// surfaced as an `Err(String)` at config load.
fn build_enforcer(source: EnforcerSource) -> Result<Enforcer, String> {
    std::thread::spawn(move || -> Result<Enforcer, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("failed to build enforcer runtime: {e}"))?;

        rt.block_on(async move {
            match source {
                EnforcerSource::Files {
                    model_path,
                    policy_path,
                } => {
                    let model = DefaultModel::from_file(&model_path)
                        .await
                        .map_err(|e| format!("failed to load Casbin model '{model_path}': {e}"))?;
                    let adapter = FileAdapter::new(policy_path.clone());
                    Enforcer::new(model, adapter)
                        .await
                        .map_err(|e| format!("failed to build Casbin enforcer: {e}"))
                }
                EnforcerSource::Inline { model, policy } => {
                    let model = DefaultModel::from_str(&model)
                        .await
                        .map_err(|e| format!("failed to parse inline Casbin model: {e}"))?;
                    let adapter = StringAdapter::new(policy);
                    Enforcer::new(model, adapter)
                        .await
                        .map_err(|e| format!("failed to build Casbin enforcer: {e}"))
                }
            }
        })
    })
    .join()
    .map_err(|_| "Casbin enforcer build thread panicked".to_string())?
}

#[async_trait]
impl Plugin for AuthzCasbinPlugin {
    fn plugin_type(&self) -> &str {
        "authz-casbin"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let subject = self.subject(&ctx);
        let object = ctx.request.path.clone();
        let action = ctx.request.method.clone();

        match self.enforcer.enforce((subject, object, action)) {
            Ok(true) => Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            }),
            Ok(false) => Self::deny(ctx),
            Err(e) => {
                // An evaluation error (should not happen with a valid model)
                // is treated as a denial, carrying detail in the error record.
                let mut ctx = ctx;
                ctx.response.status_code = 403;
                ctx.response.body = Bytes::from(r#"{"message":"Access Denied"}"#);
                ctx.response.headers.insert(
                    "content-type".to_string(),
                    vec!["application/json".to_string()],
                );
                Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "AUTHZ_CASBIN_DENIED".to_string(),
                        message: format!("Casbin enforcement error: {e}"),
                        metadata: HashMap::new(),
                    },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    const RBAC_MODEL: &str = "\
[request_definition]
r = sub, obj, act
[policy_definition]
p = sub, obj, act
[role_definition]
g = _, _
[policy_effect]
e = some(where (p.eft == allow))
[matchers]
m = g(r.sub, p.sub) && r.obj == p.obj && r.act == p.act
";

    const RBAC_POLICY: &str = "\
p, admin, /data, GET
g, alice, admin
";

    fn inline_config() -> HashMap<String, serde_json::Value> {
        let mut config = HashMap::new();
        config.insert("model".to_string(), serde_json::json!(RBAC_MODEL));
        config.insert("policy".to_string(), serde_json::json!(RBAC_POLICY));
        config
    }

    fn ctx_for(method: &str, path: &str, user: Option<&str>, consumer: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(u) = user {
            headers.insert("x-user".to_string(), vec![u.to_string()]);
        }
        let mut message = HashMap::new();
        if let Some(c) = consumer {
            message.insert("consumer.name".to_string(), serde_json::json!(c));
        }
        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: path.to_string(),
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
            message,
            errors: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_allow_via_role_header_subject() {
        let plugin =
            AuthzCasbinPlugin::from_config(&inline_config(), &PluginResources::empty()).unwrap();
        // alice -> admin, admin can GET /data
        let out = plugin
            .execute(
                ctx_for("GET", "/data", Some("alice"), None),
                &HashMap::new(),
            )
            .await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn test_allow_via_consumer_subject() {
        let plugin =
            AuthzCasbinPlugin::from_config(&inline_config(), &PluginResources::empty()).unwrap();
        // consumer identity wins over header; alice is admin
        let out = plugin
            .execute(
                ctx_for("GET", "/data", Some("nobody"), Some("alice")),
                &HashMap::new(),
            )
            .await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn test_deny_unknown_subject() {
        let plugin =
            AuthzCasbinPlugin::from_config(&inline_config(), &PluginResources::empty()).unwrap();
        // bob has no role -> denied
        let err = plugin
            .execute(ctx_for("GET", "/data", Some("bob"), None), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 403);
        assert_eq!(err.error.code, "AUTHZ_CASBIN_DENIED");
    }

    #[tokio::test]
    async fn test_deny_wrong_action() {
        let plugin =
            AuthzCasbinPlugin::from_config(&inline_config(), &PluginResources::empty()).unwrap();
        // admin can GET /data but not POST it
        let err = plugin
            .execute(
                ctx_for("POST", "/data", Some("alice"), None),
                &HashMap::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "AUTHZ_CASBIN_DENIED");
    }

    #[test]
    fn test_requires_a_source_pair() {
        assert!(
            AuthzCasbinPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err()
        );
        // model without policy is incomplete
        let mut config = HashMap::new();
        config.insert("model".to_string(), serde_json::json!(RBAC_MODEL));
        assert!(AuthzCasbinPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_bad_model_fails_fast() {
        let mut config = HashMap::new();
        config.insert("model".to_string(), serde_json::json!("not a valid model"));
        config.insert("policy".to_string(), serde_json::json!(""));
        assert!(AuthzCasbinPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }
}
