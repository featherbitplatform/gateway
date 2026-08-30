# MCP Server for Agents — Design

**Date:** 2026-08-30
**Status:** Approved design, pending implementation plan

## Motivation

Operators increasingly work with an AI agent at hand — Claude Code, Claude
Desktop, Cursor, an internal agent — and want that agent to help with the two
jobs the gateway makes hardest for a newcomer: *understanding why a request did
what it did* (which node exited on which port, why a plugin returned `false`,
`denied`, or `error`) and *authoring a valid policy, route, or supernode*
against 80+ node types with a strict port-wiring rule.

Everything an agent needs for both jobs already exists as machine-readable
state: debug traces with per-node context snapshots and diffs (`src/debug/`),
the plugin catalog and `PortSpec`s, `validate_policy` / `compile_policy`, the
`$var` catalog (`GET /api/vars`), and a single validate-compile-persist-apply
write path (`ConfigStore::commit`). What is missing is a surface an agent can
consume directly.

This design adds a **Model Context Protocol (MCP) server** to the Admin
listener. The gateway is the MCP *server*; the LLM lives in whatever agent the
user already runs. The gateway therefore never embeds an LLM client, stores a
provider API key, or pays for tokens — an infrastructure component should not
phone a third-party model. The web UI becomes MCP-aware (connection panel,
"copy as agent prompt" actions) without ever holding a token itself.

This is **control-plane** AI: agents operating the gateway. It is unrelated to
the deferred **data-plane** APISIX AI suite (`ai-proxy`, `ai-prompt-guard`,
`mcp-bridge`, …) tracked in `docs/apisix-parity.md`, which proxies LLM
traffic for end users.

## Scope

**In:**

- `admin.mcp` block in `system.yaml`: enabled flag, mount path, scoped bearer
  tokens (`read` / `write`), allowed origins. Restart-gated, env-interpolated.
- MCP **Streamable HTTP** transport (no stdio), served on the Admin listener
  at `admin.mcp.path` (default `/mcp`), implemented with the official Rust SDK
  `rmcp` (`server` + `transport-streamable-http-server` features).
- Bearer-token authentication with per-token scope; `write` implies `read`.
- **Tools**: read tools mirroring the Admin API's read surface plus
  validation, traces, and the sandbox; write tools for routes, policies,
  supernodes, plugin configs, and stores, each with `dry_run`.
- **Resources**: the plugin/concept/reference documentation pages embedded in
  the binary; YAML views of routes/policies/supernodes; traces.
- **Prompts**: the "precompiled queries" (`explain_trace`, `why_this_port`,
  `why_this_response`, `review_policy`, `design_policy`, `design_supernode`,
  `design_route`, `diagnose_route`).
- New `mcp` cargo feature, default-on, mirroring `ui`.
- Web UI: an **Agent panel** (status, endpoint, client snippets, prompt
  library) and contextual **"Copy as agent prompt"** actions in the trace
  viewer and policy editor, backed by two Basic-Auth Admin endpoints.
- Docs guide, roadmap row, `CLAUDE.md` bullet, e2e scenarios.

**Out (explicit non-goals for V1):**

- Any LLM provider client inside the gateway (Anthropic, OpenAI-compatible, or
  otherwise), and any in-browser chat UI. Both are possible follow-ups.
- stdio transport / a `featherbit mcp` subcommand.
- Consumer **writes** over MCP (reads are in, credential fields masked).
- ACME, sessions, and env-var endpoints over MCP.
- An "agent activity" feed in the UI. MCP writes are ordinary commits and
  appear in the UI on refresh like any Admin API change.
- MCP `sampling`, `elicitation`, `roots`, or notifications/subscriptions.
- The data-plane APISIX AI plugin suite.

## Configuration

```yaml
admin:
  bind: 0.0.0.0
  port: 9090
  username: ${FEATHERBIT_ADMIN_USER}
  password: ${FEATHERBIT_ADMIN_PASSWORD}
  mcp:
    enabled: ${FEATHERBIT_MCP_ENABLED:-false}
    path: /mcp                               # default
    tokens:
      - token: ${FEATHERBIT_MCP_READ_TOKEN}
        scope: read
        name: local-claude                   # optional; logs only
      - token: ${FEATHERBIT_MCP_WRITE_TOKEN}
        scope: write                         # implies read
        name: ci-agent
    allowed_origins: []                      # default: reject any Origin header
```

