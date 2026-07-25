---
title: redirect
description: Answer the request with an HTTP redirect — a URI template with variables or an automatic HTTP-to-HTTPS upgrade.
---

<span className="plugin-chip" style={{'--chip-color': '#eab308'}}>redirect</span>

Builds a redirect response from either a `uri` template (with `$var` interpolation) or the `http_to_https` shortcut, then stops the request from going upstream. In featherbit terms "stopping" means the plugin fills in `context.response` (status + `location` header) and exits through `success` — so **wire the node's `success` edge straight to `client.in`**, not through an `upstream` node, which would overwrite the response.

## Configuration

Exactly one of `uri` / `http_to_https: true` is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `uri` | string | — | Redirect target template. `$var` and `${var}` references are interpolated against the context (e.g. `$uri`, `$request_uri`, `$host`, `$arg_<name>`); unknown variables resolve to the empty string. |
| `http_to_https` | bool | `false` | Redirect plain-HTTP requests to `https://$host$request_uri` with `301` for GET/HEAD and `308` otherwise (so the method and body survive the redirect). |
| `ret_code` | integer | `302` | Status code for `uri` redirects (minimum `200`). Ignored by `http_to_https`, which picks 301/308 itself. |
| `append_query_string` | bool | `false` | Append the original query string to the target, with `?` or `&` as appropriate. Cannot be combined with `http_to_https` (config error), which already keeps the query string via `$request_uri`. |

```yaml
type: redirect
config:
  uri: https://$host/new-prefix$uri
  ret_code: 301
  append_query_string: true
```

## Behavior

This plugin never fails at execution time — it always exits through the `success` port, and the `error` port is never taken.

In **`uri` mode**, the template is interpolated, the query string is optionally appended, and the response is set: `status_code = ret_code`, `location` header = the new URI, empty body (stale `content-length`/`content-encoding` are dropped per the body-mutation convention).

In **`http_to_https` mode**, the effective scheme is the first `x-forwarded-proto` request header value when present (an outer proxy's word wins), otherwise `context.request.scheme`:

- If the scheme is already `https`, the context **passes through unchanged** — no redirect is set. Since the `success` edge goes to `client.in`, that would produce an empty response, so only attach `http_to_https` to routes served over plain HTTP.
- Otherwise the response redirects to `https://$host$request_uri` (`$host` keeps a port carried by the `Host` header) with `301` for GET/HEAD and `308` for every other method.

**Limitations:** `regex_uri` (regex-substitution targets) and `encode_uri` are not implemented; `http_to_https` always targets the default HTTPS port — there is no `https_port` plugin attribute; the template has no `\$` escape for a literal dollar sign.
