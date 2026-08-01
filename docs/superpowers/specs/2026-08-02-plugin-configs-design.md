# Shared Plugin Configs — Design Spec

Date: 2026-08-02
Status: approved (brainstormed and validated with Francesco)

## Context

Plugin instances that share the same configuration — the canonical case is one OIDC client used by many routes — are configured today by copy-pasting the same `config:` block into every node. Updating a credential or discovery URL means editing every copy and hoping none is missed. This feature adds **shared plugin configs**: a named, typed config entity stored once and referenced by any number of plugin nodes across policies and supernode definitions. Updating the shared config updates every referencing instance atomically.

Decisions made during brainstorming:

- **Merge model**: a referencing node keeps its own `config:`; effective config = shared config merged with local keys, **shallow top-level merge, local wins**. The reference stays live — inherited keys always track the shared config.
- **Typed**: each shared config declares the plugin `type` it configures; a node of a different type referencing it is a validation error. This enables the UI picker and the right config form.
- **Full-stack V1**: YAML + Admin API CRUD + etcd + web UI (library section, node-inspector picker with inherited-value display, canvas badge).
- **Compile-time resolution** (same architecture as supernode expansion): refs are materialized in-memory at the compile choke point; stored config always keeps the reference form; zero engine or plugin changes; last-good guarantee preserved.
- **Supernodes**: inner nodes of a supernode definition may carry `config_ref`; resolution runs before expansion, so every instance inherits the resolved config.
- No nesting (a shared config cannot reference another) and no partial/deep merge in V1.

## Design

### 1. Data model (`src/config/gateway.rs`)

New struct:

```rust
pub struct PluginConfigDef {
    pub name: String,                       // unique, referenced by config_ref
    #[serde(rename = "type")]
    pub plugin_type: String,                // plugin type this config is for
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub config: HashMap<String, serde_json::Value>,
}
```

`GatewayConfig` gains `#[serde(default)] pub plugin_configs: Vec<PluginConfigDef>` (fifth section; old configs stay valid; export/etcd/seeding inherit automatically). `NodeConfig` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub config_ref: Option<String>` — fully backward compatible.

YAML:

```yaml
plugin_configs:
  - name: corp-oidc
    type: openid-connect
    description: "Corporate IdP client"
    config:
      client_id: gateway
      client_secret: ${OIDC_SECRET}
      discovery: https://idp.example.com/.well-known/openid-configuration
      scope: openid
```

Usage — any plugin node, in a policy or inside a supernode definition:

```yaml
- id: auth
  type: openid-connect
  config_ref: corp-oidc
  config:
    scope: openid profile   # local key wins over the shared one
