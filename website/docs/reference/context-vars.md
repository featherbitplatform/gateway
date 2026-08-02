---
title: Context vars
description: Every $var name plugin config templates can interpolate, the ${...} syntax rules, and the web UI's autocomplete + live value preview.
---

Plugin configs reference live request/response data through `$var` interpolation — `$uri`, `$http_user_agent`, `$arg_page`, and friends — resolved by [`src/vars/mod.rs`](../concepts/context-object.md) against the request's [`Context`](../concepts/context-object.md) at execution time. This page is the full catalog: every name the resolver understands, the two ways to write a reference, which plugin config fields actually interpolate, and how the web UI's autocomplete popover and var legend help you avoid typos.

The catalog below is generated from the same source the gateway serves at `GET /api/vars` (Admin API, authed) and a Rust test (`test_catalog_matches_resolver` in `src/vars/catalog.rs`) keeps it from drifting out of sync with the resolver — if you see a var here, it works, and nothing the resolver accepts is missing from this list.

## Syntax

Two forms, both interpolated by the same pass over the template string:

- **`$name`** — bare form. The name is read as a maximal run of `[A-Za-z0-9_]` immediately after the `$`. Use this for every var whose name is only letters, digits, and underscores (which is all of them, `msg_*` included, as long as the `context.message` key itself has no dots).
- **`${name}`** — brace form, **required** when the name contains a character outside `[A-Za-z0-9_]` — in practice, a dotted `context.message` key such as `${msg_consumer.name}`. Without braces, `$msg_consumer.name` would resolve `msg_consumer` (stopping at the dot) and leave the literal text `.name` behind.

Rules that apply to both forms:

- **No escape syntax.** A literal dollar sign followed by something that isn't a valid var start (e.g. `cost: 5$`, `$` at end of string) passes through unchanged — there is no `\$` to force a literal dollar before a name-like token.
- **Unknown vars silently interpolate to empty string.** A typo'd name, or a known family member that is absent on this request (missing header, missing query param, ...), resolves to `""` — no error, no warning in the logs. This is the exact gap the autocomplete and legend exist to close: see them before you rely on a name you haven't verified.

## Statics

Fixed names, each resolving to at most one value.

| Var | Description |
|---|---|
| `$uri` | Request path (no query string) |
| `$request_uri` | Path plus `?query` when query params exist |
| `$method` | HTTP method (alias: `$request_method`) |
| `$request_method` | HTTP method (alias of `$method`) |
| `$host` | Request `Host` |
| `$scheme` | `http` or `https` |
| `$protocol` | HTTP protocol version (`http1`, `http2`, ...) |
| `$remote_addr` | Client IP without port |
| `$remote_port` | Client port |
| `$query_string` | Full query string, rebuilt and sorted |
| `$status` | Response status code |
| `$resp_body` | Response body (lossy UTF-8) |
| `$request_body` | Request body (lossy UTF-8) |
| `$consumer_name` | Authenticated consumer name (set by auth plugins) |
| `$consumer_group_id` | Authenticated consumer group |

## Families