`McpConfig` lives in `src/config/system.rs` as `Option<McpConfig>` on
`AdminConfig` (`None` = section absent = disabled). It is parsed and stored
regardless of the `mcp` cargo feature so a `system.yaml` carrying `mcp:` loads
on a build without it (same pattern as `ui_enabled`).

Validation at config load — each failure names the offending key:

| Rule | Error |
|---|---|
| `enabled: true` and `tokens` empty | `admin.mcp.tokens must declare at least one token when admin.mcp.enabled is true` |
| a `token` resolves to the empty string (unset env var) | `admin.mcp.tokens[i].token is empty (is the environment variable set?)` |
| a `token` shorter than 16 characters | `admin.mcp.tokens[i].token must be at least 16 characters` |
| two entries with the same `token` value | `admin.mcp.tokens[i] duplicates tokens[j]` |
| `scope` not `read` or `write` | serde enum error |
| `path` not starting with `/`, equal to `/`, or under `/api` | `admin.mcp.path must be an absolute path outside /api` |
| `mcp` present but `admin` absent | unreachable by construction (it is nested under `admin`) |

Docs recommend 32+ random bytes per token (`openssl rand -base64 32`).
Because `system.yaml` is env-interpolated on the raw text, tokens never need
to appear literally in the file.

`enabled` is the single runtime switch. Building without the `mcp` feature or
running with `enabled: false` are indistinguishable from the outside (both
answer `404`, see below) — the only difference is a log line at startup:
`MCP server disabled (admin.mcp.enabled = false)` vs
`MCP server not compiled in (built without the "mcp" feature); admin.mcp ignored`.

## Architecture

### Mounting and transport

`rmcp`'s `StreamableHttpService` is a tower `Service<Request<Body>>`. It is
nested into the admin `Router` at `admin.mcp.path` **outside** the Basic Auth
layer, wrapped in the MCP bearer middleware. In `build_router`
(`src/admin/mod.rs`):

```text
Router
├── /api/*, /healthz, /readyz, /metrics   ← basic_auth_middleware (unchanged)
├── {mcp.path}                            ← mcp::auth middleware → rmcp service   (feature "mcp" && enabled)
├── {mcp.path}                            ← 404 handler                           (otherwise)
└── fallback: SPA / 404                   (unchanged)
```

The `404` for the disabled case is served by an explicit route, not the
fallback, so `ui_enabled: true` cannot turn `/mcp` into the SPA index page.
It logs a `WARN` naming `admin.mcp.enabled` (the `/api/debug/*` convention).

Streamable HTTP details are rmcp's: `POST` for JSON-RPC messages (single JSON
response or SSE stream), `GET` for server-initiated streams, `DELETE` to end a
session, `Mcp-Session-Id` header. The session manager is rmcp's in-memory
`LocalSessionManager`; sessions are a transport artefact only — no
authorization state is kept in them (see below). The service is constructed
once per process and shares the same `Arc<SharedState>` as the Admin API.

Graceful shutdown: the admin listener already drains via hyper's
`GracefulShutdown`; open SSE streams are closed when the connection is dropped
at the end of the drain window, same as long-lived Admin connections today.

### Authentication and scope (`src/mcp/auth.rs`)

An axum `from_fn_with_state` middleware runs before rmcp on every request:

1. **Origin check.** If an `Origin` header is present and its value is not in
   `allowed_origins` (exact string match, scheme + host + port), respond
   `403 {"error":"origin_not_allowed"}`. MCP clients that are not browsers
   send no `Origin`, so the empty default is the safe default; this is the
   spec's DNS-rebinding mitigation.
2. **Bearer token.** Require `Authorization: Bearer <token>`. Missing or
   malformed → `401` with `WWW-Authenticate: Bearer realm="featherbit-mcp"`.
   The token is compared against every configured token with a constant-time
   comparison (`subtle::ConstantTimeEq`; `subtle` is already in the tree via
   rustls and becomes a direct dependency); mismatch → same `401`.
   No distinction is leaked between "unknown token" and "malformed header".
