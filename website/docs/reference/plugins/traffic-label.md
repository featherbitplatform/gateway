---
title: traffic-label
description: Match requests with condition expressions and tag them — set request headers and context labels, with weighted action selection.
---

<span className="plugin-chip" style={{'--chip-color': '#14b8a6'}}>traffic-label</span>

Matches requests with condition expressions and tags them. The first matching rule picks one of its weighted actions and applies it: `set_headers` writes request headers (visible upstream), and `set_labels` writes `label.<key>` entries into `context.message` (visible to downstream nodes like `logging` and `script` without touching the wire request). Useful for A/B cohorts, canary tagging, and traffic annotation.

## Configuration

All expressions are validated at config load; unknown action keys are rejected.

| Key | Type | Default | Description |
|---|---|---|---|
| `rules` | array | **required**, non-empty | Evaluated in order; the first rule whose `match` passes applies one action. |
| `rules[].match` | array | match all | Triple-array condition, rules AND-ed (e.g. `[["arg_channel", "==", "beta"]]`). Omit to match every request. |
| `rules[].actions` | array of objects | **required**, non-empty | One entry is chosen per request by weighted round-robin. |
| `actions[].set_headers` | object `{name: value}` | — | Request headers to set (replacing existing values); values support `$var` interpolation. Names are lowercased. |
| `actions[].set_labels` | object `{key: value}` | — | Written to `context.message` as `label.<key>`; values support `$var` interpolation. |
| `actions[].weight` | integer >= 1 | `1` | Selection weight among the rule's actions. |

```yaml
type: traffic-label
config:
  rules:
    - match:
        - ["arg_channel", "==", "beta"]
      actions:
        - set_headers:
            x-server-id: beta
          set_labels:
            tier: beta
          weight: 3
        - set_headers:
            x-server-id: stable
          weight: 1
```

## Behavior

Rules are checked top to bottom; a rule without `match` always matches. On the first match, one action is picked by **weighted round-robin** (a shared cursor modulo the total weight — with weights 3:1, four consecutive requests split 3/1 deterministically, not randomly). The chosen action's headers are set on `context.request.headers` (replace semantics) and its labels on `context.message` under `label.<key>`; then evaluation stops.

Two behavior details:

- An action with **only a `weight`** (no `set_headers`/`set_labels`) applies nothing and evaluation **falls through to the next rule** — this lets you label only a fraction of matching traffic.
- Values are interpolated per request (`$remote_addr`, `$uri`, `$arg_<name>`, ...); absent variables resolve to the empty string.

This node is always pass-through: it returns `Ok` in every case, the Context flows out the **`success` port**, and the `error` port is never taken. Wire it anywhere before the nodes that should see the tags (e.g. before `upstream` so headers are forwarded, or before `logging` so `msg_label.<key>` is available).

## Behavior notes

- `set_labels` has no wire effect — use it when you want tags visible to the pipeline but not on the wire.
- Weighted selection uses a deterministic round-robin cursor; the steady-state distribution matches the configured weights.
