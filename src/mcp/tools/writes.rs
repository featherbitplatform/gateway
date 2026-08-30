//! Write tools. Every mutation builds a full candidate `GatewayConfig`, then
//! either validates it (`dry_run`) or commits it through the configured
//! `ConfigStore` — exactly the path the Admin API takes.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::config::{
    GatewayConfig, PluginConfigDef, PolicyConfig, RouteConfig, StoreConfig, SupernodeConfig,
};
use crate::state::SharedState;

/// Like [`super::parse_payload`], but the `name` argument wins over (or fills
/// in) any `name` inside the payload before deserializing — payloads
/// commonly omit `name` and rely on the tool argument, exactly as the REST
/// `PUT /api/...{name}` handlers let the path segment override the body.
fn parse_named<T: serde::de::DeserializeOwned>(
    definition: Value,
    name: &str,
    what: &str,
) -> Result<T, ToolError> {
    let mut value: Value = match definition {
        Value::String(yaml) => serde_yaml::from_str(&yaml)
            .map_err(|e| ToolError::invalid_input(format!("{what}: YAML did not parse: {e}")))?,
        other => other,
    };
    match value.as_object_mut() {
        Some(obj) => {
            obj.insert("name".to_string(), Value::String(name.to_string()));
        }
        None => {
            return Err(ToolError::invalid_input(format!(
                "{what}: definition must be a JSON/YAML object"
            )))
        }
    }
    serde_json::from_value(value)
        .map_err(|e| ToolError::invalid_input(format!("{what}: JSON did not deserialize: {e}")))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PutArgs {
    /// Resource name; overrides any `name` inside the payload.
    pub name: String,
    /// The definition, as a JSON object or a YAML document string.
    pub definition: Value,
    /// Validate the whole resulting config without applying it. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteArgs {
    /// Resource name.
    pub name: String,
    /// Validate the whole resulting config without applying it. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

/// Applies `mutate` to a clone of the live config, then validates or commits.
pub async fn commit_candidate(
    state: &SharedState,
    dry_run: bool,
    changed: Vec<String>,
    mutate: impl FnOnce(&mut GatewayConfig) -> Result<(), ToolError>,
) -> Result<Value, ToolError> {
    let mut candidate = state.gateway.read().await.clone();
    mutate(&mut candidate)?;
    if dry_run {
        // `validate_gateway_dry` restores `resources.stores` on both success
        // and failure, so a dry run never durably swaps in the candidate
        // store registry (see its doc comment on `SharedState`).
        state
            .validate_gateway_dry(&candidate)
            .map_err(|e| ToolError::invalid_config(vec![e]))?;
    } else {
        // Validate first so a store failure after a clean validation is
        // reported as store_error, not invalid_config; `commit` re-validates
        // and applies, so the candidate registry it leaves live is the one
        // actually being committed.
        state
            .validate_gateway(&candidate)
            .map_err(|e| ToolError::invalid_config(vec![e]))?;
        state
            .config_store
            .clone()
            .commit(state, candidate)
            .await
            .map_err(ToolError::store_error)?;
    }
    Ok(serde_json::json!({ "applied": !dry_run, "dry_run": dry_run, "changed": changed }))
}

fn upsert<T>(items: &mut Vec<T>, item: T, same: impl Fn(&T) -> bool) {
    if let Some(existing) = items.iter_mut().find(|i| same(i)) {
        *existing = item;
    } else {
        items.push(item);
    }
}

fn remove<T>(
    items: &mut Vec<T>,
    what: &str,
    name: &str,
    same: impl Fn(&T) -> bool,
) -> Result<(), ToolError> {
    let before = items.len();
    items.retain(|i| !same(i));
    if items.len() == before {
        Err(ToolError::not_found(what, name))
    } else {
        Ok(())
    }
}

pub async fn put_route(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let route: RouteConfig = parse_named(a.definition, &a.name, "route")?;
    commit_candidate(state, a.dry_run, vec![format!("route:{}", a.name)], |gw| {
        upsert(&mut gw.routes, route, |r| r.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_route(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("route:{}", a.name)], |gw| {
        remove(&mut gw.routes, "route", &a.name, |r| r.name == a.name)
    })
    .await
}

pub async fn put_policy(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let policy: PolicyConfig = parse_named(a.definition, &a.name, "policy")?;
    commit_candidate(state, a.dry_run, vec![format!("policy:{}", a.name)], |gw| {
        upsert(&mut gw.policies, policy, |p| p.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_policy(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("policy:{}", a.name)], |gw| {
        remove(&mut gw.policies, "policy", &a.name, |p| p.name == a.name)
    })
    .await
}

pub async fn put_supernode(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let sn: SupernodeConfig = parse_named(a.definition, &a.name, "supernode")?;
    commit_candidate(
        state,
        a.dry_run,
        vec![format!("supernode:{}", a.name)],
        |gw| {
            upsert(&mut gw.supernodes, sn, |s| s.name == a.name);
            Ok(())
        },
    )
    .await
}

pub async fn delete_supernode(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(
        state,
        a.dry_run,
        vec![format!("supernode:{}", a.name)],
        |gw| {
            remove(&mut gw.supernodes, "supernode", &a.name, |s| {
                s.name == a.name
            })
        },
    )
    .await
}

pub async fn put_plugin_config(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let pc: PluginConfigDef = parse_named(a.definition, &a.name, "plugin config")?;
    commit_candidate(
        state,
        a.dry_run,
        vec![format!("plugin_config:{}", a.name)],
        |gw| {
            upsert(&mut gw.plugin_configs, pc, |p| p.name == a.name);
            Ok(())
        },
    )
    .await
}

pub async fn delete_plugin_config(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(
        state,
        a.dry_run,
        vec![format!("plugin_config:{}", a.name)],
        |gw| {
            remove(&mut gw.plugin_configs, "plugin config", &a.name, |p| {
                p.name == a.name
            })
        },
    )
    .await
}

pub async fn put_store(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let store: StoreConfig = parse_named(a.definition, &a.name, "store")?;
    commit_candidate(state, a.dry_run, vec![format!("store:{}", a.name)], |gw| {
        upsert(&mut gw.stores, store, |s| s.name == a.name);
        Ok(())
    })
    .await
}

pub async fn delete_store(state: &SharedState, a: DeleteArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec![format!("store:{}", a.name)], |gw| {
        let referrers = crate::admin::stores::store_referrers(gw, &a.name);
        if !referrers.is_empty() {
            let mut e = ToolError::invalid_config(referrers);
            e.message = format!("store '{}' is still referenced", a.name);
            e.hint = Some("remove or repoint the referrers first".into());
            return Err(e);
        }
        remove(&mut gw.stores, "store", &a.name, |s| s.name == a.name)
    })
    .await
}

pub async fn reload_config(state: &SharedState) -> Result<Value, ToolError> {
    state
        .reload_from_disk()
        .await
        .map_err(ToolError::store_error)?;
    Ok(serde_json::json!({ "status": "reloaded" }))
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    const NEW_POLICY: &str = "nodes:\n  - {id: l, type: listener}\n  - {id: e, type: echo, config: {body: hi}}\n  - {id: c, type: client}\nedges:\n  - {from: l.out, to: e.in}\n  - {from: e.out, to: c.in}\n";

    #[tokio::test]
    async fn dry_run_validates_without_applying() {
        let s = state("{}", ECHO_GATEWAY);
        let v = call(
            &s,
            "put_policy",
            obj(serde_json::json!({"name": "p2", "definition": NEW_POLICY, "dry_run": true})),
        )
        .await
        .unwrap();
        assert_eq!(v["applied"], false);
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["changed"][0], "policy:p2");
        assert!(s
            .gateway
            .read()
            .await
            .policies
            .iter()
            .all(|p| p.name != "p2"));
    }

    #[tokio::test]
    async fn put_then_route_then_delete_are_applied_and_hot_compiled() {
        let s = state("{}", ECHO_GATEWAY);
        let v = call(
            &s,
            "put_policy",
            obj(serde_json::json!({"name": "p2", "definition": NEW_POLICY})),
        )
        .await
        .unwrap();
        assert_eq!(v["applied"], true);
        assert!(s
            .gateway
            .read()
            .await
            .policies
            .iter()
            .any(|p| p.name == "p2"));

        call(
            &s,
            "put_route",
            obj(serde_json::json!({"name": "r2", "definition": {"match": {"path": "/two"}, "policy": "p2"}})),
        )
        .await
        .unwrap();
        assert_eq!(s.routes.read().await.len(), 2);

        // Deleting a policy still referenced by a route is rejected whole.
        let err = call(&s, "delete_policy", obj(serde_json::json!({"name": "p2"})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_config");
        assert!(err.errors[0].contains("p2"), "{:?}", err.errors);

        call(&s, "delete_route", obj(serde_json::json!({"name": "r2"})))
            .await
            .unwrap();
        call(&s, "delete_policy", obj(serde_json::json!({"name": "p2"})))
            .await
            .unwrap();
        assert_eq!(s.routes.read().await.len(), 1);
        let err = call(&s, "delete_policy", obj(serde_json::json!({"name": "p2"})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[tokio::test]
    async fn invalid_policy_is_rejected_with_engine_errors() {
        let s = state("{}", ECHO_GATEWAY);
        let bad = "nodes:\n  - {id: l, type: listener}\n  - {id: k, type: key-auth, config: {keys: [k1]}}\n  - {id: c, type: client}\nedges:\n  - {from: l.out, to: k.in}\n  - {from: k.out, to: c.in}\n";
        let err = call(
            &s,
            "put_policy",
            obj(serde_json::json!({"name": "bad", "definition": bad})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "invalid_config");
        assert!(err.hint.as_deref().unwrap().contains("wired"));
        assert!(s
            .gateway
            .read()
            .await
            .policies
            .iter()
            .all(|p| p.name != "bad"));
        let err = call(
            &s,
            "put_policy",
            obj(serde_json::json!({"name": "bad", "definition": "nodes: ["})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }

    #[tokio::test]
    async fn supernode_plugin_config_store_round_trip() {
        let s = state("{}", ECHO_GATEWAY);
        call(
            &s,
            "put_plugin_config",
            obj(serde_json::json!({"name": "shared-echo", "definition": {"type": "echo", "config": {"body": "hi"}}})),
        )
        .await
        .unwrap();
        assert_eq!(s.gateway.read().await.plugin_configs.len(), 1);
        call(
            &s,
            "delete_plugin_config",
            obj(serde_json::json!({"name": "shared-echo"})),
        )
        .await
        .unwrap();

        // Stores: only exercised when the redis-store feature is compiled in
        // (without it, declaring a store fails validation by design).
        if cfg!(feature = "redis-store") {
            call(
                &s,
                "put_store",
                obj(serde_json::json!({"name": "st", "definition": {"type": "redis", "url": "redis://127.0.0.1:1"}})),
            )
            .await
            .unwrap();
            call(
                &s,
                "put_plugin_config",
                obj(serde_json::json!({"name": "lc", "definition": {"type": "limit-count", "config": {"count": 1, "time_window": 1, "policy": "redis", "store": "st"}}})),
            )
            .await
            .unwrap();
            let err = call(&s, "delete_store", obj(serde_json::json!({"name": "st"})))
                .await
                .unwrap_err();
            assert_eq!(err.code, "invalid_config");
            assert!(
                err.errors[0].contains("plugin_config 'lc'"),
                "{:?}",
                err.errors
            );
            call(
                &s,
                "delete_plugin_config",
                obj(serde_json::json!({"name": "lc"})),
            )
            .await
            .unwrap();
            call(&s, "delete_store", obj(serde_json::json!({"name": "st"})))
                .await
                .unwrap();
        }
    }

    /// `dry_run: true` must not leave a durable trace in the live store
    /// registry: `compile_routes` (called by `validate_gateway`) installs the
    /// candidate registry as a side effect of validating, and only restores
    /// the previous one on *failure* — every other caller follows with an
    /// apply that keeps that candidate for real. The MCP dry-run path has no
    /// such follow-up, so without `validate_gateway_dry` a dry-run
    /// `delete_store` would durably drop the store's live client and a
    /// dry-run `put_store` would durably stand one up.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn store_dry_run_does_not_swap_the_live_registry() {
        let gw = format!(
            "{ECHO_GATEWAY}\nstores:\n  - name: st\n    type: redis\n    url: redis://127.0.0.1:1\n"
        );
        let s = state("{}", &gw);
        assert!(s.resources.stores.load().contains("st"));

        // dry_run delete_store: registry and gateway config both keep 'st'.
        let v = call(
            &s,
            "delete_store",
            obj(serde_json::json!({"name": "st", "dry_run": true})),
        )
        .await
        .unwrap();
        assert_eq!(v["applied"], false);
        assert!(
            s.resources.stores.load().contains("st"),
            "dry-run delete_store must not durably drop the live client"
        );
        assert!(s
            .gateway
            .read()
            .await
            .stores
            .iter()
            .any(|store| store.name == "st"));

        // dry_run put_store for a brand-new name: registry never gets it.
        let v = call(
            &s,
            "put_store",
            obj(serde_json::json!({"name": "new-st", "dry_run": true, "definition": {"type": "redis", "url": "redis://127.0.0.1:1"}})),
        )
        .await
        .unwrap();
        assert_eq!(v["applied"], false);
        assert!(
            !s.resources.stores.load().contains("new-st"),
            "dry-run put_store must not durably install an uncommitted client"
        );
        assert!(s
            .gateway
            .read()
            .await
            .stores
            .iter()
            .all(|store| store.name != "new-st"));

        // Sanity: the original store is still functionally there afterwards.
        assert!(s.resources.stores.load().contains("st"));
    }
}
