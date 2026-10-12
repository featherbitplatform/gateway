//! OpenAPI 3.1 description of the Admin API.
//!
//! The spec is assembled from the same [`OpenApiRouter`]s that serve the
//! endpoints (`utoipa-axum`), so a route cannot exist without a spec entry
//! and vice versa. Every handler carries a `#[utoipa::path]` annotation.
//!
//! The gateway's config types (`RouteConfig`, `PolicyConfig`, ...) already
//! derive [`schemars::JsonSchema`] for the MCP tools, so their schemas are
//! not re-derived for utoipa: [`schemars_bridge!`] gives each one a
//! placeholder [`utoipa::ToSchema`] impl that only fixes its component name,
//! and [`spec`] swaps the placeholders for the schemars output (JSON Schema
//! 2020-12, which is OpenAPI 3.1's schema dialect).
//!
//! The rendered spec is served at `GET /api/openapi.json`, committed at
//! `openapi/admin-api.json` (kept current by a test — regenerate with
//! `UPDATE_OPENAPI=1 cargo test openapi`), and published on the docs site.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use axum::Json;
use schemars::generate::SchemaSettings;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::openapi::{RefOr, Schema};
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::state::SharedState;

/// Name of the Basic Auth security scheme every authed operation requires.
pub(crate) const BASIC_AUTH: &str = "basicAuth";

/// Title given to the placeholder schemas [`schemars_bridge!`] emits; [`spec`]
/// replaces every one of them, and a test asserts none survives.
const BRIDGE_PLACEHOLDER: &str = "__schemars_bridge__";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "featherbit Admin API",
        description = "REST API of the featherbit gateway's admin listener: CRUD for routes, \
policies, supernodes, plugin configs, consumers and stores; server-side sessions; ACME \
certificates; debug traces and the plugin sandbox; status, health and metrics.\n\n\
All endpoints require HTTP Basic authentication except `/healthz` and `/readyz`. The MCP \
endpoint (`admin.mcp.path`, default `/mcp`) is a Streamable HTTP MCP transport with its own \
bearer tokens and is not described here.",
        license(name = "Apache-2.0", identifier = "Apache-2.0"),
    ),
    servers((url = "http://localhost:9090", description = "Default admin listener")),
    modifiers(&SecurityAddon),
    security(("basicAuth" = [])),
    tags(
        (name = "routes", description = "Route table: match rules bound to a policy, in evaluation order."),
        (name = "policies", description = "Node-graph policies, policy validation, and the node-type catalog."),
        (name = "supernodes", description = "Reusable named subgraphs inlined into policies."),
        (name = "plugin-configs", description = "Named, typed plugin config profiles referenced via `config_ref`."),
        (name = "consumers", description = "Named API clients with per-auth-plugin credentials."),
        (name = "stores", description = "Named redis/valkey connections referenced by plugin config."),
        (name = "sessions", description = "Server-side sessions held in redis-backed stores."),
        (name = "acme", description = "Automatically managed (ACME) certificates."),
        (name = "cache", description = "proxy-cache purging."),
        (name = "debug", description = "Policy-execution traces and the plugin sandbox (requires `debug.enabled`)."),
        (name = "authoring", description = "Context-var and env-var catalogs used by editors and agents."),
        (name = "mcp", description = "MCP server status and the precompiled agent prompts."),
        (name = "operations", description = "Health, readiness, status, metrics, config export and reload."),
    ),
)]
struct ApiDoc;

/// Registers the Basic Auth security scheme (`utoipa` has no attribute form
/// for components' `securitySchemes`).
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                BASIC_AUTH,
                SecurityScheme::Http(Http::new(HttpAuthScheme::Basic)),
            );
    }
}

/// The documented admin API: every Basic-Auth-protected endpoint (plus the
/// auth-exempt probes) with its spec. `admin::build_router` splits it into
/// the axum router it serves; [`spec`] keeps the OpenAPI half.
pub(crate) fn api_router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(super::routes::router())
        .merge(super::acme::router())
        .merge(super::cache::router())
        .merge(super::policies::router())
        .merge(super::plugin_configs::router())
        .merge(super::supernodes::router())
        .merge(super::consumers::router())
        .merge(super::mcp::router())
        .merge(super::sessions::router())
        .merge(super::status::router())
        .merge(super::stores::router())
        .merge(super::debug::router())
        .merge(super::vars::router())
        .merge(super::env_vars::router())
        .routes(routes!(get_openapi))
}

