---
title: Templates
description: The universal {{namespace.path}} template engine — syntax, namespaces, load- vs request-time resolution, pass-through and warning semantics, exclusions, and the legacy $var mapping.
---

featherbit renders `{{namespace.path}}` references in **every traffic-bound plugin config
field** (request headers, response bodies, rejection messages, FaaS endpoints, and so on) —
not just the ~15 fields that historically interpolated `$var`. This page is the canonical
reference for the syntax, when each kind of reference resolves, what happens when a
reference doesn't match anything, which fields are excluded and why, and how the legacy
`$var`/`${var}` syntax (still documented in full on [Context vars](./context-vars.md))
relates to it.

If you're looking for the exhaustive list of legacy `$name` var names, their descriptions,
and the web UI's autocomplete/live-preview behavior, see [Context vars](./context-vars.md) —
that page now defers to this one for anything about `{{...}}` and keeps the `$var` material
that's unique to the legacy syntax.

## Syntax

```
{{ <namespace path> }}
```

`{{`, optional whitespace, a namespace path, optional whitespace, `}}`. No nesting, no
escape syntax needed (see [Pass-through](#pass-through--warnings) below for why).

| Namespace path | Resolves to |
|---|---|
| `request.method` | HTTP method |
| `request.path` | Request path (no query string) |
| `request.host` | Request `Host` |
| `request.scheme` | `http` or `https` |
| `request.body` | Request body (lossy UTF-8) |
| `request.headers.<name>` | First value of a request header (case-insensitive; write the name with dashes, e.g. `request.headers.x-user-id`) |
| `request.query.<name>` | First value of a query parameter |
| `request.cookies.<name>` | Value from the `Cookie` request header |
| `response.status` | Response status code |
| `response.body` | Response body (lossy UTF-8) |
| `response.headers.<name>` | First value of a response header (case-insensitive) |
| `message.<key>` | Any `context.message` key, stringified; `<key>` may itself contain dots (matched greedily to the closing `}}`, so no `${...}`-style bracing is ever needed for dotted keys) |
| `client.ip` | Client IP without port |
| `client.port` | Client port |
| `env.<NAME>` | Process environment variable — resolved at **parse time**, not render time; see [Load-time vs request-time](#load-time-vs-request-time-resolution) |

Value semantics (first-header/query value, lossy-UTF-8 bodies, stringified message values)
are identical to the legacy resolver's — the two systems share internal implementation so
they can't drift.

## Load-time vs request-time resolution

- **`env.<NAME>`** is substituted once, when the template is parsed — the same moment
  `gateway.yaml`'s own `${ENV_VAR:-default}` interpolation runs, i.e. at config load or
  hot-reload. A rendered template never sees the literal text `env.NAME` — by the time
  requests are served, it has already become the environment variable's value (or, if
  unset, a passed-through literal — see below). **Reload the gateway to pick up an
  environment variable change**; a running process does not re-read `env.*` references.
  Unlike `${VAR:-default}` at the config-file level, `{{env.NAME}}` has **no `:-default`
  fallback syntax** — an unset name always falls through to the pass-through/warning path,
  never a default value.
- Every other namespace (`request.*`, `response.*`, `message.*`, `client.*`) resolves at
  **request time**, once per request, against that request's [`Context`](../concepts/context-object.md).

## Pass-through & warnings

`{{...}}` is unambiguous by construction: anything that isn't a recognized reference is
left as literal text, verbatim — never silently emptied, and with no escape syntax needed
to write a literal `{{` in a config value that isn't meant as a template. This is also a
security property: a stored secret, a regex `$`/`$1`, or JSON-Schema's `$ref` can never be
accidentally corrupted by a syntax that only activates when both braces are well-formed
*and* the inner path matches a known namespace.

Three cases, each verified by a unit test in `src/vars/template.rs`:

1. **Unknown first segment — silent pass-through, no warning.** `{{body.x}}`, `{{mustache}}`,
   `{{ $1 }}` are not templates at all as far as the engine is concerned; they render as
   the exact text you wrote, and nothing is logged.
2. **Known namespace, malformed/unknown leaf — pass-through *plus* a load-time warning.**
   `{{request.headres.x}}` (typo) or `{{client.mac}}` (not a recognized `client.*` leaf)
   render as literal text too, but `Template::parse` also returns a warning string, which
   the gateway logs via `tracing::warn!` when the config compiles (or the debug sandbox
   builds a policy) — coordinates included, e.g. `policy 'p' node 'n' key 'body': unknown
   template reference '{{request.headres.x}}' (passed through literally)`. Nothing fails to
   compile; this is advisory only.
3. **Unset `env.<NAME>`** behaves like case 2 — literal pass-through plus a warning
   (`unset environment variable 'NAME' referenced in template '{{env.NAME}}' (passed through
   literally)`) — rather than case 1's silence, since the namespace and name *are*
   well-formed, just unresolvable right now.

An unclosed `{{` (no matching `}}` anywhere after it) is treated as literal text to the end
of the string, silently — there's no hang and no warning.

**Absent subject at render time renders empty, not a warning.** A well-formed, known
reference whose subject doesn't exist on *this* request — `{{request.headers.missing}}`
when that header wasn't sent, `{{message.foo}}` when `foo` isn't in `context.message` —
renders as an empty string. This is a per-request, runtime condition (the reference itself
is valid), so it's different from the load-time warning cases above and produces no log
line. **`{{response.status}}` is a special case of this**: templated in a request-phase
field (before any response exists), it renders `"0"` — `Context.response.status_code`
starts at `0` and only becomes meaningful after the upstream call (or an early
error/mocked response) sets it.

The compile-time walk that surfaces warnings inspects every string leaf generically (no
per-plugin field semantics), so it's a coordinate-carrying advisory pass, not a source of
truth for which fields actually render templates at request time — for that, see the
[exclusions table](#exclusions) and the per-plugin reference pages.

## Legacy `$var` interop

Fields that historically interpolated `$var`/`${var}` (the 15 listed below) keep doing so
via `Template::render_with_legacy`, which composes the two systems in one specific,
deliberately safe order: **the legacy `$`-pass applies only to the template's literal
segments — text outside any `{{...}}` — never to the rendered output of a `{{...}}`
reference.** A `{{request.headers.x-password}}` reference whose value happens to be
`pa$sword4`, or a client-supplied header crafted to look like `$http_authorization`, passes
through byte-for-byte; it is never re-interpreted as a `$var` read primitive. This is a
security property, not an optimization — treating the fully-rendered string as one more
pass of `$`-interpolation would let request-controlled data read arbitrary context fields
back out through a legacy-enabled field, and would corrupt runtime values that legitimately
contain a `$`. New adopters of the template engine never get this legacy pass at all — they
call plain `render`, which never touches `$`.

### The 15 legacy fields

| Plugin | Field(s) |
|---|---|
| [`redirect`](./plugins/redirect.md) | `uri` |
| [`exit-transformer`](./plugins/exit-transformer.md) | `body` |
| [`mocking`](./plugins/mocking.md) | `response_example`, each `response_headers` value |
| [`limit-count`](./plugins/limit-count.md) | `key` |
| [`limit-conn`](./plugins/limit-conn.md) | `key` (non-constant `key_type` only) |
| [`lago`](./plugins/lago.md) | `event_transaction_id`, `subscription_id` |
| [`workflow`](./plugins/workflow.md) | nested `limit-count` action's `key` |
| [`proxy-cache`](./plugins/proxy-cache.md) | each `cache_key` component |
| [`fault-injection`](./plugins/fault-injection.md) | `abort.body`, each `abort.headers` value |
| [`traffic-label`](./plugins/traffic-label.md) | each `set_headers`/`set_labels` value |
| [`forward-auth`](./plugins/forward-auth.md) | each `extra_headers` value |
| [`response-rewrite`](./plugins/response-rewrite.md) | each `add_headers`/`set_headers` value |
| All 17 loggers (shared `log_format`) | each string `log_format` value |

Every other field that adopted `Template` as part of this feature (the ~30-plugin sweep —
`proxy-rewrite` header values, `cors` allowed methods/headers, every `rejected_msg`, FaaS
endpoint URIs, and so on) renders with plain `render` and **never** processes `$`, even if
the string happens to contain a `$`.

## COMPAT: literal `{{known.namespace...}}` text now substitutes

If any of the 15 legacy fields above, or any of the newly-swept fields, previously
contained literal text that happens to look like `{{request.method}}` or
`{{env.SOME_NAME}}` — for instance because you wrote a documentation example inline, or a
JSON body template with a `{{...}}`-shaped placeholder of your own — **that text now
resolves as a template reference instead of passing through unchanged**. Before this
feature, `{{...}}` had no special meaning anywhere in the gateway; now it does, universally.
This is the one behavior change existing configs can hit on upgrade. Text that merely
*looks* like a mustache placeholder but doesn't match a known namespace (`{{my_var}}`,
`{{body.x}}`) is unaffected — it still passes through literally, per the pass-through rules
above.

## Exclusions

Structural fields — values compiled into something other than a string at load time, or
config with its own established templating dialect — don't route through the universal
`Template` engine. `{{env.NAME}}` still applies to these at config-load time (it's a text
substitution before the value is ever parsed into its structural form), but `{{request.*}}`/
`{{response.*}}`/`{{message.*}}`/`{{client.*}}` do not apply to them.

| Class | Examples | Reason |
|---|---|---|
| Regex/pattern fields | `ip-restriction` CIDRs, `ua-restriction`/`uri-blocker` regex lists, `data-mask` `regex` | The string is compiled into a `Regex`/CIDR matcher once at load; rendering it per request would mean recompiling a pattern from live data on every request, and a request-controlled regex is its own hazard class |
| JSON-Schema / OpenAPI documents | `request-validation` `header_schema`/`body_schema`, `oas-validator` `spec` | Compiled into a schema validator at load; not a plain string at request time |
| Lua sources | `script` `source`/`inline`, `serverless-*-function` `functions` | Compiled/loaded as Lua code, not interpolated text |
| Casbin models/policy | `authz-casbin` `model`/`model_path`/`policy`/`policy_path` | Parsed into a Casbin enforcer at load |
| Balancer targets/ports, numeric fields | `upstream` `targets[].port`, every `type: number` field | Not strings; `Template` only ever operates on `String` config values |
| IP lists | `ip-restriction` allow/deny, `real-ip` `trusted_addresses` | Parsed into CIDR matchers at load |
| TLS material | cert/key file paths across the config | Filesystem paths read once at load, not per-request data |
| Logger endpoints/file paths | every logger's `endpoint_addr`/`uri`/`path`/host fields | Connection targets resolved once; the UI still offers `{{env.NAME}}` suggestions on these (`template: 'env-only'`) since parameterizing a collector endpoint by environment is a real use case, but not by live request data |
| `cors.allowed_origins` | the list itself | Each origin is compared verbatim against the request's `Origin` header at the CORS decision point — value equality is the whole point, and no plugin here has ever suggested templating it (unlike `allowed_methods`/`allowed_headers`, which *are* templated, since those are inserted into response headers, not compared against request data) |
| `body-transformer`'s own `{{...}}` | `body-transformer` `request.template`/`response.template` | Predates this feature and keeps its **own**, different `{{...}}` dialect (e.g. `{{body.user.name}}`, not `{{request.body}}`) — deliberately not migrated, to avoid breaking existing body-transformer configs whose `{{...}}` already means something else. Documented follow-up (out of scope here): its dialect resolves `{{request.method}}`-shaped input to empty rather than either rendering it or passing it through, which is a minor inconsistency with every other field on this page |
| `error-handler`'s own body engine | `error-handler` `body_template` | Also predates this feature and keeps its own light templating (`{{error.code}}`) rather than adopting the universal namespace grammar |

## `openwhisk`: percent-encoding and an SSRF note

`openwhisk`'s `namespace`, `package`, and `action` fields are templated (`{{...}}` renders,
then the rendered value is **percent-encoded** before being spliced into the invocation URL
path — byte-wise, RFC 3986 `pct-encode`) because these segments become part of a URL path;
an unencoded rendered value (e.g. from a request header) could otherwise break out of its
path segment or inject a query string. `api_host` is also templated for FaaS-family
endpoint-routing consistency, but it is **not** percent-encoded (encoding a scheme+host
value the same way would corrupt it) and templating it from request-controlled data is
**discouraged** — a request-driven `api_host` is a server-side request forgery (SSRF)
vector, letting a client redirect the outbound call to an arbitrary host. Prefer a static
value or an `{{env.NAME}}` reference for `api_host`.

## Admin API

- **`GET /api/env-vars`** (authed, same Basic Auth as every other admin route) — sorted
  names of every environment variable visible to the gateway process, **values never
  included**:
  ```json
  { "names": ["HOME", "PATH", "REGION", "..."] }
  ```
  This is what powers the web UI's `{{env.NAME}}` suggestions in every text-like field.
- **`GET /api/vars`** — the same catalog documented on [Context vars](./context-vars.md),
  now with a `path` field on each entry giving its `{{...}}` equivalent (empty string for
  the handful of legacy names with no direct template mapping — see the table below).

## Legacy `$var` → `{{path}}` mapping

Every name `$var`/`${var}` interpolation understands, and the template path it corresponds
to. Three families are **legacy-only** — there is no `{{...}}` equivalent, because nothing
in the `Context` cleanly represents them as a single addressable path (`protocol` is
metadata about the connection rather than the request/response payload; `query_string` is
a derived, re-sorted reconstruction rather than one field; `post_arg_*` reads a parsed
form body the template engine's namespaces don't expose).

| Legacy name | Template path |
|---|---|
| `$uri` | `request.path` |
| `$request_uri` | `request.path` (drops the query string — `request.path` never included it) |
| `$method` / `$request_method` | `request.method` |
| `$host` | `request.host` |
| `$scheme` | `request.scheme` |
| `$protocol` | *(legacy only)* |
| `$remote_addr` | `client.ip` |
| `$remote_port` | `client.port` |
| `$query_string` | *(legacy only)* |
| `$status` | `response.status` |
| `$resp_body` | `response.body` |
| `$request_body` | `request.body` |
| `$consumer_name` | `message.consumer.name` |
| `$consumer_group_id` | `message.consumer.group` |
| `$arg_<name>` | `request.query.<name>` |
| `$http_<name>` | `request.headers.<name>` |
| `$cookie_<name>` | `request.cookies.<name>` |
| `$post_arg_<name>` | *(legacy only)* |
| `$msg_<key>` | `message.<key>` |
| `$sent_http_<name>` | `response.headers.<name>` |

The web UI's context-vars legend renders this same table (plus live values, where
available) as its bottom "Legacy `$var` mapping" section.

