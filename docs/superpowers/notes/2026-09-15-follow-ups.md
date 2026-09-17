# Follow-up notes — capabilities and fixes worth building

**Date:** 2026-09-15
**Origin:** the APISIX→featherbit migration and the streaming-responses work (0.9.0). Each item below came from a real thing we hit, not from speculation.

## 1. Persisted state for policies — `store-get` / `store-set`

**What we hit.** Bounding the OIDC callback retry to one attempt needed a value that survives a request. The only cross-request store a policy can write today is a cookie, via `response-rewrite` `headers.add: ["Set-Cookie: …"]`, read back as `cookie_<name>`. That worked, but it is client-side, size-limited, spoofable, and per-browser.

**The gap.** `stores:` (redis/valkey) exist and are already wired for `limit-count` and server-side sessions, but no node can read or write an arbitrary key. The OIDC session is not a substitute: it is owned by `openid-connect`, has no policy-facing write API, and in the retry case does not exist yet — the whole problem is that login has not completed.

**Shape.** A `store-set` node (`store`, `key` — both templated, `value`, `ttl_seconds`) and a `store-get` node that puts the value into `context.message` so it reads as `$msg_<name>` downstream. Keys templated the same way every other traffic-bound field is, so `key: "retry:{{request.headers.x-session}}"` works.

**Why it is worth it.** Retry/backoff counters, per-user feature flags, idempotency keys, cross-instance coordination that today has to be faked with `limit-count`. It also removes the temptation to reach for a Lua `script` node, which is currently the only way to compute anything (see §2).

## 2. Arithmetic in `set-vars`

**What we hit.** `n = n + 1` is not expressible declaratively. `set-vars` derives values (templates, JSONPath, regex captures) but cannot compute. Bounding retries to N>1 would need either a chain of conditions (`absent → 1`, `1 → 2`, `2 → stop`, three nodes per extra retry) or a Lua `script`.

**Caveat that makes this matter more.** `script`'s `timeout_ms` is **stored but not enforced** — a script that loops has no bound and hangs the request. Pushing simple arithmetic into Lua means pushing it past that missing guard. Either enforce the timeout or give `set-vars` enough arithmetic that scripts stay rare.

## 3. Conditional opt-out for the logger family

**What we hit.** All 18 loggers force response buffering, including ones whose `log_format` never references the body. A policy with `upstream → http-logger → client` cannot stream — a large and ordinary class of policies. `logging` itself genuinely reads `ctx.response.body.len()`, so it must block; the other 17 have a real `log_format` and could opt out when it carries no body reference.

**Shape.** Compute `log_format_references_body` once at config load (scan for `resp_body` / `response.body`) and have `reads_response_body()` return it. Same pattern `response-rewrite` already uses for `filters`/`body`.

## 4. Instance-aware `reads_response_body` for template-evaluating nodes

**What we hit.** `reads_response_body` is *field*-scoped, but `$resp_body`, `{{response.body}}` and `response_body:$…` JSONPath are readable from any template or condition. Four nodes that opt out evaluate one after the upstream: `traffic-label`'s matcher, `response-rewrite`'s `vars` gate, `proxy-rewrite`'s response-phase `add_headers`, and `request-id`'s `header_name`.

**The failure.** A `traffic-label` matching on `resp_body` silently stops matching on a streaming route — empty body, no error, no log, no `buffering` entry. Documented as a known limitation in 0.9.0; the fix is to have those four scan their parsed `Template`/`Expression` for a response-body reference and report `true` when found.

## 5. UI — the editor is getting cluttered

**Status:** fixed. The sidebar now shows one library at a time, chosen from a strip of buttons under the header (Supernodes / Plugin configs / Stores, each with a count). Routes is the default and owns the whole body instead of the quarter it had when four libraries were stacked at 260px each.

**What we hit (operator report).** Routes are barely visible on the canvas. Supernodes and plugin configs compete with them for the same space.

**Shape.** Move supernodes and plugin configs out of the main canvas area and into the top-right toolbar as buttons, alongside the existing Notifications / Agent / Chat / Sessions / Certificates set. Routes get the canvas back; the library-style panels open on demand like the others already do. Consistent with where the UI already puts panel-shaped things.

## 6. `max_traces` default is too small for a real deployment

**Status:** fixed. Every trace listing (Admin API and MCP `list_traces`) now carries a `retention` block — `truncated`, `evicted`, `oldest_seq`, `retained`, `max_traces` — so an empty result is no longer ambiguous, and the Debug panel says so in prose. The default is raised from 50 to 1000.

**What we hit.** The default is **50**. On a live app generating ~80 traces/minute, a trace survives about 35 seconds — long enough that a filtered query returns empty while an unfiltered one taken moments earlier showed matches. That cost real debugging time and produced a false "the filter is broken" conclusion.

**Shape.** Raise the default substantially (low thousands), or make it time-based rather than count-based, or surface the eviction in the trace list response (`"truncated": true`, oldest retained `seq`) so an empty filtered result is distinguishable from a rotated-out one. The last of those is the cheapest and removes the ambiguity entirely.

## 7. Debug panel: streamed responses render as 0-byte bodies

**Status:** fixed in 0.9.0 (`BodyCapture.streamed` + a badge in `TraceViewer`), noted here only because the same shape will recur for any future capture the backend marks but the UI does not render.

## 8. A store outage hangs the request instead of failing fast

**Status:** fixed. `connect_budget_ms` on `StoreConfig` (default 5000) bounds connect
+ retries + backoff together, enforced by a `tokio::time::timeout` around the first
connect. Measured before the fix: **17.96s** against a refused connection, which is
also the assertion the regression test now makes. A failed connect is not cached, so
the next request retries.