/// The complete OpenAPI document as JSON, with the schemars-derived
/// component schemas filled in. Built once per process.
pub fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| {
        let (_, openapi) = api_router().split_for_parts();
        let mut doc = serde_json::to_value(&openapi).expect("OpenAPI document serializes");
        let schemas = doc
            .pointer_mut("/components/schemas")
            .and_then(Value::as_object_mut)
            .expect("the admin API references at least one component schema");
        for (name, schema) in bridged_definitions() {
            schemas.insert(name, schema);
        }
        schemas.sort_keys();
        add_unauthorized_responses(&mut doc);
        doc
    })
}

/// The Basic Auth middleware answers `401` before any handler runs, so no
/// `#[utoipa::path]` can declare it: add it to every operation that inherits
/// the global security requirement (the probes override it with `[]`).
fn add_unauthorized_responses(doc: &mut Value) {
    doc["components"]["responses"]["Unauthorized"] = serde_json::json!({
        "description": "Missing or wrong Basic Auth credentials.",
        "headers": {
            "WWW-Authenticate": {
                "description": "`Basic realm=\"featherbit admin\"` — omitted when the request carries `X-Featherbit-Client` (the web UI), so browsers don't open their native login dialog.",
                "schema": {"type": "string"},
            },
        },
    });
    let paths = doc["paths"].as_object_mut().expect("paths");
    for op in paths
        .values_mut()
        .filter_map(Value::as_object_mut)
        .flat_map(|item| item.values_mut())
    {
        if op.get("security").is_none() {
            op["responses"]["401"] =
                serde_json::json!({"$ref": "#/components/responses/Unauthorized"});
        }
    }
}

/// `GET /api/openapi.json` — this OpenAPI document.
#[utoipa::path(
    get,
    path = "/api/openapi.json",
    tag = "operations",
    responses((status = 200, description = "The OpenAPI 3.1 document describing this API.", body = Object)),
)]
async fn get_openapi() -> Json<&'static Value> {
    Json(spec())
}

/// Implements [`ToSchema`] for types that already derive
/// [`schemars::JsonSchema`]: the component name is the schemars name and the
/// schema a placeholder, replaced in [`spec`] by the definitions
/// [`bridged_definitions`] collects for every listed type.
macro_rules! schemars_bridge {
    ($($ty:ty),* $(,)?) => {
        $(
            impl utoipa::PartialSchema for $ty {
                fn schema() -> RefOr<Schema> {
                    placeholder()
                }
            }

            impl ToSchema for $ty {
                fn name() -> Cow<'static, str> {
                    <$ty as schemars::JsonSchema>::schema_name()
                }
            }
        )*

        /// JSON Schema 2020-12 definitions for every bridged type and every
        /// type they reference, with `$ref`s already pointing into
        /// `#/components/schemas/`.
        fn bridged_definitions() -> serde_json::Map<String, Value> {
            let mut generator = SchemaSettings::draft2020_12()
                .with(|s| {
                    s.definitions_path = "/components/schemas".into();
                    s.meta_schema = None;
                })
                .into_generator();
            $( generator.subschema_for::<$ty>(); )*
            generator.take_definitions(true)
        }

        /// Component names of the bridged types (for the tests).
        #[cfg(test)]
        fn bridged_names() -> Vec<Cow<'static, str>> {
            vec![$( <$ty as schemars::JsonSchema>::schema_name() ),*]
        }
    };
}

fn placeholder() -> RefOr<Schema> {
    Schema::Object(
        utoipa::openapi::ObjectBuilder::new()
            .title(Some(BRIDGE_PLACEHOLDER))
            .build(),
    )
    .into()
}

schemars_bridge!(
    crate::config::GatewayConfig,
    crate::config::RouteConfig,
    crate::config::PolicyConfig,
    crate::config::NodeConfig,
    crate::config::SupernodeConfig,
    crate::config::PluginConfigDef,
    crate::config::StoreConfig,
    crate::consumers::ConsumerConfig,
);

/// `{"status": "..."}` — the acknowledgement most mutations return.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub(crate) struct StatusResponse {
    /// What happened, e.g. `created`, `updated`, `deleted`.
    pub status: String,
}

