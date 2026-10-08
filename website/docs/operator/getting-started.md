---
title: Getting started
description: Install the operator, bind a chart-installed gateway to a ConfigMap or etcd sink, apply a route and a policy, and find out what to check when something is wrong.
---

This walkthrough installs the operator, binds a chart-installed gateway to it, and serves a first route from custom resources. It uses the ConfigMap sink; the [etcd variant](#the-etcd-sink-variant) follows. You need a cluster, `kubectl` and `helm` 3.10 or later.

## 1. Install the operator

```bash
helm install featherbit-operator oci://ghcr.io/featherbitplatform/charts/featherbit-operator \
  --namespace featherbit-system --create-namespace
kubectl -n featherbit-system rollout status deploy/featherbit-operator
```

The chart installs the seven CRDs (from its `crds/` directory, so Helm never deletes them), the operator Deployment and RBAC, and a fail-closed validating webhook. By default Helm generates the webhook's CA and serving certificate and keeps them across upgrades. Under ArgoCD or Flux in template mode, where Helm's `lookup` is unavailable, set `webhook.certManager.enabled=true` instead.

:::note
While the operator is down, creating or updating `featherbit.io` resources is rejected by the webhook (`webhook.failurePolicy: Fail`). Resources created before the operator starts are reconciled once it is running.
:::

## 2. Install a gateway that reads its config from a ConfigMap

```bash
helm install edge oci://ghcr.io/featherbitplatform/charts/featherbit-gateway \
  --namespace gateway-system --create-namespace \
  --set config.gatewayConfigMap=edge-gateway-config
```

With `config.gatewayConfigMap` set, the gateway chart renders `system.yaml` only and mounts `gateway.yaml` from the named ConfigMap, which must live in the release namespace. The ConfigMap is optional at pod start: until the operator renders it the gateway serves no routes, and it hot-reloads once the file appears. This value needs gateway chart 0.16 or later, or `develop` until it is released. See [Deployment](../guides/deployment.md#your-configuration).

## 3. Bind the gateway

A `FeatherbitGateway` says where to write and which resources to accept:

```bash
kubectl apply -f - <<'YAML'
apiVersion: featherbit.io/v1alpha1
kind: FeatherbitGateway
metadata:
  name: edge
  namespace: gateway-system
spec:
  sink:
    configMap:
      name: edge-gateway-config
  resources:
    namespaces:
      from: Same
YAML

kubectl -n gateway-system get featherbitgateways
```

`resources.namespaces.from: Same` (the default) accepts resources from the binding's own namespace only. Use `All`, or `Selector` with a namespace label selector, to let other teams' namespaces contribute. An optional `resources.selector` further filters the objects by label. Within a few seconds `READY` shows `True` and the ConfigMap exists, without routes:

```bash
kubectl -n gateway-system get configmap edge-gateway-config -o jsonpath='{.data.gateway\.yaml}'
```

## 4. Apply a policy and a route

The policy is written exactly as in the [gateway docs](../concepts/policies-and-graphs.md); `metadata.name` becomes its gateway-level name, and `spec` is the policy minus `name`:

```bash
kubectl apply -f - <<'YAML'
apiVersion: featherbit.io/v1alpha1
kind: Policy
metadata:
  name: hello
  namespace: gateway-system
spec:
  nodes:
    - {id: listener, type: listener}
    - id: mock
      type: mocking
      config:
        response_example: '{"gateway": "featherbit", "path": "$uri"}'
    - {id: client, type: client}
  edges:
    - {from: listener.out, to: mock.in}
    - {from: mock.success, to: client.in}
---
apiVersion: featherbit.io/v1alpha1
kind: Route
metadata:
  name: hello
  namespace: gateway-system
spec:
  match: {path: /hello}
  policy: hello
YAML

kubectl -n gateway-system get routes,policies
```

Both objects show `ACCEPTED` and `PROGRAMMED` as `True`. The operator has rendered the whole config, written it to the ConfigMap, and recorded its hash:

```bash
kubectl -n gateway-system get featherbitgateways edge -o jsonpath='{.status.configHash}{"\n"}'
kubectl -n gateway-system get configmap edge-gateway-config -o jsonpath='{.data.gateway\.yaml}'
```

## 5. Call it

The kubelet refreshes the mounted ConfigMap within about a minute, and the gateway then hot-reloads. After that:

```bash
kubectl -n gateway-system port-forward svc/edge-featherbit-gateway 8080:80 &
curl -i http://localhost:8080/hello
```

The service name is the Helm release name followed by `-featherbit-gateway` (just the release name when it already contains `featherbit-gateway`).

## 6. See validation work

Apply a policy whose `mocking` node has no `success` edge. The webhook rejects it at `kubectl apply`, with the gateway's own compiler message prefixed by the object's kind and name:

```bash
kubectl apply -f - <<'YAML'
apiVersion: featherbit.io/v1alpha1
kind: Policy
metadata: {name: broken, namespace: gateway-system}
spec:
  nodes:
    - {id: listener, type: listener}
    - {id: mock, type: mocking, config: {response_example: "x"}}
    - {id: client, type: client}
  edges:
    - {from: listener.out, to: mock.in}
YAML
```

Now apply a route that names a policy which does not exist:

```bash
kubectl apply -f - <<'YAML'
apiVersion: featherbit.io/v1alpha1
kind: Route
metadata: {name: orphan, namespace: gateway-system}
spec:
  match: {path: /orphan}
  policy: missing
YAML

kubectl -n gateway-system get routes
kubectl -n gateway-system get route orphan -o jsonpath='{.status.conditions}'
```

The `orphan` route is accepted by the webhook (a route is checked alone), but reconcile finds that `missing` does not resolve: `ResolvedRefs=False` with reason `PolicyNotFound`, and `Programmed=False` with reason `Excluded`. The `hello` route stays `Programmed=True` and keeps serving. Delete `orphan` when you are done.

## The etcd sink variant

For a gateway running as an etcd cluster, the gateway ignores `gateway.yaml` after its first boot, so the operator writes the etcd prefix directly. Install the gateway chart in etcd mode with an empty seed:

```bash
helm install edge oci://ghcr.io/featherbitplatform/charts/featherbit-gateway \
  --namespace gateway-system --create-namespace \
  --set replicaCount=2 \
  --set config.source=etcd \
  --set 'config.etcd.endpoints={http://etcd.gateway-system.svc:2379}' \
  --set-string config.gatewayRaw='routes: []'
```

The empty seed matters: the gateway seeds an empty prefix from its local `gateway.yaml` on first boot, and the chart's default seed would add demo routes that the first reconcile then has to replace. Values-file form: `config: { gatewayRaw: "routes: []" }`. `config.gateway: {}` does not work because Helm merges it with the chart defaults. The chart does not run etcd; point `endpoints` at yours.

Then bind with the etcd sink, using the same endpoints and prefix as the gateway:

```bash
kubectl apply -f - <<'YAML'
apiVersion: featherbit.io/v1alpha1
kind: FeatherbitGateway
metadata:
  name: edge
  namespace: gateway-system
spec:
  sink:
    etcd:
      endpoints: ["http://etcd.gateway-system.svc:2379"]
      prefix: /featherbit
      # credentialsSecretRef:      # optional Secret with keys `user` and `password`
      #   name: edge-etcd
YAML
```

Apply the same `Policy` and `Route` as above. There is no ConfigMap to inspect; the objects are one JSON document each under `<prefix>/routes/<name>`, `<prefix>/policies/<name>` and so on, readable with `etcdctl`. Every gateway replica converges within one poll interval (2 s). The Admin API and UI stay usable for inspection, but edits are overwritten by the next reconcile: the custom resources remain the source of truth.

## What to check when something is wrong

Work from the outside in; see [Conditions](./conditions.md) for the full reason tables.

1. **`kubectl apply` failed.** The webhook message is the validator's output, prefixed with the kind and name. Fix the object and re-apply. If the error is a timeout or `connection refused` on the webhook, the operator is down or its certificate is wrong: check `kubectl -n featherbit-system get pods` and its logs.
2. **Applied, but not serving.** `kubectl get featherbit -A` shows `ACCEPTED` and `PROGRAMMED` per object. `kubectl describe` on the object shows the failing condition and a `Warning` / `Excluded` event.
3. **Nothing is excluded but the config is stale.** `kubectl -n gateway-system describe featherbitgateway edge`: `Ready=False` with `CompileFailed` means the whole-config check failed and the last rendered config is still in place; `SinkUnavailable` means the ConfigMap or etcd write failed.
4. **Rendered but the gateway has not changed.** In file mode the kubelet needs up to about a minute to refresh the mount. Check the gateway's own `/readyz` and logs. In etcd mode check that the gateway's `config.etcd` prefix equals the binding's `spec.sink.etcd.prefix`.
5. **Not selected.** An object with only `Programmed=False` / `NotSelected` is not matched by any binding: check `resources.namespaces.from` and the namespace and object labels.
