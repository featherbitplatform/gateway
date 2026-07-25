---
title: Admin API
description: REST API for routes, policies, config reload, and operational endpoints, with HTTP Basic authentication.
---

The admin API runs on a dedicated port (default `9090`), separate from the data plane. It is enabled by the `admin` section of `system.yaml`; omitting that section disables the admin server entirely.

## Authentication

All endpoints require HTTP Basic authentication, with two exceptions: `/healthz` and `/readyz` bypass auth so orchestrators can probe them without credentials.

Credentials come from `system.yaml` (typically via environment variables):

```yaml
admin:
  port: ${ADMIN_PORT:-9090}
  username: ${ADMIN_USER:-admin}
  password: ${ADMIN_PASSWORD:-admin}
```

Requests without a matching `Authorization: Basic <base64(user:pass)>` header receive `401 Unauthorized` with a `WWW-Authenticate: Basic realm="featherbit admin"` challenge.

The embedded [Web UI](./web-ui.md) is served as an unauthenticated fallback on the same port; its API calls carry the credentials.

## Endpoint reference

| Method | Path | Purpose | Errors |
|---|---|---|---|
| `GET` | `/api/routes` | List all routes | — |
| `POST` | `/api/routes` | Create a route (`201 Created`) | `409` name already exists; `400` validation/recompile failed |
| `GET` | `/api/routes/:name` | Get a route | `404` unknown route |
| `PUT` | `/api/routes/:name` | Replace an existing route | `404` unknown route (**not** upserted); `400` recompile failed |
| `DELETE` | `/api/routes/:name` | Delete a route | `404` unknown route; `400` recompile failed |
| `GET` | `/api/policies` | List all policies | — |
| `GET` | `/api/policies/:name` | Get a policy (full node graph) | `404` unknown policy |
| `PUT` | `/api/policies/:name` | Create **or** update a policy (upsert) | `400` validation/recompile failed |
| `DELETE` | `/api/policies/:name` | Delete a policy | `404` unknown policy; `400` recompile failed (e.g. a route still references it) |
| `GET` | `/api/consumers` | List all consumers (with credentials) | — |
| `GET` | `/api/consumers/:name` | Get a consumer | `404` unknown consumer |
| `POST` | `/api/consumers` | Create a consumer | `409` name taken; `400` store rebuild rejected |
| `PUT` | `/api/consumers/:name` | Create **or** update a consumer (upsert) | `400` store rebuild rejected |
| `DELETE` | `/api/consumers/:name` | Delete a consumer | `404` unknown consumer |
| `GET` | `/api/plugins` | Static catalog of node/plugin types (id + description) | — |
| `GET` | `/api/scripts` | List scripted-plugin files (`.lua`) in the `plugins/` directory next to the config directory; missing directory yields an empty list | — |
| `GET` | `/api/status` | Gateway version plus route and policy counts | — |
| `GET` | `/api/config/export` | Live in-memory config (routes + policies) rendered as YAML (`text/yaml`) | `500` serialization failed |
| `GET` | `/api/debug/config` | Effective [debug-mode](./debugging.md) settings; answers even when debug is off | — |
| `GET` | `/api/debug/traces` | Recorded traces, newest first; filter with `?route=&policy=&status=&source=&limit=` | `404` debug mode off |
| `GET` | `/api/debug/traces/:id` | One trace with per-step context changes | `404` unknown/evicted, or debug off |
| `DELETE` | `/api/debug/traces` | Clears the trace buffer | `404` debug mode off |
| `POST` | `/api/debug/sandbox` | Runs plugins or a policy against a synthetic context | `400` bad request/config; `404` unknown policy or debug off; `504` timeout |
| `POST` | `/api/config/reload` | Re-read `gateway.yaml` from disk, recompile, swap in | `500` no config path set, or parse/validate/compile failed (running config unchanged) |
| `GET` | `/healthz` | Liveness probe (auth-exempt) | — |
| `GET` | `/readyz` | Readiness probe (auth-exempt) | `503` while the route table is empty |
| `GET` | `/metrics` | Prometheus metrics in text exposition format | — |

Notes on mutation semantics:

- **Upsert asymmetry**: `PUT /api/policies/:name` and `PUT /api/consumers/:name` create the resource if it does not exist, while `PUT /api/routes/:name` returns `404` for an unknown route — routes are created only via `POST /api/routes`.
- **Consumer mutations** rebuild the consumer store and hot-swap it (no graph recompile); a rejected rebuild (duplicate credential, malformed credential object) leaves the previous store active.
- For both `PUT` endpoints, the name in the URL path overrides any name in the JSON body.
- Every mutation triggers validation and recompilation of all route graphs. On failure the endpoint returns `400` and the previously compiled routes stay active.
- Changes take effect immediately (hot-reload, no restart).

## Examples

List routes:

```bash
curl -u admin:admin http://localhost:9090/api/routes
```

Upsert a policy and trigger a config reload:

```bash
curl -u admin:admin -X PUT http://localhost:9090/api/policies/my-policy \
  -H 'Content-Type: application/json' \
  -d '{"nodes": [{"id": "listener", "type": "listener"}], "edges": []}'

curl -u admin:admin -X POST http://localhost:9090/api/config/reload
```

Export the running config as YAML (the `gateway.yaml` equivalent of whatever you have built through the UI or the API):

```bash
curl -u admin:admin http://localhost:9090/api/config/export -o gateway.yaml
```

The export reflects the **live in-memory** config, so it includes UI/API-authored changes that were never written back to disk. Values keep their `${ENV_VAR}` templates — env interpolation happens when a policy is compiled, not in the stored config — so the export is safe to commit and reload without baking in resolved secrets. The Web UI surfaces the same thing behind the **View YAML** button in the sidebar (with copy and download).

Successful mutations respond with a status body such as `{"status": "updated"}`; see [Observability](./observability.md) for the health and metrics endpoints in detail.
