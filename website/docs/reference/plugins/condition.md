---
title: condition
description: Branch the policy graph on a boolean condition expression — the request exits on the true or false port.
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

The expression grammar, operators, and evaluation semantics are shared with [`request-validation`](request-validation.md) and every other node that takes a condition expression — evaluation is **lenient**:

- an absent variable subject (e.g. `arg_interactive` with no `?interactive=` query param) evaluates as the empty string, so a positive comparison over it is simply `false` (and `!=` is `true`); use the existence tests `present` / `absent` to branch on absence explicitly;
- a JSONPath subject over a body that is empty, not valid JSON, or whose path matches nothing, matches zero nodes — comparison rules are `false`, `absent` is `true`.

Evaluation is left-to-right with short-circuiting. The node itself never fails: the request always leaves on `true` or `false`.

The plugin does not write to `context.message`.

## Ports

`condition` declares three output ports and no `success` port — the request always leaves on `true` or `false`, both mandatory: the policy compiler rejects any policy that leaves either unwired. `error` remains declared (so existing policies that wired it still compile) but the node never emits on it.

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
```
