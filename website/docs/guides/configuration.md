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
```

| Section | Keys and defaults |
|---|---|
| `listener` | `bind` (default `0.0.0.0`), `port` (default `8080`) — the data-plane HTTP listener |
| `timeouts` | `connection_seconds`, `read_seconds`, `write_seconds` (default `30` each), `idle_seconds` (default `300`) |
| `logging` | `level` (default `info`), `format` (`json` is the default; any other value produces plain text) |
| `admin` | `bind` (default `0.0.0.0`), `port` (default `9090`), `username` and `password` (required, typically supplied via `${ENV_VAR}`). Omitting the whole section disables the admin server entirely |

The `RUST_LOG` environment variable, when set, overrides `logging.level` at startup.

:::note Planned
`tls` (certificate/key paths, minimum TLS version) and `http2` sections are parsed but TLS termination and HTTP/2 are not yet implemented.
:::

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

All configuration values support shell-style interpolation. It runs on the raw file text **before** YAML parsing, so `${VAR}` works anywhere in the file — keys, values, and free-form plugin config alike.

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

### Plugin node config authored through the Admin API / Web UI

Config that arrives as structured data rather than raw YAML text — a plugin node
created or edited through the [Admin API](./admin-api.md) or the Web UI node
editor, or delivered over etcd — is **also** interpolated, but at a different
point: each string value in a node's `config` is resolved when the policy graph
is compiled, not by the file-text pass above. So a node field set to
`client_id: ${CLIENT_ID}` in the UI resolves from the environment exactly as the
same value would in `gateway.yaml`.

- Interpolation applies to string leaves of a node's `config` (including strings
  nested in arrays and objects). Non-string values are untouched.
- The **stored** value keeps the `${VAR}` template — the UI shows `${CLIENT_ID}`,
  while the running plugin sees the resolved value. Env vars are resolved fresh on
  every (re)compile, so changing the variable and reloading picks up the new value
  without rewriting the config.
- The env var must be set in the **gateway process's** environment. A value only
  present in your shell or the browser is not visible to the gateway.
- Because an authenticated Admin API caller can read a resolved value back (e.g.
  by echoing it into a response header), only expose environment holding secrets
  to operators you trust with the Admin API.

## Hot-reload

`gateway.yaml` changes apply without a restart, through two mechanisms:

**File watcher.** The gateway watches the config file's parent directory (recursively) for modify/create events. Events are debounced: after the first event the reloader waits 500 ms and drains any further events, so a burst of filesystem notifications (as editors typically produce) results in a single reload.

**Admin API.** `POST /api/config/reload` re-reads `gateway.yaml` from disk (with env interpolation), recompiles all route graphs, and swaps them in. See [Admin API](./admin-api.md).

**Last-good-config guarantee.** Every reload path validates and recompiles the full configuration before swapping anything. If the new file fails to parse, validate, or compile, the failure is logged (or returned as an error by the reload endpoint) and the previously loaded configuration stays active — traffic keeps flowing on the last good config.

`system.yaml` is fixed for the process lifetime; changing it requires a restart.

## Debug mode

`system.yaml` also accepts a `debug:` section enabling per-request policy tracing and the plugin sandbox. It is off by default and, because `system.yaml` is not hot-reloaded, toggling it requires a restart — deliberately, so context capture cannot be switched on remotely. See [Debugging & sandbox](./debugging.md) for the full key reference.

```yaml
debug:
  enabled: ${FEATHERBIT_DEBUG:-false}
  capture_bodies: ${FEATHERBIT_DEBUG_BODIES:-false}
```
