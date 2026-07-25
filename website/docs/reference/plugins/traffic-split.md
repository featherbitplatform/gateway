---
title: traffic-split
description: Weighted / conditional traffic steering for canary, blue-green, and A/B deployments.
---

<span className="plugin-chip" style={{'--chip-color': '#14b8a6'}}>traffic-split</span>

Steers matching requests to a weighted set of upstream targets, or lets them fall through to the route's normal upstream. A request is matched against an ordered list of `rules`; the **first** rule whose `match` passes selects a set of `weighted_upstreams`, and one weighted slot is chosen by weighted round-robin. This is the building block for canary, blue-green, and A/B rollouts. Place it **before** the route's normal `upstream` node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `rules` | array | — (**required**, non-empty) | Evaluated in order; the first rule whose `match` passes is used. |
| `rules[].match` | array | absent = match all | A triple-array condition (rules AND-ed). Omit to match every request. |
| `rules[].weighted_upstreams` | array | — (**required**, non-empty) | The weighted slots; one is chosen by weighted round-robin. |
| `rules[].weighted_upstreams[].upstream` | object | absent = default slot | `{targets: [{host, port}], ...}`. When present, the slot proxies to one of the targets (round-robin within the set). When absent, the slot is the "default" — requests fall through to the route's normal upstream. |
| `rules[].weighted_upstreams[].weight` | integer >= 0 | `1` | Selection weight. Every rule needs at least one slot with weight > 0. |
| `timeout_ms` | integer | `60000` | Whole-call deadline for requests the plugin proxies itself. |

Config is validated at load: `match` expressions are compiled, weights must be non-negative integers, there must be at least one rule, and each rule must have at least one positively-weighted slot.

```yaml
type: traffic-split
config:
  timeout_ms: 60000
  rules:
    - match:
        - ["arg_canary", "==", "1"]
      weighted_upstreams:
        # 90% fall through to the route's normal upstream
        - weight: 90
        # 10% proxied to the canary target set
        - upstream:
            targets:
              - host: canary-backend
                port: 8080
          weight: 10
```

## Behavior

For each request the plugin finds the **first** rule whose `match` condition passes (a rule with no `match` matches every request). Within that rule it picks one `weighted_upstreams` slot by weighted round-robin over the slot weights — a slot with weight 3 is chosen 3 out of every `total_weight` calls. Zero-weight slots are never selected.

The picked slot has one of two shapes:

- **Default slot** (no `upstream`) — the plugin returns the Context unchanged through the `success` port, and the request continues to the route's normal `upstream` node.
- **Target slot** (has `upstream.targets`) — the plugin proxies the request itself to one of the targets (round-robin within the set), reusing the shared outbound HTTP client. It forwards the method, headers (overriding `Host` with the target), and body, writes the backend's status/headers/body onto `context.response`, and **short-circuits** by failing with error code `TRAFFIC_SPLIT_ROUTED` through the `error` port.

If no rule matches at all, the request passes through the `success` port unchanged.

## Split-node wiring

Because featherbit pipelines need a distinct port for "stop here, send this response" versus "keep going", this node uses its two ports as follows (the same convention `fault-injection` and `mocking` use):

- Wire **`success` → the route's normal `upstream` node**. This is the path for default slots and non-matching requests.
- Wire **`error` → `client.in`**. This is the path for traffic the plugin proxied itself: the response is already populated on the Context and reaches the client directly.

If a split target is unreachable, the node fails with code `TRAFFIC_SPLIT_UPSTREAM_ERROR` and a prepared `502` JSON body (`{"error": "bad_gateway", "message": "traffic-split target unreachable"}`, `content-type: application/json`) — also routed through the `error` port to the client.

## Behavior notes

- There is no shared upstream registry at this node: a target slot is **proxied by the plugin itself** and short-circuited through the `error` port; a default slot falls through to the route's `upstream` node.
- Upstream references are inline target lists (`upstream.targets: [{host, port}]`) only — `upstream_id` references to a shared upstream store are not supported.
- Selection is a deterministic weighted round-robin (a per-rule cursor); the long-run distribution matches the configured weights.
