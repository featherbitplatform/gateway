---
title: Plugins
description: Reference for every featherbit node type — native plugins, the Lua script node, and the structural listener/client nodes.
---

Every node in a featherbit policy graph is a plugin. A single factory — `create_plugin()` in `src/plugins/mod.rs` — maps each node's `type` string from the YAML config to a plugin instance. The plugin system is two-tier:

- **Native plugins** — node types implemented in Rust and compiled into the binary. Two of them, `listener` and `client`, are structural: they mark the entry and exit of every graph, take no configuration, and are documented together on their own page.
- **Scripted plugins** — the `script` node runs custom plugin logic written in Lua (Luau runtime), loaded from a file or inline.

Every plugin implements the same contract: `async fn execute(ctx, named_inputs) -> Result<PluginOutput, PluginExecutionError>`. On success, the (possibly mutated) Context flows out of the node's `success` port; on failure, the error carries the Context so the graph engine can route it through the node's `error` port instead. Invalid node configuration is rejected by the plugin's `from_config` at config load time, never at request time.

featherbit expresses the classic proxy phase model (rewrite/access/header_filter/body_filter) as explicit graph position: a "request-phase" plugin is a node wired before `upstream`; a "response-phase" plugin comes after it. Where a plugin supports a subset of a config schema, its page carries a note describing the exact behavior.

## Structural & core proxy

| Type | Description |
|---|---|
| [`listener`](listener-client.md) / [`client`](listener-client.md) | Fixed graph entry and exit nodes (no config) |
| [`upstream`](upstream.md) | Forward to a backend pool with round-robin, least-connections, or IP-hash balancing |
| [`proxy-rewrite`](proxy-rewrite.md) | Rewrite request path and headers |
| [`response-rewrite`](response-rewrite.md) | Rewrite response status, headers, and body (regex filters, encoding-aware) |
| [`body-transformer`](body-transformer.md) | Rewrite request/response JSON bodies via templates |
| [`degraphql`](degraphql.md) | Expose a REST endpoint backed by a GraphQL upstream |
| [`redirect`](redirect.md) | HTTP redirect, or force HTTP→HTTPS |
| [`echo`](echo.md) | Wrap or replace the response body (demo/testing) |
| [`gzip`](gzip.md) / [`brotli`](brotli.md) | Compress the response body when the client accepts it |
| [`request-id`](request-id.md) | Attach a unique request-id header |
| [`real-ip`](real-ip.md) | Recover the client IP from a trusted proxy header |

## Error handling & mocking

| Type | Description |
|---|---|
| [`error-handler`](error-handler.md) | Render custom error responses with template variables |
| [`error-page`](error-page.md) | Replace 404/500/502/503 bodies with configured pages |
| [`exit-transformer`](exit-transformer.md) | Remap status and rewrite the body of gateway-generated exits |
| [`mocking`](mocking.md) | Respond with a configured mock instead of proxying (terminal node) |

## Security & access control

| Type | Description |
|---|---|
| [`cors`](cors.md) | CORS preflight and response header management |
| [`csrf`](csrf.md) | Double-submit CSRF token validation |
| [`ip-restriction`](ip-restriction.md) | Allow/deny by IP or CIDR |
| [`ua-restriction`](ua-restriction.md) | Allow/deny by User-Agent regex |
| [`referer-restriction`](referer-restriction.md) | Allow/deny by Referer host |
| [`uri-blocker`](uri-blocker.md) | Block requests matching URI regex rules |
| [`request-size-limit`](request-size-limit.md) | Reject over-sized request bodies |
| [`request-validation`](request-validation.md) | Validate headers/body against JSON Schema |
| [`data-mask`](data-mask.md) | Mask or remove sensitive fields in bodies, headers, query |

## Traffic control

Several traffic plugins need to act **both before and after** the upstream call. featherbit expresses this as a **pair of nodes** wired around `upstream`, both configured with the same `id` (or key) and sharing process-wide state — the same request/response split `proxy-rewrite` uses. Each such page documents the pairing.

