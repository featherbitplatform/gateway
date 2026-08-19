---
title: Configuration
description: The two YAML configuration files, environment variable interpolation, and hot-reload behavior.
---

featherbit is driven by two YAML files, passed on the command line:

```bash
featherbit --system-config config/system.yaml --gateway-config config/gateway.yaml
```

| File | Contents | Reload behavior |
|---|---|---|
| `system.yaml` | Process-level settings: data-plane listener, TLS, HTTP/2, timeouts, logging, admin API | Loaded once at startup, never hot-reloaded |
| `gateway.yaml` | Routes and node-graph policies | Hot-reloaded on file change; also mutated at runtime by the [Admin API](./admin-api.md) |

## system.yaml

Every top-level section has a default, so any section may be omitted:

```yaml
listener:
  bind: "0.0.0.0"
  port: ${GATEWAY_PORT:-8080}

http2:
  enabled: true

timeouts:
  connection_seconds: 30
  read_seconds: 30
  write_seconds: 30
  idle_seconds: 300

logging:
  level: ${LOG_LEVEL:-info}
  format: text

admin:
  bind: "0.0.0.0"
  port: ${ADMIN_PORT:-9090}
  username: ${ADMIN_USER:-admin}
  password: ${ADMIN_PASSWORD:-admin}
  ui_enabled: ${ADMIN_UI_ENABLED:-true}
```

| Section | Keys and defaults |
|---|---|
| `listener` | `bind` (default `0.0.0.0`), `port` (default `8080`) — the data-plane HTTP listener |
| `timeouts` | `connection_seconds`, `read_seconds`, `write_seconds` (default `30` each), `idle_seconds` (default `300`) |
| `logging` | `level` (default `info`), `format` (`json` is the default; any other value produces plain text) |
| `admin` | `bind` (default `0.0.0.0`), `port` (default `9090`), `username` and `password` (required, typically supplied via `${ENV_VAR}`), `ui_enabled` (default `true`) — serve the embedded web UI; `false` gives 404 on non-API paths. Inert in the `-headless` image, whose binary omits the UI entirely. Omitting the whole section disables the admin server entirely |

The `RUST_LOG` environment variable, when set, overrides `logging.level` at startup.

The `tls` (certificate/key paths, minimum version, mTLS, SNI) and `http2` sections are fully implemented — see [TLS & HTTP/2](tls.md) for the complete reference.

## gateway.yaml

`gateway.yaml` contains two lists, both defaulting to empty:

- `routes` — match rules bound to a policy name, evaluated in declaration order (see [Routing](./routing.md))
- `policies` — named node graphs referenced by routes; a route referencing an unknown policy fails compilation

```yaml
routes:
  - name: echo-api
    match:
      path: /api/*
      methods: [GET, POST, PUT, DELETE]
    policy: echo-policy

policies:
  - name: echo-policy
    error_handler: error-handler
    nodes:
      - id: listener
        type: listener
      - id: backend
        type: upstream
        config:
          targets:
            - host: ${ECHO_BACKEND_HOST:-localhost}
              port: ${ECHO_BACKEND_PORT:-3000}
      - id: error-handler
        type: error-handler
        config:
          status_code: 502
          body_template: '{"error": "{{error.code}}"}'
      - id: client
        type: client
    edges:
      - from: listener.out
        to: backend.in
      - from: backend.success
        to: client.in
      - from: backend.error
        to: error-handler.in
      - from: error-handler.success
        to: client.in
```

## Environment variable interpolation

All configuration values support shell-style interpolation:

| Pattern | Result |
|---|---|
| `${VAR}` | The variable's value, or the **empty string** if unset |
| `${VAR:-default}` | The variable's value, or `default` if unset |

```yaml
listener:
  port: ${GATEWAY_PORT:-8080}      # 8080 unless GATEWAY_PORT is set

admin:
  password: ${ADMIN_PASSWORD}      # empty string if ADMIN_PASSWORD is unset
```

Rules:

