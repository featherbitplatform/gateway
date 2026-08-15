# Shared Plugin Configs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Named, typed, shared plugin configurations (`plugin_configs:`) referenced by plugin nodes via `config_ref`, resolved at compile time with local-wins shallow merge, across policies and supernode definitions.

**Architecture:** A fifth top-level `GatewayConfig` entity plus one optional `config_ref` field on `NodeConfig`; a pure `resolve_plugin_configs()` pass materializes refs in-memory at the two compile choke points (`compile_routes`, debug sandbox) before supernode expansion. Stored config always keeps the reference form. Zero engine/plugin changes.

**Tech Stack:** Rust (serde, axum, tokio), React 19 (ui/), Playwright (e2e/), Docusaurus (website/).

**Spec:** `docs/superpowers/specs/2026-08-02-plugin-configs-design.md` — read it first.

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer**. Branch: `feature/plugin-configs` (exists, off `develop`).
- No new Rust or npm dependencies.
- Merge semantics: effective config = shared `config` with the node's local `config` keys written over it (**shallow, top-level, local wins**). The resolved copy is in-memory only — stored config, export, and etcd always keep `config_ref`.
- Resolution runs BEFORE supernode validation/expansion so definitions' inner nodes resolve and instances inherit.
- Reserved types may not be used as a shared config's `type`: `listener`, `client`, `supernode`, `input`, `output`, `error`. Nodes of type `supernode` may not carry `config_ref`.
- Admin endpoints use hyphen (`/api/plugin-configs`); the YAML section and etcd key family use underscore (`plugin_configs`).
- Every Rust task: `cargo test <filter>`, then full `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` — all green at every commit; implementers paste actual command output in reports.
- UI verification: `cd ui && npm run build`. Website: `cd website && npm run build`.
- The `upstream` plugin's config is `targets: [{host, port}]` (never `url:`); the `mocking` plugin takes `{response_status, response_example, content_type}` (used by the e2e task).

---

### Task 1: Config model — `PluginConfigDef` + `NodeConfig.config_ref`

**Files:**
- Modify: `src/config/gateway.rs` (structs + tests at bottom)
- Modify: `src/config/mod.rs` (re-export line)
- Modify: `src/config_store/etcd.rs` (`gateway_from_kvs` struct literal + any test literals — compiler will point at them)

**Interfaces:**
- Produces: `crate::config::PluginConfigDef { name: String, plugin_type: String (serde "type"), description: Option<String>, config: HashMap<String, serde_json::Value> }`; `GatewayConfig.plugin_configs: Vec<PluginConfigDef>`; `NodeConfig.config_ref: Option<String>`.

- [ ] **Step 1: Write the failing test** — append inside the existing `mod tests` of `src/config/gateway.rs`:

```rust
    /// Old configs stay valid; a shared config and a `config_ref` round-trip
    /// through YAML; `config_ref` is omitted from output when unset.
    #[test]
    fn test_plugin_configs_default_empty_and_roundtrip() {
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(gw.plugin_configs.is_empty());

        let yaml = r#"
plugin_configs:
  - name: corp-oidc
    type: openid-connect
    description: "Corporate IdP client"
    config:
      client_id: gateway
      scope: openid
policies:
  - name: p
    nodes:
      - { id: auth, type: openid-connect, config_ref: corp-oidc, config: { scope: "openid profile" } }
"#;
        let gw: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(gw.plugin_configs.len(), 1);
        let def = &gw.plugin_configs[0];
        assert_eq!(def.name, "corp-oidc");
        assert_eq!(def.plugin_type, "openid-connect");
        assert_eq!(def.description.as_deref(), Some("Corporate IdP client"));
        assert_eq!(def.config["client_id"], serde_json::json!("gateway"));
        let node = &gw.policies[0].nodes[0];
        assert_eq!(node.config_ref.as_deref(), Some("corp-oidc"));
        assert_eq!(node.config["scope"], serde_json::json!("openid profile"));

        // Round-trip keeps the reference form; nodes without a ref omit the key.
        let out = serde_yaml::to_string(&gw).unwrap();
        assert!(out.contains("config_ref: corp-oidc"), "{out}");
        let plain: GatewayConfig =
            serde_yaml::from_str("policies:\n  - name: q\n    nodes:\n      - { id: a, type: cors }\n").unwrap();
        let plain_out = serde_yaml::to_string(&plain).unwrap();
        assert!(!plain_out.contains("config_ref"), "{plain_out}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_plugin_configs_default_empty_and_roundtrip -- --exact`
Expected: COMPILE ERROR — no field `plugin_configs`, no struct `PluginConfigDef`, no field `config_ref`.

- [ ] **Step 3: Implement.** In `src/config/gateway.rs`:

(a) `GatewayConfig` — after the `supernodes` field:

```rust
    /// Named, typed plugin configurations shared by any number of plugin
    /// nodes via `config_ref`; resolved at compile time (src/config/resolve.rs).
    #[serde(default)]
    pub plugin_configs: Vec<PluginConfigDef>,
```

(b) `NodeConfig` — after the `config` field (before `position`):

```rust
    /// Optional name of a shared [`PluginConfigDef`] to inherit configuration
    /// from. The effective config is the shared config with this node's own
    /// `config` keys layered on top (shallow merge, local wins), materialized
    /// at compile time — the stored form always keeps the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_ref: Option<String>,
```

(c) New struct after `SupernodeConfig`:

```rust
/// A named, shared plugin configuration, referenced by plugin nodes of the
/// matching type via [`NodeConfig::config_ref`]. Editing a shared config
/// re-resolves and recompiles every referencing policy atomically.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PluginConfigDef {
    /// Unique name, referenced by `config_ref`.
    pub name: String,
    /// Plugin type this config is for (YAML key: `type`); only nodes of the
    /// same type may reference it.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Optional human-readable description (shown in the UI library).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The shared plugin configuration; same shape as [`NodeConfig::config`].
    #[serde(default)]
    pub config: HashMap<String, serde_json::Value>,
}
```

(d) `src/config/mod.rs`: add `PluginConfigDef` to the `pub use gateway::{...}` list.

(e) Fix the `GatewayConfig` struct literals the compiler now rejects in `src/config_store/etcd.rs` (`gateway_from_kvs` and any test literal): add `plugin_configs: Vec::new(),`.

- [ ] **Step 4: Run tests**

Run: `cargo test test_plugin_configs_default_empty_and_roundtrip -- --exact` → PASS; full `cargo test` + `cargo fmt --check` + `cargo clippy --all-targets --locked -- -D warnings` → green.

- [ ] **Step 5: Commit**

```bash
git add src/config/gateway.rs src/config/mod.rs src/config_store/etcd.rs
git commit -m "feat(config): add PluginConfigDef and NodeConfig.config_ref"
```

---

### Task 2: `KNOWN_PLUGIN_TYPES` registry constant