**What we hit.** Writing the live tests for the `store-*` nodes, the "point a store at a
closed port" case did not fail fast -- it hung for minutes. The test had to be rewritten
around a `WRONGTYPE` error on an already-connected client to stay deterministic.

**The gap.** `RedisStoreClient::conn` builds its `ConnectionManagerConfig` with
`set_connection_timeout` and `set_response_timeout` (both from `connect_timeout_ms`), but
leaves the retry policy at the crate defaults: `DEFAULT_NUMBER_OF_CONNECTION_RETRIES = 6`
and `max_delay: None` -- uncapped exponential backoff. `connect_timeout_ms` bounds a single
attempt, not the sum of attempts plus the waits between them.

**Why it matters beyond these nodes.** This is the shared path for *every* store consumer:
server-side sessions, `limit-count` with `policy: redis`, ACME certificate storage, and now
the `store-*` nodes. The documented contract is that a store failure surfaces as a `503` and
never fails open. That is true, but a `503` that arrives minutes later is a hang, and it
arrives on the first request after an outage begins -- exactly when a gateway should shed
load fastest.

**Shape.** Surface the retry policy on `StoreConfig` (a `max_retry_delay_ms`, or a total
budget bounding connect + retries), defaulting to something a request can survive. Leaving
the crate defaults is the bug; the value itself is a judgment call.

**It also unblocks a test.** Until the backoff is bounded, `store-*`'s "an unreachable store
exits `error`, never `miss`" case cannot be tested against a genuinely unreachable store --
only against a backend error, which exercises the same code path but not the same failure.

## 9. Browser-level e2e coverage for the `store-*` nodes is still owed

**Status:** fixed. `e2e/tests/policy-state.spec.ts` adds E2E-STORE-10..13, driving the nodes through real routes on the data plane (gated on `FEATHERBIT_TEST_REDIS_URL`, skipping cleanly without it). E2E-STORE-12 was restructured after a mutation check showed the first version could not fail against an unconditional `EXPIRE`.

**What we hit.** The final review of the `store-*` branch found four testbook rows
(`E2E-STORE-10..13`) describing browser-level scenarios for `store-get`/`store-incr`/
`store-delete` -- miss vs. success through a real route, TTL-bounded reset, delete-then-miss,
and the streaming/`buffering` report -- that were never backed by an actual Playwright test.
The rows were removed from `e2e/E2E_TESTBOOK.md` rather than left unbacked, since the gated
Rust live tests in `src/plugins/util/store_kv.rs` already exercise the same behavior through
the real engine.

**The gap.** None of that is exercised at the browser/e2e level: through an actual route,
against the admin API's policy-validate/buffering report, or via the UI. The e2e harness has
no redis available today, so these scenarios cannot be gated the way `E2E-SESS-*` is until
that changes.

**Shape.** Add a `store-kv` route/policy fixture (mirroring `oidc-redis`'s gating) once the
e2e harness has a redis service available, and re-add the four scenarios (or their
equivalents) to the testbook with real Playwright tests behind them, gated on
`FEATHERBIT_TEST_REDIS_URL` the same way `E2E-SESS-*` is.

## 10. A split `proxy-cache` pair is silent and total

**Status:** fixed. `validate_cache_pairs` runs at the head of `compile_policy`: a pair whose halves disagree about `policy` or `store` is rejected, naming both nodes; a half with no counterpart is reported in `cache_pairs` on both validate paths.

**What we hit.** The final whole-branch review of the distributed response cache found
that `proxy-cache`'s lookup and store nodes are validated independently: the compiler
checks each node's own `policy`/`store`/`id` in isolation, but nothing checks that the two
nodes of a pair *agree*. A lookup node configured `policy: redis, store: cache-a` paired
with a store node left on the `policy: local` default (or pointed at a different `store`)
compiles cleanly, runs without error, and produces a permanent 100% miss rate — the two
halves simply write to and read from different places and never see each other's entries.

**The gap.** The metrics make it worse, not better: the misses land under one `backend`
(and `store`) label, the never-read writes land under another, so neither series looks
wrong on its own. There is no cross-node `proxy-cache` validation anywhere in `src/graph/`
today (confirmed by grep), and the same shape has always existed for an `id` mismatch
between the two nodes — this is just the first version where the invariant spans two
independently-configured subsystems (`policy` and `store`) instead of one string.

**Why it is not in this fix round.** Validating that a `proxy-cache` pair agrees is real
design work of its own: the compiler would need to associate nodes across a policy graph
by `id` and role, decide what "agree" means when one side omits a key that defaults
(`policy` defaulting to `local`), and decide whether a mismatch is a hard compile error or
a lint-level warning. That is a different shape of change than the observability gaps this
round closed, and deserves its own design pass rather than a bolted-on check.

**Shape (for whoever picks this up).** At policy-compile time, group `proxy-cache` nodes by
`id`, and for each group with both a `lookup` and a `store` node, require `policy` (and, for
`redis`, `store`) to match; reject the policy otherwise, the same way an unwired
`success`/outcome port is rejected today.

## Not worth building (recorded so it is not re-proposed)

- **Storing retry counters in the OIDC session.** Wrong vehicle: the session does not exist pre-auth, which is exactly when the retry logic runs.
- **A query-parameter retry marker.** Does not survive the OIDC round trip — the marker lands on `/`, but the callback arrives at the fixed `redirect_uri` carrying no such param, so the guard never sees it and loops anyway. A cookie survives; a query param does not.