3. **Scope extension.** On success, insert `McpPrincipal { name: Option<String>,
   scope: Scope }` into the request's `http::Extensions`. rmcp's
   `StreamableHttpService` consumes the body and injects the remaining
   `http::request::Parts` into `RequestContext.extensions` (verified against
   rmcp 3.1 docs), so a handler reads the principal with
   `ctx.extensions.get::<http::request::Parts>()?.extensions.get::<McpPrincipal>()`.
   Every tool call therefore sees the principal that authenticated **this**
   request.

The principal is resolved per request, never cached on the MCP session: a
token can be rotated at restart, and a client that opened a session with a
write token cannot keep write access by presenting a read token later.

Basic Auth credentials are not accepted on the MCP path, and MCP tokens are not
accepted on `/api/*`. Two surfaces, two credentials.

### Server handler (`src/mcp/server.rs`)

Implements rmcp's `ServerHandler`:

- `initialize` → server name `featherbit`, version from `CARGO_PKG_VERSION`,
  capabilities `tools`, `resources`, `prompts` (no `listChanged`
  notifications in V1), and the **instructions** text (below).
- `tools/list` → the tool registry filtered by the request's scope. Read
  tokens never see write tools.
- `tools/call` → dispatch by name. A write tool called with a read token
  returns a tool error `forbidden` (defence in depth, since a client may call
  a tool it was not listed).
- `resources/list`, `resources/templates/list`, `resources/read` → docs and
  config resources (all `read`).
- `prompts/list`, `prompts/get` → prompt templates rendered with live data.

Tool input schemas are declared with `schemars` derives on the argument
structs (rmcp generates JSON Schema from them); every tool has a
one-paragraph description written for an agent, stating when to use it and
what it returns.

**Instructions** (returned at `initialize`, ~25 lines): what the gateway is;
that a policy is a node graph with declared-port routing and that **every
`success`/outcome port must be wired** or compilation fails; that plugin
config knowledge is in `get_node_type` (docs page + ports); the authoring
loop `get_node_type → draft → validate_policy → put_*(dry_run=true) →
put_*`; that debug tools need `debug.enabled`; and that with a read-scoped
token the agent should return YAML for a human to apply instead of
attempting writes.

### Tools (`src/mcp/tools/`)

Each tool is a function `async fn(state: &SharedState, principal: &McpPrincipal,
args: Args) -> Result<serde_json::Value, ToolError>`. The registry is a static
table `(name, scope, description, schema, handler)`; a unit test asserts every
write tool is tagged `write` by checking the handler module it lives in.

#### Read scope

| Tool | Args | Returns |
|---|---|---|
| `list_node_types` | — | `[{type, description, ports: {input, outputs: [{name, kind: success\|outcome\|error, description}]}}]` — exactly `plugin_catalog()` + `PortSpec` (the server-side catalog has no categories; they live in the UI only) |
| `get_node_type` | `type` | the catalog entry, its `PortSpec`, and the **docs page** for the type (Markdown, see Resources). The primary authoring tool. |
| `list_routes` / `get_route` | — / `name` | as `GET /api/routes[/{name}]` |
| `list_policies` / `get_policy` | — / `name` | as `GET /api/policies[/{name}]`; `get_policy` also returns `referenced_by_routes: [names]` (and `get_supernode` returns `used_by_policies`) so the agent sees what a change affects; compile status comes from `validate_policy` |
| `list_supernodes` / `get_supernode` | — / `name` | as Admin API |
| `list_plugin_configs` / `get_plugin_config` | — / `name` | as Admin API |
| `list_stores` | — | as Admin API (`${ENV}` placeholders raw, as today) |
| `list_consumers` / `get_consumer` | — / `name` | as Admin API **with credential fields masked** (`"<masked>"`): every value under `credentials`/`plugins.*` keys named `key`, `secret`, `password`, `token`, `private_key`, `client_secret`, plus anything already covered by the debug redaction denylist. Shared helper `consumers::mask_credentials`, unit-tested. |
| `list_vars` | — | the `$var` catalog (`src/vars/catalog.rs`) |
| `validate_policy` | `policy` (PolicyConfig JSON/YAML string) | `{valid, errors: [string]}` from `validate_policy` **then** `compile_policy` against the live `PluginResources` — so plugin config deserialization errors and unknown `config_ref`/`store` names surface. Nothing is persisted. |
| `validate_supernode` | `definition` | structural validation (`validate_supernode`: boundary nodes, reserved ids, inner wiring); node **config** errors surface when a policy using it is validated or saved with `dry_run`, since a supernode alone has no listener to compile |
| `list_traces` | `route?`, `policy?`, `limit?` | `TraceSummary` list (newest first) |
| `get_trace` | `id`, `include_snapshots?=false` | trace metadata, `initial` request summary, final response, and steps as `{seq, node_id, node_type, outcome, port, duration_ms, diff}`; `before`/`after` snapshots only when `include_snapshots` is true |
| `get_trace_step` | `id`, `node_id` (or `seq`) | one step in full: `before`, `after`, `diff`, outcome, port, error, plus the node's config from the trace's policy |
| `run_sandbox` | `SandboxRequest` fields (`nodes?`, `policy?`, `on_error?`, `context`) | as `POST /api/debug/sandbox`. Classed `read`: it executes plugins but mutates no stored config. Gated by `debug.enabled` **and** `debug.sandbox`. |
| `get_status` | — | `/api/status` payload + `debug: {enabled, sandbox}` + `mcp: {scope}` |
| `export_config` | — | full `gateway.yaml` as YAML string, placeholders raw |