**Files:**
- Modify: `src/plugins/mod.rs`

**Interfaces:**
- Produces: `crate::plugins::KNOWN_PLUGIN_TYPES: &[&str]` — every type `create_plugin` accepts. Task 3 uses it to validate a shared config's `type` at save time.

- [ ] **Step 1: Write the failing drift test** — in `src/plugins/mod.rs`'s `#[cfg(test)] mod tests` (create the module at the bottom of the file if absent, following the file's conventions). This mirrors the source-parsing guard already used by `factory_types()` in `src/admin/policies.rs:360` (read it first):

```rust
    /// KNOWN_PLUGIN_TYPES must track create_plugin's match arms exactly, in
    /// both directions. Same source-parsing guard the admin catalog uses.
    #[test]
    fn test_known_plugin_types_matches_factory() {
        let factory: std::collections::BTreeSet<String> = include_str!("mod.rs")
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix('"')?;
                let (name, tail) = rest.split_once('"')?;
                tail.trim_start().starts_with("=>").then(|| name.to_string())
            })
            .collect();
        let listed: std::collections::BTreeSet<String> =
            KNOWN_PLUGIN_TYPES.iter().map(|s| s.to_string()).collect();
        assert_eq!(listed, factory, "KNOWN_PLUGIN_TYPES drifted from create_plugin");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_known_plugin_types_matches_factory -- --exact` → COMPILE ERROR (`KNOWN_PLUGIN_TYPES` not found).

- [ ] **Step 3: Implement.** Above `create_plugin` in `src/plugins/mod.rs`, add:

```rust
/// Every plugin type [`create_plugin`] can build, for save-time validation of
/// references to plugin types (e.g. a shared config's `type`). Guarded against
/// drift from the factory's match arms by `test_known_plugin_types_matches_factory`.
pub const KNOWN_PLUGIN_TYPES: &[&str] = &[
    // ... one entry per create_plugin match arm, copied from the match ...
];
```

Populate it by listing every quoted type in `create_plugin`'s match (the test tells you exactly which are missing/extra — iterate until it passes). Keep the entries in the same order as the match arms for readability.

- [ ] **Step 4: Run tests**

Run: `cargo test test_known_plugin_types_matches_factory -- --exact` → PASS; full suite + fmt + clippy green.

- [ ] **Step 5: Commit**

```bash
git add src/plugins/mod.rs
git commit -m "feat(plugins): KNOWN_PLUGIN_TYPES registry constant with drift guard"
```

---

### Task 3: Resolution module — `src/config/resolve.rs`

**Files:**
- Create: `src/config/resolve.rs`
- Modify: `src/config/mod.rs` (module decl + re-export)

**Interfaces:**
- Consumes: `PluginConfigDef`, `NodeConfig.config_ref` (Task 1); `crate::plugins::KNOWN_PLUGIN_TYPES` (Task 2).
- Produces: `crate::config::resolve_plugin_configs(gw: &GatewayConfig) -> Result<GatewayConfig, String>` — validates the defs, then returns a copy with every `config_ref` in policies AND supernode definitions materialized and cleared. Tasks 4-5 call it.

- [ ] **Step 1: Create `src/config/resolve.rs`** with module doc, tests, and a stub that fails to compile until Step 3:

```rust
//! Compile-time resolution of shared plugin configs (`config_ref`).
//!
//! [`resolve_plugin_configs`] validates the `plugin_configs` definitions and
//! returns an in-memory copy of the gateway config where every node's
//! `config_ref` — in policies and supernode definitions alike — has been
//! materialized: effective config = shared config with the node's local keys
//! written over it (shallow, top-level, local wins), `config_ref` cleared.
//! Runs at the top of `SharedState::compile_routes` (before supernode
//! expansion, so instances inherit resolved inner configs) and in the debug
//! sandbox. The resolved copy is never stored: gateway.yaml, the Admin API,
//! etcd, and the export endpoint always keep the reference form.

use std::collections::HashMap;

use serde_json::Value;

use super::gateway::{GatewayConfig, NodeConfig, PluginConfigDef};

/// Types that may not be used as a shared config's `type`: pipeline endpoints,
/// supernode instances, and supernode boundary pseudo-nodes.
const RESERVED_TYPES: [&str; 6] = ["listener", "client", "supernode", "input", "output", "error"];

// resolve_plugin_configs + helpers go here (Step 3)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EdgeConfig, PolicyConfig, SupernodeConfig};
    use serde_json::json;

    fn def(name: &str, plugin_type: &str, config: serde_json::Value) -> PluginConfigDef {
        PluginConfigDef {
            name: name.into(),
            plugin_type: plugin_type.into(),
            description: None,
            config: serde_json::from_value(config).unwrap(),
        }
    }

    fn node(id: &str, ty: &str, config_ref: Option<&str>, config: serde_json::Value) -> NodeConfig {
        NodeConfig {
            id: id.into(),
            node_type: ty.into(),
            config: serde_json::from_value(config).unwrap(),
            config_ref: config_ref.map(String::from),
            position: None,
        }
    }

    fn gw_with(policy_nodes: Vec<NodeConfig>, defs: Vec<PluginConfigDef>) -> GatewayConfig {
        let mut gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        gw.plugin_configs = defs;
        gw.policies = vec![PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: policy_nodes,
            edges: Vec::new(),
        }];
        gw
    }

    #[test]
    fn test_merge_local_wins_and_ref_cleared() {
        let gw = gw_with(
            vec![node(
                "auth",
                "openid-connect",
                Some("corp"),
                json!({"scope": "openid profile"}),
            )],
            vec![def("corp", "openid-connect", json!({"client_id": "gw", "scope": "openid"}))],
        );
        let out = resolve_plugin_configs(&gw).unwrap();
        let n = &out.policies[0].nodes[0];
        assert_eq!(n.config["client_id"], json!("gw"));      // inherited
        assert_eq!(n.config["scope"], json!("openid profile")); // local wins
        assert!(n.config_ref.is_none(), "ref must be cleared in resolved copy");
        // Source is untouched (pure function).
        assert_eq!(gw.policies[0].nodes[0].config_ref.as_deref(), Some("corp"));
    }

    #[test]
    fn test_no_ref_passthrough_unchanged() {
        let gw = gw_with(vec![node("a", "cors", None, json!({"x": 1}))], vec![]);
        let out = resolve_plugin_configs(&gw).unwrap();
        assert_eq!(out.policies[0].nodes[0].config["x"], json!(1));
    }

    #[test]
    fn test_unknown_ref_is_error() {
        let gw = gw_with(vec![node("a", "cors", Some("nope"), json!({}))], vec![]);
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(err.contains("unknown plugin config 'nope'") && err.contains("'a'"), "{err}");
    }

    #[test]
    fn test_type_mismatch_is_error() {
        let gw = gw_with(
            vec![node("a", "cors", Some("corp"), json!({}))],
            vec![def("corp", "openid-connect", json!({}))],
        );
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(err.contains("'corp'") && err.contains("openid-connect") && err.contains("cors"), "{err}");
    }

    #[test]
    fn test_supernode_definition_nodes_resolve() {
        let mut gw = gw_with(vec![], vec![def("m", "mocking", json!({"response_status": 200}))]);
        gw.supernodes = vec![SupernodeConfig {
            name: "sn".into(),
            description: None,
            nodes: vec![
                node("input", "input", None, json!({})),
                node("output", "output", None, json!({})),
                node("error", "error", None, json!({})),
                node("mock", "mocking", Some("m"), json!({"content_type": "text/plain"})),
            ],
            edges: vec![
                EdgeConfig { from: "input.out".into(), to: "mock.in".into() },
                EdgeConfig { from: "mock.success".into(), to: "output.in".into() },
            ],
        }];
        let out = resolve_plugin_configs(&gw).unwrap();
        let inner = &out.supernodes[0].nodes[3];
        assert_eq!(inner.config["response_status"], json!(200));
        assert_eq!(inner.config["content_type"], json!("text/plain"));
        assert!(inner.config_ref.is_none());
    }

    #[test]
    fn test_supernode_instance_node_cannot_carry_ref() {
        let gw = gw_with(
            vec![node("sn", "supernode", Some("m"), json!({"name": "x"}))],
            vec![def("m", "mocking", json!({}))],
        );
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(err.contains("supernode") && err.contains("config_ref"), "{err}");
    }

    #[test]
    fn test_duplicate_def_names_rejected() {
        let gw = gw_with(vec![], vec![def("d", "cors", json!({})), def("d", "cors", json!({}))]);
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(err.contains("Duplicate plugin config name 'd'"), "{err}");
    }

    #[test]
    fn test_unknown_and_reserved_def_types_rejected() {
        let gw = gw_with(vec![], vec![def("d", "openid-conect", json!({}))]);
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(err.contains("unknown plugin type 'openid-conect'"), "{err}");

        for ty in RESERVED_TYPES {
            let gw = gw_with(vec![], vec![def("d", ty, json!({}))]);
            let err = resolve_plugin_configs(&gw).unwrap_err();
            assert!(err.contains("reserved"), "type {ty}: {err}");
        }
    }
}
```

