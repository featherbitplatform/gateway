# Named Output Ports — Design

**Date:** 2026-08-07
**Status:** Draft — pending review
**Motivation:** roadmap entry "Terminal nodes / CORS preflight" (known bug, `E2E-DP-09` expected-failure)

## Problem

The graph engine routes on exactly two output ports: `success` and `error`
(`src/graph/engine.rs`, `compile_policy`). This forces every plugin outcome
into a binary that misrepresents reality:

1. **cors** prepares a 204 preflight response but returns `Ok`, so the engine
   follows `success` into `upstream`, which proxies the `OPTIONS` and
   overwrites the 204. Preflight never short-circuits (the roadmap bug).
   `redirect` and `fault-injection` (abort mode) have the same latent defect —
   they only work when a policy author wires their `success` edge directly to
   `client`.
2. **openid-connect** routes its interactive login redirect (302) through
   `Err` (`OIDC_REDIRECT`), as does its 401 reject. Every successful login
   round-trip increments the `node_errors` Prometheus counter, appends a
   phantom entry to `ctx.errors` (visible to all 17 loggers and error-handler
   templates), and shows as a failure step in debug traces. The ~20 plugins in
   the auth/restriction family (`key-auth`, `jwt-auth`, `acl`,
   `ip-restriction`, …) do the same for deliberate 401/403/413 rejections, and
   `rate-limit` for 429.

The error port means "this node failed" to the metrics pipeline, the debug
tracer, `ctx.errors`, and the catch-all error handler — but half its traffic
today is successful outcomes that merely need different routing.

## Decision summary

Decisions made during brainstorming (2026-08-07, with Francesco):

| Decision | Choice |
| --- | --- |
| Mechanism | Port name on the `Ok` path: `PluginOutput.port` selects a declared named output port; `Err` keeps meaning genuine failure (approach A; approaches B "unified outcome type" and C "port tag on the error type" rejected) |
| Scope | Full sweep: every plugin audited and migrated in this effort |
| Back-compat | Clean break — no fallback routing from unwired named ports to the error edge; migration guide instead |
| Unwired ports | Compile error: every declared output port except `error` must be wired |
| Inputs | Metadata only — single `in` port stays; ports get descriptions for UI/docs; dead `named_inputs`/`named_outputs` plumbing is removed |

## Port model

Every node type declares a static **PortSpec**:

- **Input**: always exactly one `in` port (except `listener`, which has none),
  with a human-readable description of what the node expects.
- **Outputs**: an ordered list of `{name, kind, description}`.
  - `kind` ∈ `success` | `outcome` | `error`. `success` and `error` keep
    today's semantics. `outcome` is the new flavor: the node completed its
    logic and chose an alternate routing result (usually a fully prepared
    client-facing response).
  - Names are lowercase kebab-case identifiers. Reserved: `in`, `out`,
    `success`, `error` (plugins may not declare custom ports with these
    names; `out` remains an accepted alias for `success` in edge endpoints,
    as used by `listener`).

**The criterion** separating `error` from an `outcome` port, applied to every
plugin in the sweep:

> `Err` (error port) is reserved for *the node could not do its job* —
> configuration, parse, or infrastructure failures (upstream unreachable,
> store down, malformed input the node cannot process).
> An `outcome` port is *the node did its job and the result is an alternate
> route* — deliberate client-facing responses (deny, redirect, throttle,
> preflight) or a routing decision.

### Standard port vocabulary

To keep policies and the UI consistent, swept plugins draw from a small
shared vocabulary rather than inventing per-plugin names:

| Port | Meaning | Typical status | Known adopters (from code audit) |
| --- | --- | --- | --- |
| `denied` | Deliberate policy rejection | 401/403/405/413 | `acl`, `authz-casbin`, `authz-casdoor`, `authz-keycloak`, `basic-auth`, `cas-auth`, `csrf`, `dingtalk-auth`, `feishu-auth`, `hmac-auth`, `ip-restriction`, `jwe-decrypt`, `jwt-auth`, `key-auth`, `ldap-auth`, `multi-auth`, `openid-connect`, `consumer-restriction`, `referer-restriction`, `request-size-limit`, `ua-restriction`, `uri-blocker`, `workflow` (respond action), `request-validation`, `oas-validator` |
| `redirect` | Deliberate 3xx response | 301/302/307/308 | `openid-connect` (login/logout/callback), `cas-auth`, `authz-casdoor`, `redirect` |
| `limited` | Traffic-control rejection | 429 | `rate-limit`, `limit-conn`, `limit-count`, `workflow` (limit-count action) |
| `broken` | Circuit breaker open | 502/503 | `api-breaker` |
| `preflight` | CORS preflight answered | 204 | `cors` |
| `abort` | Injected fault response | configurable | `fault-injection` |
| `routed` | Steered to and served by an alternate weighted target | n/a (backend-defined) | `traffic-split` |
| `hit` | Served from cache | n/a (cached status) | `proxy-cache` |

