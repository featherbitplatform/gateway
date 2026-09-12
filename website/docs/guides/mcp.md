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

The web UI's **Agent** panel (footer) shows these snippets with your actual endpoint and a prompt library with per-prompt **Copy** buttons; the Debug panel's trace header also has **Copy prompt** for the troubleshooting prompt, data inlined, for an external agent.

## Chat in the UI

The web UI has a **Chat** panel (footer button, or `Ctrl+K` → "Open Chat") that talks to an OpenAI-compatible Chat Completions endpoint **from your browser** and uses this gateway's MCP tools during the conversation. The gateway itself still runs no model and holds no provider key.

Settings (gear icon in the panel) are stored in this browser's local storage under `featherbit.chat.settings`:

| Field | Meaning |
|---|---|
| Base URL | `https://api.openai.com/v1` by default; any compatible server works (Azure, OpenRouter, a local Ollama, …) |
| Model | Free text; **Load models** fetches the provider's `GET /models` list into a suggestion dropdown, so nothing is hardcoded |
| API key | Sent only to the base URL above |
| MCP token | One of `admin.mcp.tokens`; sent only to this gateway's MCP endpoint. Leave empty for a toolless chat |

Threads live under `featherbit.chat.threads` (50 newest kept, tool results truncated at 32 000 characters). Each thread has **Delete**; **Clear all chats** removes them all; **Forget credentials** clears the key and token but keeps base URL and model. Nothing in the chat is sent to the gateway's Admin API.

**Secrets.** The gateway already keeps most secrets out of what the chat can see: traces redact sensitive headers, query parameters and message keys when they are captured, MCP tools mask consumer credentials, and config is served with raw `${ENV}` placeholders. The chat adds a client-side pass on top (**Redact secrets before sending**, on by default): before any text is stored or sent to the provider — seeded prompts, what you type, tool results — it replaces `Bearer`/`Basic` credentials, `Cookie` values, values of secret-looking keys (`password`, `secret`, `api_key`, `*_token`, `private_key`, …), JWTs, PEM private keys, well-known key prefixes, and your own API key and MCP token with `[REDACTED]`. It is a heuristic, not a guarantee: keep genuinely sensitive request bodies out of traces you hand to a third-party model, and turn the toggle off only when you need the model to see a real value (the connection line says when it is off). Because the model only ever sees `[REDACTED]`, a config value it read redacted would be written back literally if it proposes a `put_*` — check the Run/Skip card's arguments, and keep secrets as `${ENV}` placeholders, which are never redacted.

**Tools.** With a token set, the panel loads `tools/list` and hands the schemas to the model. Read tools run as soon as the model asks. Write tools (`put_*`, `delete_*`, `reload_config`) render a card with **Run** and **Skip**; Skip returns "Declined by the user." to the model so it can propose something else. Tick **Auto-run writes** in the chat header (or in settings) to skip the card and let the model apply changes and run the sandbox without stopping; the connection line shows `auto-run writes ON` while it is active, and the toggle is disabled for read-only tokens. Retries of a failed tool fold into one card that shows the current attempt and a collapsible list of the failed ones. `run_sandbox` asks for confirmation too, even though its scope is `read`: it executes the node list the model wrote, outbound calls included. A turn stops after 16 tool rounds; **Stop** aborts the current request.

**Troubleshoot with AI.** A trace in the Debug panel has one primary action, **Troubleshoot with AI**: it opens the chat seeded with the `troubleshoot_trace` prompt (the trace inlined, the final status and the node that set it named) and asks the model to diagnose node by node, verify with the trace/policy/docs tools, propose a fix as YAML validated with `validate_policy`, and ask you when something is missing. A selected step has **Ask AI about this step** (`why_this_port`), the policy toolbar has **Review with AI** (`review_policy`), the `Ctrl+K` palette has `AI: review this policy` and `AI: design a policy / supernode / route…`, and the Agent panel's prompt library has an **Ask** beside every prompt. Each starts a thread you keep asking into. The Debug panel stays open behind the chat.

**Origins.** Browsers send `Origin` on every POST, so the MCP endpoint accepts a request whose `Origin` authority equals its `Host` (the UI calling the gateway it was served from) even with an empty `allowed_origins`. The Vite dev server on another port still needs listing. Behind a reverse proxy that rewrites `Host` (nginx `proxy_pass` does by default) or a TLS terminator that adds `:443`, either preserve the original `Host` or list the public origin in `allowed_origins`. Self-hosted providers must allow the admin origin in their own CORS configuration (for Ollama: `OLLAMA_ORIGINS`).

## Tools, resources, prompts

22 read tools and 11 write tools return JSON; failures come back as tool errors `{code, message, errors?, hint?}` with codes `not_found`, `invalid_input`, `invalid_config` (with the compiler's error list), `debug_disabled`, `sandbox_disabled`, `forbidden`, `store_error`, `internal`.

Resources: every plugin/concept/reference documentation page is embedded in the binary (`featherbit://docs/plugins/{type}`, `featherbit://docs/concepts/{name}`, `featherbit://docs/reference/{name}`) — `get_node_type` returns the page too — plus `featherbit://routes/{name}`, `featherbit://policies/{name}`, `featherbit://supernodes/{name}` (YAML) and `featherbit://traces/{id}`.

Prompts (the precompiled questions): `troubleshoot_trace`, `explain_trace`, `why_this_port`, `why_this_response`, `review_policy`, `design_policy`, `design_supernode`, `design_route`, `diagnose_route`. The same texts drive the UI's chat actions (Troubleshoot with AI, Ask AI about this step, Review with AI) and can be copied from the Agent panel's prompt library or the trace header's **Copy prompt**, with the data inlined so they work in any chat.

Trace and sandbox tools need [debug mode](./debugging.md); they say so when it is off.

## Security notes

- Two credentials, two surfaces: Basic Auth never works on the MCP path; MCP tokens never work on `/api/*`.
- Constant-time token comparison; disabled MCP is indistinguishable from a missing route.
- A request carrying an `Origin` header is refused unless it is same-origin (its authority equals the request's `Host`) or listed in `allowed_origins` (DNS-rebinding defence — and either way the bearer token is still required). Non-browser agents send none.
- Writes go through the same validate → compile → commit path as the Admin API and are logged (`mcp tool call token=… scope=… tool=… outcome=…`). With the file config source, edits are live but not written back to `gateway.yaml` — the same as Admin API edits; with etcd they persist cluster-wide.
- `reload_config` re-reads `gateway.yaml` from disk. It is never needed after a `put_*`/`delete_*` (those are live at once), and because it would revert every unsaved API/MCP edit it refuses with `unsaved_changes` — listing the routes, policies, stores… that differ — unless called with `discard_unsaved: true`. Use `export_config` to get the YAML to persist.
- `${ENV}` placeholders are served raw, never resolved. Consumer credentials are masked on read.
- Put TLS on the admin listener (`admin.tls`) when the agent is remote.
