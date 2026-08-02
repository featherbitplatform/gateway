---
title: Shared Plugin Configs
description: Named, typed config profiles referenced by nodes via config_ref — update the profile once, every referencing instance picks it up.
---

Plugin instances that share the same configuration — the canonical case is one OIDC client used by many routes — are usually configured by copy-pasting the same `config:` block into every node. Updating a credential or a discovery URL then means editing every copy and hoping none is missed. A **shared plugin config** is a named, typed config entity stored once, top-level in `gateway.yaml` under `plugin_configs:`, and referenced by any number of plugin nodes across policies and [supernode](supernodes.md) definitions via `config_ref`. Editing the shared config updates every referencing instance atomically — the same one-edit-updates-all promise supernodes make for subgraphs, applied to plugin config instead.

## Defining a shared config

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

`type` names the plugin this profile configures — it's what lets the Admin API and the UI validate a reference and pick the right config form. `config` is the shared key/value block, with the same `${VAR}` interpolation as anywhere else in `gateway.yaml`.

## Referencing a shared config

Any plugin node — in a policy or inside a supernode definition — can attach a shared config with `config_ref`, alongside its own `config:`:

```yaml
- id: auth
  type: openid-connect
  config_ref: corp-oidc
  config:
    scope: openid profile   # local key wins over the shared one
```

## Merge semantics

The effective config a node runs with is the shared config merged with the node's own `config:` — **shallow, top-level, local wins**. Keys the node doesn't set are inherited from the shared config; keys it does set (including a key explicitly set to `null`) override the shared value.

| Key | `corp-oidc` (shared) | `auth` node (local) | Effective |
|---|---|---|---|
| `client_id` | `gateway` | — | `gateway` (inherited) |
| `scope` | `openid` | `openid profile` | `openid profile` (local wins) |
| `discovery` | `https://idp.example.com/...` | — | `https://idp.example.com/...` (inherited) |

Setting a local key to `null` still counts as the node setting it, so it's the way to blank an inherited key rather than take the shared value — the merge is a plain overwrite, not a "skip if absent" merge, and `null` is a value like any other.

## Typing

A shared config's `type` must match the `type` of every node that references it — a `key-auth` node cannot reference an `openid-connect` profile. This is checked at save time (`PUT`, `POST`, or a `gateway.yaml` reload), not deferred to the first request that hits the node, and it's what lets the web UI's node inspector offer only the profiles that fit the node it's editing.

## Inside supernodes

Nodes inside a [supernode](supernodes.md) definition may carry `config_ref` exactly like a policy node. Resolution runs before supernode expansion, so every instance of the supernode inherits the already-resolved config for its inner nodes — attach a shared config to the inner node once, in the definition, and every instance gets it.

## Compile-time resolution

Resolution follows the same discipline as supernode expansion: `config_ref` is materialized in-memory at the compile choke point (before supernode expansion, so expanded instances carry already-resolved configs), and the resolved copy is never persisted. `gateway.yaml`, the Admin API, `GET /api/config/export`, and etcd always keep the reference form — a node's stored config still reads `config_ref: corp-oidc`, not the expanded key/value block. This also means the last-good guarantee holds the same way it does for policies and supernodes: an edit that breaks resolution (an unknown reference, a type mismatch) is rejected before the swap, and the previously compiled routes keep running.

## Delete protection

Deleting a shared config that's still referenced anywhere — a policy node or a supernode-definition node — fails commit validation: the Admin API responds `400` and nothing changes, the same mechanism [supernodes](supernodes.md#using-a-supernode-from-a-policy) use to protect a definition still in use.

## V1 limits

- **No nesting.** A shared config cannot reference another shared config.
- **No deep merge.** The merge is shallow and top-level only — if a key's value is itself an object, the node's value replaces the shared value wholesale; there's no field-by-field merge inside nested structures.

## Export and seeding

`PluginConfigDef` is part of `GatewayConfig`, so shared configs travel with the rest of the config wherever it does: `GET /api/config/export` (see the [Admin API guide](../guides/admin-api.md)) includes a `plugin_configs:` section alongside `routes:`, `policies:`, and `supernodes:`, and a `gateway.yaml` that has one seeds it into a fresh instance or etcd cluster on first load.