#### Write scope

| Tool | Args | Behavior |
|---|---|---|
| `put_route` | `name`, `route`, `dry_run?` | upsert |
| `delete_route` | `name`, `dry_run?` | |
| `put_policy` | `name`, `policy`, `dry_run?` | upsert; `name` overrides `policy.name` as the Admin API does |
| `delete_policy` | `name`, `dry_run?` | |
| `put_supernode` / `delete_supernode` | `name`, `definition`, `dry_run?` | |
| `put_plugin_config` / `delete_plugin_config` | `name`, `config`, `dry_run?` | |
| `put_store` / `delete_store` | `name`, `store`, `dry_run?` | |
| `reload_config` | — | as `POST /api/config/reload` |

All go through one helper:

```rust
async fn commit_candidate(
    state: &SharedState,
    mutate: impl FnOnce(&mut GatewayConfig) -> Result<(), ToolError>,
    dry_run: bool,
) -> Result<CommitOutcome, ToolError>
```

which clones the current `GatewayConfig`, applies `mutate`, and then either
runs the exact validation `ConfigStore::commit` performs (dry run: validate +
compile every policy, build the route table, resolve stores — with no
persistence and no swap) or calls `commit` itself. The dry-run path is the
existing `SharedState::validate_gateway(&candidate)` — the non-swapping half
of `apply_gateway`, which every `ConfigStore::commit` implementation already
calls — so the two cannot drift. Result:
`{applied: bool, dry_run: bool, changed: [names], warnings: []}`.

Persistence follows the configured `ConfigStore` exactly as Admin API writes
do: the etcd store persists and converges the cluster; the file store applies
in memory and — by existing design — does **not** rewrite `gateway.yaml`.
MCP does not change that behavior.

Payload arguments (`route`, `policy`, `definition`, …) accept **either a JSON
object or a YAML string**; YAML is what the docs show and what agents
naturally produce. A string is parsed with `serde_yaml`, an object is
deserialized directly.

#### Tool errors

Domain failures are MCP tool results with `isError: true` and a single text
content block containing JSON:

```json
{"code": "invalid_config", "message": "policy 'api' failed validation",
 "errors": ["Node 'auth' output port 'denied' is not wired", "..."],
 "hint": "Every success/outcome port must be wired. Use get_node_type('key-auth') to see its ports."}
```

| `code` | When |
|---|---|
| `not_found` | named route/policy/… does not exist |
| `invalid_input` | payload failed to parse (YAML/JSON/serde error text included) |
| `invalid_config` | validation/compile errors (list) |
| `debug_disabled` | trace/sandbox tool while `debug.enabled` is false; hint names `debug.enabled` / `FEATHERBIT_DEBUG` |
| `sandbox_disabled` | `run_sandbox` while `debug.sandbox` is false |
| `forbidden` | write tool with read token; hint: "this token has scope read; write tools need a token with scope write" |
| `store_error` | persistence backend failure (file/etcd), message from the store |
| `internal` | anything else; message is the error's Display |

