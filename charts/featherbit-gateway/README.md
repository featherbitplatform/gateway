# featherbit-gateway

Helm chart for [featherbit](https://featherbitplatform.github.io/gateway/), a single-binary API gateway with node-graph routing policies.

## Install

```bash
helm install featherbit oci://ghcr.io/featherbitplatform/charts/featherbit-gateway --version <chart version>
# or, the same chart from Docker Hub
helm install featherbit oci://registry-1.docker.io/featherbit/featherbit-gateway --version <chart version>
```

The chart version equals the gateway version it installs. A bare install serves a `/hello` mock route and an `/api/*` route to `${UPSTREAM_HOST}`; replace `config.gateway` with your own routes and policies.

```bash
kubectl port-forward svc/featherbit-featherbit-gateway-admin 9090:9090   # admin UI, user admin
kubectl get secret featherbit-featherbit-gateway-admin -o jsonpath='{.data.password}' | base64 -d
```

## Exposure

| Setup | Values |
|---|---|
| featherbit *is* the edge | `service.type=LoadBalancer` (plus `tls.existingSecret` or ACME under `config.system`) |
| behind an Ingress controller | `ingress.enabled=true`, `ingress.hosts[0].host=…` |
| behind a Gateway API Gateway | `httpRoute.enabled=true`, `httpRoute.parentRefs[0].name=…` |

The admin Service is `ClusterIP` and has no Ingress unless `adminIngress.enabled=true`. It carries Basic Auth credentials on every request and has full write access to the configuration; keep it internal.

## Configuration

- `config.system` is deep-merged over the chart's default `system.yaml`. Any gateway key works: `timeouts`, `http2`, `debug`, `cache`, `acme`, `stream`, `admin.tls`.
- `config.gateway` is the full `gateway.yaml` as a map (or `config.gatewayRaw` as text). `${ENV}` placeholders pass through to the gateway and resolve from the pod environment (`extraEnv`, `extraEnvFrom`).
- `config.scripts` mounts Lua files under `/etc/gateway/plugins`.
- Changing `system.yaml` rolls the pods (checksum annotation). Changing `gateway.yaml` or a script hot-reloads in place once the kubelet refreshes the ConfigMap mount.
- `config.source=etcd` with `config.etcd.endpoints` moves routes/policies into etcd (shared by every replica, Admin API edits persist). The chart does not run etcd for you.

## Credentials

The chart creates a Secret with `username`/`password`; an empty `admin.password` is generated once and kept across upgrades. `admin.existingSecret` (keys `username`, `password`) replaces it. MCP tokens go in the same Secret (`mcp.tokens[].value`) or in `mcp.existingSecret` keyed by token name.

Health probes hit `/healthz` and `/readyz`, which are behind Basic Auth. The chart sends the header when it knows the password (explicit `admin.password`, or a previously stored one on `helm upgrade`); otherwise probes fall back to a TCP check. Set `probes.authHeader` to force HTTP probes with an existing Secret.

## TLS and ACME

`tls.existingSecret` mounts a `kubernetes.io/tls` Secret and wires `tls.cert_path`/`key_path`; rotations are picked up by the gateway's cert watcher. ACME (TLS-ALPN-01) is configured under `config.system.acme` and `config.system.tls.acme`; the gateway must be reachable on 443 directly (a `LoadBalancer` Service, not an Ingress), and multiple replicas need the redis storage backend.

## Multiple replicas

The gateway is stateless. For cluster-accurate rate limits, sessions and ACME storage, declare redis `stores` in `config.gateway` and use etcd mode so config converges. `autoscaling` and `podDisruptionBudget` are available; `terminationGracePeriodSeconds` defaults to the gateway's drain timeout plus five seconds.

## Values

See the commented [`values.yaml`](./values.yaml) for the full reference; `values.schema.json` validates types and enums at install time.
