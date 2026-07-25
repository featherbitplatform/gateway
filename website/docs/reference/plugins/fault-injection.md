---
title: fault-injection
description: Inject artificial delays and abort responses into matching requests for chaos and resilience testing.
---

<span className="plugin-chip" style={{'--chip-color': '#ef4444'}}>fault-injection</span>

Injects an artificial `delay` and/or an `abort` response into matching requests. Both faults are gated independently by a `percentage` sample and a `vars` condition, so you can slow down or fail a controlled slice of traffic. Place it early in the pipeline, before `upstream`.

## Configuration

At least one of `abort` / `delay` is required. Condition expressions and shapes are validated at config load.

| Key | Type | Default | Description |
|---|---|---|---|
| `abort` | object | — | Injected response (see below). |
| `abort.http_status` | integer >= 200 | **required** (within `abort`) | Status code of the injected response. |
| `abort.body` | string | empty body | Response body; supports `$var` interpolation (e.g. `$uri`, `$arg_name`). |
| `abort.headers` | map `{name: value}` or array `[{name, value}]` | — | Response headers; string values support `$var` interpolation. Names are lowercased. |
| `abort.percentage` | integer 0–100 | always | Chance the abort triggers. `0` never triggers, `100` always does. |
| `abort.vars` | array | always | Condition expressions in triple-array form, **OR-ed across items, AND-ed within each** (e.g. `[[["arg_debug", "==", "1"]]]`). A flat list of rules (`[["arg_debug", "==", "1"]]`) is accepted as one AND-ed expression. |
| `delay` | object | — | Injected latency (see below). |
| `delay.duration` | number (seconds, may be fractional) | **required** (within `delay`) | Sleep applied before the request continues. |
| `delay.percentage` / `delay.vars` | — | always | Same gating as for `abort`, evaluated independently. |

```yaml
type: fault-injection
config:
  delay:
    duration: 0.5
    percentage: 30
  abort:
    http_status: 503
    body: '{"error": "injected for $uri"}'
    headers:
      x-injected: "true"
    percentage: 10
    vars:
      - [["arg_debug", "==", "1"]]
```

## Behavior

The delay (if it triggers) is applied first via a non-blocking async sleep, then the abort check runs.

- **Delay triggers** — when `delay` is configured, its `percentage` sample hits, and its `vars` match: the request is held for `duration` seconds, then continues.
- **Abort triggers** — when `abort` is configured, its sample hits, and its `vars` match: the plugin writes the configured status, interpolated body, and headers onto `context.response` and fails with error code `FAULT_INJECTED`, routing the Context through the **`error` port**.
- **Nothing triggers** — the request passes through the **`success` port** untouched.

### Wiring the abort early-exit

In a featherbit graph the two outcomes need distinct ports, so an abort does not end the request directly — it exits via `error` with the response **already prepared**. Wire the `error` port on a pass-through path so the injected response reaches the client:

```yaml
edges:
  - { from: fault.success, to: upstream.in }   # normal traffic continues
  - { from: fault.error,   to: client.in }     # aborted: prepared response goes out as-is
```

Routing `error` through an `error-handler` instead will *replace* the injected body with the handler's template; wire directly to `client.in` to preserve the configured abort response.

### Sampling

`percentage` uses a cheap hash-based pseudo-random draw (no rand crate, freshly seeded per call). It is statistically casual — fine for chaos testing, but not a deterministic sequence and not cryptographic.

## Behavior notes

- Abort exits through the `error` port with code `FAULT_INJECTED` rather than ending the request directly (graph-wiring mechanics; the injected response reaches the client when wired as above).
- `vars` additionally accepts the flat single-expression shape as a convenience.