- [ ] **Step 2: Wire and verify failure** — in `src/config/mod.rs` add `mod resolve;` and `pub use resolve::resolve_plugin_configs;` next to the existing items.

Run: `cargo test resolve` → COMPILE ERROR (`resolve_plugin_configs` not found).

- [ ] **Step 3: Implement** (replace the marker comment):

```rust
/// Validates shared plugin configs and materializes every `config_ref`.
///
/// Definition rules: unique names; `type` must be a factory-known plugin type
/// and not a reserved type. Reference rules: the named def must exist and its
/// type must equal the node's type; `supernode` instance nodes cannot carry a
/// ref. All errors name the offending policy/supernode and node.
pub fn resolve_plugin_configs(gw: &GatewayConfig) -> Result<GatewayConfig, String> {
    let mut seen = std::collections::HashSet::new();
    for def in &gw.plugin_configs {
        if !seen.insert(def.name.as_str()) {
            return Err(format!("Duplicate plugin config name '{}'", def.name));
        }
        if RESERVED_TYPES.contains(&def.plugin_type.as_str()) {
            return Err(format!(
                "Plugin config '{}' uses reserved type '{}'",
                def.name, def.plugin_type
            ));
        }
        if !crate::plugins::KNOWN_PLUGIN_TYPES.contains(&def.plugin_type.as_str()) {
            return Err(format!(
                "Plugin config '{}' references unknown plugin type '{}'",
                def.name, def.plugin_type
            ));
        }
    }

    let by_name: HashMap<&str, &PluginConfigDef> =
        gw.plugin_configs.iter().map(|d| (d.name.as_str(), d)).collect();

    let mut out = gw.clone();
    for policy in &mut out.policies {
        let ctx = format!("policy '{}'", policy.name);
        resolve_nodes(&mut policy.nodes, &by_name, &ctx)?;
    }
    for sn in &mut out.supernodes {
        let ctx = format!("supernode '{}'", sn.name);
        resolve_nodes(&mut sn.nodes, &by_name, &ctx)?;
    }
    Ok(out)
}

/// Materializes `config_ref` on each node in place: shared config first, then
/// the node's own keys written over it (local wins), ref cleared.
fn resolve_nodes(
    nodes: &mut [NodeConfig],
    by_name: &HashMap<&str, &PluginConfigDef>,
    ctx: &str,
) -> Result<(), String> {
    for node in nodes {
        let Some(ref_name) = node.config_ref.take() else {
            continue;
        };
        if node.node_type == "supernode" {
            return Err(format!(
                "{ctx}: supernode instance node '{}' cannot use config_ref",
                node.id
            ));
        }
        let def = by_name.get(ref_name.as_str()).ok_or_else(|| {
            format!(
                "{ctx}: node '{}' references unknown plugin config '{}'",
                node.id, ref_name
            )
        })?;
        if def.plugin_type != node.node_type {
            return Err(format!(
                "{ctx}: node '{}' ({}) references plugin config '{}' of type '{}'",
                node.id, node.node_type, ref_name, def.plugin_type
            ));
        }
        let mut merged: HashMap<String, Value> = def.config.clone();
        merged.extend(node.config.drain());
        node.config = merged;
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test resolve` → all 8 PASS; full suite + fmt + clippy green.

- [ ] **Step 5: Commit**

```bash
git add src/config/resolve.rs src/config/mod.rs
git commit -m "feat(config): compile-time resolution of shared plugin configs"
```

---

### Task 4: Wire resolution into `compile_routes` + the sandbox

**Files:**
- Modify: `src/state.rs` (`compile_routes` top, `use` line, tests)
- Modify: `src/admin/debug.rs` (`run_sandbox`, right before the existing supernode-expansion block)

**Interfaces:**
- Consumes: `resolve_plugin_configs` (Task 3).
- Produces: every config path (file load, Admin API commit, etcd, hot reload, sandbox) resolves refs before compiling. Delete-while-referenced protection falls out for free.

- [ ] **Step 1: Write the failing tests** — append inside `src/state.rs`'s existing `mod tests` (reuse the existing `state_from_yaml` helper — read it first; it validates a candidate `GatewayConfig` through `validate_gateway`):