The audit table above comes from grepping deliberate `status_code` writes; the
implementation plan must still walk all plugins in `src/plugins/` against the
criterion (including ones with no status write, e.g. routing-decision plugins
like `traffic-split`) and record a port-or-error verdict for each. A plugin
with no alternate outcomes keeps the default `success` + `error` pair and
needs no code change beyond the mechanical trait-signature update.

## Plugin contract changes

`src/plugins/mod.rs`:

```rust
pub struct PluginOutput {
    pub context: Context,
    /// Declared output port this result leaves on. None = `success`.
    pub port: Option<&'static str>,
}

impl PluginOutput {
    pub fn success(context: Context) -> Self;          // port: None
    pub fn on_port(context: Context, port: &'static str) -> Self;
}

#[async_trait]
pub trait Plugin: Send + Sync {
    fn plugin_type(&self) -> &str;
    /// Static declaration of this node type's ports.
    fn ports(&self) -> &'static PortSpec;              // default: success+error, generic descriptions
    async fn execute(&self, ctx: Context) -> PluginResult;
}
```

- `named_inputs` parameter and `named_outputs` field are **removed** (dead
  plumbing — the engine has always passed an empty map). This touches every
  plugin file; it rides along with the sweep.
- `PluginExecutionError` is unchanged; `Err` routes to the error port exactly
  as today.
- `PortSpec` is a static structure: input description + `&[PortDecl]` where
  `PortDecl { name, kind, description }`. A `port_spec(type_name)` lookup
  lives alongside `create_plugin` in the factory so the admin catalog can
  serve port metadata without instantiating a node; the trait method and the
  lookup return the same statics — one source of truth.

Example migrations:

- `cors`: preflight branch returns `PluginOutput::on_port(ctx, "preflight")`
  with the 204 prepared. Non-preflight requests return `success` (headers
  added, flow continues to upstream).
- `openid-connect`: `redirect()` helper returns `on_port(ctx, "redirect")`;
  `reject()` returns `on_port(ctx, "denied")`. `Err` remains for discovery/
  JWKS/introspection failures.
- `key-auth`: missing/unknown key → `on_port(ctx, "denied")`; consumer-store
  failure → `Err`.

## Engine changes (`src/graph/engine.rs`)

- `CompiledGraph` replaces `success_edges`/`error_edges` with
  `edges: HashMap<(String /* node */, String /* port */), String /* target */>`.
  `out` normalizes to `success` at parse time.
- On `Ok(output)`: port = `output.port.unwrap_or("success")`; look up
  `(node, port)`. Terminal `client` handling is unchanged. A missing edge
  cannot happen at runtime: validation guarantees every non-error port is
  wired, so success-path walks always end at a terminal `client` node. (The
  engine keeps a defensive end-of-chain break, but reaching it is a bug.)
- On `Err`: unchanged — per-node error edge, else policy catch-all, else
  default 500.

### Compile-time validation

`compile_policy` (and supernode expansion before it) rejects a policy when:

1. An edge's `from` port is not declared by that node's type (replaces
   today's blanket "unknown port" check).
2. A declared output port of kind `success` or `outcome` on any node instance
   has no outgoing edge. **Exception:** none — this includes `success`;
   every non-terminal node must wire every non-error port. (`client` declares
   no outputs; `listener` declares only `out`.)
3. The `error` port is exempt from mandatory wiring: its documented fallback
   chain (per-node edge → policy catch-all `error_handler` → default 500)
   is preserved unchanged.
4. Two edges leave the same `(node, port)` (fan-out remains unsupported, as
   today — previously implicit via map overwrite, now an explicit error).

Admin API `PUT /policies` and file/etcd loads all pass through
`compile_policy`, so invalid policies are rejected at the door with a message
naming the node, port, and rule violated.

## Supernodes (`src/graph/expand.rs`)

- Boundary pseudo-nodes stay `input` / `output` / `error` — no new boundary
  types.
- Inner nodes' named ports follow the same mandatory-wiring rule *inside the
  definition*: each must route to another inner node, to `output`, or to
  `error`. Validation runs on the expanded graph, so the existing expansion
  machinery mostly needs port-name passthrough (it already splits endpoints
  on `node.port`); the `success`/`error` special cases extend to "success or
  outcome kind" vs "error kind".
- The black-box guarantee for unwired inner *error* ports (exit via the
  supernode's error boundary) is unchanged.

## Observability

- **Metrics:** `node_errors` now counts only genuine `Err` results. No label
  changes to existing metrics (dashboards keep working); no new port-level
  counter in this effort.
- **Debug traces:** `EdgeKind` gains a `Port` variant carrying the port name
  (serialized into the trace step); the Debug panel shows which port a step
  left on. `StepOutcome::Success` applies to outcome-port emissions —
  they are successes.
- **`ctx.errors`:** no longer collects entries for deny/redirect/throttle
  outcomes. Loggers that need to distinguish these outcomes have the response
  status code, as before.

## Admin API & UI

- The plugin-catalog endpoint (`src/admin/`) adds each type's `ports`:
  `{ input: {description}, outputs: [{name, kind, description}] }`, sourced
  from the same static `PortSpec` the engine validates against — one source
  of truth.
- `ui/src/components/PluginNode.tsx` renders handles from the catalog instead
  of the hard-coded `in`/`success`/`error` trio: `in` left; outputs stacked
  right, colored by kind (success = green, outcome = amber/accent,
  error = red), tooltip = description. Handle ids are port names, so edge
  serialization (`node_id.port`) is unchanged.
- The editor surfaces unwired mandatory ports as a validation warning before
  save (the server rejects on save regardless).
- `pluginMeta.tsx` keeps icon/color; port data comes from the catalog API.

## Migration (clean break)

Existing `gateway.yaml` policies **will fail to load** after upgrade when they
use swept plugins, until every new mandatory port is wired (typically
straight to `client`). This is deliberate (explicitness over silent behavior
change). Deliverables:

- A migration guide on the docs site: per-plugin table of new ports + the
  one-line edge to add for the common case (`from: <node>.denied`,
  `to: client.in`).
- Release notes flagging the config-breaking change (pre-1.0 minor bump).
- The e2e fixtures and example configs in the repo are updated in the same
  change.

## Testing

- **Engine unit tests:** routing by named port; each compile-validation rule
  (undeclared port, unwired mandatory port, duplicate `(node, port)` edge);
  error-port fallback chain unchanged.
- **Expansion tests:** named ports through supernode prefixing; inner
  mandatory-wiring validation; error black-box guarantee still holds.
- **Per-plugin tests:** each swept plugin's alternate outcomes assert the
  port name on `Ok` instead of `Err` (e.g. `cors` preflight, `oidc`
  redirect/denied, `key-auth` denied, `rate-limit` limited).
- **E2E:** `E2E-DP-09` flips from `test.fail` to a passing assertion (204 +
  CORS headers actually reach the client). New scenarios: oidc login redirect
  reaches the browser with `node_errors` unchanged; a policy with an unwired
  `denied` port is rejected by the Admin API with a clear message. UI
  scenario: a `cors` node renders three output handles and the editor blocks
  save on an unwired mandatory port.
- **Docs:** roadmap entry moves to fixed; plugin pages document their ports;
  the node-graph concepts page explains port kinds and the vocabulary table.

## Non-goals

- Multiple input ports / join-merge semantics (single `in` stays).
- Data-passing between nodes via named outputs (`named_outputs` is removed,
  not repurposed).
- Fan-out (multiple edges from one port) and conditional edges.
- Port-level Prometheus counters or metric label changes.
- Automatic config migration tooling (docs-guide only).
- Custom ports on scripted (Lua) nodes — `script` keeps `success` + `error`
  in this effort; letting a script declare and emit dynamic ports needs its
  own design (dynamic port names break the static `PortSpec` contract).
