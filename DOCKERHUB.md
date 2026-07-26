# featherbit

A lightweight, high-performance API gateway delivered as a **single Rust
binary**. Routes are configured as visual node graphs — each plugin is a node,
wired together through success and error ports. The data-plane server, the
admin REST API, and the node-graph web editor are all served by the same
executable; this image ships it on a `scratch` base.

- **Source & issues:** https://github.com/featherbitplatform/gateway
- **Documentation:** https://featherbitplatform.github.io/gateway/
- **Plugin reference (80+ node types):** https://featherbitplatform.github.io/gateway/docs/reference/plugins

## Tags

| Tag | Meaning |
| --- | --- |
| `latest` | The latest release (tip of `main`). |
| `X.Y.Z`, `X.Y` | Immutable release versions (e.g. `0.2.0`, `0.2`). |
| `edge` | Tip of `develop` — unreleased, may break. |

All tags are multi-arch manifests for `linux/amd64` and `linux/arm64`.

## Quick start

```console
docker run --rm -p 8080:8080 -p 9090:9090 featherbit/featherbit
```

The image ships a working example configuration. The data plane listens on
**8080**, the admin API + web editor on **9090** (default credentials are in
the example `system.yaml` — change them before exposing anything).

With your own configuration:

```yaml
# compose.yaml
services:
  gateway:
    image: featherbit/featherbit:latest
    ports:
      - "8080:8080"
      - "9090:9090"
    volumes:
      - ./system.yaml:/etc/gateway/system.yaml:ro
      - ./gateway.yaml:/etc/gateway/gateway.yaml:ro
```

## Configuration

- Config lives at `/etc/gateway/system.yaml` (listeners, TLS, timeouts, admin
  API) and `/etc/gateway/gateway.yaml` (routes + node-graph policies); mount
  your own files over them.
- Every YAML value supports `${ENV_VAR:-default}` interpolation, so secrets
  and per-environment settings can come from the container environment.
- `gateway.yaml` hot-reloads on change; `system.yaml` changes need a restart.
- The container runs as non-root (uid 65532) from `scratch` — there is no
  shell inside the image.

Full configuration reference: https://featherbitplatform.github.io/gateway/