```rust
    // `upstream` is used deliberately: its `targets` key is REQUIRED, so an
    // UNRESOLVED ref leaves the node without targets and create_plugin fails —
    // giving this test a genuine red state before Task 4 is implemented.
    // (A permissive plugin like `mocking` would compile even unresolved.)
    const PLUGIN_CONFIG_GATEWAY: &str = r#"
plugin_configs:
  - name: shared-up
    type: upstream
    config: { targets: [ { host: "127.0.0.1", port: 9 } ] }
supernodes:
  - name: wrapped
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: up, type: upstream, config_ref: shared-up }
    edges:
      - { from: input.out,  to: up.in }
      - { from: up.success, to: output.in }
routes:
  - name: r
    match: { path: "/*" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: direct, type: upstream, config_ref: shared-up, config: { strategy: "round_robin" } }
      - { id: sn, type: supernode, config: { name: wrapped } }
      - { id: client, type: client }
    edges:
      - { from: listener.out,  to: direct.in }
      - { from: direct.success, to: sn.in }
      - { from: sn.success,    to: client.in }
"#;

    /// Refs resolve for a direct policy node AND for a node inside a
    /// supernode definition, and the whole thing compiles.
    #[test]
    fn test_plugin_config_refs_compile() {
        assert_eq!(state_from_yaml(PLUGIN_CONFIG_GATEWAY), Ok(()));
    }

    #[test]
    fn test_unknown_plugin_config_ref_rejected() {
        let yaml = PLUGIN_CONFIG_GATEWAY.replace("config_ref: shared-up, config:", "config_ref: nope, config:");
        let err = state_from_yaml(&yaml).unwrap_err();
        assert!(err.contains("unknown plugin config 'nope'"), "{err}");
    }

    /// Removing the shared config while a supernode inner node still
    /// references it is rejected — this is the delete-protection mechanism.
    #[test]
    fn test_delete_referenced_plugin_config_rejected() {
        let yaml = PLUGIN_CONFIG_GATEWAY.replace(
            "  - name: shared-up\n    type: upstream\n    config: { targets: [ { host: \"127.0.0.1\", port: 9 } ] }\n",
            "",
        );
        let err = state_from_yaml(&yaml).unwrap_err();
        assert!(err.contains("unknown plugin config 'shared-up'"), "{err}");
    }
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test test_plugin_config_refs_compile -- --exact`
Expected: FAIL — the unresolved `upstream` nodes have no `targets` key, so `create_plugin` errors and the assert on `Ok(())` fails.

- [ ] **Step 3: Implement.**

(a) `src/state.rs` — extend the config import to include `resolve_plugin_configs` (match the existing import style), then at the very top of `compile_routes`:

```rust
        // Materialize shared plugin configs first: supernode definitions and
        // policies both resolve against them, and expansion below copies the
        // resolved inner configs into instances. In-memory only — the stored
        // gateway config keeps the `config_ref` form.
        let gateway = resolve_plugin_configs(gateway)?;
        let gateway = &gateway;
```

