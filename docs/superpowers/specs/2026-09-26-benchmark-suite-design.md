# Competitive benchmark suite

**Status:** approved in discussion, 2026-09-26
**Branch:** `feature/benchmark-suite`

## 1. Goal

Measure Featherbit's data-plane performance against the main open-source API gateways with a methodology strong enough to **publish**: reproducible by anyone, every competitor configured natively and tuned by its own vendor guidance, every number traceable to raw samples and the exact configs that produced it.

Constraints agreed in discussion:

- **Publishable** results are the end goal, but **only this 8-core Windows laptop (Docker Desktop / WSL2)** is available for now. The harness is developed and dry-run here; laptop results are always labelled *not for publication*. Publishable numbers come later from Linux hosts, with **no harness changes** — only a different topology file.
- **Competitors:** Apache APISIX, Kong (OSS), Envoy, Tyk (OSS), KrakenD (CE), Traefik, plus stock **nginx** as a zero-feature ceiling.
- **Scenario families:** core proxy, common plugins, payload & protocol, scripting.

Success means: one command runs a filtered or full matrix and produces a `run.json` plus a self-contained HTML report, where every cell is either a validated measurement or an explicitly labelled non-result (`n/a`, `invalid`, `error`, `loadgen-bound`, `upstream-bound`).

## 2. Approach

A containerized harness with a pluggable topology. Every role — load generator, gateway, upstream — is a pinned container. A stdlib-only Python orchestrator walks the matrix. Topology decides placement:

- `local` — one host; each role pinned to disjoint cores via Docker `cpuset`.
- `remote` — the same containers placed on 2–3 Linux hosts via Docker contexts (SSH); gateways use `network_mode: host`.

Rejected: infrastructure-as-code with native installs (heaviest, cannot dry-run on the laptop) and forking an existing public gateway benchmark (stale versions, vendor-authored, cannot express the plugin/scripting scenarios).

## 3. Layout

A new top-level `bench/` directory, independent of the Rust crate (no effect on `cargo test` or CI):

```
bench/
  README.md                 # methodology + how to run (local & remote) + config-PR invitation
  bench.py                  # orchestrator CLI (stdlib only; Python >= 3.11 for tomllib)
  topology/
    local.toml              # single host, cpuset per role
    remote.example.toml     # loadgen / gateway / upstream hosts as docker contexts
  scenarios/
    scenarios.toml          # scenario catalog: id, family, request shape, probes
  upstream/
    nginx.conf              # static bodies /1k /64k /1m, header echo, keepalive tuned
  loadgen/
    Dockerfile              # wrk, wrk2, oha — versions pinned
  certs/                    # generated at run time (ECDSA P-256), gitignored
  gateways/
    featherbit/ apisix/ kong/ envoy/ tyk/ krakend/ traefik/ nginx/
      gateway.toml          # adapter: image@digest, mounts, core-count knob, health URL,
                            #          supported scenarios, N/A reasons
      TUNING.md             # every non-default setting, with the vendor source for it
      <config-name>/        # native, hand-written config; gateway.toml maps each
                            # scenario to one (e.g. core.proxy, payload.* share `proxy`)
  results/                  # gitignored; <timestamp>-<topology>/ per run
  report/
    render.py               # run.json -> report.html
  tests/                    # unittest suite for the harness
```

## 4. Components

### 4.1 Upstream

Stock nginx on its own dedicated cores. It serves `/1k`, `/64k`, `/1m` from memory. It also echoes selected request headers (`X-Bench-*`) back as response headers, so probes can observe request-side rewrites from the client. Every gateway proxies to the identical upstream over plaintext HTTP/1.1 keep-alive.

A **baseline cell** (loadgen → upstream directly) runs in every run. It proves the upstream is not the bottleneck and anchors the overhead view (§7).

### 4.2 Gateway adapters

Each `gateways/<name>/` is data only — the orchestrator contains no per-gateway code paths. `gateway.toml` declares:

- `image` pinned by digest; for Featherbit, either `build = "../.."` (build the current checkout) or a published tag.
- config mount: the per-scenario directory maps onto the container path the gateway expects.
- `cores_env` / `cores_arg`: how to set worker count = the profile's core count. Examples:
  - nginx-based gateways (APISIX, Kong, nginx): `worker_processes`
  - Featherbit: `TOKIO_WORKER_THREADS` (its `#[tokio::main]` runtime honours it)
  - Go gateways (Tyk, KrakenD, Traefik): `GOMAXPROCS`
  - Envoy: `--concurrency`
- `health`: URL + expected status.
- `listen`: plain port, TLS port.
- `dependencies`: extra containers (Tyk: Redis). They are placed on the upstream's cores and disclosed in the report.
- `scenarios`: the supported set. Every unsupported scenario has a one-line `na_reason`.

Deployment modes: APISIX standalone YAML (no etcd), Kong DB-less, Featherbit file config, Tyk OSS with its mandatory Redis.

### 4.3 Load generator image

One pinned image with three tools:

- `wrk` — rough throughput ceiling.
- `wrk2` — fixed-rate, HDR histogram, coordinated-omission-corrected latency.
- `oha` — HTTP/2. It replaces `h2load`, which reports no latency percentiles and has no coordinated-omission correction; `oha --latency-correction` has both.

Per-scenario request headers (API key, pre-signed HS256 JWT) are passed with each tool's `-H` flag; no request scripts are needed.

### 4.4 Orchestrator (`bench.py`)

```
bench.py run      --topology local [--gateways a,b] [--scenarios 'core.*,plugin.jwt']
                  [--profiles 1,4] [--reps 5] [--quick] [--publish] [--resume <dir>]
bench.py validate --topology local [filters]      # probes only, no measurement
bench.py report   <run-dir>                       # -> <run-dir>/report.html
bench.py clean                                    # remove containers labelled featherbit-bench=1
```

- `--quick`: 1 repetition, 5 s steps, short latency ladder — for local iteration.
- `--publish`: refused unless all of these hold:
  - the topology is `remote` on Linux
  - host networking is on
  - the CPU governor is `performance` on every host
  - every parameter equals the `scenarios.toml` catalog (no `--quick`, no reduced `--reps`)

## 5. Scenarios

| ID | Family | Definition |
|---|---|---|
| `core.proxy` | Core | Plain proxy, 1 KB response, HTTP/1.1 keep-alive |
| `core.routes-1k` | Core | 1,000 prefix routes; the request hits the last-defined one |
| `plugin.key-auth` | Plugins | API key header, 1 consumer |
| `plugin.jwt` | Plugins | HS256 JWT validation, pre-signed token |
| `plugin.rate-limit` | Plugins | Local in-memory counter; limit far above offered load (never trips) |
| `plugin.header-rewrite` | Plugins | Add 2 request headers, remove 1 response header |
| `plugin.chain` | Plugins | key-auth → rate-limit → header-rewrite |
| `payload.64k` / `payload.1m` | Payload | Larger upstream bodies |
| `proto.tls` | Protocol | HTTPS to client: ECDSA P-256, TLS 1.3, session resumption on; plaintext upstream |
| `proto.h2` | Protocol | HTTP/2 over TLS to client (oha) |
| `script.header` | Scripting | Script reads a request header, computes a value, sets a request header (the upstream echoes it back, so the probe sees it on the response). Request side because Tyk's JSVM and Traefik's Yaegi plugins cannot portably set response headers. |

### 5.1 The N/A rule

A cell runs only when the gateway implements the scenario **natively in its open-source edition**: a built-in plugin/filter/middleware, plain config, or its first-party scripting mechanism. No sidecars, no external auth services, no enterprise features. Anything else is `n/a`, with the reason shown in the report.

### 5.2 Expected support matrix

Every cell of this matrix is verified against the pinned versions during implementation, and corrected here if wrong.

