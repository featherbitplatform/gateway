# Competitive benchmark suite

Measures Featherbit's data plane against Apache APISIX, Kong (OSS), Envoy, Tyk (OSS),
KrakenD (CE) and Traefik, with stock nginx as a zero-feature ceiling and a `direct`
(no gateway) baseline. Design: `docs/superpowers/specs/2026-09-26-benchmark-suite-design.md`.

## Quick start (local, indicative numbers only)

Requirements: Docker, Python >= 3.11. Nothing to pip-install.

```bash
npm --prefix ui ci && npm --prefix ui run build        # Featherbit's Dockerfile copies ui/dist
python bench/bench.py plan --quick                      # what would run, and a worst-case duration
python bench/bench.py validate                          # correctness probes for every cell
python bench/bench.py run --gateways featherbit,nginx,direct --scenarios 'core.*' --quick
python bench/bench.py report bench/results/<run-dir>
```

## Commands

| Command | What it does |
|---|---|
| `plan` | Lists the cells a selection expands to, and the worst-case duration. |
| `validate` | Boots every supported cell once and runs its probes. Exit 1 if any fail. |
| `run` | The measurement. Filters: `--gateways`, `--scenarios` (globs), `--profiles 1,4`, `--reps`, `--quick`. `--image featherbit=featherbit/featherbit:<tag>` benchmarks a released image. |
| `run --resume <dir>` | Continues an interrupted run with its stored selection. |
| `run --publish` | Refuses to start unless the topology is `remote`, uses host networking, and every host runs the `performance` CPU governor. |
| `report <dir>` | Writes `<dir>/report.html`: self-contained, no network access needed. |
| `clean` | Removes every container labelled `featherbit-bench=1`. |

## What a result means

Each **cell** is one (gateway, scenario, gateway core count). Per repetition:

1. Boot the upstream, the load generator and the gateway on disjoint CPUs.
2. Run the **correctness probes** (key rejected without a key, 429 on the probe route, headers really rewritten, TLS 1.3 negotiated, and so on). A failing probe makes the cell **invalid**, and it is never measured.
3. Flood for a rough ceiling, then warm up.
4. **Max sustainable rate**: bisect fixed-rate wrk2 steps for the highest rate with **p99 <= 10 ms** and < 0.1% errors.
5. **Latency ladder**: 60 s at 1k, 5k, 10k, 25k, … req/s up to that rate. It gives p50/p90/p99/p99.9, corrected for coordinated omission.
6. Sample CPU and memory throughout. If the load generator or upstream went above 85% CPU, the result is marked **>= X** (the gateway sustained at least X), not X.

There are 5 repetitions with a full restart each, interleaved across gateways. The report shows the median (min–max).

Statuses: `ok`, `invalid` (probes failed), `error` (boot/tool failure; see `logs/`), `n/a` (not natively supported in the gateway's open-source edition; the reason is shown), `loadgen-bound` or `upstream-bound`.

**Keep the machine idle.** Anything else using CPU during a run (a build, a browser tab
playing video, an IDE indexing) shows up as gateway latency: a concurrent `cargo build` pushed
even the no-gateway baseline past the 10 ms SLO in testing. The host load average at start is
recorded in `run.json`.

**Runtime.** The full matrix at default parameters is ~900 repetitions of up to ~11 minutes each, so **up to ~6 days**. `plan` prints the estimate for any selection. Use filters, `--reps`, or `--quick` (iteration only; never publish `--quick` numbers).

## Fairness rules

- Every competitor runs natively configured, open-source edition only, tuned per its vendor's own guidance. See `gateways/<name>/TUNING.md`, which lists every non-default setting and its source.
- For everyone: access logs off, debug off, workers = pinned cores, upstream keep-alive on, the same TLS cert and protocol, and the same upstream.
- A scenario a gateway can't do natively is `n/a`, never approximated with sidecars or external services.
- Images are pinned by digest. `run.json` records the digests, the host fingerprint and the git SHA, and `configs/` holds the exact rendered configs.

**Config PRs are welcome**, from vendors especially. Before any public release, we ask each project's maintainers to review their adapter.

## Local vs publishable

`topology/local.toml` runs everything on one machine with cpusets. On Docker Desktop that's a WSL2/Hyper-V VM, where the load generator shares the machine and frequency scaling is uncontrolled. Those numbers are **indicative only**, and the report carries a banner saying so.

Publishable numbers need `topology/remote.example.toml`: three Linux hosts (load generator, gateway, upstream) reached through docker contexts, with host networking and the `performance` governor. Run with `--publish`.

## Load tools

`loadgen/Dockerfile` pins wrk 4.2.0, wrk2 and oha. wrk2 carries **one local patch**: its
`time_us()` reads `CLOCK_MONOTONIC` instead of `gettimeofday()`. Stock wrk2 aborts on an
HdrHistogram assertion whenever the wall clock steps backwards mid-request (routine under
Docker Desktop/WSL2 host time sync), because the latency it records is unsigned.

## Adding a gateway

Create `gateways/<name>/gateway.toml` (see the existing ones), one native config directory per distinct config, and a `TUNING.md`. Every scenario must appear under `[configs]` or `[na]` (with a reason). Configs use `@@TOKEN@@` placeholders (`benchlib/templating.py`, `benchlib/tokens.py`). `config_dir` must be a path whose parent already exists in the image (`docker cp` creates only
the last component). Then run `python bench/bench.py validate --gateways <name>`.

## Tests

```bash
python -m unittest discover -s bench/tests -t bench -v
```