(shadowing keeps the rest of the function's `gateway.…` references compiling unchanged).

(b) `src/admin/debug.rs` — in `run_sandbox`, replace the current supernode-clone block with one that resolves first (keep the existing comment style):

```rust
    // Resolve shared plugin configs and inline supernode references the same
    // way compile_routes does — the sandbox must never diverge from what the
    // data plane executes. Resolution runs on a synthetic single-policy
    // gateway so ad-hoc nodes and stored policies behave identically.
    let (supernodes, plugin_configs) = {
        let gw = state.gateway.read().await;
        (gw.supernodes.clone(), gw.plugin_configs.clone())
    };
    let resolved = {
        let mut tmp: crate::config::GatewayConfig = serde_yaml::from_str("{}").unwrap();
        tmp.policies = vec![policy];
        tmp.supernodes = supernodes;
        tmp.plugin_configs = plugin_configs;
        match crate::config::resolve_plugin_configs(&tmp) {
            Ok(r) => r,
            Err(e) => return bad_request(e),
        }
    };
    let policy = resolved.policies.into_iter().next().expect("one policy in");
    let policy = match crate::graph::expand_policy(&policy, &resolved.supernodes) {
        Ok(p) => p,
        Err(e) => return bad_request(e),
    };
```

(c) Add a sandbox test next to `test_sandbox_expands_supernodes` (mirror its state-building style — read it first):

```rust
    /// A sandbox run resolves config_ref exactly like the data plane: the
    /// mocking node inherits the shared config and answers with its body.
    #[tokio::test]
    async fn test_sandbox_resolves_plugin_configs() {
        let system = SystemConfig {
            debug: DebugConfig { enabled: true, ..Default::default() },
            ..serde_yaml::from_str::<SystemConfig>("{}").unwrap()
        };
        let gateway: GatewayConfig = serde_yaml::from_str(
            r#"
plugin_configs:
  - name: m
    type: mocking
    config: { response_status: 200, response_example: "from-shared", content_type: "text/plain" }
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: mock, type: mocking, config_ref: m }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: mock.in }
      - { from: mock.success, to: client.in }
"#,
        )
        .unwrap();
        let s = Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from("gateway.yaml"))),
            )
            .unwrap(),
        );

        let req = SandboxRequest { policy: Some("p".to_string()), ..Default::default() };
        let resp = run_sandbox(State(s), Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let steps = v["trace"]["steps"].as_array().unwrap();
        // The mocking node ran with the shared body -> response body captured
        // in the final snapshot contains it (bodies off => assert via status).
        assert!(
            steps.iter().any(|s| s["node_id"] == "mock" && s["outcome"]["kind"] == "success"),
            "{v}"
        );
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test state`, `cargo test test_sandbox_resolves_plugin_configs -- --exact`, full suite, fmt, clippy → all green.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs src/admin/debug.rs
git commit -m "feat(state): resolve shared plugin configs in compile_routes and sandbox"
```

---

### Task 5: Admin API — `/api/plugin-configs` CRUD

**Files:**
- Create: `src/admin/plugin_configs.rs`
- Modify: `src/admin/mod.rs` (module decl + `.merge(plugin_configs::router())` inside the authed block)

**Interfaces:**
- Consumes: `PluginConfigDef` (Task 1); commit-path validation (Task 4).
- Produces: `GET /api/plugin-configs`, `GET/PUT/DELETE /api/plugin-configs/{name}` (PUT upsert, path name wins; DELETE 404 absent / 400 referenced). UI Task 7 calls these.

- [ ] **Step 1: Create the file** — mirror `src/admin/supernodes.rs` exactly (read it first; same handler shapes, doc-comment style, commit pattern), with `SupernodeConfig`→`PluginConfigDef`, `supernodes`→`plugin_configs`, paths `/api/plugin-configs`. Tests (same axum-oneshot pattern as supernodes.rs's tests):

```rust
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
                Arc::new(FileConfigStore::new(std::path::PathBuf::from("gateway.yaml"))),
            )
            .unwrap(),
        )
    }

    fn app(state: Arc<SharedState>) -> Router {
        router().with_state(state)
    }

    const VALID_DEF: &str = r#"{
        "name": "shared-mock",
        "type": "mocking",
        "config": { "response_status": 200, "response_example": "hi", "content_type": "text/plain" }
    }"#;

    async fn put_def(state: &Arc<SharedState>, name: &str, body: &str) -> StatusCode {
        app(state.clone())
            .oneshot(
                Request::put(format!("/api/plugin-configs/{name}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn test_put_list_get_delete_roundtrip() {
        let state = test_state("{}");
        assert_eq!(put_def(&state, "shared-mock", VALID_DEF).await, StatusCode::OK);

        let resp = app(state.clone())
            .oneshot(Request::get("/api/plugin-configs/shared-mock").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state.clone())
            .oneshot(Request::delete("/api/plugin-configs/shared-mock").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state)
            .oneshot(Request::get("/api/plugin-configs/shared-mock").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_put_unknown_type_is_400() {
        let state = test_state("{}");
        let bad = r#"{ "name": "x", "type": "openid-conect", "config": {} }"#;
        assert_eq!(put_def(&state, "x", bad).await, StatusCode::BAD_REQUEST);
    }

    /// Referenced from a SUPERNODE DEFINITION (the harder case): delete must 400.
    #[tokio::test]
    async fn test_delete_while_referenced_by_supernode_is_400() {
        let state = test_state(
            r#"
plugin_configs:
  - name: shared-mock
    type: mocking
    config: { response_status: 200, response_example: "hi", content_type: "text/plain" }
supernodes:
  - name: wrapped
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: mock, type: mocking, config_ref: shared-mock }
    edges:
      - { from: input.out,    to: mock.in }
      - { from: mock.success, to: output.in }
"#,
        );
        let resp = app(state)
            .oneshot(Request::delete("/api/plugin-configs/shared-mock").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_delete_missing_is_404() {
        let state = test_state("{}");
        let resp = app(state)
            .oneshot(Request::delete("/api/plugin-configs/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test plugin_configs` → COMPILE ERROR (module not declared).

- [ ] **Step 3: Mount** — `mod plugin_configs;` in `src/admin/mod.rs` next to `mod supernodes;`, and `.merge(plugin_configs::router())` after `.merge(supernodes::router())` (inside the authed block, before the auth layer).

- [ ] **Step 4: Run tests** — `cargo test plugin_configs` → PASS; the three catalog drift-guard tests untouched and green; full suite + fmt + clippy green.

- [ ] **Step 5: Commit**

```bash
git add src/admin/plugin_configs.rs src/admin/mod.rs
git commit -m "feat(admin): plugin config CRUD endpoints"
```

---

### Task 6: etcd — `plugin_configs/` key family

**Files:**
- Modify: `src/config_store/etcd.rs`

**Interfaces:**
- Produces: `<prefix>/plugin_configs/<name>` keys with seed/commit/load parity.

- [ ] **Step 1: Write the failing tests** — in the existing `#[cfg(test)]` module (mirror the supernode ones added there):

```rust
    #[test]
    fn test_gateway_from_kvs_parses_plugin_configs() {
        let def = serde_json::json!({ "name": "corp", "type": "cors", "config": {} });
        let kvs = vec![(
            "gw/plugin_configs/corp".to_string(),
            serde_json::to_vec(&def).unwrap(),
        )];
        let gw = gateway_from_kvs("gw", kvs).unwrap();
        assert_eq!(gw.plugin_configs.len(), 1);
        assert_eq!(gw.plugin_configs[0].name, "corp");
    }

    #[test]
    fn test_gateway_from_kvs_bad_plugin_config_json_is_error() {
        let kvs = vec![("gw/plugin_configs/x".to_string(), b"not json".to_vec())];
        let err = gateway_from_kvs("gw", kvs).unwrap_err();
        assert!(err.contains("bad plugin config"), "{err}");
    }

    #[test]
    fn test_is_empty_counts_plugin_configs() {
        let mut gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(is_empty(&gw));
        gw.plugin_configs.push(PluginConfigDef {
            name: "c".into(),
            plugin_type: "cors".into(),
            description: None,
            config: Default::default(),
        });
        assert!(!is_empty(&gw));
    }
```

(add `PluginConfigDef` to the test module's imports).

- [ ] **Step 2: Run to verify failure** — `cargo test etcd` → first test asserts len 1, gets 0 (unknown category skipped); `is_empty` ignores the new field.

- [ ] **Step 3: Implement** the five mechanical additions, each mirroring the supernodes handling exactly (read the file first):
1. `fn plugin_config_key(&self, name: &str) -> String { format!("{}/plugin_configs/{}", self.prefix, name) }`
2. `write_all`: fifth loop over `&gw.plugin_configs`.
3. `commit`: fifth put-loop over `&candidate.plugin_configs` — **including `desired.insert(key)`** (omitting it makes the stale-delete sweep GC the keys just written).
4. `gateway_from_kvs`: `"plugin_configs"` arm deserializing `PluginConfigDef`, error text `bad plugin config '{key}': {e}`; import the type.
5. `is_empty`: `&& gw.plugin_configs.is_empty()`.
Also update the module doc's key-family list AND the `gateway_from_kvs` fn doc (the fn doc was missed for supernodes and had to be fixed in review — do both here).

- [ ] **Step 4: Run tests** — `cargo test etcd` → PASS; full suite + fmt + clippy green.

- [ ] **Step 5: Commit**

```bash
git add src/config_store/etcd.rs
git commit -m "feat(etcd): persist plugin configs under <prefix>/plugin_configs/"
```

---

### Task 7: UI — types, client, node round-trip, badge

**Files:**
- Modify: `ui/src/types/index.ts`, `ui/src/api/client.ts`, `ui/src/components/PluginNode.tsx`, `ui/src/components/GraphCanvas.tsx`

**Interfaces:**
- Produces: `PluginConfigDef` TS type; `PolicyNode.config_ref?: string`; `api.listPluginConfigs/getPluginConfig/updatePluginConfig/deletePluginConfig`; `PluginNodeData.configRef?: string` round-tripped by `policyToNodes`/`nodesToPolicy`; a link badge on nodes with a ref. Tasks 8-9 consume these.

- [ ] **Step 1: Types** — `ui/src/types/index.ts`: add to `PolicyNode` (after `config`):

```ts
  /** Optional name of a shared plugin config to inherit from (local keys win). */
  config_ref?: string;
```

and after the `Supernode` interface:

```ts
/**
 * A named, typed shared plugin configuration, referenced by plugin nodes
 * via {@link PolicyNode.config_ref}; resolved at compile time by the gateway.
 *
 * @remarks
 * Served and persisted by the CRUD handlers in src/admin/plugin_configs.rs.
 */
export interface PluginConfigDef {
  /** Unique name, referenced by `config_ref`. */
  name: string;
  /** Plugin type this config is for; only nodes of the same type may reference it. */
  type: string;
  /** Optional human-readable description shown in the library. */
  description?: string;
  /** The shared plugin configuration (same shape as a node's `config`). */
  config: Record<string, unknown>;
}
```

- [ ] **Step 2: API client** — `ui/src/api/client.ts`: import `PluginConfigDef`; after the Supernodes block add:

```ts
  // Plugin configs
  /** `GET /api/plugin-configs` — returns all shared plugin configs. */
  listPluginConfigs: () => request<PluginConfigDef[]>('/api/plugin-configs'),
  /** `GET /api/plugin-configs/{name}` — returns the named shared config. */
  getPluginConfig: (name: string) => request<PluginConfigDef>(`/api/plugin-configs/${name}`),
  /** `PUT /api/plugin-configs/{name}` — upserts the named shared config (also used to create). */
  updatePluginConfig: (name: string, def: PluginConfigDef) =>
    request(`/api/plugin-configs/${name}`, { method: 'PUT', body: JSON.stringify(def) }),
  /** `DELETE /api/plugin-configs/{name}` — removes the shared config; 400 while referenced. */
  deletePluginConfig: (name: string) =>
    request(`/api/plugin-configs/${name}`, { method: 'DELETE' }),
```

- [ ] **Step 3: Round-trip + badge.**
- `ui/src/components/PluginNode.tsx`: add `configRef?: string;` to `PluginNodeData` (doc-commented like the other fields). In the body, under the label div, render the badge when set:

```tsx
      {nodeData.configRef && (
        <div
          className="flex items-center"
          style={{
            gap: 4,
            padding: '0 10px 8px',
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-2xs)',
            color: 'var(--text-muted)',
          }}
          title={`Inherits shared config '${nodeData.configRef}'`}
        >
          <Link2 size={10} style={{ flexShrink: 0 }} />
          {nodeData.configRef}
        </div>
      )}
```

(import `Link2` from `lucide-react` next to the existing imports).
- `ui/src/components/GraphCanvas.tsx`: in `policyToNodes`'s data mapping add `configRef: node.config_ref,`; in `nodesToPolicy`'s node mapping add `...(data.configRef ? { config_ref: data.configRef } : {}),` so unset refs stay omitted from the saved JSON. In `handleAddPlugin`/`handleAddScript`/`handleAddSupernode` no change (new nodes start without a ref).

- [ ] **Step 4: Verify + commit**

Run: `cd ui && npm run build` → success.

```bash
git add ui/src/types/index.ts ui/src/api/client.ts ui/src/components/PluginNode.tsx ui/src/components/GraphCanvas.tsx
git commit -m "feat(ui): plugin config types, client, node round-trip and badge"
```

---

### Task 8: UI — NodeInspector shared-config picker

**Files:**
- Modify: `ui/src/components/NodeInspector.tsx`, `ui/src/components/GraphCanvas.tsx`

**Interfaces:**
- Consumes: `PluginConfigDef`, `PluginNodeData.configRef` (Task 7).
- Produces: `NodeInspectorProps` gains `pluginConfigs: PluginConfigDef[]` and `onUpdateConfigRef: (nodeId: string, ref: string | undefined) => void`; `GraphCanvasProps` gains `pluginConfigs: PluginConfigDef[]`. Task 9 (App) supplies the list.

- [ ] **Step 1: NodeInspector.** Add the two props (doc-commented). In the render, for regular plugin nodes (the `schema`/`JsonConfigEditor` branch — NOT `isFixed`, NOT `isSupernode`), insert ABOVE the config editor:

```tsx
        {!isFixed && !isSupernode && (
          <div>
            <label style={labelStyle}>Shared config</label>
            <select
              value={data.configRef ?? ''}
              onChange={(e) => onUpdateConfigRef(node.id, e.target.value || undefined)}
              className="w-full"
              style={{
                padding: '6px 10px',
                borderRadius: 'var(--radius-sm)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-sm)',
                background: 'var(--surface-input)',
                color: 'var(--text-primary)',
                border: '1px solid var(--border)',
              }}
            >
              <option value="">None</option>
              {pluginConfigs
                .filter((p) => p.type === data.pluginType)
                .map((p) => (
                  <option key={p.name} value={p.name}>
                    {p.name}
                  </option>
                ))}
            </select>
            {data.configRef && (
              <div style={{ marginTop: 8 }}>
                <label style={labelStyle}>
                  Inherited from {data.configRef} (local keys below override)
                </label>
                <div
                  style={{
                    padding: '8px 10px',
                    borderRadius: 'var(--radius-sm)',
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-xs)',
                    background: 'var(--surface-sunken)',
                    color: 'var(--text-muted)',
                    border: '1px solid var(--border)',
                    maxHeight: 160,
                    overflowY: 'auto',
                    whiteSpace: 'pre',
                  }}
                >
                  {JSON.stringify(
                    pluginConfigs.find((p) => p.name === data.configRef)?.config ?? {},
                    null,
                    2
                  )}
                </div>
              </div>
            )}
          </div>
        )}
```

The existing config editor below it keeps editing the node's local `config` (the overrides). Update the component doc comment to describe the picker.

- [ ] **Step 2: GraphCanvas.** Add `pluginConfigs: PluginConfigDef[]` to `GraphCanvasProps` (doc-commented; import the type). Add the handler next to `handleUpdateConfig`:

```ts
  const handleUpdateConfigRef = (nodeId: string, ref: string | undefined) => {
    setNodes((nds) =>
      nds.map((n) => (n.id === nodeId ? { ...n, data: { ...n.data, configRef: ref } } : n))
    );
  };
```

Pass both to `<NodeInspector ... pluginConfigs={pluginConfigs} onUpdateConfigRef={handleUpdateConfigRef} />`. At the `App.tsx` callsite, if the build breaks on the new required prop before Task 9 lands, add the temporary `pluginConfigs={[]}` stub (Task 9 replaces it) — note it in the report.

- [ ] **Step 3: Verify + commit**

Run: `cd ui && npm run build` → success.

```bash
git add ui/src/components/NodeInspector.tsx ui/src/components/GraphCanvas.tsx ui/src/App.tsx
git commit -m "feat(ui): shared-config picker with inherited-value display in inspector"
```

---

### Task 9: UI — library section (App + Sidebar + editor panel)

**Files:**
- Create: `ui/src/components/PluginConfigPanel.tsx`
- Modify: `ui/src/App.tsx`, `ui/src/components/Sidebar.tsx`, `ui/src/components/NodeInspector.tsx` (one-line export change)

**Interfaces:**
- Consumes: everything from Tasks 7-8.
- Produces: "Plugin Configs" sidebar section (list/create/delete/select); selecting one opens a form panel instead of the canvas; the canvas gets the real `pluginConfigs` prop.

- [ ] **Step 1: Export the JSON fallback editor.** In `NodeInspector.tsx`, change `function JsonConfigEditor(` to `export function JsonConfigEditor(` (it is reused by the panel; no other change).

- [ ] **Step 2: Create `PluginConfigPanel.tsx`** — a full-height panel replacing the canvas when a shared config is selected:

```tsx
/**
 * Form editor for one shared plugin config: description plus the config
 * body, schema-driven when the profile's plugin type declares a form schema
 * (SchemaForm), raw JSON otherwise (JsonConfigEditor). Saving PUTs the whole
 * definition; the gateway re-resolves and recompiles every referencing
 * policy atomically (400 surfaces in the toast on breaking edits).
 *
 * @module components/PluginConfigPanel
 */
import { useState } from 'react';
import type { PluginConfigDef } from '../types';
import { getPluginMeta } from '../pluginMeta';
import { getPluginConfigSchema } from '../pluginConfig';
import { SchemaForm } from './SchemaForm';
import { JsonConfigEditor } from './NodeInspector';

interface PluginConfigPanelProps {
  /** The shared config being edited (a copy; edits are local until Save). */
  def: PluginConfigDef;
  /** Fires with the edited definition when Save is clicked. */
  onSave: (def: PluginConfigDef) => void;
}

export function PluginConfigPanel({ def, onSave }: PluginConfigPanelProps) {
  const [description, setDescription] = useState(def.description ?? '');
  const [config, setConfig] = useState<Record<string, unknown>>(def.config ?? {});
  const meta = getPluginMeta(def.type);
  const Icon = meta.icon;
  const schema = getPluginConfigSchema(def.type);

  return (
    <div className="flex-1 overflow-y-auto" style={{ background: 'var(--bg-canvas)' }}>
      <div style={{ maxWidth: 560, margin: '32px auto', padding: '0 16px' }}>
        <div className="flex items-center" style={{ gap: 10, marginBottom: 4 }}>
          <span
            className="flex items-center justify-center"
            style={{
              width: 30,
              height: 30,
              borderRadius: 'var(--radius-sm)',
              background: meta.color,
              color: '#fff',
            }}
          >
            <Icon size={15} />
          </span>
          <div>
            <h2
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-md)',
                fontWeight: 600,
                color: 'var(--text-primary)',
                margin: 0,
              }}
            >
              {def.name}
            </h2>
            <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: 0 }}>
              shared config for <code>{def.type}</code>
            </p>
          </div>
        </div>
        <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: '8px 0 16px' }}>
          Nodes referencing this config inherit every key here; their local keys override.
          Saving applies to all of them atomically.
        </p>

        <label
          style={{
            display: 'block',
            fontSize: 'var(--text-xs)',
            fontWeight: 500,
            color: 'var(--text-secondary)',
            marginBottom: 4,
          }}
        >
          Description
        </label>
        <input
          type="text"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          className="w-full"
          style={{
            padding: '6px 10px',
            marginBottom: 16,
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-sm)',
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
        />

        {schema.length > 0 ? (
          <SchemaForm schema={schema} value={config} onChange={setConfig} />
        ) : (
          <JsonConfigEditor key={def.name} config={config} onApply={setConfig} />
        )}

        <button
          onClick={() =>
            onSave({ ...def, description: description || undefined, config })
          }
          className="w-full transition-colors"
          style={{
            marginTop: 16,
            padding: '8px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-sm)',
            fontWeight: 500,
            background: 'var(--accent)',
            color: 'var(--text-on-accent)',
          }}
        >
          Save Plugin Config
        </button>
      </div>
    </div>
  );
}
```

- [ ] **Step 3: App.tsx.** Read the file first; mirror the supernode wiring exactly:
- state: `pluginConfigs: PluginConfigDef[]`, `selectedPluginConfig: string | null`, `createPluginConfigOpen`, plus create-dialog fields (`newPcName`, `newPcType` — default `''`);
- `loadData`: add `api.listPluginConfigs()` to the `Promise.all`;
- three-way exclusive selection: extend `handleSelectRoute`/`handleSelectSupernode` to clear `selectedPluginConfig`, add `handleSelectPluginConfig` clearing the other two;
- render: when `selectedPluginConfig` resolves to a def, render `<PluginConfigPanel key={def.name} def={def} onSave={handleSavePluginConfig} />` in place of `<GraphCanvas .../>`; otherwise render the canvas with the real `pluginConfigs={pluginConfigs}` prop (replacing any Task 8 stub);
- `handleSavePluginConfig`: `api.updatePluginConfig(def.name, def)` → `loadData()` → success toast; catch → error toast with `${e}` (surfaces the 400 text);
- create dialog: name field + a `<select>` of plugin types built from the already-loaded `plugins` catalog, excluding `listener`, `client`, and `script` (mirrors the palette's exclusions; `supernode` and boundary types are not in the catalog). Submit does `api.updatePluginConfig(name, { name, type: newPcType, config: {} })`, reloads, selects it;
- delete confirmation dialog mirroring supernodes (400 text in the toast).

- [ ] **Step 4: Sidebar.tsx.** Add props `pluginConfigs: PluginConfigDef[]`, `selectedPluginConfig: string | null`, `onSelectPluginConfig`, `onCreatePluginConfig`, `onDeletePluginConfig` (doc-commented). Render a third section under Supernodes, same row idiom (eyebrow "Plugin Configs", + New button with `aria-label="New plugin config"` — the accessible-name lesson from E2E-UI-06 — rows showing name + `description || type`, hover delete X, `maxHeight: 260` scroll like the supernodes list).

- [ ] **Step 5: Verify + commit**

Run: `cd ui && npm run build` → success. Quick manual smoke if convenient (`cargo run` + create a config in the UI); otherwise defer to Task 12's sweep.

```bash
git add ui/src/components/PluginConfigPanel.tsx ui/src/components/NodeInspector.tsx ui/src/App.tsx ui/src/components/Sidebar.tsx
git commit -m "feat(ui): plugin config library section and editor panel"
```

---

### Task 10: E2E scenario + testbook

**Files:**
- Create: `e2e/tests/plugin-configs.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: helpers in `e2e/helpers/admin.ts` (`adminApi`, `dataPlane`, `deleteRouteIfPresent` — read the file + `supernodes.spec.ts` first and reuse their idioms).

- [ ] **Step 1: Write the spec.** The scenario proves the feature's core promise — one edit, applied everywhere, including inside a supernode:

```ts
/**
 * Shared plugin config scenarios. See E2E_TESTBOOK.md ("Plugin configs").
 */
import {test, expect} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

const DEF = (body: string) => ({
  name: 'e2e-shared-mock',
  type: 'mocking',
  config: {response_status: 200, response_example: body, content_type: 'text/plain'},
});

test.describe('Plugin configs', () => {
  test('E2E-PC-01: shared config CRUD, one-edit-updates-all, supernode inheritance, delete protection', async () => {
    const api = await adminApi();
    for (const r of ['pc-a', 'pc-b']) await deleteRouteIfPresent(api, r);
    for (const p of ['pc-a-policy', 'pc-b-policy']) await api.delete(`/api/policies/${p}`);
    await api.delete('/api/supernodes/pc-wrap');
    await api.delete('/api/plugin-configs/e2e-shared-mock');

    // Shared config + a supernode whose inner node references it.
    expect((await api.put('/api/plugin-configs/e2e-shared-mock', {data: DEF('v1')})).ok()).toBeTruthy();
    expect(
      (
        await api.put('/api/supernodes/pc-wrap', {
          data: {
            name: 'pc-wrap',
            nodes: [
              {id: 'input', type: 'input', config: {}},
              {id: 'output', type: 'output', config: {}},
              {id: 'error', type: 'error', config: {}},
              {id: 'mock', type: 'mocking', config_ref: 'e2e-shared-mock', config: {}},
            ],
            edges: [
              {from: 'input.out', to: 'mock.in'},
              {from: 'mock.success', to: 'output.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();

    // Route A: direct reference with a local override (status 201).
    // Route B: reference via the supernode.
    expect(
      (
        await api.put('/api/policies/pc-a-policy', {
          data: {
            name: 'pc-a-policy',
            nodes: [
              {id: 'listener', type: 'listener', config: {}},
              {id: 'mock', type: 'mocking', config_ref: 'e2e-shared-mock', config: {response_status: 201}},
              {id: 'client', type: 'client', config: {}},
            ],
            edges: [
              {from: 'listener.out', to: 'mock.in'},
              {from: 'mock.success', to: 'client.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();
    expect(
      (
        await api.put('/api/policies/pc-b-policy', {
          data: {
            name: 'pc-b-policy',
            nodes: [
              {id: 'listener', type: 'listener', config: {}},
              {id: 'sn', type: 'supernode', config: {name: 'pc-wrap'}},
              {id: 'client', type: 'client', config: {}},
            ],
            edges: [
              {from: 'listener.out', to: 'sn.in'},
              {from: 'sn.success', to: 'client.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();
    for (const [route, policy] of [['pc-a', 'pc-a-policy'], ['pc-b', 'pc-b-policy']] as const) {
      expect(
        (
          await api.post('/api/routes', {
            data: {name: route, match: {path: `/${route}/*`, methods: ['GET']}, policy},
          })
        ).ok(),
      ).toBeTruthy();
    }

    const dp = await dataPlane();
    // v1 everywhere; route A's local override wins on status only.
    let a = await dp.get('/pc-a/x');
    expect(a.status()).toBe(201);
    expect(await a.text()).toBe('v1');
    let b = await dp.get('/pc-b/x');
    expect(b.status()).toBe(200);
    expect(await b.text()).toBe('v1');

    // ONE edit to the shared config -> both routes change.
    expect((await api.put('/api/plugin-configs/e2e-shared-mock', {data: DEF('v2')})).ok()).toBeTruthy();
    a = await dp.get('/pc-a/x');
    expect(a.status()).toBe(201); // local override still wins
    expect(await a.text()).toBe('v2');
    b = await dp.get('/pc-b/x');
    expect(await b.text()).toBe('v2');

    // Export keeps the reference form: config_ref present, body text only in the def.
    const yaml = await (await api.get('/api/config/export')).text();
    expect(yaml).toContain('plugin_configs:');
    expect(yaml).toContain('config_ref: e2e-shared-mock');
    expect(yaml.split('v2').length - 1).toBe(1); // materialized copies would duplicate it

    // Delete protection, then teardown order matters: consumers first.
    expect((await api.delete('/api/plugin-configs/e2e-shared-mock')).status()).toBe(400);
    for (const r of ['pc-a', 'pc-b']) await api.delete(`/api/routes/${r}`);
    for (const p of ['pc-a-policy', 'pc-b-policy']) await api.delete(`/api/policies/${p}`);
    // Still referenced by the supernode definition:
    expect((await api.delete('/api/plugin-configs/e2e-shared-mock')).status()).toBe(400);
    await api.delete('/api/supernodes/pc-wrap');
    expect((await api.delete('/api/plugin-configs/e2e-shared-mock')).ok()).toBeTruthy();

    await dp.dispose();
    await api.dispose();
  });
});
```

Adapt helper usage to the real shapes in `e2e/helpers/admin.ts` if they differ (they matched `APIRequestContext` directly for the supernodes spec).

- [ ] **Step 2: Run**

```bash
cargo build --release
cd e2e && npx playwright test plugin-configs.spec.ts   # then the FULL suite: npm test
```
Expected: new spec passes; full suite stays green.

- [ ] **Step 3: Testbook** — add a "Plugin configs" section to `e2e/E2E_TESTBOOK.md` (ID E2E-PC-01) in the existing table format.

- [ ] **Step 4: Commit**

```bash
git add e2e/tests/plugin-configs.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): shared plugin config propagation, overrides, delete protection"
```

---

### Task 11: Docs

**Files:**
- Create: `website/docs/concepts/plugin-configs.md`
- Modify: `website/sidebars.ts` (after `concepts/supernodes`), `website/docs/guides/admin-api.md`, `website/docs/reference/roadmap.md`, `CLAUDE.md`

- [ ] **Step 1: Concept page** — front-matter/tone of `website/docs/concepts/supernodes.md` (read it first). Cover: the copy-paste problem and the one-edit-updates-all promise; YAML (`plugin_configs:` + `config_ref`, using the spec's corp-oidc example); merge semantics (shallow, local wins) with a short before/after table; typing rule; works inside supernode definitions; compile-time resolution (stored form keeps the reference — same discipline as supernode expansion; link to the supernodes page); delete protection; V1 limits (no nesting, no deep merge). Any `upstream` example must use `targets:`.
- [ ] **Step 2: Admin API guide** — add the four `/api/plugin-configs` endpoints, formatted like the supernodes section, incl. 400-on-breaking-PUT and 400-delete-while-referenced.
- [ ] **Step 3: Roadmap + CLAUDE.md** — roadmap: mark shared plugin configs shipped; CLAUDE.md Core features: `- **Shared plugin configs** — named, typed config profiles referenced by nodes via config_ref, resolved at compile time`.
- [ ] **Step 4: Verify + commit**

Run: `cd website && npm run build` → success.

```bash
git add website CLAUDE.md
git commit -m "docs: shared plugin configs concept page, admin API reference, roadmap"
```

---

### Task 12: Final verification

- [ ] `cargo test` full suite green; `cargo fmt --check`; `cargo clippy --all-targets --locked -- -D warnings`.
- [ ] `cargo build --release && cd e2e && npm test` — FULL suite green (release binary embeds the UI — rebuild after any UI change).
- [ ] `cd ui && npm run build` and `cd website && npm run build` green.
- [ ] Manual sweep per spec: create a profile in the UI, attach to a node on one route and to a supernode inner node; verify inherited/override display in the inspector and the canvas badge; edit the profile once and verify both routes change; `GET /api/config/export` shows `config_ref`, not materialized copies.
- [ ] Use superpowers:requesting-code-review, then superpowers:finishing-a-development-branch (PR target: `develop`).
