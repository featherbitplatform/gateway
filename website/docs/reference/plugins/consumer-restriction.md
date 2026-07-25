---
title: consumer-restriction
description: Allow or deny already-authenticated consumers by name or group, with optional per-consumer HTTP method restrictions.
---

<span className="plugin-chip" style={{'--chip-color': '#f97316'}}>consumer-restriction</span>

Restricts which already-authenticated consumers may reach a route. It does **not** authenticate — an upstream auth node (e.g. `key-auth`) must have attached the consumer identity first (the `consumer.*` keys in `context.message`). This node then matches the consumer's name or group against a whitelist/blacklist and, optionally, confines a named consumer to specific HTTP methods. Place it after auth and before the upstream node.

## Configuration

At least one of `whitelist`, `blacklist`, or `allowed_by_methods` is required; `whitelist` and `blacklist` are mutually exclusive. Invalid combinations fail at config load.

| Key | Type | Default | Description |
|---|---|---|---|
| `type` | string | `consumer_name` | Consumer attribute the lists match. `consumer_name` matches `consumer.name`; `consumer_group_id` matches `consumer.group`. `service_id` / `route_id` have no featherbit analogue and are rejected at config load. |
| `whitelist` | array of strings | — | Values allowed through. A consumer whose value is not listed is rejected. Mutually exclusive with `blacklist`. |
| `blacklist` | array of strings | — | Values rejected on match. |
| `allowed_by_methods` | array of `{ user, methods }` | — | Confines the named consumer (`user`) to the listed HTTP methods. Consumers with no entry are unrestricted. |
| `rejected_code` | integer | `403` | HTTP status for list rejections. |
| `rejected_msg` | string | — | Custom rejection message; overrides the default. |

```yaml
type: consumer-restriction
config:
  type: consumer_name
  whitelist: ["alice", "bob"]
  allowed_by_methods:
    - user: alice
      methods: ["GET", "POST"]
  rejected_code: 403
  rejected_msg: "You shall not pass"
```

## Behavior

Checks run in order:

1. **No consumer attached** — if the configured value (`consumer.name` or `consumer.group`) is absent from `context.message`, the request is rejected with status `401` (regardless of `rejected_code`).
2. **Blacklist** — if `blacklist` is non-empty and the value matches, the request is rejected.
3. **Whitelist** — if `whitelist` is non-empty and the value is not listed, the request is rejected.
4. **Method restriction** — if `allowed_by_methods` is set and the consumer was *not* already cleared by the whitelist, and an entry exists for this consumer, the request method must be one of that entry's methods or the request is rejected.

Every rejection writes a JSON body (`{"message": ...}` with `content-type: application/json`) onto `context.response`, sets the status, and routes the Context through the `error` port with error code `CONSUMER_RESTRICTED`. Permitted requests pass through the `success` port unchanged.

:::note Limitations
`type: service_id` and `route_id` are not supported — featherbit has no service/route object on the context, so those values are rejected at config load; use `consumer_name` or `consumer_group_id`.
:::
