# Docker Compose examples

Self-contained deployment stacks. Each directory holds a `compose.yaml` and its
own `config/`, uses the published `featherbit/featherbit` image, and refers to
nothing outside itself — copy a directory anywhere and it runs.

| Stack | What it shows | Run it |
|---|---|---|
| [`minimal/`](./minimal) | One gateway, one upstream, file-based config. The baseline to copy into your own project. | `docker compose -f examples/compose/minimal/compose.yaml up` |
| [`etcd-single/`](./etcd-single) | Config stored in etcd on a persistent volume, so Admin API and web UI edits survive a restart. | `docker compose -f examples/compose/etcd-single/compose.yaml up` |
| [`etcd-cluster/`](./etcd-cluster) | Two gateway replicas sharing one etcd — a change on one converges to the other. | `docker compose -f examples/compose/etcd-cluster/compose.yaml up` |

All three expose the data plane on `:8080` and the Admin API and web UI on
`:9090` (`admin` / `admin`), and route `/api/*` to a
[`traefik/whoami`](https://github.com/traefik/whoami) container that echoes back
the request it received — so you can see exactly what the policy delivered
upstream:

```bash
curl http://localhost:8080/api/users     # upstream sees GET /users; the /api prefix is stripped
open http://localhost:9090               # node-graph editor
```

Replace the `whoami` service with your own upstream, and point the `upstream`
node in `config/gateway.yaml` at it.

## Pinning a version

Every stack defaults to the `latest` image tag. Override it to pin a release:

```bash
FEATHERBIT_TAG=0.8.0 docker compose -f examples/compose/minimal/compose.yaml up
```

Add `-headless` to any tag for the build without the embedded web UI.

## File config vs. etcd config

`minimal/` runs the default `config.source: file`: the mounted `config/` **is**
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