```

### 2. Resolution (new module, `src/config/resolve.rs`)

One pure function:

```rust
pub fn resolve_plugin_configs(gw: &GatewayConfig) -> Result<GatewayConfig, String>
```

Returns an in-memory copy where every `config_ref` in **policies and supernode definitions** is materialized:

- effective config = shared `config` ⊕ node `config` (shallow top-level merge, node key wins); `config_ref` cleared on the resolved copy;
- unknown ref → `policy 'p': node 'auth' references unknown plugin config 'x'` (supernode variant names the supernode);
- type mismatch → `node 'auth' (openid-connect) references plugin config 'y' of type 'key-auth'`.

Call sites (the only two compile paths):

- `SharedState::compile_routes` — first step, before supernode validation/expansion, so expansion copies already-resolved inner configs;
- the debug sandbox (`src/admin/debug.rs`) — resolve against the live gateway's `plugin_configs` before expansion, so sandbox runs behave identically (ad-hoc nodes may also carry `config_ref`).

The resolved copy is **never stored**: gateway.yaml, Admin API, etcd, and `GET /api/config/export` always keep the reference form. Env-var interpolation is unchanged — it already happens at compile time on the structured config, so `${VAR}` values inside a shared config resolve exactly like inline ones.

### 3. Validation

All collected before the last-good swap (new checks live with `resolve_plugin_configs` / `compile_routes`):

- duplicate `plugin_configs` names rejected;
- `type` must be a plugin type known to the factory catalog (typo fails at save, not at first request); reserved types rejected (`listener`, `client`, `supernode`, `input`, `output`, `error`);
- every `config_ref` must name an existing shared config whose `type` equals the node's type;
- nodes of type `supernode` must not carry `config_ref`;
- deleting a shared config still referenced anywhere (policy or supernode definition) fails commit validation → Admin API 400, nothing changes (same mechanism as supernodes).

### 4. Admin API (new `src/admin/plugin_configs.rs`, mirror of `supernodes.rs`)

- `GET /api/plugin-configs` — list; `GET /api/plugin-configs/{name}` — fetch (404 absent);
- `PUT /api/plugin-configs/{name}` — upsert, path name wins; commits through `config_store.commit()` (re-resolve + recompile everything; breaking edits → 400 before any change);
- `DELETE /api/plugin-configs/{name}` — 404 absent, 400 while referenced;
- mounted inside the authed block in `build_router`. `/api/plugins` catalog untouched.

### 5. etcd (`src/config_store/etcd.rs`)

Fifth key family `<prefix>/plugin_configs/<name>`: key fn, `write_all` loop, `commit` put-loop + `desired.insert`, `gateway_from_kvs` arm (`bad plugin config '{key}': {e}`), `is_empty` clause, module-doc update. Seeding and convergence identical to the other kinds.

### 6. Web UI (`ui/`)

- **Types/client**: `PluginConfigDef` mirror; `config_ref?: string` on `PolicyNode`; CRUD methods for `/api/plugin-configs`.
- **Library**: "Plugin Configs" sidebar section (same idiom as Supernodes): create (name + plugin-type picker from the catalog), edit in a form view reusing `SchemaForm`/`pluginConfig` schemas keyed by the profile's type (raw-JSON fallback for types without a schema), delete (400 surfaces in the toast).
- **Node inspector**: for plugin nodes, a "Shared config" picker listing profiles of the node's type plus "None". With one attached: inherited values shown read-only/dimmed, local `config` edited as overrides, "detach" clears `config_ref`. Same inspector serves the supernode editor, so inner nodes get the feature for free.
- **Canvas**: nodes with `config_ref` show a small link badge with the profile name (no new node type).

### 7. Testing

- Unit — resolution: merge precedence (local wins, inherited keys survive), unknown ref, type mismatch, refs inside supernode definitions, ref + expansion together (instance inherits resolved config), no-ref passthrough unchanged, `config_ref` cleared in resolved output; validation: every rule above; serde round-trip incl. `config_ref` omitted when absent.
- Admin API — CRUD incl. delete-while-referenced → 400 for both a policy reference and a supernode-inner reference.
- etcd — fifth key family (parse, bad JSON, `is_empty`).
- E2E (Playwright) — create a shared config via API; two routes whose policies reference it; both serve; update the shared config once and verify both routes change behavior; delete-protection; export contains `plugin_configs:` and the reference form (no materialized copies).
- Docs — concept page under `website/docs/concepts/`, Admin API guide entries, roadmap update, CLAUDE.md core-features bullet.

## Critical files

| Area | Files |
|---|---|
| Config model | `src/config/gateway.rs`, `src/config/mod.rs` |
| Resolution | `src/config/resolve.rs` (new), `src/state.rs` (`compile_routes`), `src/admin/debug.rs` |
| Validation | `src/config/resolve.rs` + `src/state.rs` (catalog check needs `crate::plugins`) |
| Admin API | `src/admin/plugin_configs.rs` (new), `src/admin/mod.rs` |
| etcd | `src/config_store/etcd.rs` |
| UI | `ui/src/types/index.ts`, `ui/src/api/client.ts`, `ui/src/App.tsx`, `ui/src/components/Sidebar.tsx`, `ui/src/components/NodeInspector.tsx`, `ui/src/components/PluginNode.tsx`, new library editor component |
| E2E/docs | `e2e/tests/`, `e2e/E2E_TESTBOOK.md`, `website/docs/` |

## Verification

- `cargo test` green incl. new suites; fmt + clippy (`-D warnings`) clean
- `cargo build --release && cd e2e && npm test` — full suite incl. the new scenario
- Manual: create `corp-oidc`-style profile in the UI, attach to nodes on two routes (one inside a supernode), verify inherited/override display; update the profile once; verify both routes pick it up; `GET /api/config/export` shows the reference form only