Prefix families — one entry per actual value present on the request/response, not a fixed set. `<name>` is filled in by you (or picked from the autocomplete popover's live suggestions).

| Var | Source | Description |
|---|---|---|
| `$arg_<name>` | query params | First value of a query parameter |
| `$http_<name>` | request headers | First value of a request header; underscores in `<name>` map to dashes, compared case-insensitively (`$http_user_agent` → `User-Agent`) |
| `$cookie_<name>` | cookies | Value from the `Cookie` request header |
| `$post_arg_<name>` | form body | Form field, only for `application/x-www-form-urlencoded` request bodies |
| `$msg_<key>` | `context.message` | Any `context.message` key, stringified; dotted keys need `${msg_key.with.dots}` |
| `$sent_http_<name>` | response headers | First value of a response header; same underscore→dash mapping as `$http_*`, mirrored exactly |

`$sent_http_*` and `$request_body` complete the var surface so every field of the `Context` (request and response alike) is reachable — see the [Context object](../concepts/context-object.md).

## Where `$var` templates work

The web UI's SchemaForm attaches autocomplete to fields the plugin author has explicitly flagged as accepting `$var` templates (`vars: true` in `ui/src/pluginConfig.ts`). As of this writing, that's:

- [`lago`](./plugins/lago.md) — `event_transaction_id`, `subscription_id`
- [`limit-count`](./plugins/limit-count.md) — `key`
- [`limit-conn`](./plugins/limit-conn.md) — `key`
- [`proxy-cache`](./plugins/proxy-cache.md) — `cache_key` (each list item)
- [`redirect`](./plugins/redirect.md) — `uri`
- [`exit-transformer`](./plugins/exit-transformer.md) — `body`
- [`fault-injection`](./plugins/fault-injection.md) — `abort` (JSON body/headers)
- [`traffic-label`](./plugins/traffic-label.md) — `rules` (JSON, `set_headers`/`set_labels` values)
- [`mocking`](./plugins/mocking.md) — `response_example`, each `response_headers` value
- [`body-transformer`](./plugins/body-transformer.md) — each transform's `template`

:::note Some interpolating fields are raw JSON/YAML, not schema-form fields
A handful of config values that genuinely interpolate `$var` templates aren't backed by a schema-form field the popover can attach to, because they're edited as a raw JSON/YAML blob instead — the [`logging`](./plugins/logging.md) family's `log_format`, [`forward-auth`](./plugins/forward-auth.md)'s `extra_headers`, and [`response-rewrite`](./plugins/response-rewrite.md)'s header maps are the current examples. The vars still work there exactly as documented on this page — you just type them by hand, without a dropdown or live preview.
:::

Env-var and secret-looking fields (API keys, tokens, connection strings) are deliberately left unflagged even where the underlying string is technically templated the same way, to avoid nudging you toward putting request data where credentials go.

## Autocomplete and live value preview

Typing `$` or `${` in a flagged field opens a popover: a substring-filtered list of var names (filter = whatever you typed after the `$`), navigable with the arrow keys and `Enter`/`Tab` to insert, `Esc` to dismiss. Each row shows the var name in monospace and, when one is available, a dimmed preview of its current value. A footer row explains why a value isn't shown when it isn't, or links to the full **var legend** (also reachable from a "Context vars" button in the node inspector header) — the same catalog as this page, grouped, with the live values inlined for whichever node is selected.

**Names are always offered**; **live values** require all of the following:

1. Debug mode is enabled (`debug.enabled` in `system.yaml` — see [Debugging & sandbox](../guides/debugging.md)).
2. The node under edit has an incoming edge, so there's a predecessor whose output the values come from (the success-edge predecessor wins on fan-in; a node with no incoming edge, or a node inside a supernode's own definition editor, only ever gets names).
3. A debug trace exists for the policy containing that predecessor — i.e. a request has actually gone through it since the trace buffer was last cleared or the process last started.

When a value is available it comes from the **latest handled request**'s debug trace: family members are discovered from what that request actually carried (`$http_x_request_id` shows up once a request sent that header), not guessed. A few caveats carry over from debug mode itself:

- **Redacted fields show `<redacted>` verbatim** — the popover does not un-redact anything; a header or query param on the built-in or configured redaction lists previews as the literal string `<redacted>`, same as the trace it's drawn from.
- **Cookies are never previewable.** `$cookie_*` family members list by name only, with no value, by design (see [Redaction](../guides/debugging.md#redaction)).
- **`$resp_body` / `$request_body` previews need `capture_bodies: true`.** With bodies off, `body.len` is captured but not the content, so these two previews stay unavailable even with a trace present.
- **Values are single-line and truncated** (~80 characters) for the popover and legend display — a preview, not a full dump; use the [debug trace API](../guides/debugging.md#the-trace-api) to see the real thing in full.

## The var catalog API

`GET /api/vars` (Admin API, same Basic Auth as every other admin route) returns the catalog as JSON:

```bash
curl -u admin:admin http://localhost:9090/api/vars | jq
```

```json
{
  "vars": [
    { "name": "uri", "kind": "static", "description": "Request path (no query string)", "example": "$uri" },
    { "name": "http_*", "kind": "family", "family_source": "request_headers", "description": "First value of a request header (underscores map to dashes)", "example": "$http_user_agent" }
  ]
}
```

This is the same payload the UI fetches once per session to drive the autocomplete popover and the legend; there is no per-request cost to calling it yourself when building tooling against the same var surface.
