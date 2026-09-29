# Featherbit: tuning

Image: built from the checkout (`featherbit-bench/featherbit:dev`, git SHA in run.json),
or the released image via `--image featherbit=featherbit/featherbit:<tag>` for publishable runs.

| Setting | Value | Why |
|---|---|---|
| `TOKIO_WORKER_THREADS` | profile cores | One runtime worker per pinned core (tokio honours the env var in `#[tokio::main]`). |
| admin server | disabled (no `admin:` section) | Data plane only, like every competitor. |
| `logging.level` | warn | No per-request logging; same rule for every gateway. |
| TLS | `min_version: "1.3"`, ECDSA P-256, HTTP/2 enabled (default) | Same cert and protocol for every gateway. |
| limit-count | `policy: local`, count 100,000,000 / 60 s (probe route: count 1) | Never trips at benchmark load; each node counts on its own. |

Deliberately **not** tuned: no custom allocator, no build-profile changes, no plugin shortcuts. The image is what users get.

Known costs Featherbit always pays, disclosed so the numbers are read correctly:
- Per-route and per-node Prometheus metrics are recorded on every request. They can't be disabled.
- The image is a static musl build (`FROM scratch`) using mimalloc as the allocator and the `dist` profile (fat LTO): the same build users get.
- `script` runs every execution in a fresh Luau VM (docs: reference/plugins/script.md).
- Responses pass through the node graph's buffered path unless a node permits streaming (docs: reference/plugins/upstream.md).
