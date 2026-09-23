---
title: Admin API
description: REST API for routes, policies, stores, server-side sessions, config reload, and operational endpoints, with HTTP Basic authentication.
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

The embedded [Web UI](./web-ui.md) is served as an unauthenticated fallback on the same port; its API calls carry the credentials. The UI can be disabled at runtime with `admin.ui_enabled: false` (restart required), and the `-headless` Docker image omits it at compile time.

## Endpoint reference

| Method | Path | Purpose | Errors |
|---|---|---|---|
| `GET` | `/api/routes` | List all routes | — |
| `POST` | `/api/routes` | Create a route (`201 Created`) | `409` name already exists; `400` validation/recompile failed |
| `PUT` | `/api/routes` | Reorder routes — their match priority, since the first matching route wins. Body `{"order": ["a", "b", ...]}` naming every route exactly once, highest priority first | `400` not a permutation of the existing names (missing, duplicate or unknown); `400` recompile failed |
| `GET` | `/api/routes/:name` | Get a route | `404` unknown route |
| `PUT` | `/api/routes/:name` | Replace an existing route | `404` unknown route (**not** upserted); `400` recompile failed |
| `DELETE` | `/api/routes/:name` | Delete a route | `404` unknown route; `400` recompile failed |
| `GET` | `/api/policies` | List all policies | — |
| `POST` | `/api/policies/validate` | Validate + compile a policy body against the live supernodes, plugin configs and stores, without persisting it. Returns `{"valid": bool, "errors": [...], "buffering": [...]}` — `buffering` names every upstream node the policy forces to buffer instead of stream, and the node responsible (`{"upstream": "up", "blocked_by": "rw", "node_type": "response-rewrite"}`); a policy with a non-empty `buffering` is still `valid` | — (structural/compile failures are reported as `valid: false`, not an HTTP error) |
| `GET` | `/api/policies/:name` | Get a policy (full node graph) | `404` unknown policy |
| `PUT` | `/api/policies/:name` | Create **or** update a policy (upsert) | `400` validation/recompile failed |
| `DELETE` | `/api/policies/:name` | Delete a policy | `404` unknown policy; `400` recompile failed (e.g. a route still references it) |
| `GET` | `/api/supernodes` | List all [supernode](../concepts/supernodes.md) definitions | — |
| `GET` | `/api/supernodes/:name` | Get a supernode definition | `404` unknown supernode |
| `PUT` | `/api/supernodes/:name` | Create **or** update a supernode definition (upsert) | `400` validation/recompile failed |
| `DELETE` | `/api/supernodes/:name` | Delete a supernode definition | `404` unknown supernode; `400` recompile failed (e.g. a policy still references it) |
| `GET` | `/api/plugin-configs` | List all [shared plugin config](../concepts/plugin-configs.md) definitions | — |
| `GET` | `/api/plugin-configs/:name` | Get a shared plugin config definition | `404` unknown plugin config |
| `PUT` | `/api/plugin-configs/:name` | Create **or** update a shared plugin config definition (upsert) | `400` validation/recompile failed |
| `DELETE` | `/api/plugin-configs/:name` | Delete a shared plugin config definition | `404` unknown plugin config; `400` recompile failed (e.g. a node still references it via `config_ref`) |
| `GET` | `/api/stores` | List named stores (redis/valkey connections) | — |
| `POST` | `/api/stores` | Create a store | `409` if the name exists; `400` on validation failure |
| `GET` | `/api/stores/:name` | Get one store (raw config — `${ENV}` placeholders are never resolved) | `404` unknown store |
| `PUT` | `/api/stores/:name` | Create **or** update a store (upsert) | `400` on validation failure |
| `DELETE` | `/api/stores/:name` | Delete a store | `404` unknown store; `409` `{"error":"in_use","referrers":[...]}` if referenced |
| `POST` | `/api/stores/:name/ping` | Connectivity check: latency + server version | `404` unknown store; `400` bad config; `502` unreachable; `504` timeout; `501` headless build |
| `GET` | `/api/consumers` | List all consumers (with credentials) | — |
| `GET` | `/api/consumers/:name` | Get a consumer | `404` unknown consumer |
| `POST` | `/api/consumers` | Create a consumer | `409` name taken; `400` store rebuild rejected |
| `PUT` | `/api/consumers/:name` | Create **or** update a consumer (upsert) | `400` store rebuild rejected |
| `DELETE` | `/api/consumers/:name` | Delete a consumer | `404` unknown consumer |
| `GET` | `/api/sessions?store=&subject=&plugin=&limit=&cursor=` | List server-side session metadata (never payloads) | `400` missing `store`; `404` unknown store; `502` store outage; `501` headless build |
| `DELETE` | `/api/sessions/:store/:id` | Revoke one session | `400` bad id; `404` unknown store; `502` store outage; `501` headless build |
| `DELETE` | `/api/sessions?store=&subject=` | Revoke every session for a subject (`{"revoked": N}`) | `400` missing `store`/`subject`; `404` unknown store; `502` store outage; `501` headless build |
| `GET` | `/api/plugins` | Static catalog of node/plugin types (id + description) | — |
| `GET` | `/api/vars` | Static catalog of `$var` names plugins can interpolate, with kinds, sources and descriptions (see [Context variables](../reference/context-vars.md)) | — |
| `GET` | `/api/env-vars` | Names of the environment variables visible to the process, sorted — **names only, never values**, for the config editor's `${ENV}` suggestions | — |
| `GET` | `/api/scripts` | List scripted-plugin files (`.lua`) in the `plugins/` directory next to the config directory; missing directory yields an empty list | — |
| `GET` | `/api/status` | Gateway version plus route and policy counts | — |
| `GET` | `/api/config/export` | Live in-memory config (routes + policies + supernodes + plugin configs + stores) rendered as YAML (`text/yaml`) | `500` serialization failed |
| `GET` | `/api/debug/config` | Effective [debug-mode](./debugging.md) settings; answers even when debug is off | — |
| `GET` | `/api/debug/traces` | Recorded traces, newest first; filter with `?route=&policy=&status=&source=&limit=` | `404` debug mode off |
| `GET` | `/api/debug/traces/:id` | One trace with per-step context changes | `404` unknown/evicted, or debug off |
| `DELETE` | `/api/debug/traces` | Clears the trace buffer | `404` debug mode off |
| `POST` | `/api/debug/sandbox` | Runs plugins or a policy against a synthetic context | `400` bad request/config; `404` unknown policy or debug off; `504` timeout |
| `DELETE` | `/api/cache/:id` | Purge a `proxy-cache` pair by its `id` on every backend that holds it (`{"id": ..., "purged": [{"backend": ..., "store": ..., "removed": N}, ...]}`) | `404` no pair with that id in any policy; `502` a backend could not be reached (lists what succeeded first) |
| `GET` | `/api/acme/certs` | Every [ACME](./tls.md#automatic-certificates-acme)-managed certificate with state, domains, validity and last error; `{"enabled": false, "certs": []}` when `acme:` is not configured. Never includes key material | — |
| `POST` | `/api/acme/certs/:id/renew` | Nudge the renewal manager for one certificate (`?force=true` to ignore the renewal window). `202 {"scheduled": true}`, or `200 {"scheduled": false, "reason": "not_due"}` | `404` unknown certificate id; `409` `{"error":"in_progress"}`; `501` ACME not configured |
| `GET` | `/api/mcp/status` | Whether the [MCP server](./mcp.md) is compiled in and enabled, its path, token count and distinct scopes — **never token values** | — |
| `GET` | `/api/mcp/prompts` | The precompiled agent prompts (name + description) | — |
| `GET` | `/api/mcp/prompts/:name` | One prompt rendered with live data, behind the UI's "copy as agent prompt" actions; takes the same arguments as the MCP prompt | `404` unknown prompt; `400` invalid arguments, or debug/sandbox disabled for a prompt that needs them |
| `POST` | `/api/config/reload` | Re-read `gateway.yaml` from disk, recompile, swap in | `500` no config path set, or parse/validate/compile failed (running config unchanged) |
| `GET` | `/healthz` | Liveness probe (auth-exempt) | — |
| `GET` | `/readyz` | Readiness probe (auth-exempt) | `503` while the route table is empty |
| `GET` | `/metrics` | Prometheus metrics in text exposition format | — |

### Cache purge

`DELETE /api/cache/:id` purges every backend holding a [`proxy-cache`](../reference/plugins/proxy-cache.md) pair with that `id` — the same action the MCP `purge_cache` tool triggers on demand, and a `phase: purge` node triggers automatically on a write path. A successful purge returns the id and what each backend reported:

```json
{"id": "checkout-cache", "purged": [{"backend": "redis", "store": "cache-store", "removed": 12}]}
```

`store` is omitted from a `local` backend entry — there is no named store to report. `404` means no compiled policy has a pair with that `id`: a typo must not read as a successful flush of nothing. `502` means a backend could not be reached; the response body still lists what succeeded first (`{"error": "cache_purge_failed", ...}`).

A `redis` backend purge `SCAN`s the store's whole keyspace incrementally, so its cost grows with the store's total key count, not with the pair's own entry count; `UNLINK` frees the matched keys' memory off-thread. Avoid wiring a purge to a high-rate write path on a large shared store, and note that entries written concurrently during a purge may survive it — it is best-effort under concurrent writes, not a snapshot.

A `policy: local` purge clears the instance that received the request only; `policy: redis` purges are cluster-wide because the store is shared.

Notes on mutation semantics:

- **Upsert asymmetry**: `PUT /api/policies/:name`, `PUT /api/supernodes/:name`, `PUT /api/plugin-configs/:name`, and `PUT /api/consumers/:name` create the resource if it does not exist, while `PUT /api/routes/:name` returns `404` for an unknown route — routes are created only via `POST /api/routes`.
- **Supernodes** are inlined into every referencing policy at compile time (see [Supernodes](../concepts/supernodes.md)), so a `PUT`/`DELETE` on `/api/supernodes/:name` re-expands and recompiles all routes just like a policy edit — deleting a definition still referenced by a policy fails with `400`, leaving the previous definition and compiled routes active.
- **Shared plugin configs** are resolved (`config_ref` materialized into the effective node config) at the same compile choke point as supernode expansion, before it (see [Shared plugin configs](../concepts/plugin-configs.md)). A `PUT`/`DELETE` on `/api/plugin-configs/:name` revalidates and recompiles all routes just like a policy edit — an unknown reference or a `type` mismatch introduced by the edit, or a delete while still referenced by any policy or supernode node, fails with `400` and leaves the previous definition and compiled routes active.
- **Consumer mutations** rebuild the consumer store and hot-swap it (no graph recompile); a rejected rebuild (duplicate credential, malformed credential object) leaves the previous store active.
- `stores` follow the standard semantics (POST rejects duplicates, PUT upserts) with one addition: DELETE is guarded — a store referenced by any node or shared plugin config returns `409 {"error":"in_use","referrers":[...]}` naming each referrer. Ping resolves `${ENV_VAR}` placeholders at call time; responses never echo connection details. Binaries built without the `redis-store` feature reject any declared store at config load and answer ping with 501.
- **Sessions** are read/write against the named store's live data, not the compiled route graph, so they never trigger a recompile. Listing and both revoke endpoints return only `SessionMeta` — subject, plugin, policy, route, timestamps — never the sealed session payload. Revocation applies to **store-backed sessions only**: a plugin configured with `session.storage: cookie` (the default) keeps its session entirely client-side, so there is nothing server-side to revoke — only `session.storage: redis` sessions are listable/revocable here. Binaries built without the `redis-store` feature answer every `/api/sessions*` route with `501`.
- For both `PUT` endpoints, the name in the URL path overrides any name in the JSON body.
- Every mutation triggers validation and recompilation of all route graphs. On failure the endpoint returns `400` and the previously compiled routes stay active.
- Changes take effect immediately (hot-reload, no restart).

## Examples

List routes:

```bash
curl -u admin:admin http://localhost:9090/api/routes
```

Give `admin` priority over the broader `api` route (every route must be listed):

```bash
curl -u admin:admin -X PUT http://localhost:9090/api/routes \
  -H 'Content-Type: application/json' \
  -d '{"order": ["admin", "api", "catch-all"]}'
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
