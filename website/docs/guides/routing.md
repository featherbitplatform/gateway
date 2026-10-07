---
title: Routing
description: Route match rules, path wildcard semantics, and first-match-wins ordering.
---

Routes decide which policy graph handles an incoming request. Each route in `gateway.yaml` binds a match rule to a named policy:

```yaml
routes:
  - name: echo-api
    match:
      path: /api/*
      methods: [GET, POST, PUT, DELETE]
    policy: echo-policy
```

| Field | Purpose |
|---|---|
| `name` | Unique route name, used in logs, metrics labels, and the Admin API |
| `match` | Conditions the request must satisfy (see below) |
| `policy` | Name of the policy to execute; must exist or compilation fails |

## Match rules

A match rule can constrain the request on four criteria: path, method, headers, and virtual host. **All criteria present in the rule must match (logical AND)**; every field defaults to unset/empty, which matches any request.

| Criterion | Semantics |
|---|---|
| `path` | Exact match, trailing `/*` prefix wildcard, or `*` segment wildcards (see below); `None` matches any path |
| `methods` | List of allowed HTTP methods, compared case-insensitively; an empty list matches any method |
| `headers` | Map of required header name → value pairs; every key must be present with an exactly equal value (names compared case-insensitively, values exactly) |
| `hosts` | List of `Host` patterns the request must match one of — an exact hostname or a one-label wildcard such as `*.example.com` (see [Virtual hosts](#virtual-hosts)); empty matches any host |
| `host` | Single `Host` pattern, same syntax; kept for existing configs and merged with `hosts` |

```yaml
match:
  path: /api/*
  methods: [GET, POST]
  headers:
    x-api-version: "1"
  hosts: [api.example.com, "*.example.org"]
```

## Virtual hosts

`hosts` turns a route into a virtual-host route: it only triggers when the request's `Host` header matches one of its patterns. A route with neither `hosts` nor `host` matches every host, so the same path can be served differently per hostname by declaring one route per host and relying on first-match-wins order.

- The request's **port is ignored**: `api.example.com` matches `Host: api.example.com:8443`. Patterns must not carry a port — the route table build rejects one.
- Comparison is **ASCII case-insensitive**.
- A pattern is either an **exact hostname** or a **wildcard with a single leading `*.`**: `*.example.com` matches `api.example.com` but not `example.com` (no label to match) nor `a.b.example.com` (two labels). `*` elsewhere, a bare `*`, or `*.` with nothing after it are rejected at save time with a message naming the pattern.
- IPv6 literals keep their brackets: `hosts: ["[::1]"]` matches `Host: [::1]:8080`.

```yaml
routes:
  - name: tenant-a
    match:
      path: /app/*
      hosts: [a.example.com]
    policy: tenant-a-policy

  - name: any-tenant               # *.example.com, except a.example.com (matched above)
    match:
      path: /app/*
      hosts: ["*.example.com"]
    policy: shared-policy
```

The web UI's **New route** / **Edit route** dialog exposes the same fields (a comma-separated hosts list and a method picker), and the sidebar row shows the first host in front of the path when one is set.

## Path wildcards

Three pattern forms are supported:

- **Exact**: `/api/v1/users` matches only that path.
- **Trailing `/*` prefix wildcard**: matches the bare prefix itself and anything below it — but not sibling prefixes.
- **`*` segment wildcard**: matches exactly one path segment, so the pattern must have the same number of segments as the path.

| Pattern | Path | Matches? | Why |
|---|---|---|---|
| `/api/v1/users` | `/api/v1/users` | yes | exact |
| `/api/v1/users` | `/api/v1/other` | no | exact only |
| `/api/v1/*` | `/api/v1` | yes | trailing `/*` matches the bare prefix |
| `/api/v1/*` | `/api/v1/users` | yes | descendant of the prefix |
| `/api/v1/*` | `/api/v1/users/123` | yes | any depth below the prefix |
| `/api/v1/*` | `/api/v10` | no | sibling prefix, not a descendant |
| `/api/v1/*` | `/api/v2/users` | no | different prefix |
| `/api/*/users` | `/api/v1/users` | yes | `*` matches the one segment `v1` |
| `/api/*/users` | `/api/v2/users` | yes | `*` matches `v2` |
| `/api/*/users` | `/api/v1/posts` | no | last segment differs |
| `/api/*/users` | `/api/v1/v2/users` | no | segment counts must be equal |

## First match wins

Routes are evaluated in the order they are declared in `gateway.yaml`; the **first** route whose rule matches handles the request. Put more specific routes before broader ones:

```yaml
routes:
  - name: admin-endpoints        # evaluated first
    match:
      path: /api/admin/*
    policy: admin-policy

  - name: catch-all-api          # evaluated second
    match:
      path: /api/*
    policy: default-policy
```

See [Configuration](./configuration.md) for the file layout and hot-reload behavior of `gateway.yaml`.
