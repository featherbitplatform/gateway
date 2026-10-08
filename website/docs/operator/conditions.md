---
title: Conditions
description: The status conditions the operator sets on its resources and on FeatherbitGateway, every reason, and where to look when something is wrong.
---

Every failure is visible in one of three places: a webhook rejection, a condition on the object that caused it, or `Ready=False` on the gateway. Events are emitted on the object for exclusions (`Warning` / `Excluded`) and on the gateway for sink and compile failures.

## Resource kinds

The six resource kinds report `status.conditions`, `status.observedGeneration` and `status.gateways: [{name, namespace}]`:

| Type | True | False reasons |
|---|---|---|
| `Accepted` | passes its per-object validation, is not a conflict loser, and (policies) compiles against the selecting gateway's shared objects | `Invalid` (message = validator output), `Conflicted`, `CompileFailed` (policies only; message = compiler output, covers unknown `store:` names and bad plugin config) |
| `ResolvedRefs` | every reference resolves inside the selecting gateway's set | `PolicyNotFound`, `PluginConfigNotFound`, `SupernodeNotFound` |
| `Programmed` | included in at least one gateway's rendered config | `NotSelected`, `Excluded` (Accepted or ResolvedRefs is False), `GatewayNotReady` |

`ResolvedRefs` and `Programmed` are evaluated per selecting gateway; an object selected by two gateways reports the worst result and names both in `status.gateways`. An object selected by no gateway has only `Programmed=False` / `NotSelected`.

A conflict is two selected objects of the same kind and name. The one with the oldest `creationTimestamp` wins (ties are broken by namespace and name order); the loser is excluded with `Accepted=False`, reason `Conflicted`, and a message naming the winner. A route whose policy was excluded for any reason is excluded with `Programmed=False` / `Excluded`.

## FeatherbitGateway

`FeatherbitGateway` reports `status.conditions`, `status.observedGeneration`, `status.configHash`, `status.lastRenderedAt` and `status.counts: {routes, policies, supernodes, pluginConfigs, stores, consumers, excluded}`:

| Type | True | False reasons |
|---|---|---|
| `Ready` | last reconcile rendered and wrote the sink | `InvalidSpec`, `CompileFailed` (message = compiler output), `SinkUnavailable` |

`Ready=False` / `CompileFailed` means the whole surviving set failed the gateway's own load-time check. Nothing was written and the last rendered config stays in place, so the gateway keeps serving what it had. The operator never deletes a rendered config and never writes one that failed this check.

## Where to look when

Follow the same order a change takes through the system, and stop at the first place that explains the symptom.

1. **The webhook message.** `kubectl apply` fails with the validator's own text, prefixed with the object's kind and name, the same string the Admin API would return in a 400. This catches an object judged alone: an unknown node type, an unwired port, a malformed match rule, a bad store or consumer, an invalid `FeatherbitGateway` spec, and a self-contained policy that does not compile. The webhook can be bypassed (a `failurePolicy` override, a bulk restore, objects created before the operator was installed), so reconcile re-runs every check; the webhook only makes feedback faster.
2. **Events from `kubectl describe`.** `kubectl -n <ns> describe <kind> <name>` lists `Warning` / `Excluded` events on an excluded object, and sink or compile failures on the `FeatherbitGateway`. Events age out, so check the conditions if none are shown.
3. **The object's conditions.** `kubectl get featherbit -A` shows `ACCEPTED` and `PROGRAMMED`. For the reason and message:

   ```bash
   kubectl -n <ns> get <kind> <name> -o jsonpath='{range .status.conditions[*]}{.type}={.status} {.reason}: {.message}{"\n"}{end}'
   ```

   `Accepted=False` means look at the object itself (`Invalid`, `Conflicted`, `CompileFailed`). `ResolvedRefs=False` means a name does not exist in the selecting gateway's set, possibly because the target lives in a namespace that gateway does not admit. `Programmed=False` / `NotSelected` means no `FeatherbitGateway` matches: check its `resources.namespaces` and `resources.selector` against the namespace and object labels.
4. **The gateway's `Ready` condition.** `kubectl -n <ns> describe featherbitgateway <name>`. `InvalidSpec` is a spec the schema allowed but the validators did not. `CompileFailed` is a gap between per-object checks and the whole-config compile, for example two routes with the same match. `SinkUnavailable` is a failed ConfigMap or etcd write; the operator retries with exponential backoff capped at 5 minutes.
5. **The gateway itself.** If `Ready=True` and the hash is current, the operator has done its part. In file mode the kubelet takes up to about a minute to refresh the mount before the gateway hot-reloads; in etcd mode every replica converges within the 2 s poll. Check the gateway's own `/readyz`, logs and `featherbit_*` metrics. The operator's `featherbit_operator_rendered_config_info{gateway,hash}` gauge tells you which hash it believes is live.

An object that is `Accepted=True` and `Programmed=True` but absent from a running gateway is almost always a sink or reload delay, not an operator problem.

## Metrics

The operator serves Prometheus metrics at `/metrics` on port 8080, without authentication (cluster-internal). See [Releases](./releases.md#observability) for the metric names.