| | core | key-auth | jwt | rate-limit | header | chain | payload/tls/h2 | script |
|---|---|---|---|---|---|---|---|---|
| Featherbit | ✓ | `key-auth` | `jwt-auth` | `limit-count` | `proxy-rewrite` + `response-rewrite` | ✓ | ✓ | `script` (Luau) |
| APISIX | ✓ | key-auth | jwt-auth | limit-count | proxy-rewrite + response-rewrite | ✓ | ✓ | serverless-pre-function |
| Kong | ✓ | key-auth | jwt | rate-limiting (`policy: local`) | request-/response-transformer | ✓ | ✓ | pre-function |
| Envoy | ✓ | api_key_auth *(if in pinned version, else n/a)* | jwt_authn | local_ratelimit | route header mutation | ✓ | ✓ | Lua filter |
| Tyk OSS | ✓ | auth token | JWT | API rate limit | header transforms | ✓ | ✓ | JSVM middleware |
| KrakenD CE | ✓ (no-op encoding) | n/a — API keys are Enterprise | auth/validator | qos/ratelimit | martian modifiers | n/a | ✓ | Lua |
| Traefik | ✓ | n/a — no native API key | n/a — JWT is Traefik Hub | rateLimit middleware | headers middleware | n/a | ✓ | Yaegi local plugin |
| nginx | ✓ | n/a | n/a | limit_req | proxy_set_header / proxy_hide_header | n/a | ✓ | n/a (baseline) |

### 5.3 Resource profiles

Each cell runs with the gateway pinned to **1 core** and to **4 cores**. The upstream (plus any dependencies) gets fixed dedicated cores, and the load generator gets the rest.

On the 8-core laptop, the 4-core profile leaves 2 cores each for the upstream and the load generator. That is acceptable for dry runs only; the saturation check (§6 step 7) flags it when it bites.

`local.toml` defines the core assignment for each profile. `remote` topologies give each role a whole host.

## 6. Measurement procedure

For each (gateway, scenario, profile) cell:

1. **Boot.** Start the upstream, any dependencies, and the gateway with the cell's config, pinned to their cpusets. Wait for health, with a 60 s timeout. On failure: status `error`, container logs saved, continue with the next cell.
2. **Probes.** Run the scenario's probes from `scenarios.toml`. Any failure: status `invalid`, and the cell is not measured. Probes by scenario:
   - key-auth: good key → 200, missing key → 401/403.
   - jwt: valid token → 200, tampered token → 401/403.
   - rate-limit: the benchmark route → 200; a dedicated low-limit probe route returns 429 on its second request.
   - header-rewrite / script: the expected headers are observed on the response (request side via the upstream echo).
   - tls: TLS 1.3 negotiated. h2: ALPN `h2` negotiated.
   - all scenarios: response body size matches the scenario.
3. **Ceiling and warm-up.** A 10 s `wrk` flood gives a rough ceiling C (it also serves as the first warm-up). Then 30 s of `wrk2` at 0.2·C. Both results are discarded except C.
4. **Max sustainable throughput.**
   - (a) Bisect with `wrk2` at fixed rates between 0.1·C and 1.2·C. Each step lasts 20 s, with 64 connections and 4 loadgen threads. A step passes when p99 ≤ **10 ms**, the error rate is < **0.1%**, and the tool actually delivered ≥ 95% of the offered rate (a step that served nothing never passes). If the gateway container died during measurement, the repetition is an `error`, not a low result.
   - (b) The search stops when the bracket is within 2%. The highest passing rate is the **max sustainable RPS**.

   `proto.h2` uses `oha` (`-q` rate, `--latency-correction`) with the same pass criteria.
5. **Latency ladder.** 60 s `wrk2` runs at a fixed rate ladder shared by all gateways: 1k, 5k, 10k, 25k, 50k, 100k, 200k req/s. It stops at the first rate above the cell's max sustainable RPS. Recorded: p50, p90, p99, p99.9, max (HDR, CO-corrected), and the error count.
6. **Resource sampling.** During step 5, `docker stats` sampled at 1 Hz for the gateway, upstream and loadgen containers. Reported per rate:
   - gateway median CPU (cores)
   - peak RSS
   - **RPS per core consumed**