/// `{"error": "..."}` — the body of every 4xx/5xx the admin API produces
/// itself.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub(crate) struct ErrorResponse {
    /// Human-readable reason, or a short code such as `not_found`.
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMITTED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi/admin-api.json");

    /// The committed `openapi/admin-api.json` (published on the docs site)
    /// must match what the code generates. `UPDATE_OPENAPI=1` rewrites it.
    #[test]
    fn committed_openapi_spec_is_current() {
        let generated = serde_json::to_string_pretty(spec()).unwrap() + "\n";
        if std::env::var_os("UPDATE_OPENAPI").is_some() {
            std::fs::create_dir_all(std::path::Path::new(COMMITTED).parent().unwrap()).unwrap();
            std::fs::write(COMMITTED, &generated).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(COMMITTED)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert!(
            committed == generated,
            "openapi/admin-api.json is stale; regenerate it with `UPDATE_OPENAPI=1 cargo test openapi`"
        );
    }

    #[test]
    fn every_placeholder_schema_is_replaced() {
        let text = spec().to_string();
        assert!(
            !text.contains(BRIDGE_PLACEHOLDER),
            "a bridged schema was not replaced"
        );
        let schemas = spec()["components"]["schemas"].as_object().unwrap();
        for name in bridged_names() {
            assert!(
                schemas.contains_key(name.as_ref()),
                "missing component {name}"
            );
        }
    }

    /// Every `$ref` is an in-document JSON pointer that resolves.
    #[test]
    fn every_ref_resolves() {
        fn walk(v: &Value, bad: &mut Vec<String>) {
            match v {
                Value::Object(map) => {
                    if let Some(Value::String(r)) = map.get("$ref") {
                        match r.strip_prefix('#') {
                            Some(pointer) if spec().pointer(pointer).is_some() => {}
                            _ => bad.push(r.clone()),
                        }
                    }
                    map.values().for_each(|v| walk(v, bad));
                }
                Value::Array(items) => items.iter().for_each(|v| walk(v, bad)),
                _ => {}
            }
        }
        let mut bad = Vec::new();
        walk(spec(), &mut bad);
        assert!(bad.is_empty(), "dangling $refs: {bad:?}");
    }

    /// A bare `.route(...)` on an `OpenApiRouter` serves the endpoint but
    /// leaves it out of the spec; every admin endpoint must go through
    /// `.routes(routes!(...))`. Test modules may build throwaway routers.
    #[test]
    fn no_admin_endpoint_bypasses_the_spec() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/admin");
        let route = regex::Regex::new(r"\.route\(\s*\x22").unwrap();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let non_test = source.split("#[cfg(test)]").next().unwrap();
            assert!(
                !route.is_match(non_test),
                "{} registers an endpoint with `.route(...)`; use `.routes(routes!(...))` and #[utoipa::path]",
                path.display()
            );
        }
    }

    /// `operationId`s default to the handler's fn name, and several modules
    /// share names like `status` — those need an explicit `operation_id`.
    #[test]
    fn operation_ids_are_unique() {
        let mut seen = std::collections::HashMap::new();
        for (path, item) in spec()["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                let id = op["operationId"].as_str().unwrap().to_string();
                let at = format!("{} {path}", method.to_uppercase());
                if let Some(prev) = seen.insert(id.clone(), at.clone()) {
                    panic!("operationId `{id}` used by both {prev} and {at}");
                }
            }
        }
    }

    /// Every operation is tagged, summarized, and — unless it is one of the
    /// auth-exempt probes — inherits the Basic Auth requirement.
    #[test]
    fn every_operation_is_tagged_and_secured() {
        let paths = spec()["paths"].as_object().unwrap();
        for (path, item) in paths {
            for (method, op) in item.as_object().unwrap() {
                let at = format!("{} {path}", method.to_uppercase());
                assert!(
                    op["tags"].as_array().is_some_and(|t| !t.is_empty()),
                    "{at} has no tag"
                );
                assert!(
                    op["summary"].as_str().is_some_and(|s| !s.is_empty()),
                    "{at} has no summary"
                );
                let exempt = path == "/healthz" || path == "/readyz";
                assert_eq!(
                    op.get("security").is_some(),
                    exempt,
                    "{at}: security override mismatch"
                );
            }
        }
    }
}
