---
title: proxy-mirror
description: Fire-and-forget mirroring of requests to a shadow upstream for traffic shadowing.
---

<span className="plugin-chip" style={{'--chip-color': '#8b5cf6'}}>proxy-mirror</span>

Sends a fire-and-forget clone of the incoming request to a shadow upstream. Useful for comparing a new backend against production, capturing traffic, or pre-warming a cache. The mirror is dispatched on a detached background task; its response and any error are ignored, and the request path is **never** blocked or affected by it. Place it before `upstream` so it observes the request as it will be proxied. It always continues through the `success` port and never errors.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | — (**required**) | Shadow base URL — scheme + host + optional port, e.g. `http://shadow:8080`. Must start with `http://` or `https://`; no trailing path. |
| `path` | string | — | Overrides the mirrored request path; when unset the original request path is mirrored. The original query string is always appended. |
| `sample_ratio` | number | `1.0` | Fraction of requests to mirror, in `0.0..=1.0`. `1.0` mirrors every request; `0.0` mirrors none. |

```yaml
type: proxy-mirror
config:
  host: "http://shadow:8080"
  path: "/mirror"
  sample_ratio: 0.5
```

## Behavior

On each request the plugin decides whether to mirror per `sample_ratio`: `>= 1.0` always mirrors and `0.0` never mirrors; otherwise a **pseudo-random** draw in `[0.0, 1.0)` is compared against the ratio. The draw uses a freshly seeded hasher (the same casual, non-cryptographic source the [`fault-injection`](./fault-injection.md) plugin uses) — it is not a deterministic sequence and not suitable for security decisions.

When a request is sampled, the plugin builds a mirror request (method, headers, and body copied from the incoming request; URL is `host` + the `path` override or the original path + the original query string) and spawns a detached background task that sends it via the shared outbound HTTP client. Everything the task needs is cloned before it is spawned, so:

- The mirror **never** blocks the request path — `execute` returns immediately with the Context unchanged and continues through the `success` port.
- The mirror's response and any transport/timeout error are dropped (best-effort). A failed or unreachable shadow host has no effect on the client's response.

The mirrored call uses a 60-second whole-call deadline; since it runs on a detached task it does not hold up request handling.
