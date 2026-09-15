# Docker Compose examples

Self-contained deployment stacks. Each directory holds a `compose.yaml` and its
own `config/`, uses the published `featherbit/featherbit` image, and refers to
nothing outside itself — copy a directory anywhere and it runs.

| Stack | What it shows | Run it |
|---|---|---|
| [`minimal/`](./minimal) | One gateway, one upstream, file-based config. The baseline to copy into your own project. | `docker compose -f examples/compose/minimal/compose.yaml up` |
| [`etcd-single/`](./etcd-single) | Config stored in etcd on a persistent volume, so Admin API and web UI edits survive a restart. | `docker compose -f examples/compose/etcd-single/compose.yaml up` |
| [`etcd-cluster/`](./etcd-cluster) | Two gateway replicas sharing one etcd — a change on one converges to the other. | `docker compose -f examples/compose/etcd-cluster/compose.yaml up` |
| [`tls/`](./tls) | TLS termination with a self-signed certificate generated at startup, owned by the gateway's uid. | `docker compose -f examples/compose/tls/compose.yaml up` |

Each exposes the Admin API and web UI on `:9090` (`admin` / `admin`) and the
data plane on `:8080` — except `tls/`, which serves HTTPS on `:8443`. All four
route `/api/*` to a
[`traefik/whoami`](https://github.com/traefik/whoami) container that echoes back
the request it received — so you can see exactly what the policy delivered
upstream:

```bash
curl http://localhost:8080/api/users     # upstream sees GET /users; the /api prefix is stripped
curl -k https://localhost:8443/api/users # the tls/ stack (self-signed, hence -k)
open http://localhost:9090               # node-graph editor
```

Replace the `whoami` service with your own upstream, and point the `upstream`
node in `config/gateway.yaml` at it.

## Pinning a version

Every stack defaults to the `latest` image tag. Override it to pin a release:

```bash
FEATHERBIT_TAG=0.9.0 docker compose -f examples/compose/minimal/compose.yaml up
```

Add `-headless` to any tag for the build without the embedded web UI.

## Certificates and the container uid

`tls/` generates its self-signed pair at startup into a named volume — nothing
private is committed, and `down -v` regenerates it.

The `certs` service does one thing that is easy to miss and is the reason the
stack exists: it `chown`s the key to **uid 65532**. `openssl` writes a private
key mode `0600` regardless of your umask, and the featherbit image is
`FROM scratch` running as uid 65532, so a key owned by anyone else fails startup:

```
failed to read TLS private key '/etc/gateway/tls/key.pem': Permission denied (os error 13)
```

Only the key is named there — the certificate is `0644` and loads fine. If you
bring your own pair, either `chown 65532:65532 key.pem` (keeping it `0600`), or
run the gateway as yourself with `user: "${UID}:${GID}"` — it binds only
unprivileged ports and needs no root.

## File config vs. etcd config

`minimal/` and `tls/` run the default `config.source: file`: the mounted `config/` **is**
the configuration. Editing `gateway.yaml` on the host hot-reloads the running
gateway, but changes made through the Admin API or the web UI apply to the
running process only and are gone on the next start.

The two etcd stacks run `config.source: etcd`. On first boot the gateway finds
an empty prefix and seeds it from the mounted `gateway.yaml`; after that etcd
holds the live config, Admin API writes go to etcd, and the named volume keeps
them across `docker compose down` / `up`. To discard that state and re-seed from
the file, bring the stack down with `down -v`.

## Not these

- The [`docker-compose.yaml`](../../docker-compose.yaml) at the repository root
  is the **development** stack: it builds the gateway from source, mounts the
  repo's own `config/` and Lua plugins, and turns debug tracing on. That is the
  one `docker compose up` runs, and what the
  [Quick Start](https://featherbitplatform.github.io/gateway/docs/getting-started/quick-start) means.
- [`tests/docker-compose.yml`](../../tests/docker-compose.yml) adds Keycloak for
  the OIDC integration tests, and
  [`dev/pebble/docker-compose.yml`](../../dev/pebble/docker-compose.yml) runs a
  local ACME server for the TLS tests. Both are test fixtures, not examples.

See the [Deployment guide](https://featherbitplatform.github.io/gateway/docs/guides/deployment) for the
container image, graceful shutdown, and multi-instance topologies.