JSON-RPC protocol errors are reserved for genuinely malformed requests
(unknown tool name, schema violation), which rmcp already produces.

### Resources (`src/mcp/docs.rs` and `server.rs`)

**Documentation, embedded.** `rust-embed` (already a dependency behind `ui`;
it becomes non-optional so every build carries the pages — the Admin API's
prompt renderer uses them too) embeds at build time:

- `website/docs/reference/plugins/*.md` → `featherbit://docs/plugins/{type}`
- `website/docs/concepts/*.md` → `featherbit://docs/concepts/{name}`
- `website/docs/reference/{context-vars,conditions,templates}.md` →
  `featherbit://docs/reference/{name}`

At read time the page is cleaned for an agent: YAML frontmatter is dropped
(title kept as an `# ` heading), `<span className="plugin-chip" …>…</span>`
and other single-line JSX is removed, relative `.md` links are rewritten to
the matching `featherbit://docs/...` URI where one exists (else left as
text). A unit test asserts every type in `plugin_catalog()` has a docs page
(it is what `get_node_type` returns), so a new plugin without docs fails CI.
Approximate cost: ~300 KB in the binary.

**Config views.** Resource templates `featherbit://routes/{name}`,
`featherbit://policies/{name}`, `featherbit://supernodes/{name}` return the
definition as `application/yaml`; `resources/list` enumerates the concrete
URIs for the current config. `featherbit://traces/{id}` returns the trace as
JSON (same shape as `get_trace` with snapshots). Resources need `read` scope
like everything else; they exist for clients that browse resources rather
than call tools, and for prompts to embed.

### Prompts (`src/mcp/prompts.rs`)

Each prompt is a template rendered with live data into a single `user`
message (text plus embedded resources where the client can use them). The
renderer is a plain Rust function `render(name, args, state) -> Result<RenderedPrompt, ToolError>`
used by both `prompts/get` and the Admin endpoint `GET /api/mcp/prompts/{name}`
(Section "Web UI"), so the UI's "copy as prompt" text and the MCP prompt are
byte-identical apart from the trailing MCP hint line the UI appends.

| Prompt | Args | Bundles | Asks |
|---|---|---|---|
| `explain_trace` | `trace_id` | trace metadata, request summary, final response, steps with outcome/port/diff | "Walk through what happened to this request, node by node. Which node produced the final response, and why?" |
| `why_this_port` | `trace_id`, `node_id` | that step's `before`/`after`/`diff`, node config, docs page of its type | "Why did node `{node_id}` (`{type}`) exit on port `{port}`? Point at the config keys and context values that decided it." |
| `why_this_response` | `trace_id` | as `explain_trace` plus the step that last changed `response.status` | "The client received `{status}`. Which node set it and why?" |
| `review_policy` | `policy_name` | policy YAML, catalog entries for the node types used | "Review for: unreachable nodes, error ports left to the catch-all where they should be handled, ordering problems (auth after upstream, rewrite after proxy), redundant nodes. Propose concrete YAML edits." |
| `design_policy` | `goal`, `name?` | instructions text, `list_node_types` output | "Design a policy that {goal}. Use get_node_type for each node's config, validate with validate_policy, then put_policy(dry_run=true) before put_policy. If your token is read-only, return the YAML." |
| `design_supernode` | `goal`, `name?` | same + supernodes concept page | same shape with the supernode boundary-node rules |
| `design_route` | `goal` | route docs, existing routes and policies list | same shape for a route (match rules + policy reference) |
| `diagnose_route` | `method`, `path`, `headers?` | route table summary | "Determine which route matches; then use run_sandbox on its policy with this request to explain what would happen." |

Prompts render only data the `read` scope may see (consumer credentials
masked). The `design_*` prompts do not require write scope to *list*; the
rendered text tells the agent what to do if writes are refused.

### Web UI

**Admin endpoints (`src/admin/mcp.rs`, Basic Auth, never gated):**

- `GET /api/mcp/status` → `{"compiled": bool, "enabled": bool, "path": "/mcp",
  "token_count": n, "scopes": ["read","write"]}` (scopes actually configured).
  Token values and names are never returned.
