---
title: Operator
description: The featherbit Kubernetes operator - routes, policies and the shared kinds as custom resources, validated at admission with the gateway's own code and rendered into chart-installed gateways.
---

The featherbit operator makes the gateway's configuration a set of Kubernetes resources. Routes, policies, supernodes, plugin configs, stores and consumers become namespaced custom resources in the `featherbit.io/v1alpha1` API group. They are validated at admission with the gateway's own validation code, and the operator renders them into gateways that are still installed with the [Helm chart](../guides/deployment.md#kubernetes-helm). The operator owns configuration only: the chart stays the install path, and the operator never restarts pods or touches `system.yaml`.

It lives in its own repository, [`featherbitplatform/gateway-operator`](https://github.com/featherbitplatform/gateway-operator), and is documented here next to the chart and the Admin API.

```bash
kubectl get featherbit -A     # every operator kind, in every namespace
```

## How it works

A `FeatherbitGateway` object binds one gateway installation to the namespaces and labels it accepts resources from. Resources never name a gateway. For each binding the operator selects the matching objects, excludes the invalid ones with a condition, compiles what is left with the gateway's own compiler, and writes the result to the binding's sink.

```text
Route/Policy/... CRs ──watch──▶ operator ──select+validate+compile──▶ sink
                                                                      ├── ConfigMap gateway.yaml ──kubelet──▶ /etc/gateway ──notify──▶ gateway hot-reload
                                                                      └── etcd <prefix>/<kind>/<name> ──2s poll──▶ every gateway replica
```

- **Admission webhook.** `kubectl apply` of a `Policy` with an unwired port, an unknown node type or a malformed graph fails with the same message the [Admin API](../guides/admin-api.md) would return. The webhook judges one object alone; a policy that depends on shared objects (`config_ref`, supernodes) is judged structurally at admission and compiled at reconcile.
- **Per-object exclusion.** An invalid object gets `Accepted=False` (or `ResolvedRefs=False`) and is left out of the rendered config. Other teams' valid objects still ship. See [Conditions](./conditions.md).
- **Safe writes.** The whole surviving set is compiled with the gateway's load-time check before anything is written. If that fails, the last rendered config stays in place and the `FeatherbitGateway` reports `Ready=False`. The operator never deletes a rendered config, and deleting a `FeatherbitGateway` leaves the last ConfigMap or etcd prefix untouched so a pod restart of a running gateway keeps working.
- **Names are flat per gateway**, exactly as in `gateway.yaml`. `spec.policy: api` refers to whichever selected `Policy` is named `api` in any admitted namespace. Two selected objects of the same kind and name are a conflict: the oldest `creationTimestamp` wins and the loser is excluded with reason `Conflicted`.
- **Secrets stay placeholders.** `${ENV}` and `${ENV:-default}` are rendered verbatim and resolve inside the gateway pod from `extraEnv` / `extraEnvFrom`, as with the chart today.

The reconciled config is rendered with the gateway's own serde types, so the file is byte-identical to what the Admin API's export produces for the same config.

## The CRDs are the source of truth

:::warning
On an operator-managed gateway the custom resources win. Edits made through the Admin API, the web UI or the MCP write tools are not written back to the resources: in file mode they live in memory and are overwritten by the next hot-reload, and in etcd mode they are durable but are overwritten by the next reconcile. Use the UI to inspect and debug, not to edit.
:::

## Two sinks

Pick the sink that matches the gateway's `config.source`. A `FeatherbitGateway` sets exactly one.

| | ConfigMap sink | etcd sink |
|---|---|---|
| Gateway install | `config.source: file` (default) with `config.gatewayConfigMap: <name>` | `config.source: etcd` with `config.etcd.*` and `config.gateway: {}` |
| `FeatherbitGateway` | `spec.sink.configMap.name` | `spec.sink.etcd` (`endpoints`, `prefix`, `timeoutMs`, `credentialsSecretRef`) |
| What the operator writes | key `gateway.yaml` of a ConfigMap in the gateway's namespace, by server-side apply | one JSON document per object under `<prefix>/<kind>/<name>`, using the gateway's own reconcile code |
| How the gateway picks it up | kubelet refreshes the projected volume (up to about a minute), then the file watcher hot-reloads | every replica re-reads the prefix every 2 s |
| Replicas | each replica mounts the same ConfigMap | all replicas converge on the same prefix |
| Drift | a manual edit or deletion of the ConfigMap is re-rendered on the next reconcile | the prefix is re-applied by the 10-minute requeue |

Use the **ConfigMap sink** unless you already run the gateway as an etcd cluster. In etcd mode the gateway never watches `gateway.yaml` after its first boot (it only seeds an empty prefix), so a ConfigMap could never reach it; that is why the etcd sink exists. `config.gatewayConfigMap` needs gateway chart 0.16 or later (or `develop` until it is released).

The ConfigMap carries `app.kubernetes.io/managed-by=featherbit-operator` and an annotation naming the gateway that rendered it. Because deleting a `FeatherbitGateway` leaves it in place, removing a gateway for good is a manual cleanup step: delete the ConfigMap, or the etcd prefix.

## Validation without the gateway's filesystem

The operator validates outside the gateway process, where the gateway's files do not exist. It therefore uses an offline form of the gateway's whole-config check that does not read files at compile time: `script.source`, the `google-cloud-logging` `auth_file` and a store's `tls.ca_cert_path` are not read, and everything else is validated exactly as online. The gateway itself checks those file-backed values when it loads the rendered config. The offline check is added in [gateway PR #90](https://github.com/featherbitplatform/gateway/pull/90), which is still open, so this operator depends on it landing in the gateway library.

## Version coupling

The operator pins one gateway library version. Its webhook knows that version's node catalog, and its CRD schemas are generated from that version's types.

| Rule | Why |
|---|---|
| Run an operator version **at least as new** as the gateways it serves. | An older operator rejects node types a newer gateway accepts. |
| Operator minor versions track gateway minor versions (`0.16.x` operator with `0.16.x` gateway). Patch versions are independent. | The node catalog and config types change in minor releases. |
| Schema changes within `v1alpha1` are additive only. | A field removal requires a new API version. |

See [Releases](./releases.md) for the artifacts and how to verify them.

## Where next

- [Getting started](./getting-started.md): install the operator, bind a gateway, apply a route and a policy.
- [CRD reference](./crds.md): every kind and field.
- [Conditions](./conditions.md): what each status condition means and where to look when something is wrong.
- [Releases](./releases.md): images, charts, SBOMs and metrics.

## Not in v1

Gateway API (`GatewayClass`, `Gateway`, `HTTPRoute`) support, operator-managed gateway Deployments and Services, pushing configuration through the Admin API, `secretRef` fields on stores and consumers, leader election (the operator runs as a single replica) and conversion webhooks are planned for later versions. See the [roadmap](../reference/roadmap.md).