| Type | Description |
|---|---|
| [`rate-limit`](rate-limit.md) | Token-bucket rate limiting per IP or header key |
| [`limit-count`](limit-count.md) | Fixed-window request-count limiting (shared counters) |
| [`limit-conn`](limit-conn.md) | Concurrent-request limiting (acquire/release node pair) |
| [`api-breaker`](api-breaker.md) | Circuit breaker on unhealthy upstreams (check/observe pair) |
| [`traffic-split`](traffic-split.md) | Weighted / conditional traffic steering (canary, blue-green) |
| [`proxy-mirror`](proxy-mirror.md) | Fire-and-forget clone of requests to a shadow upstream |
| [`proxy-cache`](proxy-cache.md) | Cache upstream responses (lookup/store node pair) |
| [`fault-injection`](fault-injection.md) | Inject delays and abort responses (percentage + vars gated) |
| [`workflow`](workflow.md) | Ordered rules — reject or rate-limit the first matching case |
| [`traffic-label`](traffic-label.md) | Tag matching requests with headers and context labels |

## Serverless & FaaS

The FaaS plugins invoke an external function and return its reply as the gateway response — they **replace the upstream**, so wire their `success` edge to `client.in`. The serverless functions run inline Lua at a graph position (before or after the upstream) via the same runtime as the `script` node.

| Type | Description |
|---|---|
| [`serverless-pre-function`](serverless-pre-function.md) | Run inline Lua before the upstream |
| [`serverless-post-function`](serverless-post-function.md) | Run inline Lua after the upstream |
| [`oas-validator`](oas-validator.md) | Validate requests against an inline OpenAPI 3 spec |
| [`aws-lambda`](aws-lambda.md) | Invoke an AWS Lambda (SigV4 or API-key auth) |
| [`azure-functions`](azure-functions.md) | Invoke an Azure Function |
| [`openwhisk`](openwhisk.md) | Invoke an Apache OpenWhisk action |
| [`openfunction`](openfunction.md) | Invoke an OpenFunction function |

## Observability & logging

Logger plugins ship access logs to an external sink. They are **fire-and-forget**: a logger node builds a JSON entry from the request, hands it to a shared batch queue ([`BatchSink`](../../concepts/architecture.md)), and returns immediately — the request path never blocks on log I/O. Place a logger **after `upstream`** (so status and body are populated); wire it on error paths too if you want failures logged. All loggers share the batch keys (`batch_max_size`, `inactive_timeout`, `buffer_duration`, `max_retry_count`, `retry_delay`) and an optional `log_format` map of `name → "$var"` templates.

| Type | Description |
|---|---|
| [`logging`](logging.md) | Structured JSON access logging to stdout |
| [`http-logger`](http-logger.md) | Ship logs to an HTTP endpoint |
| [`tcp-logger`](tcp-logger.md) / [`udp-logger`](udp-logger.md) | Ship logs over a raw TCP / UDP socket |
| [`syslog`](syslog.md) | Ship logs via syslog (RFC 5424) over TCP or UDP |
| [`file-logger`](file-logger.md) | Append logs to a local file |
| [`error-log-logger`](error-log-logger.md) | Ship request-level errors to a TCP sink |
| [`elasticsearch-logger`](elasticsearch-logger.md) | Bulk-index logs into Elasticsearch |
| [`clickhouse-logger`](clickhouse-logger.md) | Insert logs into ClickHouse |
| [`loki-logger`](loki-logger.md) | Push logs to Grafana Loki |
| [`splunk-hec-logging`](splunk-hec-logging.md) | Ship logs to Splunk HEC |
| [`datadog`](datadog.md) | Emit DogStatsD metrics to the Datadog agent |
| [`loggly`](loggly.md) | Ship logs to SolarWinds Loggly |
| [`google-cloud-logging`](google-cloud-logging.md) | Ship logs to Google Cloud Logging |
| [`sls-logger`](sls-logger.md) | Ship logs to Alibaba Cloud SLS |
| [`tencent-cloud-cls`](tencent-cloud-cls.md) | Ship logs to Tencent Cloud CLS |
| [`skywalking-logger`](skywalking-logger.md) | Ship logs to Apache SkyWalking |
| [`lago`](lago.md) | Meter requests as Lago billing events |

## Tracing & metrics

featherbit exposes **built-in Prometheus metrics** (per-route request counts and latency, per-node execution metrics) at the admin `/metrics` endpoint — always on, no plugin required (see [Observability](../../guides/observability.md)). The plugins here add distributed tracing and extra metric dimensions.