- `GET /api/mcp/prompts` → `[{name, description, arguments: [{name, required, description}]}]`.
- `GET /api/mcp/prompts/{name}?arg=value…` → `{"text": "<rendered prompt>"}`.
  Works with MCP disabled: the render exposes only what Basic Auth can read.
  Returns `404 not_found` for an unknown trace/policy, `400` for missing args.

**Agent panel** (`ui/src/components/AgentPanel.tsx`; footer button beside
Sessions/Certificates; fetches `/api/mcp/status` on open):

- Off / not compiled: empty state naming `admin.mcp.enabled` and
  `FEATHERBIT_MCP_ENABLED`, and the token env vars, in the Debug panel's
  off-state style.
- On: endpoint URL built from `window.location.origin + path`; client
  snippets with a literal `<TOKEN>` placeholder — Claude Code
  (`claude mcp add --transport http featherbit <url> --header "Authorization: Bearer <TOKEN>"`),
  generic `mcpServers` JSON (Claude Desktop, Cursor, Windsurf), `curl`
  `initialize` smoke test; a read/write scope explainer listing which tools
  each scope unlocks. The list is static in `ui/src/agentPrompts.ts`; e2e
  scenario `E2E-MCP-02` asserts it matches the live `tools/list` so it cannot
  drift silently.
- Prompt library: the `/api/mcp/prompts` list with arguments, each with a
  "Copy" that opens the same small argument dialog the contextual actions use.

**Contextual "Copy as agent prompt"** (`ui/src/agentPrompts.ts` + hooks):

- `TraceViewer`: trace header menu → `explain_trace`, `why_this_response`;
  step row menu → `why_this_port`.
- Policy editor toolbar → **Review with agent** (`review_policy` for the open
  policy); Ctrl+K palette (`commands.ts`) → `review_policy`,
  `design_policy` / `design_supernode` / `design_route`
  (dialog for `goal`).
- Copies `text` from `GET /api/mcp/prompts/{name}` plus a final line:
  *"If the `featherbit` MCP server is connected, prefer its tools
  (`get_trace_step`, `get_node_type`, `validate_policy`) over the data
  inlined above."* Emits a success toast via the existing notification log.

### Feature flag and build

- `Cargo.toml`: `mcp = ["dep:rmcp"]` in `default`. `rmcp` with
  `default-features = false, features = ["server", "macros",
  "transport-streamable-http-server"]`; no TLS or HTTP client features.
  `schemars` (tool input schemas), `subtle` (constant-time compare) and
  `rust-embed` (docs pages) are unconditional dependencies.
- Only the transport is gated: `src/mcp/server.rs` (the `rmcp` adapter) and
  the mounting of the live endpoint compile under the feature; `auth`,
  `tools`, `docs`, `prompts` and the `/api/mcp/*` Admin endpoints compile in
  every build, so the UI's "copy as agent prompt" works even on a binary
  built without `mcp`.
- `cargo deny` must pass; the ring-only constraint is unaffected (rmcp has no
  TLS dependency of its own). Any new license goes into `deny.toml` only after
  review.
- `--no-default-features` (headless) drops `ui`, `redis-store`, and `mcp`.
  A `--no-default-features --features ui,redis-store` build is added to CI's
  feature matrix so "everything but MCP" compiles.

## Security considerations

- **Never anonymous**: config load fails if enabled without tokens.
- **Least privilege**: read tokens cannot see or call write tools; consumers
  are masked on read; `dry_run` is available for every write.
- **Not advertised**: disabled → `404`, identical to a missing route.
- **DNS rebinding**: `Origin` allowlist, default deny-when-present.
- **Constant-time** token comparison; no timing distinction between failure
  modes.
- **Same persistence path** as the Admin API: MCP writes are validated,
  compiled, committed through the configured `ConfigStore` (etcd persists;
  the file store applies in memory only, as today), and hot-applied
  identically; nothing bypasses `commit`.
- **Audit**: every `tools/call` logs `INFO mcp tool call token=<name|"unnamed"> scope=<s> tool=<t> outcome=<ok|error:<code>> duration_ms=<n>`; write commits additionally reuse the existing commit logging.
- **Secrets in traces**: traces are redacted at capture time already; MCP
  adds no new exposure beyond what `GET /api/debug/traces` gives Basic Auth.
