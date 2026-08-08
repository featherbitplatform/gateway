# Named Output Ports — Per-Plugin Port Audit Ledger

**Date:** 2026-08-08
**Scope:** every entry in `KNOWN_PLUGIN_TYPES` (`src/plugins/mod.rs:97-182`, 86 types).
**Status:** authoritative worklist for the migration tasks (6-10) that follow. If this
ledger and the plan's per-task lists disagree, this ledger wins — the executing task
notes the delta in its commit message.

## Method

For every type, `create_plugin`'s match arm (`src/plugins/mod.rs`) was used to locate
the plugin file under `src/plugins/native/` (or `src/plugins/script/` for `script`).
Fast triage was:

```
grep -n "status_code = \|Err(PluginExecutionError" src/plugins/native/<file>.rs
```

Every hit's surrounding function was then read in full and classified against the
criterion:

> `Err` (error port) is reserved for *the node could not do its job* — configuration,
> parse, or infrastructure failures (upstream unreachable, store down, malformed input
> the node cannot process).
> An `outcome` port is *the node did its job and the result is an alternate route* —
> deliberate client-facing responses (deny, redirect, throttle, preflight) or a routing
> decision.

Standard port vocabulary (no synonyms invented anywhere in this ledger): `denied`,
`redirect`, `limited`, `broken`, `preflight`, `abort`, plus `routed` and `hit` — added
2026-08-08 by human decision to resolve the `traffic-split`/`proxy-cache` structural
exceptions (see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #3).

The design draft's "known adopters" table (`docs/superpowers/specs/2026-08-07-named-output-ports-design.md`)
was treated as a hypothesis to verify, not ground truth. Where the code disagreed, the
code wins — every such case is called out in the row's evidence and in
[Discrepancies](#discrepancies-vs-the-design-drafts-expectations) below.

## Ledger

| type | verdict | outcomes moved off Err | evidence (file:line) |
|---|---|---|---|
| proxy-rewrite | default | n/a | `src/plugins/native/proxy_rewrite.rs:169-224` — pure header/path mutation, always `Ok(success)`; doc comment: "never fails at execution time." |
| upstream | default | n/a | `src/plugins/native/upstream.rs:251-252` (WS upgrade, status 101, is success — a protocol handshake, not a rejection); `:289-323` Err only for `OutboundError::{Timeout,InvalidRequest,Transport}` (genuine connect failure); `:326-330` relays backend status verbatim as success, no branch on the value. |
| aws-lambda | default | n/a | `src/plugins/native/aws_lambda.rs:288-333` — success relays the function's own reply (`apply_response`) regardless of its status; `Err` (319-338) only for outbound Timeout/InvalidRequest/Transport. |
| azure-functions | default | n/a | `src/plugins/native/azure_functions.rs:141-177` — same shape: relay-as-success, `Err` only on outbound infra failure via `faas::classify_error`. |
| openwhisk | default | n/a | `src/plugins/native/openwhisk.rs:193-272` — success maps the OpenWhisk JSON envelope's own `statusCode`/`body`; `Err` (250-256) on transport failure, and (265, 503) on an unparseable envelope — "malformed input the node cannot process," stays Err. |
| openfunction | default | n/a | `src/plugins/native/openfunction.rs:122-158` — identical relay-as-success pattern; `Err` only for outbound Timeout/InvalidRequest/Transport. |
| error-handler | default (structural) | n/a | `src/plugins/native/error_handler.rs:63-85` — always `Ok(success)`; consumes `ctx.errors` to render the catch-all body, never produces an outcome itself. |
| listener | default (structural) | n/a | `src/plugins/native/listener.rs:22-27` — unconditional `Ok(success)`, graph entry point, no `in` port. |
| client | default (structural) | n/a | `src/plugins/native/client.rs:22-27` — unconditional `Ok(success)`, graph terminal, no outputs. |
| cors | preflight | 204 OPTIONS short-circuit, currently on `success` (this plugin has no `Err` path at all) | `src/plugins/native/cors.rs:172` |
| rate-limit | limited | 429 rate-limit rejection | `src/plugins/native/rate_limit.rs:158,175` — only alternate branch; no infra-failure path exists in this plugin. |
| limit-conn | limited | over-ceiling connection rejection (configurable `rejected_code`) | `src/plugins/native/limit_conn.rs:227,244` — only alternate branch; no infra-failure path exists. |
| api-breaker | broken | circuit-open short-circuit (`break_response_code`) | `src/plugins/native/api_breaker.rs:266,276`. Note: the `Role::Observe` state-update arm (282-294) never returns `Err` — after this migration `api-breaker` has **no** remaining `Err` path (discrepancy: the draft assumed one would remain — see below). |
| proxy-cache | hit | cache **HIT** short-circuit | `src/plugins/native/proxy_cache.rs:293-318` (pre-Task-10 line numbers; migrated in Task 10) — cache HIT short-circuits, now via `Ok(PluginOutput::on_port(ctx, "hit"))` with the cached response already populated; there is no cache-backend-failure path anywhere in the file (`cache.get`/`cache.put` are infallible in-process calls), so this is a deliberate "skip upstream, serve this" routing decision, not an infra failure. **Resolved 2026-08-08** (human decision, see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #3): the vocabulary gained the `hit` term rather than staying on `default`. |
| limit-count | limited | 429-style `rejected_code` quota rejection | `src/plugins/native/limit_count.rs:253,260`. The 500 at `:223,229` (counter-backend failure, `allow_degradation` fail-open/closed) is a genuine infra failure and stays `Err`. |
| proxy-mirror | default | n/a | `src/plugins/native/proxy_mirror.rs:168-184` — fire-and-forget mirror; mirrored call's response/errors are dropped; always `Ok(success)`. |
| ip-restriction | denied | 403 deny-list / allow-list-miss rejection | `src/plugins/native/ip_restriction.rs:126-144,147-165` |
| consumer-restriction | denied | 401 no-consumer / 403 blacklist-whitelist-method rejection | `src/plugins/native/consumer_restriction.rs:195-211,237,249,257,267` |
| acl | denied | 401 no-consumer / 403 denied_by/allowed_by rejection | `src/plugins/native/acl.rs:99-116,136,154,162` |
| attach-consumer-label | default | n/a — no `status_code` write, no `Err` anywhere in the file | `src/plugins/native/attach_consumer_label.rs` (whole file); pure passthrough that never rejects. Not in the design draft's table for this reason. |
| ua-restriction | denied | 403/`rejected_code` blocked-User-Agent rejection | `src/plugins/native/ua_restriction.rs:130-147,171,187` |
| referer-restriction | denied | 403 blocked/missing-referer rejection | `src/plugins/native/referer_restriction.rs:159-175,208` |
| uri-blocker | denied | 403/`rejected_code` matching-block-rule rejection | `src/plugins/native/uri_blocker.rs:125-150` |
| csrf | denied | 401 missing/mismatched/invalid CSRF token | `src/plugins/native/csrf.rs:227-243,275,281,285,289` |
| request-size-limit | denied | 413 body-exceeds-`max_bytes` rejection | `src/plugins/native/request_size_limit.rs:57-76` |
| key-auth | denied | 401 missing/invalid API key | `src/plugins/native/key_auth.rs:127-145,218` |
| basic-auth | denied | 401 missing/invalid Basic credentials | `src/plugins/native/basic_auth.rs:115-137,230` |
| jwt-auth | denied | 401 missing/invalid/unverifiable JWT | `src/plugins/native/jwt_auth.rs:134-154,220,231,241,261,264,267` |
| hmac-auth | denied | 401 missing/invalid signature, clock skew, missing signed header, unknown key | `src/plugins/native/hmac_auth.rs:326-349,530,535,545,565,574,588` |
| jwe-decrypt | denied | 401 missing/malformed/undecryptable token, unknown `kid` | `src/plugins/native/jwe_decrypt.rs:216-236,298,308,315,323,327,334,337,342,346,350,359,372` |
| multi-auth | denied | 401 when every configured sub-plugin fails | `src/plugins/native/multi_auth.rs:104-122,150` |
| forward-auth | denied | non-2xx auth-service reply mirrored to client | `src/plugins/native/forward_auth.rs:266-289,346` (`build_deny`); `build_error` (`:293-304`, used at `:341`) is a genuine callout failure and stays `Err`. Draft flagged this plugin as unverified — confirmed clean separation, only `denied` needed. |
| opa | denied | `allow:false` decision, OPA-supplied status/body/headers | `src/plugins/native/opa.rs:222-244,444` (`build_deny`); `build_error` (`:248-259`, used at `:405,427,439`) covers encode/callout/parse failures and stays `Err`. Draft flagged this plugin as unverified — confirmed, only `denied` needed. |
| opentelemetry | default | n/a | `src/plugins/native/opentelemetry.rs:345-354` — `execute()` always `Ok(success)`; export failures are silently swallowed via fire-and-forget `tokio::spawn` (`:268-270,284-286`). The grep hit for `status_code = 503` (`:433`) is **inside `#[cfg(test)] mod tests`** (module starts `:358`) — test-fixture data for `build_otlp`'s span-status mapping, not a production write. See [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #1 — the draft's "its 503 is an infra failure" premise was based on a misread of that grep hit; net verdict (`default`) is unchanged, but the reasoning was wrong. |
| zipkin | default | n/a | `src/plugins/native/zipkin.rs:315-324` — always `Ok(success)`; `status_code` (`:298`) read-only for the span tag; export-send failures swallowed (`:247-249,263-265`). |
| skywalking | default | n/a | `src/plugins/native/skywalking.rs:322-327` — always `Ok(success)`; `status_code` (`:293`) read-only; fire-and-forget export. |
| prometheus | default | n/a | `src/plugins/native/prometheus.rs:104-121` — always `Ok(success)`; no `status_code` writes, no `Err`; pure counter bump. |
| ldap-auth | denied | 401 missing/malformed header, empty creds, bind rejected | `src/plugins/native/ldap_auth.rs:133-157,224,229,236,250`. Note: `:251-267` (connection error) correctly stays a raw `Err`, but `:268` (bind **timeout**) is currently routed through the same `reject()`/401 helper as the deliberate denials — see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #4. |
| wolf-rbac | denied | 401 missing/invalid token or wolf-server deny decision | `src/plugins/native/wolf_rbac.rs:121-142,253,258,338`. `:294-309` (outbound `Err`, 500) correctly stays on a raw `Err`, not through `reject()`. Draft flagged this plugin as unverified — confirmed clean, only `denied` needed. |
| cas-auth | denied, redirect | 401 invalid/missing ticket → denied; 302 login/callback/logout → redirect | reject(): `src/plugins/native/cas_auth.rs:181-202` used at `:343,357`; redirect(): `:206-231` used at `:321,341,350` |
| authz-casbin | denied | 403 when `enforce()` returns false | `src/plugins/native/authz_casbin.rs:175-192,253`. `:254-273` (enforcer evaluation `Err`, doc'd "should not happen with a valid model") is currently folded into the same deny path — flagged as a judgment call, see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #5. |
| authz-keycloak | denied | 403 no-permissions / missing-bearer / Keycloak-denied decision | `src/plugins/native/authz_keycloak.rs:164-182,276,284,313`. `:317-323` (outbound timeout/transport) is folded into the same `deny()` path today — should split to `Err` post-migration, see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #4. |
| authz-casdoor | denied, redirect | 403 stateless-token rejection → denied; 302 login/callback/logout → redirect | deny(): `src/plugins/native/authz_casdoor.rs:250-267` used at `:413,417,421,479,500`; redirect(): `:271-291` used at `:379,443,472`. `:426` and `:507-514` mix callout-failure handling into the same deny/redirect helpers — see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #4. |
| openid-connect | denied, redirect | 401 (missing/invalid bearer, CSRF/nonce/session failure) → denied; 302 (login/callback/logout) → redirect | reject(): `src/plugins/native/openid_connect.rs:548-572` used at ~12 call sites incl. `:641,658,697,700,706,718,723,734,737,747`; redirect(): `:1032-1054` used at `:585,683,766`. Several `reject()` call sites also cover genuine JWKS/discovery/token-endpoint/JSON-parse infra failures folded into the same 401 path — see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #4. |
| dingtalk-auth | denied | 401 missing code / DingTalk-rejected code | `src/plugins/native/dingtalk_auth.rs:249-269,389,394,399`. The plugin already distinguishes `DingtalkError::Unauthorized` vs `::Upstream` internally, but `execute()` routes both through the same `reject()`/401 call — `Upstream` (callout failure) should split to `Err` post-migration, see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #6. |
| feishu-auth | denied | 401 missing code / Feishu-rejected code/token | `src/plugins/native/feishu_auth.rs:233-253,364,369,374`. Same `Unauthorized`/`Upstream` conflation as dingtalk-auth — see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #6. |
| logging | default | n/a | `src/plugins/native/logging.rs:59-85` — always `Ok(success)`; `status_code` (`:68`) read-only for the log record. |
| http-logger | default | n/a | `src/plugins/native/http_logger.rs:249-257` — always `Ok(success)`; batch-flush failures become an internal `FlushError` (`:85-89`), never surfaced as `PluginExecutionError`. |
| loki-logger | default | n/a | `src/plugins/native/loki_logger.rs:271-279` — always `Ok(success)`; send failures → internal `FlushError` (`:88-92`); config-parse `Err` (`:157`) is construction-time only. |
| splunk-hec-logging | default | n/a | `src/plugins/native/splunk_hec_logging.rs:225-233` — always `Ok(success)`; send failures → internal `FlushError` (`:94-98`). |
| datadog | default | n/a | `src/plugins/native/datadog.rs:243-249` — always `Ok(success)`; socket errors → internal `FlushError` (`:127-136`). |
| loggly | default | n/a | `src/plugins/native/loggly.rs:214-222` — always `Ok(success)`; send failures → internal `FlushError` (`:90-94`). |
| tcp-logger | default | n/a | `src/plugins/native/tcp_logger.rs:175-183` — always `Ok(success)`; only `Err` is construction-time "TLS not yet supported" (`:133`). |
| udp-logger | default | n/a | `src/plugins/native/udp_logger.rs:155-163` — always `Ok(success)`; per-socket failures wrapped in internal `FlushError` (`:57-71`), never surfaced. |
| syslog | default | n/a | `src/plugins/native/syslog.rs:280-297` — always `Ok(success)`; only construction-time Errs (`:220,231`). |
| file-logger | default | n/a | `src/plugins/native/file_logger.rs:144-152` — always `Ok(success)`; no `status_code`/`Err(PluginExecutionError` matches at all. |
| error-log-logger | default | n/a | `src/plugins/native/error_log_logger.rs:166-172` — always `Ok(success)`; the `status_code: 500` at `:204` is inside a test-context builder (sample data), not plugin behavior. |
| google-cloud-logging | default | n/a | `src/plugins/native/google_cloud_logging.rs:457-466` — always `Ok(success)`; send failures → internal `FlushError` (`:435-443`). |
| skywalking-logger | default | n/a | `src/plugins/native/skywalking_logger.rs:205-221` — always `Ok(success)`; send failures → internal `FlushError` (`:183-191`). |
| elasticsearch-logger | default | n/a | `src/plugins/native/elasticsearch_logger.rs:263-271` — always `Ok(success)`; send failures → internal `FlushError` (`:241-249`). |
| clickhouse-logger | default | n/a | `src/plugins/native/clickhouse_logger.rs:237-245` — always `Ok(success)`; send failures → internal `FlushError` (`:215-223`). |
| sls-logger | default | n/a | `src/plugins/native/sls_logger.rs:451-459` — always `Ok(success)`; send failures → internal `FlushError` (`:429-437`). |
| tencent-cloud-cls | default | n/a | `src/plugins/native/tencent_cloud_cls.rs:323-338` — always `Ok(success)`; send failures → internal `FlushError` (`:301-309`). |
| lago | default | n/a | `src/plugins/native/lago.rs:235-253` — always `Ok(success)`; send failures → internal `FlushError` (`:213-221`). |
| request-id | default | n/a | `src/plugins/native/request_id.rs:96-122` — always `Ok(success)`; only `Err` is construction-time format validation (`:67`). |
| real-ip | default | n/a | `src/plugins/native/real_ip.rs:195-230` — every branch (bad/missing/untrusted address) falls through a local `passthrough()` returning `Ok(success)`; no `Err` in `execute` at all; construction-time Errs (`:114,127`) only. |
| redirect | redirect | 3xx redirect response (both `uri`-template and `http_to_https` modes) | `src/plugins/native/redirect.rs:161`. Distinct pass-through branch (`:137`, already-HTTPS request under `http_to_https` mode) stays `default`/`success` — confirms the design draft's suspicion that this plugin has a non-redirect branch too. |
| echo | default | n/a | `src/plugins/native/echo.rs:143-193` — single always-`Ok` body-mutation path (wraps/replaces the upstream body per config); not in the design draft's table. |
| fault-injection | abort | configured abort response (`abort.http_status`) | `src/plugins/native/fault_injection.rs:319,325` — only alternate branch besides success/delay-then-fallthrough; no infra-failure path exists. |
| workflow | denied, limited | `return` action's configured-status response → denied (approximation, see note); `limit-count` action's quota-exceeded rejection → limited | reject(): `src/plugins/native/workflow.rs:238-260`; `return` action `:296-305`; limit-count rejection `:335-347`. The counter-backend `Err` at `:318-327` is a genuine infra failure and stays `Err`. Note: the `return` action is a generic "respond with any configured status 100-599" mechanism (`:152-161`), not specifically a deny — mapping it to `denied` is an approximation; `abort` is a plausible alternative reading (see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #2). The `limit-count` action is not mentioned in the design draft's table at all. |
| traffic-label | default | n/a | `src/plugins/native/traffic_label.rs:204-243` — whole `execute`, single always-`Ok` path, no `status_code` writes, no `Err`. |
| traffic-split | routed | target-slot-picked proxy short-circuit | `src/plugins/native/traffic_split.rs:354-360,363-369` (pre-Task-10 line numbers) — no-rule-matched / default-slot-picked → `Ok(success)`, genuine passthrough. `:373-385` — target-slot picked, request proxied and a real reply obtained → now `Ok(PluginOutput::on_port(ctx, "routed"))`, replacing the prior `Err(TRAFFIC_SPLIT_ROUTED)` convention — this is a deliberate routing decision per the criterion's own wording. `:386-405` (target unreachable, 502 `TRAFFIC_SPLIT_UPSTREAM_ERROR`) is a genuine infra failure and correctly stays `Err`. **Resolved 2026-08-08** (human decision, see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #3): the vocabulary gained the `routed` term rather than staying on `default`. |
| mocking | default | n/a | `src/plugins/native/mocking.rs:219` — the plugin's *only* path, always `Ok(success)`; per the task brief, a mock response is this plugin's success, not an alternate outcome. |
| response-rewrite | default | n/a | `src/plugins/native/response_rewrite.rs:423-461` — unconditional config-driven status/body/header rewrite; always `Ok(success)`. (`:278` grep hit is config parsing, not a runtime write.) |
| gzip | default | n/a | `src/plugins/native/gzip.rs:207-243` — compression failure is logged and swallowed, body left uncompressed; always `Ok(success)`. |
| brotli | default | n/a | `src/plugins/native/brotli.rs:137-176` — same swallow-and-continue pattern as gzip; always `Ok(success)`. |
| error-page | default | n/a | `src/plugins/native/error_page.rs:127-149` — never writes `status_code` itself; only rewrites body/headers for an already-set, already-gateway-generated status (`ctx.errors` non-empty gate, `:133`); always `Ok(success)`. Not in the design draft's table. |
| exit-transformer | default | n/a | `src/plugins/native/exit_transformer.rs:116-141` — remaps an existing status via `status_map` / renders a body only when `always` or the status is already gateway-generated; always `Ok(success)`. |
| data-mask | default | n/a | `src/plugins/native/data_mask.rs:338-386` — no runtime `Err`; a non-JSON body silently skips body rules (`:365`, `Err(_) => continue` is a soft skip, not a returned error); always `Ok(success)`. |
| request-validation | denied *(judgment confirmed by human 2026-08-08)* | header/body schema-validation rejection (missing body, bad JSON, schema mismatch) | `src/plugins/native/request_validation.rs:183,224,230,252,258` (pre-Task-10 line numbers; migrated in Task 10, all via `self.reject`); no infra-failure path exists in this plugin. See [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #7 — the design draft doesn't cover this plugin; `denied` was flagged as the closest vocabulary term but a reasoned, not spec-confirmed, call. Confirmed by human decision 2026-08-08 alongside the rest of the restriction family. |
| body-transformer | default | n/a | `src/plugins/native/body_transformer.rs:220-239` (`fail()` helper) — per its own doc comment, an error-port rejection for "an undecodable body," i.e. malformed input the node cannot process — matches the `Err` criterion literally, unlike request-validation/oas-validator (see note there). |
| degraphql | default | n/a | `src/plugins/native/degraphql.rs:184-190` (405 unsupported method), `:218-229` (400 undecodable JSON body it must transform) — malformed input the node cannot process, stays `Err`. |
| oas-validator | denied *(judgment confirmed by human 2026-08-08)* | missing required query/header param, missing body, invalid JSON, schema mismatch | `src/plugins/native/oas_validator.rs:358,399,406,413,430,435` (pre-Task-10 line numbers; migrated in Task 10, all via `self.reject`); no infra-failure path exists. Same reasoning as `request-validation` — see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #7. Confirmed by human decision 2026-08-08 alongside the rest of the restriction family. |
| serverless-pre-function | default | n/a | `src/plugins/native/serverless_pre_function.rs:111-159` — runs configured Lua via the shared `ServerlessRunner`; any Lua execution error propagates through `?` to `Err` (`LUA_EXECUTION_ERROR`) — a genuine script-execution failure. A script author can itself write `ctx.response` and return success (script content, not a Rust-level branch), so there is nothing here for the engine to route on a named port. |
| serverless-post-function | default | n/a | `src/plugins/native/serverless_post_function.rs:55-61` — identical shape and reasoning to serverless-pre-function. |
| script | default | n/a — kept `success`+`error` by design; dynamic ports for Lua scripts are an explicit non-goal of this migration | `src/plugins/script/mod.rs:126-146` — `Plugin` trait impl confirmed: Lua `Ok` → success, `Err` → error, no custom ports. |

## Discrepancies vs. the design draft's expectations

1. **opentelemetry** — the draft's premise ("its 503 is an overload/infra failure → stays
   error") was based on a misread: the only `status_code = 503` write in the file is
   inside `#[cfg(test)] mod tests`, used to verify `build_otlp`'s span-status mapping —
   there is no production write at all. `execute()` unconditionally returns
   `Ok(success)`; export failures are silently swallowed. Net verdict (`default`) matches
   the draft, but for a different reason than assumed.
2. **workflow** — its `return` action is a generic "respond with any configured status
   100-599" mechanism, not specifically a deny; mapping it to `denied` is an
   approximation kept for consistency with the rest of the auth/restriction family. Its
   separate `limit-count` action (a quota-exceeded rejection) is not mentioned in the
   design draft's table at all and needed independent classification (`limited`).
3. **traffic-split** and **proxy-cache** — both have a genuine bimodal shape (continue to
   the normal upstream vs. terminate with a self-produced response), and both terminal
   branches are deliberate routing decisions per the criterion's own wording ("...or a
   routing decision"). Neither "steered to and served by an alternate weighted backend"
   (traffic-split) nor "served from cache" (proxy-cache) matched any of the original
   six vocabulary terms (`denied`, `redirect`, `limited`, `broken`, `preflight`, `abort`).
   This ledger originally recorded both as `default` (keeping the pre-Task-10 `Err`
   short-circuit convention) and surfaced the tension for whoever picked up tasks 6-10.
   **Resolved 2026-08-08** (human decision, ahead of Task 10): the vocabulary was
   extended with two new terms — `routed` for traffic-split's proxied-and-served
   short-circuit and `hit` for proxy-cache's served-from-cache short-circuit. Both
   plugins were migrated in Task 10; their rows above reflect the new verdicts. The
   draft's own framing of `traffic-split` as a "routing-decision plugin... with no
   status write" was also factually wrong (it does write status and does proxy) —
   corrected here.
4. **authz-keycloak, authz-casdoor, ldap-auth, openid-connect** — each folds a genuine
   outbound-infra failure (timeout, transport error, unparseable provider response) into
   the same helper/status as its deliberate denial or redirect (authz-keycloak
   `:317-323`; authz-casdoor `:426`,`:507-514`; ldap-auth `:268` bind timeout, notably
   *unlike* the adjacent `:251-267` connection-error branch which already stays on a raw
   `Err`; openid-connect's shared `reject()` helper across ~12 call sites). Splitting
   these into "denied/redirect on deliberate rejection, Err on callout failure" requires
   case-by-case code changes during migration, not just a port declaration — flagged so
   the migration task doesn't assume a single mechanical `Err`→`on_port` swap is
   sufficient for these four.
5. **authz-casbin** — the `enforcer.enforce()` evaluation-error branch (doc'd "should not
   happen with a valid model") is currently folded into the same 403-deny path as a
   legitimate policy denial. Arguably belongs on `Err` (it's an evaluation/config bug,
   not a policy decision) rather than `denied` — flagged as uncertain, not asserted.
6. **dingtalk-auth, feishu-auth** — both already distinguish `Unauthorized` vs.
   `Upstream` (callout failure) internally at the error-type level, but `execute()`
   currently routes both variants through the same `reject()`/401 call. `Upstream`
   should split to `Err` post-migration.
7. **request-validation, oas-validator** — not covered by the design draft's table at
   all. Both plugins' sole purpose is gating non-conforming requests (no infra-failure
   path exists in either file), so per the criterion a rejection is the node doing its
   job, not failing — `denied` was chosen as the closest vocabulary term. An alternative
   reading groups them with `degraphql`/`body-transformer` ("malformed input the node
   cannot process," stays `Err`) since their rejection is a byproduct of failing to
   validate a request rather than a policy-style deny. This ledger originally recorded
   them as `denied` but flagged the classification as debatable. **Resolved 2026-08-08**
   (human decision, ahead of Task 10): both migrate to `denied` alongside the rest of
   the restriction family. Migrated in Task 10.
8. **api-breaker** — the draft assumed a distinct "infra failure updating breaker state"
   path would remain on `Err` alongside the new `broken` port. Reading the code, no such
   path exists: the `Role::Observe` (state-update) arm never returns `Err` at all. After
   adding `broken`, this plugin has zero remaining `Err` paths — worth flagging since the
   draft assumed one would survive.
9. **jwe-decrypt** — its consumer-credential-misconfiguration branches ("consumer has no
   jwe-decrypt credential," "secret must be 32 bytes") were classified as `denied` for
   consistency with the rest of the auth family, though they arguably resemble a
   configuration problem on a specific consumer's credential rather than a purely
   client-facing deliberate rejection. Low-confidence judgment call, not flagged as a
   full discrepancy.
10. **cors** — its 204 preflight response is on today's plain `success` port (this
    plugin has no `Err` path at all), so "moved off Err" doesn't literally apply here —
    it moves off the generic `success` port onto the dedicated `preflight` port.
11. **attach-consumer-label, echo, error-page** — none of these three appear in the
    design draft's known-adopters table. All three were independently confirmed as
    `default` (zero deliberate-response writes) — their absence from the draft's table
    was correct, not an oversight.

## Sanity checks

- **Row count**: 86 rows in the ledger table, matching `KNOWN_PLUGIN_TYPES.len()` (86,
  confirmed via `awk`/`grep` count against `src/plugins/mod.rs:97-182`).
- **Vocabulary**: every outcome-port name used in the `verdict` column is one of
  `denied`, `redirect`, `limited`, `broken`, `preflight`, `abort`, `routed`, `hit` — no
  further synonyms invented. `routed` and `hit` were added 2026-08-08 (human decision)
  specifically to resolve the `traffic-split`/`proxy-cache` structural exceptions noted
  below — see [Discrepancies](#discrepancies-vs-the-design-drafts-expectations) #3.
- **Evidence**: every row with a non-`default` verdict cites `file:line` for the
  deliberate response write(s) driving it. Default rows cite either "no status_code/Err
  writes" or the specific line(s) showing why an existing write is not a deliberate
  alternate-routing decision (relay-only, test fixture, config-time, swallowed failure,
  etc).

## Verdict summary

**Updated 2026-08-08** (post-Task-10) — `traffic-split` and `proxy-cache` moved out of
`default` once the vocabulary gained `routed`/`hit`; counts below reflect the final state.

- **86** total rows (one per `KNOWN_PLUGIN_TYPES` entry).
- **49** stay `default` (includes 3 structural nodes — `listener`, `client`,
  `error-handler`; `script`'s explicit non-goal note).
- **37** gain at least one new outcome port:
  - `denied` only — **24**: acl, authz-casbin, authz-keycloak, basic-auth,
    consumer-restriction, csrf, dingtalk-auth, feishu-auth, hmac-auth, ip-restriction,
    jwe-decrypt, jwt-auth, key-auth, ldap-auth, multi-auth, referer-restriction,
    request-size-limit, ua-restriction, uri-blocker, wolf-rbac, forward-auth, opa,
    request-validation, oas-validator
  - `denied` + `redirect` — **3**: cas-auth, authz-casdoor, openid-connect
  - `denied` + `limited` — **1**: workflow
  - `limited` only — **3**: rate-limit, limit-conn, limit-count
  - `broken` only — **1**: api-breaker
  - `preflight` only — **1**: cors
  - `redirect` only — **1**: redirect
  - `abort` only — **1**: fault-injection
  - `routed` only — **1**: traffic-split
  - `hit` only — **1**: proxy-cache
