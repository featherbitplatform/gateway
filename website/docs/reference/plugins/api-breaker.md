---
title: api-breaker
description: Circuit breaker that trips on unhealthy upstream responses, expressed as a check/observe node pair sharing one breaker by id.
---

<span className="plugin-chip" style={{'--chip-color': '#ef4444'}}>api-breaker</span>

A circuit breaker both *decides* whether to admit a request (before the upstream
call) and *observes* the outcome (after it) — two moments a single graph node
cannot span. So `api-breaker` is a **pair of nodes** sharing one breaker, linked
by a required `id`:

- a **check** node placed *before* `upstream`, which trips to a short-circuit
  response while the breaker is open, and
- an **observe** node placed *after* `upstream`, which feeds the response status
  back into the breaker so it opens and closes.

Both nodes carry the same `id`, so they resolve to the same process-wide breaker
state.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `phase` (or `role`) | string | — (**required**) | `check` (before upstream) or `observe` (after upstream). |
| `id` | string | — (**required**) | Shared breaker identity; the check and observe nodes of one pair must match. |
| `unhealthy.http_statuses` | array | `[500]` | Statuses counted as failures. |
| `unhealthy.failures` | integer | `3` | Consecutive failures before the breaker opens. |
| `healthy.http_statuses` | array | `[200]` | Statuses counted as successes. |
| `healthy.successes` | integer | `3` | Consecutive successes before the breaker fully closes. |
| `break_response_code` | integer | `502` | Status returned while the breaker is open. |
| `break_response_body` | string | — | Body returned while the breaker is open. |
| `break_base_sec` | integer | `2` | Base cooldown; the open window grows as `break_base_sec * 2^trip` (exponential backoff). |
| `max_breaker_sec` | integer | `300` (min `3`) | Ceiling on the cooldown window. |

## Wiring

The check node goes **before** `upstream`; its `error` port routes to
`client.in`, so while the breaker is open the request short-circuits to the
client with the break response. The observe node goes **after** `upstream` on the
path(s) carrying the real upstream response; it passes the Context through
untouched and simply records the status.

```yaml
nodes:
  - id: breaker-check
    type: api-breaker
    config: { phase: check, id: orders-api, break_response_code: 502,
              unhealthy: { http_statuses: [500, 503], failures: 3 },
              healthy: { http_statuses: [200], successes: 3 } }
  - id: upstream
    type: upstream
    config: { targets: [{ host: orders, port: 8080 }] }
  - id: breaker-observe
    type: api-breaker
    config: { phase: observe, id: orders-api,
              unhealthy: { http_statuses: [500, 503], failures: 3 },
              healthy: { http_statuses: [200], successes: 3 } }
edges:
  - { from: listener.out,          to: breaker-check.in }
  - { from: breaker-check.success, to: upstream.in }
  - { from: breaker-check.error,   to: client.in }        # open → 502
  - { from: upstream.success,      to: breaker-observe.in }
  - { from: breaker-observe.success, to: client.in }
```

## Behavior

The **check** node calls the breaker: if it is open (and the cooldown has not
elapsed) the node writes `break_response_code` / `break_response_body` onto
`context.response` and fails with error code `API_BREAKER_OPEN`, routing the
Context through the `error` port. Once the cooldown elapses the breaker becomes
half-open and lets one probe through.

The **observe** node reads `context.response.status_code`. A status in
`unhealthy.http_statuses` records a failure; after `unhealthy.failures`
consecutive failures the breaker opens for `min(max_breaker_sec, break_base_sec *
2^trip)`, doubling each successive trip. A status in `healthy.http_statuses`
records a success and resets the failure streak; after `healthy.successes`
consecutive successes the breaker fully closes.

Breaker state lives in process memory and is per gateway instance.
