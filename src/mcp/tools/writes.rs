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
            .map_err(|e| ToolError::invalid_payload(format!("{what}: YAML did not parse: {e}")))?,
        other => other,
    };
    match value.as_object_mut() {
        Some(obj) => {
            obj.insert("name".to_string(), Value::String(name.to_string()));
        }
        None => {
            return Err(ToolError::invalid_payload(format!(
                "{what}: definition must be a JSON/YAML object"
            )))
        }
    }
    serde_json::from_value(value)
        .map_err(|e| ToolError::invalid_payload(format!("{what}: JSON did not deserialize: {e}")))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PutArgs {
    /// Resource name. Optional when the definition itself carries `name`;
    /// when both are given this one wins.
    #[serde(default)]
    pub name: Option<String>,
    /// The definition, as a JSON object or a YAML document string. Also
    /// accepted under the resource's own key (`policy`, `route`, `supernode`,
    /// `plugin_config`, `store`) or `body`.
    #[serde(
        alias = "policy",
        alias = "route",
        alias = "supernode",
        alias = "plugin_config",
        alias = "store",
        alias = "body"
    )]
    pub definition: Value,
    /// Validate the whole resulting config without applying it. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

impl PutArgs {
    /// The resource name: the `name` argument, else `name` inside the
    /// definition (object or YAML string).
    fn resolve_name(&self, what: &str) -> Result<String, ToolError> {
        if let Some(n) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            return Ok(n.to_string());
        }
        let inner = match &self.definition {
            Value::String(yaml) => serde_yaml::from_str::<Value>(yaml).ok(),
            other => Some(other.clone()),
        };
        inner
            .as_ref()
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                ToolError::invalid_payload(format!(
                    "{what}: give `name` as an argument or inside the definition"
                ))
            })
    }
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
    let name = a.resolve_name("route")?;
    let route: RouteConfig = parse_named(a.definition, &name, "route")?;
    commit_candidate(state, a.dry_run, vec![format!("route:{name}")], |gw| {
        upsert(&mut gw.routes, route, |r| r.name == name);
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RouteOrderArgs {
    /// Every route name, exactly once, highest priority (matched first) first.
    pub order: Vec<String>,
    /// Validate the whole resulting config without applying it. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn put_route_order(state: &SharedState, a: RouteOrderArgs) -> Result<Value, ToolError> {
    commit_candidate(state, a.dry_run, vec!["routes:order".into()], |gw| {
        crate::admin::apply_route_order(&mut gw.routes, &a.order).map_err(ToolError::invalid_input)
    })
    .await
}

pub async fn put_policy(state: &SharedState, a: PutArgs) -> Result<Value, ToolError> {
    let name = a.resolve_name("policy")?;
    let policy: PolicyConfig = parse_named(a.definition, &name, "policy")?;
    commit_candidate(state, a.dry_run, vec![format!("policy:{name}")], |gw| {
        upsert(&mut gw.policies, policy, |p| p.name == name);
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
    let name = a.resolve_name("supernode")?;
    let sn: SupernodeConfig = parse_named(a.definition, &name, "supernode")?;
    commit_candidate(state, a.dry_run, vec![format!("supernode:{name}")], |gw| {
        upsert(&mut gw.supernodes, sn, |s| s.name == name);
        Ok(())
    })
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
    let name = a.resolve_name("plugin config")?;
    let pc: PluginConfigDef = parse_named(a.definition, &name, "plugin config")?;
    commit_candidate(
        state,
        a.dry_run,
        vec![format!("plugin_config:{name}")],
        |gw| {
            upsert(&mut gw.plugin_configs, pc, |p| p.name == name);
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
    let name = a.resolve_name("store")?;
    let store: StoreConfig = parse_named(a.definition, &name, "store")?;
    commit_candidate(state, a.dry_run, vec![format!("store:{name}")], |gw| {
        upsert(&mut gw.stores, store, |s| s.name == name);
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

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ReloadArgs {
    /// Re-read the file even though it would discard put_*/delete_* edits
    /// that were never written to gateway.yaml. Default false: the tool then
    /// refuses with `unsaved_changes` and lists what would be lost.
    #[serde(default)]
    pub discard_unsaved: bool,
}

/// Names of the entries in a named-resource section, keyed for comparison.
fn named_json(section: &Value) -> Vec<(String, Value)> {
    section
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|v| (v["name"].as_str().unwrap_or("?").to_string(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// One line per resource that differs between the live config and the file:
/// `policy 'p' only in memory`, `route 'r' only on disk`, `store 's' differs`.
pub fn unsaved_changes(live: &GatewayConfig, disk: &GatewayConfig) -> Vec<String> {
    let (live, disk) = match (serde_json::to_value(live), serde_json::to_value(disk)) {
        (Ok(l), Ok(d)) => (l, d),
        _ => return vec!["config could not be serialized for comparison".to_string()],
    };
    let mut out = Vec::new();
    for (section, label) in [
        ("routes", "route"),
        ("policies", "policy"),
        ("supernodes", "supernode"),
        ("plugin_configs", "plugin config"),
        ("stores", "store"),
        ("consumers", "consumer"),
    ] {
        let mem = named_json(&live[section]);
        let file = named_json(&disk[section]);
        for (name, v) in &mem {
            match file.iter().find(|(n, _)| n == name) {
                None => out.push(format!("{label} '{name}' exists only in memory")),
                Some((_, fv)) if fv != v => {
                    out.push(format!("{label} '{name}' differs from the file"))
                }
                Some(_) => {}
            }
        }
        for (name, _) in &file {
            if !mem.iter().any(|(n, _)| n == name) {
                out.push(format!("{label} '{name}' exists only on disk"));
            }
        }
    }
    out
}

/// Re-reads `gateway.yaml`. With the file config source, `put_*`/`delete_*`
/// edits are live but never written back, so a reload silently reverts them —
/// hence the guard: unless `discard_unsaved` is set, any difference between
/// memory and disk is reported and nothing is reloaded.
pub async fn reload_config(state: &SharedState, args: ReloadArgs) -> Result<Value, ToolError> {
    let disk = state
        .load_gateway_from_disk()
        .map_err(ToolError::store_error)?;
    let pending = {
        let live = state.gateway.read().await;
        unsaved_changes(&live, &disk)
    };
    if !pending.is_empty() && !args.discard_unsaved {
        return Err(ToolError::unsaved_changes(pending));
    }
    state
        .apply_gateway(disk)
        .await
        .map_err(ToolError::store_error)?;
    Ok(serde_json::json!({ "status": "reloaded", "discarded": pending }))
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    const NEW_POLICY: &str = "nodes:\n  - {id: l, type: listener}\n  - {id: e, type: echo, config: {body: hi}}\n  - {id: c, type: client}\nedges:\n  - {from: l.out, to: e.in}\n  - {from: e.out, to: c.in}\n";

    /// A file-backed state whose `config_path` points at a temp copy of
    /// `ECHO_GATEWAY`, so reload_config has a real file to compare against.
    fn file_backed_state(
        tag: &str,
    ) -> (
        std::sync::Arc<crate::state::SharedState>,
        std::path::PathBuf,
    ) {
        use crate::config::{GatewayConfig, SystemConfig};
        use crate::config_store::FileConfigStore;
        let path =
            std::env::temp_dir().join(format!("fb_reload_{tag}_{}.yaml", std::process::id()));
        std::fs::write(&path, ECHO_GATEWAY).unwrap();
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(ECHO_GATEWAY).unwrap();
        let s = crate::state::SharedState::new(
            system,
            gateway,
            Some(path.clone()),
            std::sync::Arc::new(FileConfigStore::new(path.clone())),
        )
        .unwrap();
        (std::sync::Arc::new(s), path)
    }

    #[test]
    fn unsaved_changes_lists_per_resource_differences() {
        use crate::config::GatewayConfig;
        let disk: GatewayConfig = serde_yaml::from_str(ECHO_GATEWAY).unwrap();
        let mut live = disk.clone();
        let mut extra = live.policies[0].clone();
        extra.name = "p2".to_string();
        live.policies.push(extra);
        live.routes[0].policy = "some-other-policy".to_string();
        let diffs = super::unsaved_changes(&live, &disk);
        assert!(
            diffs
                .iter()
                .any(|d| d == "policy 'p2' exists only in memory"),
            "{diffs:?}"
        );
        assert!(
            diffs.iter().any(|d| d.contains("route 'hello' differs")),
            "{diffs:?}"
        );
        assert!(super::unsaved_changes(&disk, &disk).is_empty());
    }

    #[tokio::test]
    async fn reload_refuses_to_discard_live_edits_unless_told_to() {
        let (s, path) = file_backed_state("guard");
        // Live edit that is not in the file.
        call(
            &s,
            "put_policy",
            obj(serde_json::json!({"name": "p2", "definition": NEW_POLICY})),
        )
        .await
        .unwrap();

        let err = call(&s, "reload_config", obj(serde_json::json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "unsaved_changes");
        assert!(
            err.errors.iter().any(|e| e.contains("policy 'p2'")),
            "{err:?}"
        );
        assert!(
            s.gateway
                .read()
                .await
                .policies
                .iter()
                .any(|p| p.name == "p2"),
            "nothing reloaded"
        );

        let v = call(
            &s,
            "reload_config",
            obj(serde_json::json!({"discard_unsaved": true})),
        )
        .await
        .unwrap();
        assert_eq!(v["status"], "reloaded");
        assert!(v["discarded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d.as_str().unwrap().contains("p2")));
        assert!(!s
            .gateway
            .read()
            .await
            .policies
            .iter()
            .any(|p| p.name == "p2"));

        // In sync again: a plain reload is fine.
        let v = call(&s, "reload_config", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(v["discarded"], serde_json::json!([]));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn put_accepts_resource_key_alias_and_name_inside_definition() {
        let s = state("{}", ECHO_GATEWAY);
        // `policy` instead of `definition`, and the name carried inside it.
        let mut def: serde_json::Value = serde_yaml::from_str(NEW_POLICY).unwrap();
        def["name"] = serde_json::json!("p-alias");
        let v = call(&s, "put_policy", obj(serde_json::json!({"policy": def})))
            .await
            .unwrap();
        assert_eq!(v["changed"][0], "policy:p-alias");
        assert!(s
            .gateway
            .read()
            .await
            .policies
            .iter()
            .any(|p| p.name == "p-alias"));

        // No name anywhere: invalid_input with the shape hint.
        let err = call(
            &s,
            "put_policy",
            obj(serde_json::json!({"definition": NEW_POLICY})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert!(err.message.contains("name"), "{err:?}");
        assert!(
            err.hint.as_deref().unwrap_or("").contains("\"definition\""),
            "{err:?}"
        );
    }

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

    #[tokio::test]
    async fn put_route_order_reorders_and_rejects_partial_lists() {
        let s = state("{}", ECHO_GATEWAY);
        call(
            &s,
            "put_route",
            obj(serde_json::json!({"name": "second", "definition": {"match": {"path": "/second"}, "policy": "echo-policy"}})),
        )
        .await
        .unwrap();
        let names = |s: &crate::state::SharedState| {
            let gw = s.gateway.try_read().unwrap();
            gw.routes.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
        };

        call(
            &s,
            "put_route_order",
            obj(serde_json::json!({"order": ["second", "hello"], "dry_run": true})),
        )
        .await
        .unwrap();
        assert_eq!(names(&s), ["hello", "second"], "dry run applies nothing");

        call(
            &s,
            "put_route_order",
            obj(serde_json::json!({"order": ["second", "hello"]})),
        )
        .await
        .unwrap();
        assert_eq!(names(&s), ["second", "hello"]);

        let err = call(
            &s,
            "put_route_order",
            obj(serde_json::json!({"order": ["hello"]})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert_eq!(names(&s), ["second", "hello"]);
    }
}