7. **Saturation check.** If loadgen or upstream median CPU exceeds 85% of its cpuset during a measured step, the cell is `loadgen-bound` or `upstream-bound`. Its throughput is then reported as "≥ X", never as a result.
8. **Repetition.** Steps 1–7 run **5 times**, with a full container restart each time. Reported values: the median, with min/max as the spread. Containers are removed before the next cell.

**Ordering.** Cells are interleaved (all gateways for scenario 1, then all for scenario 2, …; repetitions interleaved the same way). This spreads drift across gateways instead of penalising whichever runs last.

**Environment fingerprint** is captured at run start, per host:
- CPU model, core count, governor, turbo state
- kernel, OS, Docker version
- load average
- image digests
- git SHA and dirty flag

**Runtime.** A full matrix is ~90 supported cells × 2 profiles × 5 reps = ~900 repetitions of up to ~11 minutes each (boot, ceiling, warm-up, ~8 search steps × 20 s, up to 7 ladder rates × 60 s): **up to ~6 days** at these defaults. `bench.py plan` prints the estimate for any filter set before a run. `--resume` skips completed cells, so interrupted runs continue.

**Failure handling.** A container crash, loadgen failure, or timeout marks one cell and never aborts the run. Ctrl+C tears down every container labelled `featherbit-bench=1`.

## 7. Results and report

`results/<timestamp>-<topology>/`:

- `run.json` — fingerprint, parameters (SLO, durations, reps, connections, ladder), and per cell: status, every raw sample, aggregated values.
- `raw/` — unmodified tool output per step.
- `configs/` — a copy of every config used.
- `logs/` — container logs for `error` cells.

`bench.py report` writes a self-contained `report.html`, with inline SVG charts and no external requests:

- A header with the fingerprint. A **"NOT FOR PUBLICATION"** banner unless the run was `--publish`.
- A **summary grid**: scenario × gateway, max sustainable RPS as median (min–max), per profile. Non-result cells show their status and reason, never a blank or a zero.
- **Per scenario:**
  - latency-vs-rate curves (p50 and p99, log y, one line per gateway)
  - RPS/core bars
  - peak RSS
- An **overhead view**: each gateway's added p50 and p99 over the direct baseline, at the highest ladder rate every compared gateway sustained.
- A **methodology section** generated from the run's actual parameters, with links to every config.

## 8. Fairness

- **Tuning by the book.** Each gateway's `TUNING.md` lists every non-default setting and cites the vendor's own performance or production guidance for it. Baseline rules for all gateways:
  - access logs off
  - debug off
  - worker count = profile cores
  - keep-alive to the upstream enabled

  Featherbit gets no tuning a competitor couldn't get. Its debug mode stays off, and its `docker-compose.yaml` debug env vars are **not** reused.
- **Versions.** Each project's latest stable release at implementation time, pinned by digest and recorded in `run.json`.
- **Openness.** `bench/README.md` invites config PRs from vendors and readers. Before any public release, maintainers of the competing projects should be asked to review their configs. That is a manual step, noted in the README.

## 9. Testing the harness

- **Unit tests** (`bench/tests/`, stdlib `unittest`, run with `python -m unittest discover bench/tests`):
  - wrk / wrk2 / oha output parsers, against captured fixtures
  - bisection search, against a fake latency model
  - status and saturation classification
  - matrix expansion and filters
  - `--resume` bookkeeping
  - report rendering from a fixture `run.json`
- **Smoke test** on the laptop: `bench.py run --quick --gateways nginx,featherbit --scenarios core.proxy` — boot, probes, measurement and report end to end in a few minutes.
- **Config check**: `bench.py validate` over the full matrix proves every config actually works, cheaply.

## 10. Out of scope

- Cloud provisioning (the `remote` topology assumes the hosts exist).
- The docs-site benchmark page — it comes later, fed by a Linux `--publish` run's `run.json`.
- CI integration (a `validate` job is a natural follow-up).
- HTTP/3, WebSocket, L4 stream, large consumer counts, redis-backed rate limiting.
- Any Featherbit performance work the results motivate — each gets its own spec.