## Migration notes

**No action needed for existing configs**, with one exception: see the
[COMPAT note](#compat-literal-knownnamespace-text-now-substitutes) above about literal
`{{known.namespace...}}` text in the 15 legacy fields and any of the newly-swept fields.
Every other existing `$var` reference in a legacy field keeps working exactly as before —
`render_with_legacy` is a strict superset of the old whole-string `$`-interpolation pass,
byte-identical for any template with no `{{...}}` references. Fields that never templated
anything before this feature are unaffected either way; adding a `{{...}}` reference to one
of them is new capability, not a behavior change to opt out of.

One env-var quirk worth calling out explicitly: `{{env.NAME}}` has no `${NAME:-default}`-style
default fallback the way `gateway.yaml`'s own env interpolation does — an unset name always
takes the pass-through-plus-warning path (see [Pass-through](#pass-through--warnings)), never
a substituted default value. If you need a default, set the environment variable itself, or
fall back to a literal in the config.

## Where the web UI offers suggestions

The node inspector's `{{`-triggered popover (and `{{env.NAME}}` suggestions) don't appear
uniformly on every text field — only on the ones the Rust plugin genuinely renders through
`Template` at request time do you get full `request`/`response`/`message`/`client` context
suggestions with live preview; every other text-like field still offers `{{env.NAME}}`
suggestions (config-load-time substitution still applies there), and a few structural
fields (dropdowns, switches, numbers, and `real-ip`'s `source`) offer no template
suggestions at all, since they're never interpolated as strings in the first place. See
[Web UI](../guides/web-ui.md#editor-workflow) for the popover mechanics and
[Context vars](./context-vars.md#autocomplete-and-live-value-preview) for the live-preview
requirements (debug mode, an incoming edge, an existing trace) — both apply the same way to
`{{...}}` suggestions as they always did to `$var` ones.
