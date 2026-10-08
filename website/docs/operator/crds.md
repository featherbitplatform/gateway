---
title: CRD reference
description: Every featherbit.io/v1alpha1 kind - the six resource kinds that mirror gateway.yaml, and the FeatherbitGateway binding, field by field.
---

All kinds live in the API group `featherbit.io`, version `v1alpha1`, and are namespaced. They carry the category `featherbit`, so `kubectl get featherbit -A` lists every kind. The schemas below are generated from the gateway's own serde types (`cargo run -- crds` in the operator repository prints the full CRD YAML), and the CRD manifests ship in the operator chart's `crds/` directory.

`kubectl explain` works against the installed CRDs:

```bash
kubectl explain policy.spec.nodes
kubectl explain featherbitgateway.spec.sink
```

| Kind | Short name | Spec is the gateway's | Printer columns |
|---|---|---|---|
| `Route` | `fbroute` | `RouteConfig` minus `name` | `Policy`, `Path`, `Accepted`, `Programmed`, `Age` |
| `Policy` | `fbpolicy` | `PolicyConfig` minus `name` | `Accepted`, `Programmed`, `Age` |
| `Supernode` | `fbsupernode` | `SupernodeConfig` minus `name` | `Accepted`, `Programmed`, `Age` |
| `PluginConfig` | `fbpluginconfig` | `PluginConfigDef` minus `name` | `Accepted`, `Programmed`, `Age` |
| `Store` | `fbstore` | `StoreConfig` minus `name` | `Accepted`, `Programmed`, `Age` |
| `Consumer` | `fbconsumer` | `ConsumerConfig` minus `name` | `Accepted`, `Programmed`, `Age` |
| `FeatherbitGateway` | `fbgw` | the binding described [below](#featherbitgateway) | `Ready`, `Routes`, `Policies`, `Hash`, `Age` |

## The six resource kinds

For these kinds `metadata.name` becomes the gateway-level `name`, and `spec` is the gateway's own configuration type with the `name` field removed. They are written exactly as the matching entry in `gateway.yaml`, so the existing reference pages describe every field and this page does not repeat them. The schema is structural: the API server prunes unknown fields in known structures, the webhook rejects them, and opaque plugin `config` maps accept anything (their keys are validated by the plugin when the policy compiles).

Names are flat per gateway, not per namespace. Two selected objects of the same kind and name are a conflict, and the oldest `creationTimestamp` wins (see [Conditions](./conditions.md)). The operator never rewrites references such as `spec.policy`, `config_ref`, or a `store:` name inside plugin config.

Secrets stay `${ENV}` / `${ENV:-default}` placeholders in every field. They are rendered verbatim and resolved inside the gateway pod from `extraEnv` / `extraEnvFrom`.

### Route

`spec` is the gateway's route minus `name`; see [Routing](../guides/routing.md).

| Field | Type | Required | Description |
|---|---|---|---|
| `match` | object | yes | Request match rule: `path`, `methods`, `headers`, `host`, `hosts`. All but `path` default to empty (any). |
| `policy` | string | yes | Name of the `Policy` to run. Must resolve to a selected, accepted policy, otherwise the route is excluded with `ResolvedRefs=False` / `PolicyNotFound`. |

### Policy

`spec` is the gateway's policy minus `name`; see [Policies and graphs](../concepts/policies-and-graphs.md) and the [plugin reference](../reference/plugins/index.md) for every node type and its config.

| Field | Type | Required | Description |
|---|---|---|---|
| `nodes` | list | no (default `[]`) | Plugin nodes: `id`, `type`, `config` (free-form), optional `config_ref`, optional `position`. |
| `edges` | list | no (default `[]`) | Connections `from: node.port` to `to: node.port`. |
| `error_handler` | string | no | Id of the node to jump to when a node errors without an explicit error edge. |

A policy with no `config_ref` on any node and no `type: supernode` instance is self-contained and is compiled standalone at admission. Otherwise it is checked structurally at admission and compiled at reconcile, where failures surface as `Accepted=False` / `CompileFailed` or `ResolvedRefs=False`.

### Supernode

`spec` is the gateway's supernode minus `name`; see [Supernodes](../concepts/supernodes.md).

| Field | Type | Description |
|---|---|---|
| `description` | string | Optional human-readable description. |
| `nodes` | list | Inner plugin nodes plus the boundary pseudo-nodes (`input`, one or more `output`, one or more `error`). |
| `edges` | list | Connections; boundary edges use `input.out`, `output.in` and `error.in`. |

Reference a supernode from a policy with a node of `type: supernode` and `config: {name: <supernode name>}`. An unresolved name is `ResolvedRefs=False` / `SupernodeNotFound`.

### PluginConfig

`spec` is the gateway's shared plugin config minus `name`; see [Plugin configs](../concepts/plugin-configs.md).

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | string | yes | Plugin type; only nodes of the same type may reference it. Must be a known node type. |
| `config` | object | no | The shared plugin configuration (free-form, validated by the plugin). |
| `description` | string | no | Optional description. |

A node references it with `config_ref: <name>`. An unresolved name is `ResolvedRefs=False` / `PluginConfigNotFound`.

### Store

`spec` is the gateway's store minus `name`; see [Stores](../concepts/stores.md).

| Field | Type | Default | Description |
|---|---|---|---|
| `type` | string | required | `redis` or `valkey` (aliases for the same backend). |
| `url` | string | required | `redis://` or `rediss://` URL; `${ENV}` placeholders pass through. |
| `password` | string | none | Overrides a password embedded in `url`; use a `${ENV}` placeholder. |
| `key_prefix` | string | `fb` | Namespace prefix for every key the store writes. |
| `connect_timeout_ms` | integer | `2000` | Timeout for one connect attempt and for `ping`. |
| `connect_budget_ms` | integer | `5000` | Total budget for the first connection, across retries. |
| `tls.ca_cert_path` | string | none | PEM CA bundle for a private CA on a `rediss://` store. |
| `description` | string | none | Optional description. |

`topology` and `urls` are reserved for HA topologies and are rejected in this gateway version.

### Consumer

`spec` is the gateway's consumer minus `name`; the consumer concept is covered in [Configuration](../guides/configuration.md) and the authentication [plugins](../reference/plugins/index.md).

| Field | Type | Description |
|---|---|---|
| `credentials` | map | Per-auth-plugin credentials keyed by plugin type, for example `key-auth: {key: ...}` or `basic-auth: {username: ..., password: ...}`. Use `${ENV}` placeholders for secrets. |
| `group` | string | Optional consumer group. |
| `labels` | map of strings | Free-form labels; `custom_id` is special-cased into the `X-Consumer-Custom-ID` header. |

### Status of the six kinds

| Field | Description |
|---|---|
| `status.conditions` | `Accepted`, `ResolvedRefs`, `Programmed`; see [Conditions](./conditions.md). |
| `status.observedGeneration` | The `metadata.generation` the conditions were computed from. |
| `status.gateways` | List of `{name, namespace}` of the `FeatherbitGateway` objects whose rendered config includes this object. |

## FeatherbitGateway

The binding between a gateway installation and the resources it accepts. It also says where the rendered config is written. Nothing else in the operator names a gateway.

```yaml
apiVersion: featherbit.io/v1alpha1
kind: FeatherbitGateway
metadata: {name: edge, namespace: gateway-system}
spec:
  sink:                                 # exactly one of configMap / etcd
    configMap:
      name: edge-gateway-config
  resources:
    namespaces:
      from: Selector
      selector: {matchLabels: {team: shop}}
    selector: {matchLabels: {gateway: edge}}
```

### spec

| Field | Type | Default | Description |
|---|---|---|---|
| `sink` | object | required | Where the rendered config is written. Exactly one of `configMap` or `etcd` must be set. |
| `sink.configMap.name` | string | required with `configMap` | ConfigMap in the gateway's namespace; the operator creates it and owns its `gateway.yaml` key. A lowercase DNS label. Must equal the gateway chart's `config.gatewayConfigMap`. |
| `sink.etcd.endpoints` | list of strings | required with `etcd` | etcd HTTP endpoints; at least one. Same values as the gateway's `config.etcd.endpoints`. |
| `sink.etcd.prefix` | string | `/featherbit` | Key prefix. Must start with `/` and must not be the root: the operator reconciles every key under it. Same as the gateway's `config.etcd.prefix`. |
| `sink.etcd.timeoutMs` | integer | `3000` | Per-request timeout. |
| `sink.etcd.credentialsSecretRef.name` | string | none | Secret in the gateway's namespace with keys `user` and `password`, the same shape as the gateway chart's `config.etcd.existingSecret`. |
| `resources.namespaces.from` | `Same` / `All` / `Selector` | `Same` | Which namespaces may contribute resources: the binding's own namespace, every namespace, or those matching `selector`. |
| `resources.namespaces.selector` | label selector | none | Namespace label selector. Required when `from` is `Selector`. |
| `resources.selector` | label selector | none | Optional label selector applied to the resource objects themselves. |

The webhook (and the first step of every reconcile) rejects a spec with zero or two sinks, a `Selector` without a selector, empty etcd `endpoints`, a root or relative etcd `prefix`, and a ConfigMap name that is not a DNS label.

The binding is changed with ordinary `kubectl apply`. Deleting it leaves the last rendered ConfigMap or etcd prefix in place.

### status

| Field | Description |
|---|---|
| `conditions` | One condition type, `Ready`. See [Conditions](./conditions.md). |
| `observedGeneration` | The `metadata.generation` the status was computed from. |
| `configHash` | SHA-256 of the last rendered `gateway.yaml`. Shown as the `Hash` column. |
| `lastRenderedAt` | When the config was last written to the sink. |
| `counts` | Objects included in the rendered config: `routes`, `policies`, `supernodes`, `pluginConfigs`, `stores`, `consumers`, plus `excluded` for objects left out. |

```bash
kubectl get featherbitgateways -A
# NAME   READY   ROUTES   POLICIES   HASH           AGE
```
