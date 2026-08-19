---
title: condition
description: Branch the policy graph on a boolean condition expression — the request exits on the true or false port, and conditions that cannot be checked exit on error.
---

<span className="plugin-chip" style={{'--chip-color': '#f59e0b'}}>condition</span>

A pure branching waypoint: evaluates a [condition expression](../conditions.md) against the context and routes the request through the `true` or `false` port. It never mutates the request or response — use it to split a policy into two distinct paths (premium vs. standard upstreams, header-gated feature rollouts, body-shape-dependent pipelines).

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `conditions` | array (required, non-empty) | — | A [condition expression](../conditions.md): rules ANDed at the top level, nested `AND`/`OR`/`NOT` groups, variable and JSONPath body subjects. |

```yaml
type: condition
config:
  conditions:
    - ["$.user.tier", "==", "premium"]
```

`conditions` is parsed at config load — a malformed expression fails policy compilation, never a live request. An empty rule list is rejected too, since it would branch unconditionally.

## Behavior

The expression grammar and operators are shared with [`request-validation`](request-validation.md), but evaluation is **strict** where `request-validation` is lenient: a condition that cannot actually be checked exits through the `error` port instead of silently counting as false. A rule is uncheckable when:

- its variable subject is absent (e.g. `http_x_tier` with no `x-tier` header) under a comparison operator — the existence tests `present` / `absent` legitimately ask about absence and still branch normally;
- its JSONPath subject targets a body that is empty or not valid JSON (existence tests included — there is no document to ask about).

A JSONPath rule over a **valid** JSON body whose path matches nothing is a checked `false` (ANY-match over zero nodes), same as everywhere else conditions are used.

Evaluation is left-to-right with short-circuiting, so an uncheckable rule only errors when it is reached before the group's outcome is decided. On error the plugin fails with code `CONDITION_UNCHECKABLE` (appended to `context.errors`) and the graph engine routes the node's `error` edge, the policy catch-all, or the default 500.

The plugin does not write to `context.message`.

## Ports

`condition` declares three output ports and no `success` port — the request always leaves on `true` or `false`, both mandatory: the policy compiler rejects any policy that leaves either unwired. `error` is optional, with the usual fallback chain.

```yaml
nodes:
  - id: tier-check
    type: condition
    config:
      conditions:
        - ["$.user.tier", "==", "premium"]

edges:
  - from: listener.out
    to: tier-check.in
  - from: tier-check.true
    to: premium-upstream.in
  - from: tier-check.false
    to: standard-upstream.in
  - from: tier-check.error
    to: error-handler.in
```
