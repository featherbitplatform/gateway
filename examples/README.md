# Examples

One directory per scenario. Each holds a `compose.yaml` and its own `config/`,
runs the published `featherbit/featherbit` image, and refers to nothing outside
itself — copy a directory anywhere and it runs.

| Scenario | What it shows | Run it |
|---|---|---|
| [`minimal/`](./minimal) | One gateway, one upstream, file-based config. The baseline to copy into your own project. | `docker compose -f examples/minimal/compose.yaml up` |
| [`tls/`](./tls) | TLS termination with a self-signed certificate generated at startup, owned by the gateway's uid. | `docker compose -f examples/tls/compose.yaml up` |
| [`etcd-single/`](./etcd-single) | Config stored in etcd on a persistent volume, so Admin API and web UI edits survive a restart. | `docker compose -f examples/etcd-single/compose.yaml up` |
| [`etcd-cluster/`](./etcd-cluster) | Two gateway replicas sharing one etcd — a change on one converges to the other. | `docker compose -f examples/etcd-cluster/compose.yaml up` |
| [`mcp/`](./mcp) | The MCP endpoint enabled behind scoped tokens, with a ready-to-copy client config for connecting an agent. | `cp examples/mcp/.env.example examples/mcp/.env` then `docker compose -f examples/mcp/compose.yaml up` |
| [`lua-scripts/`](./lua-scripts) | A policy chaining Lua `script` nodes — request id, bot blocking, response timing — with the scripts mounted from `plugins/`. | `docker compose -f examples/lua-scripts/compose.yaml up` |
| [`stream/`](./stream) | An L4 TCP stream on `:8081` relaying raw bytes to the same upstream the HTTP plane on `:8080` proxies. | `docker compose -f examples/stream/compose.yaml up` |
| [`redis-stores/`](./redis-stores) | A shared `proxy-cache` pair over redis, a `phase: purge` route that invalidates it, and a `store-incr` counter. | `docker compose -f examples/redis-stores/compose.yaml up` |

Each exposes the Admin API and web UI on `:9090` (`admin` / `admin`) and the
data plane on `:8080` — except `tls/`, which serves HTTPS on `:8443`. All route
`/api/*` to a [`traefik/whoami`](https://github.com/traefik/whoami) container
that echoes back the request it received — so you can see exactly what the
policy delivered upstream:

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
FEATHERBIT_TAG=0.10.0 docker compose -f examples/minimal/compose.yaml up
```

Add `-headless` to any tag for the build without the embedded web UI.

## Connecting an agent (`mcp/`)

The MCP endpoint lives on the admin listener at `/mcp`, outside Basic Auth,
behind bearer tokens scoped `read` (inspect config, traces, the sandbox) or
`write` (also create/replace/delete routes, policies, supernodes, plugin
configs and stores). The stack reads both tokens from `examples/mcp/.env`
(gitignored) and refuses to start without them, so a token never ends up in a
committed file:

```bash
cp examples/mcp/.env.example examples/mcp/.env    # then replace both values: openssl rand -base64 32
docker compose -f examples/mcp/compose.yaml up
```

Connect Claude Code with the one-liner, or copy
[`mcp/.mcp.json`](./mcp/.mcp.json) to your project root — it reads the token
from the same environment variable:

```bash
claude mcp add --transport http featherbit http://localhost:9090/mcp \
  --header "Authorization: Bearer $FEATHERBIT_MCP_READ_TOKEN"
```

Smoke test without a client:

```bash
curl -s -X POST http://localhost:9090/mcp -H "Authorization: Bearer $FEATHERBIT_MCP_READ_TOKEN" \
  -H 'Accept: application/json, text/event-stream' -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

The same `mcpServers` JSON works for Claude Desktop, Cursor and Windsurf. The
web UI's **Agent** panel shows these snippets with your actual endpoint. See the
[MCP guide](https://featherbitplatform.github.io/gateway/docs/guides/mcp) for
scopes, the `Origin` allow-list and the tool catalogue.

## Scripts (`lua-scripts/`)

`plugins/` is mounted at `/etc/gateway/plugins`, where the policy's `script`
nodes point. Edit a script on the host; the policy file is hot-reloaded and
scripts are read when the policy compiles. `plugins/README.md` describes each
script and the `execute(ctx)` contract.

```bash
curl -si http://localhost:8080/api/users | grep -iE 'x-request-id|x-response-time'
curl -si -A 'scrapy/2.0' http://localhost:8080/api/users | head -1       # HTTP/1.1 403 Forbidden (the script answers on its `respond` port)
```

## Streams (`stream/`)

A `stream:` entry in `config/system.yaml` binds a port at startup and relays raw
TCP or UDP to a load-balanced pool — no routes, no policies, no HTTP parsing.
Here `:8081` fronts the same `whoami` container the HTTP plane proxies:

```bash
curl http://localhost:8080/api/users   # via the policy: the upstream sees /users
curl http://localhost:8081/api/users   # via the stream: the upstream sees /api/users, bytes untouched
```

## Stores (`redis-stores/`)

This scenario needs an image with `phase: purge` and `DELETE /api/cache/{id}`
(0.11.0 or later). Until that tag is published, build one locally:
`docker build -t featherbit/featherbit:dev . && FEATHERBIT_TAG=dev docker compose -f examples/redis-stores/compose.yaml up`.

Everything here references one declared `stores:` entry by name; a second
gateway replica pointed at the same redis would share the cache and the counter.

```bash
curl -si http://localhost:8080/api/users | grep -i featherbit-cache-status   # MISS
curl -si http://localhost:8080/api/users | grep -i featherbit-cache-status   # HIT
curl -X POST http://localhost:8080/purge                                     # the phase: purge route
curl -si http://localhost:8080/api/users | grep -i featherbit-cache-status   # MISS again
curl -u admin:admin -X DELETE http://localhost:9090/api/cache/api           # the same purge, from the Admin API
curl -si http://localhost:8080/visits | grep -i x-visit-count                # 1, then 2, then 3
```

A cache backend the gateway cannot reach is a miss (the request goes upstream);
a `store-incr` it cannot reach is a `503` — a cache protects latency, a store
holds correctness. Stop the `redis` service and try both routes to see the
difference.

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

Every stack except the two etcd ones runs the default `config.source: file`: the
mounted `config/` **is** the configuration. Editing `gateway.yaml` on the host
hot-reloads the running gateway, but changes made through the Admin API, the web
UI or an MCP agent apply to the running process only and are gone on the next
start.

The two etcd stacks run `config.source: etcd`. On first boot the gateway finds
an empty prefix and seeds it from the mounted `gateway.yaml`; after that etcd
holds the live config, Admin API writes go to etcd, and the named volume keeps
them across `docker compose down` / `up`. To discard that state and re-seed from
the file, bring the stack down with `down -v`.

## Not these

- The [`docker-compose.yaml`](../docker-compose.yaml) at the repository root
  is the **development** stack: it builds the gateway from source, mounts the
  repo's own `config/` and the Lua plugins, and turns debug tracing on. That is
  the one `docker compose up` runs, and what the
  [Quick Start](https://featherbitplatform.github.io/gateway/docs/getting-started/quick-start) means.
- [`tests/docker-compose.yml`](../tests/docker-compose.yml) adds Keycloak for
  the OIDC integration tests, and
  [`dev/pebble/docker-compose.yml`](../dev/pebble/docker-compose.yml) runs a
  local ACME server for the TLS tests. Both are test fixtures, not examples.

See the [Deployment guide](https://featherbitplatform.github.io/gateway/docs/guides/deployment) for the
container image, graceful shutdown, and multi-instance topologies.
