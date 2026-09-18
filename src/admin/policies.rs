//! Admin API endpoints for policy CRUD, plus catalogs of available plugin
//! types and on-disk scripts. Policy mutations rewrite the in-memory gateway
//! config and trigger validation + graph recompilation via `SharedState::reload`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::config::PolicyConfig;
use crate::state::SharedState;

/// Builds the router for `/api/policies`, `/api/plugins`, and `/api/scripts`.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/policies", get(list_policies))
        .route("/api/policies/validate", post(validate_policy))
        .route(
            "/api/policies/{name}",
            get(get_policy).put(update_policy).delete(delete_policy),
        )
        .route("/api/plugins", get(list_plugin_types))
        .route("/api/scripts", get(list_scripts))
}

/// `GET /api/policies` — returns all configured policies as a JSON array.
async fn list_policies(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(&gw.policies).into_response()
}

/// `GET /api/policies/{name}` — returns the named policy as JSON.
///
/// Errors: `404 Not Found` if no policy with that name exists.
async fn get_policy(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.policies.iter().find(|p| p.name == name) {
        Some(policy) => Json(policy).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

/// `PUT /api/policies/{name}` — upserts a policy from the JSON body (the
/// path name overrides any name in the body), then revalidates and
/// recompiles all route graphs. Returns `{"status": "updated"}` on success.
///
/// Errors: `400 Bad Request` if the resulting configuration fails
/// validation/recompilation (the previous compiled routes stay active).
async fn update_policy(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut policy): Json<PolicyConfig>,
) -> impl IntoResponse {
    policy.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.policies.iter_mut().find(|p| p.name == name) {
            *existing = policy;
        } else {
            candidate.policies.push(policy);
        }
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "updated"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// `DELETE /api/policies/{name}` — removes the named policy, then
/// revalidates and recompiles. Returns `{"status": "deleted"}` on success.
///
/// Errors: `404 Not Found` if the policy does not exist; `400 Bad Request`
/// if recompilation fails (e.g. a route still references the policy).
async fn delete_policy(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        let before = candidate.policies.len();
        candidate.policies.retain(|p| p.name != name);
        if candidate.policies.len() == before {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "deleted"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// `POST /api/policies/validate` — validates + compiles a policy against the
/// live supernodes, plugin configs and stores, without persisting it. Body is
/// the policy definition itself (`{"nodes": [...], "edges": [...], ...}`); a
/// `name` is optional and, if absent, is not saved anywhere. Mirrors the MCP
/// `validate_policy` tool so a human in the UI and an agent driving the
/// gateway see the same verdict.
///
/// Response: `{"valid": bool, "errors": [...], "buffering": [...]}`. A
/// policy that forces one or more upstreams to buffer instead of stream is
/// still `valid` — `buffering` is informational, not an error. Each entry
/// names the blocked upstream and the node responsible:
/// `{"upstream": "up", "blocked_by": "rw", "node_type": "response-rewrite"}`.
async fn validate_policy(
    State(state): State<Arc<SharedState>>,
    Json(mut raw): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(obj) = raw.as_object_mut() {
        obj.entry("name")
            .or_insert_with(|| serde_json::Value::String("unsaved-policy".to_string()));
    }
    let policy: PolicyConfig = match serde_json::from_value(raw) {
        Ok(p) => p,
        Err(e) => {
            return Json(serde_json::json!({
                "valid": false,
                "errors": [e.to_string()],
                "buffering": []
            }))
            .into_response();
        }
    };

    let (supernodes, plugin_configs) = {
        let gw = state.gateway.read().await;
        (gw.supernodes.clone(), gw.plugin_configs.clone())
    };

    let compiled = crate::graph::prepare_policy(policy, &supernodes, &plugin_configs)
        .and_then(|p| crate::graph::compile_policy(&p, state.resources.clone()));

    let (errors, buffering, cache_pairs): (Vec<String>, serde_json::Value, serde_json::Value) =
        match compiled {
            Ok(graph) => (
                Vec::new(),
                serde_json::to_value(graph.buffering_reasons())
                    .expect("BufferingReason always serializes"),
                serde_json::to_value(graph.cache_pair_warnings())
                    .expect("CachePairWarning always serializes"),
            ),
            Err(e) => (
                e.split("; ").map(str::to_string).collect(),
                serde_json::json!([]),
                serde_json::json!([]),
            ),
        };

    Json(serde_json::json!({
        "valid": errors.is_empty(),
        "errors": errors,
        "buffering": buffering,
        "cache_pairs": cache_pairs
    }))
    .into_response()
}

/// `GET /api/scripts` — lists scripted-plugin files found in the `plugins/`
/// directory next to the config directory (currently `.lua` only).
///
/// A missing or unreadable directory yields an empty list rather than an error.
///
/// ```json
/// { "scripts": [ { "name": "my_filter", "file": "plugins/my_filter.lua", "runtime": "lua" } ] }
/// ```
async fn list_scripts(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let mut scripts = Vec::new();

    // Scan the config directory for a "plugins" subdirectory
    let config_path = state
        .config_path
        .as_deref()
        .unwrap_or(std::path::Path::new("config"));
    let plugins_dir = config_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("plugins");

    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("");
            let runtime = match ext {
                "lua" => "lua",
                _ => continue,
            };
            scripts.push(serde_json::json!({
                "name": name,
                "file": path.to_string_lossy(),
                "runtime": runtime,
            }));
        }
    }

    Json(serde_json::json!({ "scripts": scripts }))
}

/// `GET /api/plugins` — returns the static catalog of node/plugin types
/// (type id + human-readable description) that the UI's node-graph editor
/// offers in its palette. Always `200 OK`.
///
/// Every type registered in [`crate::plugins::create_plugin`] appears here, in
/// the same category order the plugin reference uses. Types the UI has no
/// config form for still work: `NodeInspector` falls back to a raw-JSON config
/// editor. Keep this in sync with the factory — `test_catalog_covers_factory`
/// fails if a registered type is missing.
async fn list_plugin_types() -> impl IntoResponse {
    Json(serde_json::json!({ "plugins": plugin_catalog() }))
}

/// The palette catalog: `(type, description)` for every registered node type.
pub(crate) fn plugin_catalog() -> Vec<serde_json::Value> {
    const CATALOG: &[(&str, &str)] = &[
        // Structural & core proxy
        ("listener", "Route entry point — receives incoming request"),
        ("client", "Route exit point — sends response to client"),
        (
            "condition",
            "Branch the policy on boolean conditions (true/false ports)",
        ),
        ("upstream", "Forward to a load-balanced backend pool"),
        ("proxy-rewrite", "Rewrite path, add/remove headers"),
        (
            "response-rewrite",
            "Rewrite response status, headers, and body",
        ),
        (
            "body-transformer",
            "Rewrite request/response JSON bodies via templates",
        ),
        (
            "set-vars",
            "Derive variables from the context (templates, JSONPath, regex captures)",
        ),
        (
            "degraphql",
            "Expose a REST endpoint backed by a GraphQL upstream",
        ),
        ("redirect", "HTTP redirect, or force HTTP→HTTPS"),
        ("echo", "Wrap or replace the response body (demo/testing)"),
        ("gzip", "Compress the response body with gzip"),
        ("brotli", "Compress the response body with Brotli"),
        ("request-id", "Attach a unique request-id header"),
        (
            "real-ip",
            "Recover the client IP from a trusted proxy header",
        ),
        // Error handling & mocking
        ("error-handler", "Custom error responses"),
        (
            "error-page",
            "Replace 404/500/502/503 bodies with configured pages",
        ),
        (
            "exit-transformer",
            "Remap status and body of gateway-generated exits",
        ),
        (
            "mocking",
            "Respond with a configured mock instead of proxying",
        ),
        // Security & access control
        ("cors", "CORS header management"),
        ("csrf", "Double-submit CSRF token validation"),
        ("ip-restriction", "Allow/deny by IP/CIDR"),
        ("ua-restriction", "Allow/deny by User-Agent regex"),
        ("referer-restriction", "Allow/deny by Referer host"),
        ("uri-blocker", "Block requests matching URI regex rules"),
        ("request-size-limit", "Reject oversized requests"),
        (
            "request-validation",
            "Validate headers/body against JSON Schema",
        ),
        (
            "data-mask",
            "Mask sensitive fields in bodies, headers, query",
        ),
        // Traffic control
        ("rate-limit", "Token bucket rate limiting"),
        ("limit-count", "Fixed-window request-count limiting"),
        (
            "limit-conn",
            "Concurrent-request limiting (acquire/release pair)",
        ),
        ("api-breaker", "Circuit breaker on unhealthy upstreams"),
        ("traffic-split", "Weighted / conditional traffic steering"),
        ("proxy-mirror", "Fire-and-forget clone to a shadow upstream"),
        (
            "proxy-cache",
            "Cache upstream responses (lookup/store pair)",
        ),
        ("fault-injection", "Inject delays and abort responses"),
        (
            "workflow",
            "Ordered rules — reject or rate-limit the first match",
        ),
        (
            "traffic-label",
            "Tag matching requests with headers and labels",
        ),
        // Authentication & consumers
        ("key-auth", "API key authentication"),
        ("basic-auth", "HTTP Basic authentication"),
        ("jwt-auth", "JWT validation"),
        (
            "hmac-auth",
            "HMAC request signing (access key / secret key)",
        ),
        ("jwe-decrypt", "Decrypt a JWE token into a forwarded header"),
        (
            "multi-auth",
            "Chain auth plugins — accept the first that succeeds",
        ),
        ("ldap-auth", "Authenticate Basic credentials against LDAP"),
        (
            "consumer-restriction",
            "Allow/deny by consumer name or group",
        ),
        ("acl", "Allow/deny by consumer group"),
        (
            "attach-consumer-label",
            "Copy consumer labels into upstream headers",
        ),
        // External auth & authorization
        (
            "forward-auth",
            "Delegate the decision to an external HTTP service",
        ),
        ("opa", "Delegate authorization to Open Policy Agent"),
        ("authz-casbin", "Embedded Casbin RBAC/ABAC enforcement"),
        ("authz-keycloak", "Keycloak UMA permission check"),
        (
            "authz-casdoor",
            "Casdoor introspection or interactive OAuth login",
        ),
        (
            "openid-connect",
            "OIDC bearer validation or interactive login",
        ),
        ("cas-auth", "CAS ticket validation or interactive SSO login"),
        ("wolf-rbac", "Wolf RBAC token check"),
        (
            "dingtalk-auth",
            "DingTalk code/token validation with optional session mode",
        ),
        (
            "feishu-auth",
            "Feishu/Lark code/token validation with optional session mode",
        ),
        // Serverless & FaaS
        (
            "serverless-pre-function",
            "Run inline Lua before the upstream",
        ),
        (
            "serverless-post-function",
            "Run inline Lua after the upstream",
        ),
        (
            "oas-validator",
            "Validate requests against an OpenAPI 3 spec",
        ),
        ("aws-lambda", "Invoke an AWS Lambda function"),
        ("azure-functions", "Invoke an Azure Function"),
        ("openwhisk", "Invoke an Apache OpenWhisk action"),
        ("openfunction", "Invoke an OpenFunction function"),
        // Observability & logging
        ("logging", "Structured access logging"),
        ("http-logger", "Ship logs to an HTTP endpoint"),
        ("tcp-logger", "Ship logs over a raw TCP socket"),
        ("udp-logger", "Ship logs over a raw UDP socket"),
        ("syslog", "Ship logs via syslog (RFC 5424)"),
        ("file-logger", "Append logs to a local file"),
        (
            "error-log-logger",
            "Ship request-level errors to a TCP sink",
        ),
        ("elasticsearch-logger", "Bulk-index logs into Elasticsearch"),
        ("clickhouse-logger", "Insert logs into ClickHouse"),
        ("loki-logger", "Push logs to Grafana Loki"),
        ("splunk-hec-logging", "Ship logs to Splunk HEC"),
        ("datadog", "Emit DogStatsD metrics to the Datadog agent"),
        ("loggly", "Ship logs to SolarWinds Loggly"),
        ("google-cloud-logging", "Ship logs to Google Cloud Logging"),
        ("sls-logger", "Ship logs to Alibaba Cloud SLS"),
        ("tencent-cloud-cls", "Ship logs to Tencent Cloud CLS"),
        ("skywalking-logger", "Ship logs to Apache SkyWalking"),
        ("lago", "Meter requests as Lago billing events"),
        // Tracing & metrics
        ("prometheus", "Per-consumer request counters"),
        ("opentelemetry", "OTLP/HTTP trace export (W3C traceparent)"),
        ("zipkin", "Zipkin v2 trace export (B3 propagation)"),
        ("skywalking", "SkyWalking segment export (sw8 propagation)"),
        // Scripting
        ("script", "Custom plugin logic written in Lua"),
        // Policy state
        (
            "store-get",
            "Read a key from a shared store into context.message (miss port when absent)",
        ),
        (
            "store-set",
            "Write a key into a shared store, with an optional TTL",
        ),
        (
            "store-delete",
            "Remove a key from a shared store (idempotent)",
        ),
        (
            "store-incr",
            "Atomically increment a counter in a shared store (TTL set at creation)",
        ),
    ];

    CATALOG
        .iter()
        .map(|(t, d)| {
            // Infallible: the drift tests keep CATALOG, KNOWN_PLUGIN_TYPES,
            // the factory, and the port registry in lockstep — see
            // `test_catalog_covers_factory` / `test_catalog_has_no_unknown_types`
            // below, `plugins::tests::test_known_plugin_types_matches_factory`,
            // and `plugins::ports::tests::test_every_known_type_has_a_valid_spec`.
            let spec = crate::plugins::port_spec(t).expect("catalog type is registered");
            serde_json::json!({
                "type": t,
                "description": d,
                "ports": serde_json::to_value(spec).unwrap()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state(gateway_yaml: &str) -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from(
                    "gateway.yaml",
                ))),
            )
            .unwrap(),
        )
    }

    fn app(state: Arc<SharedState>) -> Router {
        router().with_state(state)
    }

    /// Drives `POST /api/policies/validate` against a fresh in-memory state
    /// (no routes/policies configured) and returns the parsed JSON body.
    async fn validate_policy_json(body: serde_json::Value) -> serde_json::Value {
        let state = test_state("{}");
        let resp = app(state)
            .oneshot(
                Request::post("/api/policies/validate")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Validating a policy whose upstream cannot stream must say so, naming the
    /// node responsible. An operator who wires gzip onto an SSE route learns it
    /// here rather than from "notifications stopped working".
    #[tokio::test]
    async fn test_validate_reports_forced_buffering() {
        let body = validate_policy_json(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "rw", "type": "response-rewrite",
                  "config": { "filters": [{ "regex": "a", "replace": "b" }] } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "rw.in" },
                { "from": "rw.success", "to": "client.in" }
            ]
        }))
        .await;

        assert_eq!(body["valid"], serde_json::json!(true));
        assert_eq!(body["buffering"][0]["upstream"], serde_json::json!("up"));
        assert_eq!(body["buffering"][0]["blocked_by"], serde_json::json!("rw"));
    }

    /// Extracts the node types registered in `create_plugin`'s match arms by
    /// reading its source. The factory is a `match` on `&str`, so there is no
    /// runtime list to enumerate -- and calling it for every type would need
    /// each plugin's required config.
    fn factory_types() -> Vec<String> {
        include_str!("../plugins/mod.rs")
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix('"')?;
                let (name, tail) = rest.split_once('"')?;
                tail.trim_start()
                    .starts_with("=>")
                    .then(|| name.to_string())
            })
            .collect()
    }

    /// The palette catalog drifting behind the factory is a silent failure: the
    /// plugin still works in YAML, but the UI simply never offers it. That is
    /// exactly what happened when the APISIX plugins landed and the catalog
    /// kept advertising the original 13.
    #[test]
    fn test_catalog_covers_factory() {
        let catalog: Vec<String> = plugin_catalog()
            .iter()
            .map(|p| p["type"].as_str().unwrap().to_string())
            .collect();

        let missing: Vec<_> = factory_types()
            .iter()
            .filter(|t| !catalog.contains(t))
            .cloned()
            .collect();
        assert!(
            missing.is_empty(),
            "registered plugins missing from the UI catalog: {missing:?}"
        );
    }

    /// The reverse drift: a catalog entry the factory does not know would put a
    /// node in the palette that fails policy compilation the moment it is used.
    #[test]
    fn test_catalog_has_no_unknown_types() {
        let factory = factory_types();
        let unknown: Vec<_> = plugin_catalog()
            .iter()
            .map(|p| p["type"].as_str().unwrap().to_string())
            .filter(|t| !factory.contains(t))
            .collect();
        assert!(
            unknown.is_empty(),
            "catalog advertises types create_plugin cannot build: {unknown:?}"
        );
    }

    #[test]
    fn test_catalog_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for p in plugin_catalog() {
            let t = p["type"].as_str().unwrap().to_string();
            assert!(seen.insert(t.clone()), "duplicate catalog entry: {t}");
        }
    }

    /// Extracts the plugin types that have a visual identity, by reading the
    /// UI's `pluginMeta` map. Its entries are `type: { color, icon }` lines.
    fn types_with_an_icon() -> Vec<String> {
        include_str!("../../ui/src/pluginMeta.tsx")
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                // Map entries declare both a color and an icon. This also matches
                // getPluginMeta's fallback `|| { color: ..., icon: Box }`, whose
                // "key" is not a bare plugin name -- the charset check drops it.
                if !line.contains("color:") || !line.contains("icon:") {
                    return None;
                }
                let (key, _) = line.split_once(':')?;
                let key = key.trim().trim_matches('\'');
                let plausible = !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                plausible.then(|| key.to_string())
            })
            .collect()
    }

    /// A plugin with no `pluginMeta` entry still appears in the palette, but
    /// renders with the neutral fallback cube instead of its own icon and color
    /// -- a silent cosmetic regression that is easy to ship and hard to notice.
    #[test]
    fn test_every_catalog_plugin_has_an_icon() {
        let with_icon = types_with_an_icon();
        let missing: Vec<_> = plugin_catalog()
            .iter()
            .map(|p| p["type"].as_str().unwrap().to_string())
            .filter(|t| !with_icon.contains(t))
            .collect();
        assert!(
            missing.is_empty(),
            "plugins with no icon in ui/src/pluginMeta.tsx (they fall back to the generic cube): {missing:?}"
        );
    }

    /// The node types the UI's palette groups into categories: every
    /// plugin-shaped single-quoted token in `pluginCategories.ts`, so both
    /// one-per-line and inline `types: ['a', 'b']` arrays are seen.
    fn types_in_a_palette_category() -> Vec<String> {
        include_str!("../../ui/src/pluginCategories.ts")
            .split('\'')
            // Odd-indexed pieces are the quoted tokens.
            .skip(1)
            .step_by(2)
            .filter(|t| {
                !t.is_empty()
                    && t.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            })
            .map(str::to_string)
            .collect()
    }

    /// A plugin missing from `pluginCategories.ts` lands in the drawer's
    /// synthesized "Other" group instead of its section: it still works, but
    /// the palette taxonomy and the docs sidebar have silently diverged.
    #[test]
    fn test_every_catalog_plugin_is_in_a_palette_category() {
        let categorised = types_in_a_palette_category();
        let missing: Vec<_> = plugin_catalog()
            .iter()
            .map(|p| p["type"].as_str().unwrap().to_string())
            // Fixed graph endpoints are drawn, never picked from the palette;
            // `script` has its own drawer section fed by the script files.
            .filter(|t| !matches!(t.as_str(), "listener" | "client" | "script"))
            .filter(|t| !categorised.contains(t))
            .collect();
        assert!(
            missing.is_empty(),
            "plugins missing from ui/src/pluginCategories.ts (they fall into the palette's 'Other' group): {missing:?}"
        );
    }

    /// Every plugin needs its reference page: it is what `get_node_type`
    /// returns to an agent over MCP (embedded by `src/mcp/docs.rs`) and what
    /// the docs site links. A type with no page leaves the agent guessing at
    /// config keys.
    #[test]
    fn test_every_catalog_plugin_has_a_docs_page() {
        let pages: std::collections::HashSet<String> = std::fs::read_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/website/docs/reference/plugins"
        ))
        .expect("plugin docs directory")
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_string_lossy().to_string();
            name.strip_suffix(".md").map(str::to_string)
        })
        .collect();
        let missing: Vec<_> = plugin_catalog()
            .iter()
            .map(|p| p["type"].as_str().unwrap().to_string())
            // listener/client share one page; index.md is the catalog itself.
            .filter(|t| t != "listener" && t != "client")
            .filter(|t| !pages.contains(t))
            .collect();
        assert!(
            missing.is_empty(),
            "plugins with no website/docs/reference/plugins/<type>.md page (get_node_type returns no docs): {missing:?}"
        );
    }

    /// A docs page that no sidebar lists is unreachable on the docs site.
    #[test]
    fn test_every_plugin_docs_page_is_in_the_sidebar() {
        let sidebar = include_str!("../../website/sidebars.ts");
        let missing: Vec<_> = plugin_catalog()
            .iter()
            .map(|p| p["type"].as_str().unwrap().to_string())
            .filter(|t| t != "listener" && t != "client")
            .filter(|t| !sidebar.contains(&format!("reference/plugins/{t}'")))
            .collect();
        assert!(
            missing.is_empty(),
            "plugin docs pages missing from website/sidebars.ts: {missing:?}"
        );
    }

    /// Every catalog entry carries its port spec, and outcome ports match the registry.
    #[test]
    fn test_catalog_entries_carry_ports() {
        for p in plugin_catalog() {
            let ty = p["type"].as_str().unwrap();
            let ports = &p["ports"];
            assert!(
                ports["outputs"].is_array(),
                "'{ty}' catalog entry lacks ports.outputs"
            );
            let spec = crate::plugins::port_spec(ty).unwrap();
            let names: Vec<&str> = ports["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["name"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                spec.outputs.iter().map(|o| o.name).collect::<Vec<_>>()
            );
        }
        // spot-check kind serialization
        let cors = plugin_catalog()
            .into_iter()
            .find(|p| p["type"] == "cors")
            .unwrap();
        assert_eq!(cors["ports"]["outputs"][1]["kind"], "outcome");
    }
}
