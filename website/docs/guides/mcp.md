---
title: MCP server for agents
description: Let an AI agent (Claude Code, Cursor, …) inspect traces, explain policy behavior, and author routes, policies and supernodes through the Model Context Protocol.
---

The gateway can serve a [Model Context Protocol](https://modelcontextprotocol.io) endpoint on the Admin listener. The gateway is the MCP *server*; the model lives in whatever agent you already run, so nothing here calls a third-party LLM, stores a provider key, or costs tokens.

## Enabling

```yaml
admin:
  mcp:
    enabled: ${FEATHERBIT_MCP_ENABLED:-false}
    path: /mcp
    tokens:
      - token: ${FEATHERBIT_MCP_READ_TOKEN}
        scope: read
        name: local-agent
      - token: ${FEATHERBIT_MCP_WRITE_TOKEN}
        scope: write
    allowed_origins: []
```

Restart-gated like everything in `system.yaml`. Rules enforced at load: `enabled: true` needs at least one token; tokens are at least 16 characters (use 32+ random bytes: `openssl rand -base64 32`); duplicates are rejected; `path` must be absolute and outside `/api`, and outside the reserved `/`, `/healthz`, `/readyz`, `/metrics` paths. Disabled (the default), the path answers `404 {"error":"not_found"}` — indistinguishable from a route that was never mounted. The gateway names the config key to set at startup, and warns again the first time the path is hit — but only that once per process, so a stream of unauthenticated requests can't be used to spam the log.

::::warning Restart required
`system.yaml` is read once at startup and never hot-reloaded, so **enabling/disabling the MCP endpoint, its tokens, and its path all require a restart**. Nothing here is hot-reloadable, unlike `gateway.yaml`.
:::

### Scopes

| Scope | Unlocks |
|---|---|
| `read` | `list_node_types`, `get_node_type`, `list_vars`, `get_status`, `export_config` (whole-gateway YAML, consumer credentials masked, same as below), `list_/get_` for routes, policies, supernodes, plugin configs, stores, consumers (credentials masked), `validate_policy`, `validate_supernode`, `list_traces`, `get_trace`, `get_trace_step`, `run_sandbox` |
| `write` | everything above plus `put_/delete_` for routes, policies, supernodes, plugin configs, stores, and `reload_config`. Every `put_`/`delete_` accepts `dry_run: true`. |

Read tokens never see write tools in `tools/list`; a write call with a read token returns a `forbidden` tool error. Use a read token against production and a write token only where an agent should be allowed to change config.

:::warning run_sandbox is real plugin execution, even at read scope
`run_sandbox` sits at `read` scope by design, but with `debug.enabled: true` that means a *read* token can execute plugins for real: an ad-hoc `nodes:` list can include `script` (Lua) and any node that makes outbound calls, and shared rate-limit/circuit-breaker state is mutated — see [the sandbox's own warning](./debugging.md) about live resources. Set `debug.sandbox: false` to keep tracing while removing that capability. Do not hand read tokens to untrusted agents on a gateway with the sandbox enabled.
:::

## Connecting a client

Claude Code:

```bash
claude mcp add --transport http featherbit http://localhost:9090/mcp \
  --header "Authorization: Bearer $FEATHERBIT_MCP_READ_TOKEN"
```

Generic `mcpServers` JSON (Claude Desktop, Cursor, Windsurf):

```json
{ "mcpServers": { "featherbit": { "type": "http", "url": "http://localhost:9090/mcp",
    "headers": { "Authorization": "Bearer <TOKEN>" } } } }
```

Smoke test:

```bash
curl -s -X POST http://localhost:9090/mcp -H "Authorization: Bearer $TOKEN" \
  -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

The web UI's **Agent** panel (footer) shows these snippets with your actual endpoint.

## Tools, resources, prompts

22 read tools and 11 write tools return JSON; failures come back as tool errors `{code, message, errors?, hint?}` with codes `not_found`, `invalid_input`, `invalid_config` (with the compiler's error list), `debug_disabled`, `sandbox_disabled`, `forbidden`, `store_error`, `internal`.

Resources: every plugin/concept/reference documentation page is embedded in the binary (`featherbit://docs/plugins/{type}`, `featherbit://docs/concepts/{name}`, `featherbit://docs/reference/{name}`) — `get_node_type` returns the page too — plus `featherbit://routes/{name}`, `featherbit://policies/{name}`, `featherbit://supernodes/{name}` (YAML) and `featherbit://traces/{id}`.

Prompts (the precompiled questions): `explain_trace`, `why_this_port`, `why_this_response`, `review_policy`, `design_policy`, `design_supernode`, `design_route`, `diagnose_route`. The same texts are available from the UI's trace viewer and policy editor as "Copy as agent prompt", with the data inlined so they work in any chat.

Trace and sandbox tools need [debug mode](./debugging.md); they say so when it is off.

## Security notes

- Two credentials, two surfaces: Basic Auth never works on the MCP path; MCP tokens never work on `/api/*`.
- Constant-time token comparison; disabled MCP is indistinguishable from a missing route.
- A request carrying an `Origin` header is refused unless listed in `allowed_origins` (DNS-rebinding defence). Non-browser agents send none.
- Writes go through the same validate → compile → commit path as the Admin API and are logged (`mcp tool call token=… scope=… tool=… outcome=…`). With the file config source, edits are live but not written back to `gateway.yaml` — the same as Admin API edits; with etcd they persist cluster-wide.
- `${ENV}` placeholders are served raw, never resolved. Consumer credentials are masked on read.
- Put TLS on the admin listener (`admin.tls`) when the agent is remote.