- **Secrets in config**: `${ENV}` placeholders are stored raw and served raw
  (as the Admin API does); an agent sees `${OIDC_CLIENT_SECRET}`, never the
  value. Literal secrets typed into config are the operator's choice, as
  today — the docs guide says so.
- Docs guide recommends: a read token for production, a write token only for
  dev/staging or CI; TLS on the admin listener (`admin.tls`) when the agent is
  remote.

## Testing

**Unit**

- `McpConfig` validation: each rule in the Configuration table, plus a valid
  config round-trip with env interpolation.
- Token lookup: match, mismatch, malformed header, empty configured list
  unreachable; constant-time helper used (compile-time check by type, not
  timing).
- Scope filtering: `tools/list` under `read` contains no write tool; under
  `write` contains all; the registry test that write handlers are tagged
  `write`.
- Docs cleaning: frontmatter removed, chip span removed, link rewrite; every
  catalog type has a page.
- Prompt rendering for each prompt with a fixture trace/policy; `not_found`
  paths.
- `mask_credentials` on a consumer fixture.
- YAML-or-JSON payload parsing.

**Integration (in-process, like the existing admin tests)**

- Build the admin router with MCP enabled and drive it with rmcp's client
  over a bound loopback port (rmcp's streamable-HTTP client, dev-dependency
  with `reqwest`, which is already a dev-dependency):
  - `initialize` succeeds with a read token and a write token; fails `401`
    without one; `403` with a disallowed `Origin`.
  - `get_policy` over MCP equals `GET /api/policies/{name}` over Basic Auth.
  - `put_policy(dry_run=true)` leaves `/api/policies` unchanged; a second
    call without dry-run makes the policy visible on `/api/policies` **and**
    routable (a request to a route referencing it is served by the new graph).
  - Write tool with read token → `forbidden` tool error.
  - Invalid policy (unwired port) → `invalid_config` with the engine's error
    text.
  - Debug disabled → `debug_disabled`.
  - MCP disabled → `404` on the path; `/api/mcp/status` still answers.
  - `resources/read` of `featherbit://docs/plugins/limit-count` returns the
    cleaned page.
- Feature-off build (`--no-default-features --features ui,redis-store`)
  compiles and `/mcp` is `404`.

**e2e (Playwright, `e2e/E2E_TESTBOOK.md`)**

The suite's `system.yaml` enables MCP for the whole run (static config, same
reasoning as its `debug:` block); the disabled path is unit-tested.

- `E2E-MCP-01` over HTTP: `initialize` without a token and with Basic Auth →
  `401`; `tools/list` with the read token has no `put_*`, with the write token
  has `put_policy`; `put_policy` with the read token → tool error `forbidden`;
  an MCP token on `/api/policies` → `401`.
- `E2E-MCP-02` Agent panel shows the endpoint URL, a Claude Code snippet
  containing `<TOKEN>`, the prompt library, and a scope explainer whose tool
  names match `tools/list` fetched by the test over HTTP.
- `E2E-MCP-03` from a recorded trace step, "Why this port?" copies text
  containing the port phrase, the policy name, and the MCP hint line
  (clipboard permissions granted to the Chromium context).

**Manual smoke** (documented in the guide): `claude mcp add …`, then
"explain trace `<id>`" and "design a policy that rate-limits by API key".

## Documentation

- `website/docs/guides/mcp.md`: enabling, generating tokens, scopes, client
  setup (Claude Code / Claude Desktop / Cursor), tool and prompt reference
  tables (generated from the registry by a `cargo test`-time check that the
  doc lists every tool name), security notes.
- `website/docs/reference/roadmap.md`: new row "MCP server for agents".
- `CLAUDE.md`: core-features bullet; `docs/apisix-parity.md`: note that the
  data-plane `mcp-bridge` remains a separate epic.
- `config/system.yaml` example gains a commented-out `mcp:` block.

## Follow-ups (not in this design)

- stdio transport / `featherbit mcp` for local-only setups.
- Consumer writes over MCP.
- `listChanged`/resource subscriptions so a connected agent sees config edits
  made in the UI.
- An optional bring-your-own-LLM chat panel in the UI, built on the same
  tool layer, if a no-agent workflow proves necessary.
- Per-token allowlists of routes/policies (finer than read/write).
