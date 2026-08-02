# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commit Conventions

Use [Conventional Commits](https://www.conventionalcommits.org/) for every commit message (`feat:`, `fix:`, `docs:`, `ci:`, `chore:`, `refactor:`, `test:`, with optional scope, e.g. `ci(security): ...`). Do not add a Co-Authored-By trailer.

## Branching (Gitflow)

The repository follows the [Gitflow](https://nvie.com/posts/a-successful-git-branching-model/) workflow:

- `main` — production releases only; every commit on it is a released version, tagged `vX.Y.Z`. Never commit work directly to `main`.
- `develop` — integration branch; the default target for feature work and PRs.
- `feature/<name>` — branched from `develop`, merged back into `develop`. Start any new work here.
- `release/X.Y.Z` — branched from `develop` when preparing a release; only stabilization fixes land on it, then it merges into both `main` (tagged) and `develop`.
- `hotfix/<name>` — branched from `main` for urgent production fixes; merges back into both `main` (tagged) and `develop`.

Before starting work, branch `feature/<name>` off `develop` (create `develop` from `main` if it does not exist yet).

## Build Commands

```bash
cargo build                # debug build
cargo test                 # all tests (unit + integration, inline in src/)
cargo test test_lua_       # run Lua-related tests
docker compose up          # gateway + echo-backend
```

Run a single test:
```bash
cargo test test_strip_path_prefix -- --exact
```

End-to-end suite (Playwright: admin API + data plane + web UI). Scenario catalog
in `e2e/E2E_TESTBOOK.md`; it boots the gateway and echo backend itself, on isolated
ports, so nothing needs to be running first:
```bash
cargo build --release                              # the suite runs the release binary
cd e2e && npm install && npx playwright install chromium
npm test                                           # all scenarios
npm run test:headed                                # watch it drive the browser
```

Documentation website (Docusaurus, in `website/`):
```bash
cd website && npm run start   # dev server
cd website && npm run build   # production build (syncs rustdoc + TypeDoc into static/api/)
```

Local SAST pipeline (same scanners and thresholds as `.github/workflows/security.yml`;
tool configs at the repo root are the shared source of truth):
```powershell
./dev/sast.ps1                # all scans except the image scan (dev/sast.sh on Linux/macOS)
./dev/sast.ps1 image          # docker build + grype/trivy on the built image
```

## What This Project Is

A high-performance API gateway delivered as a single Rust binary. (The original `REQUIREMENTS.md` specification no longer exists in the repo; the closest current equivalents are the docs site under `website/docs/` and the honest-state ledger at `website/docs/reference/roadmap.md`.)

Core features:
- **Node-graph routing policies** — request/response pipelines declared in YAML with success/error port routing
- **Two-tier plugin system** — 80+ native Rust node types (structural nodes, proxy/transform, security, auth & authz incl. interactive SSO, traffic control, 17 loggers, tracing, metrics, serverless/FaaS — most ported from Apache APISIX 3.17) + scripted plugins in Lua (mlua, Luau runtime)
- **Context object** — `request`, `response`, `message`, `errors` flowing through every node
- **Admin API** — axum-based REST API on separate port with Basic Auth, CRUD for routes/policies, health/ready/metrics endpoints
- **Hot-reload** — file watcher (notify) triggers config reload on gateway.yaml changes
- **Prometheus metrics** — per-route and per-node counters/histograms at `/metrics`
- **Supernodes** — reusable named subgraphs inlined into policies at compile time
- **Shared plugin configs** — named, typed config profiles referenced by nodes via config_ref, resolved at compile time

## Architecture

**Request flow**: HTTP request → `server::listener` matches route → builds `Context` → `CompiledGraph::execute()` walks nodes following success/error edges → final `Context.response` sent to client.

**Key modules**:
- `src/graph/engine.rs` — compiles PolicyConfig into CompiledGraph, executes node graph with success/error port routing
- `src/state.rs` — SharedState with RwLock-protected routes, used by both data-plane and admin API
- `src/plugins/mod.rs` — Plugin trait + `create_plugin()` factory for all 80+ node types
- `src/plugins/script/lua_runtime.rs` — Context↔Lua table marshalling, script execution
- `src/admin/` — axum Router with basic auth middleware, CRUD endpoints, /healthz, /readyz, /metrics
- `src/debug/` — debug mode: per-request policy-execution traces (context snapshot + derived diff per node, redacted at capture time, bounded ring buffer) and the plugin sandbox; served from `/api/debug/*` and the UI's Debug panel. Off unless `debug.enabled` is set in `system.yaml` (restart-gated by design)

**Plugin contract**: `async fn execute(ctx, named_inputs) -> Result<PluginOutput, PluginExecutionError>`. Errors include the context so the graph engine can route through error ports.

**Edge format in YAML**: `from: node_id.port` / `to: node_id.port`. Ports: `out`, `success`, `error`, `in`.

## Configuration

- `config/system.yaml` — listeners, TLS, HTTP/2, timeouts, admin API, logging
- `config/gateway.yaml` — routes (match rules + policy reference), policies (nodes + edges)
- All YAML values support `${ENV_VAR:-default}` interpolation

## Available Plugin Types

80+ node types registered in the `create_plugin` factory (`src/plugins/mod.rs`). Core: `listener`, `client`, `proxy-rewrite`, `upstream`, `error-handler`, `script`. The rest are ported from Apache APISIX 3.17 across transformation, security, auth/authz, traffic control, logging, tracing, metrics, and serverless — see `docs/apisix-parity.md` (repo-internal, not published on the docs site) for the full catalog and the parity status of all 118 APISIX plugins. Shared plugin infrastructure lives in `src/plugins/util/` (content codec, cookie sessions, log entries, trace propagation), `src/vars/` (var resolver + expression engine), `src/outbound/`, `src/consumers/`, `src/ratelimit/`, `src/batch/`, and `src/traffic/`.

## Not Yet Implemented

- Python scripting runtime (pyo3) — Lua (mlua/Luau) is the shipped scripting runtime
- `unpack` node

(The web UI node-graph editor and the proxy-cache plugin **are** implemented — the UI is embedded via `rust-embed` and served from `src/admin/ui.rs`; proxy-cache is registered in the `create_plugin` factory. The UI is gated by the default-on `ui` cargo feature (`--no-default-features` = headless build, published as the `-headless` Docker variant) and by `admin.ui_enabled` at runtime.)

TLS termination, HTTP/2, WebSocket, L4 TCP/UDP stream proxying, and graceful shutdown **are** implemented. On SIGTERM/Ctrl+C every listener stops accepting and in-flight HTTP/Admin requests drain (hyper `GracefulShutdown`, bounded by `timeouts.shutdown_timeout_seconds`) before exit; signal handling + the shutdown `watch` channel live in `src/main.rs`. Set `tls:` in `system.yaml` for HTTPS on the data-plane (and `admin.tls` for the Admin API); HTTP/2 is negotiated per connection (ALPN over TLS, h2c over plaintext) and advertised to TLS upstreams. WebSocket upgrades run the policy graph then relay to a `ws://`/`wss://` upstream, accepting both HTTP/1.1 upgrades and HTTP/2 extended CONNECT (RFC 8441) from clients. L4 stream listeners under `stream:` proxy raw TCP/UDP to a load-balanced pool via the shared `src/balancer.rs`. TLS certs hot-reload on cert-file change (ArcSwap + notify watcher; `src/server/tls.rs`), mTLS client-cert verification is supported (`client_ca_path` on `TlsConfig`; the client identity — fingerprint, subject CN, SAN DNS — is exposed to the graph as `__client_cert_fingerprint`/`__client_cert_subject_cn`/`__client_cert_san_dns` via `x509-parser`), SNI multi-cert termination selects a per-hostname cert (`sni_certs` + a `ResolvesServerCert` resolver in `src/server/tls.rs`), and TCP streams support SNI-based TLS passthrough routing (`src/stream/sni.rs`; the shared wildcard matcher `SniPattern` is reused by both). Outbound mTLS is supported per `upstream` node (`client_cert_path`/`client_key_path`, plus `ca_cert_path` for private CAs), applied to both HTTPS proxying and wss relays. Transport code: `src/server/tls.rs`, `src/server/websocket.rs`, `src/stream/`. Follow-ups: CRL/OCSP revocation, RFC 8441 to the upstream, dynamic stream routes.

etcd clustering (stateful mode) **is** implemented — `config.source: etcd` in `system.yaml` delivers routes/policies/consumers over etcd's v3 HTTP/JSON gateway (`src/config_store/etcd.rs`) with cluster-wide convergence and seed-if-empty bootstrap. Follow-ups: TLS-to-etcd, streaming watch.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