- Variable names must match `[A-Za-z_][A-Za-z0-9_]*`; text that does not match the pattern is left untouched.
- There is **no escape syntax** for a literal `${...}`.
- Multiple references in one value are all expanded, e.g. `bind: ${GW_HOST}:${GW_PORT}`.

**When resolution happens differs by file.** `system.yaml` is interpolated on
the raw file text before YAML parsing, so `${VAR}` works anywhere in it — keys
and values alike.

`gateway.yaml` (and everything authored through the [Admin API](./admin-api.md)
/ Web UI or delivered over etcd) is loaded with placeholders **preserved**: the
stored configuration — what the Admin API serves, the UI displays, and config
exports contain — always keeps the literal `${VAR}` form, so a secret like
`client_secret: ${CLIENT_SECRET}` never appears resolved in an API response or
an exported file. Values are resolved from the gateway process's environment at
the point of use instead:

- **plugin node config** (including `plugin_configs` profiles and supernode
  inner nodes) — when the policy graph is compiled;
- **route `match` rules** (path, host, methods, header values) — when the route
  table is built;
- **consumer fields and credentials** — when the consumer store is built.

Resolution is fresh on every (re)compile, so changing a variable and reloading
picks up the new value without rewriting any config.

Because gateway config is parsed before resolution, a placeholder is a YAML
*string* at load time. A value that is **exactly one** `${...}` placeholder is
typed after resolution the way YAML would type the same unquoted scalar:
`port: ${ECHO_PORT:-3010}` yields the number `3010`, `enabled: ${FLAG:-false}`
yields a boolean. A resolved value that does not parse as a number or boolean —
and any placeholder embedded in wider text, like `${GW_HOST}:${GW_PORT}` —
stays a string. Two consequences to be aware of:

- YAML quoting cannot force a string: a full-placeholder value whose resolution
  *looks* numeric or boolean (say an all-digits API key) is typed as a number or
  boolean even if the YAML value was quoted. If that happens the plugin rejects
  the config loudly at compile time (a string field reads a number as missing) —
  the fix is a value that doesn't parse as a scalar, or setting the literal value
  directly instead of via `${...}`;
- `${VAR}` in gateway.yaml **keys** or in structural fields (node ids, edge
  endpoints, policy/config_ref names) is no longer interpolated — placeholders
  belong in values that are matched or handed to plugins.

The env var must be set in the **gateway process's** environment; a value only
present in your shell or the browser is not visible to the gateway. And while
the Admin API never serves resolved values, an authenticated caller can still
arrange to read one back through the data plane (e.g. by echoing it into a
response header) — only expose environment holding secrets to operators you
trust with the Admin API.

## Hot-reload

`gateway.yaml` changes apply without a restart, through two mechanisms:

**File watcher.** The gateway watches the config file's parent directory (recursively) for modify/create events. Events are debounced: after the first event the reloader waits 500 ms and drains any further events, so a burst of filesystem notifications (as editors typically produce) results in a single reload.

**Admin API.** `POST /api/config/reload` re-reads `gateway.yaml` from disk (placeholders preserved; env vars resolve as the route graphs compile), recompiles all route graphs, and swaps them in. See [Admin API](./admin-api.md).

**Last-good-config guarantee.** Every reload path validates and recompiles the full configuration before swapping anything. If the new file fails to parse, validate, or compile, the failure is logged (or returned as an error by the reload endpoint) and the previously loaded configuration stays active — traffic keeps flowing on the last good config.

`system.yaml` is fixed for the process lifetime; changing it requires a restart.

## Debug mode

`system.yaml` also accepts a `debug:` section enabling per-request policy tracing and the plugin sandbox. It is off by default and, because `system.yaml` is not hot-reloaded, toggling it requires a restart — deliberately, so context capture cannot be switched on remotely. See [Debugging & sandbox](./debugging.md) for the full key reference.

```yaml
debug:
  enabled: ${FEATHERBIT_DEBUG:-false}
  capture_bodies: ${FEATHERBIT_DEBUG_BODIES:-false}
```
