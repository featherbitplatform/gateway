---
title: acl
description: Group-based access control — admit or block already-authenticated consumers by their consumer group.
---

<span className="plugin-chip" style={{'--chip-color': '#f59e0b'}}>acl</span>

Group-based access control. Restricts which already-authenticated consumers may reach a route, keyed on the consumer's *group*. It does **not** authenticate — an upstream auth node (e.g. `key-auth`) must have attached the consumer identity first, writing `consumer.name` and (optionally) `consumer.group` into `context.message`. Place it after auth and before the upstream node.

## Configuration

At least one of `allowed_by` or `denied_by` is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `allowed_by` | array of strings | — | Consumer groups permitted through. When non-empty, a consumer whose group is not listed (including one with no group) is rejected. |
| `denied_by` | array of strings | — | Consumer groups blocked. Checked before `allowed_by` — deny wins. |
| `rejected_code` | integer | `403` | HTTP status for list rejections. |
| `rejected_msg` | string | — | Custom rejection message; overrides the default. |

```yaml
type: acl
config:
  allowed_by: ["partners", "internal"]
  denied_by: ["banned"]
  rejected_code: 403
```

## Behavior

Checks run in order:

1. **No consumer attached** — if `consumer.name` is absent from `context.message`, the request is rejected with status `401` and message `Missing authentication.`.
2. **Deny list first** — if the consumer's group is in `denied_by`, the request is rejected (deny wins even when the same group is also in `allowed_by`).
3. **Allow list** — if `allowed_by` is non-empty and the consumer's group is not listed, the request is rejected. A consumer with no group is rejected whenever `allowed_by` is set.

Every rejection writes a JSON body (`{"message": ...}` with `content-type: application/json`) onto `context.response`, sets the status, and routes the Context through the `error` port with error code `ACL_DENIED`. Admitted requests pass through the `success` port unchanged.

:::note Limitations
Matching is by consumer group only. featherbit models a consumer's membership as a single `consumer.group`, so this plugin implements the classic group-allowlist form with `allowed_by` / `denied_by` lists of group names; arbitrary consumer labels and external-user JWT claims are not matched.
:::