The three tracers are **start/end node pairs**: a `start` node (placed after the listener) extracts or creates the trace context, propagates it to the upstream, and stores the span; an `end` node (after the upstream) exports the finished span to the collector, fire-and-forget. The span is carried per-request, so the pair needs no shared id.

| Type | Description |
|---|---|
| [`prometheus`](prometheus.md) | Adds a per-consumer request counter to the built-in metrics |
| [`opentelemetry`](opentelemetry.md) | OTLP/HTTP trace export with W3C `traceparent` propagation |
| [`zipkin`](zipkin.md) | Zipkin v2 trace export with B3 propagation |
| [`skywalking`](skywalking.md) | SkyWalking segment export with `sw8` propagation |

## Authentication & consumers

featherbit models API clients as **consumers** — named identities with per-auth-plugin credentials, declared under `consumers:` in `gateway.yaml` and managed via `/api/consumers`. Auth plugins with `use_consumers: true` resolve the presented credential to a consumer and attach its identity (`consumer.*` keys in `context.message`, `X-Consumer-*` headers); the restriction plugins then act on it.

| Type | Description |
|---|---|
| [`key-auth`](key-auth.md) | API-key auth via header or query; consumer-aware |
| [`basic-auth`](basic-auth.md) | HTTP Basic authentication; consumer-aware |
| [`jwt-auth`](jwt-auth.md) | HMAC JWT validation, inline or per-consumer secrets |
| [`hmac-auth`](hmac-auth.md) | HMAC request signing (access-key/secret-key), consumer-aware |
| [`jwe-decrypt`](jwe-decrypt.md) | Decrypt a JWE token (dir + A256GCM) into a forwarded header |
| [`multi-auth`](multi-auth.md) | Chain auth plugins — accept the first that succeeds |
| [`ldap-auth`](ldap-auth.md) | Authenticate HTTP Basic credentials against an LDAP server |
| [`consumer-restriction`](consumer-restriction.md) | Allow/deny by consumer name or group |
| [`acl`](acl.md) | Allow/deny by consumer group |
| [`attach-consumer-label`](attach-consumer-label.md) | Copy consumer labels into upstream request headers |

## External auth & authorization

These plugins delegate the auth or authorization decision to an external service over HTTP (via the shared outbound client). The SSO plugins support **interactive browser login** as well as stateless token validation: featherbit has no server-side session store, but the interactive flows keep all state in an **encrypted client-side cookie** (see the [cookie-session codec](../../concepts/architecture.md)), so they work across a horizontally-scaled deployment as long as instances share the session secret. Each page's Deviations section states the exact behavior and the one remaining limitation (no server-side revocation before cookie expiry).

| Type | Description |
|---|---|
| [`forward-auth`](forward-auth.md) | Delegate the decision to an external HTTP auth service |
| [`opa`](opa.md) | Delegate authorization to an Open Policy Agent instance |
| [`authz-casbin`](authz-casbin.md) | Embedded Casbin RBAC/ABAC enforcement (no network) |
| [`authz-keycloak`](authz-keycloak.md) | Keycloak UMA permission check |
| [`authz-casdoor`](authz-casdoor.md) | Casdoor: bearer-token introspection or interactive OAuth login |
| [`openid-connect`](openid-connect.md) | OIDC: bearer-token validation or interactive Authorization Code login |
| [`cas-auth`](cas-auth.md) | CAS: ticket validation or interactive SSO login |
| [`wolf-rbac`](wolf-rbac.md) | Wolf RBAC token check |
| [`dingtalk-auth`](dingtalk-auth.md) | DingTalk code/token validation |
| [`feishu-auth`](feishu-auth.md) | Feishu/Lark code/token validation |

## Scripting

| Type | Description |
|---|---|
| [`script`](script.md) | Custom plugin logic written in Lua (Luau), from a file or inline |

## Reading the reference pages

Each plugin page documents:

- **Configuration** — the keys the plugin's `from_config` accepts, with types, defaults, and which malformed shapes are rejected at config load.
- **Behavior** — what the plugin reads and writes on the Context (`request`, `response`, `message`, `errors`), when it takes the `success` versus `error` port, and the error codes it can emit.

Unknown `type` strings fail policy compilation with `Unknown plugin type: <name>`.
