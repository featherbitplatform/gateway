# Competitive Benchmark Suite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A reproducible harness under `bench/` that benchmarks Featherbit against APISIX, Kong, Envoy, Tyk, KrakenD, Traefik and a plain-nginx ceiling, producing a `run.json` plus a self-contained HTML report.

**Architecture:** A stdlib-only Python orchestrator (`bench/bench.py` → `bench/benchlib/`) drives pinned containers through the Docker CLI. Every gateway is a pure-data adapter (`bench/gateways/<name>/gateway.toml` + hand-written native configs with `@@TOKEN@@` placeholders). A topology file decides placement: one host with cpusets (`local`), or three Linux hosts over docker contexts (`remote`). Pure logic (parsers, bisection, classification, templating, matrix, store) is unit-tested; the docker-facing driver is checked by probe validation and smoke runs.

**Tech Stack:** Python ≥ 3.11 (stdlib only: `tomllib`, `unittest`, `http.client`, `ssl`, `subprocess`), Docker (Desktop on Windows, Engine on Linux), wrk 4.2.0, wrk2, oha, nginx as the upstream, inline SVG + vanilla JS for the report.

**Spec:** `docs/superpowers/specs/2026-09-26-benchmark-suite-design.md` — read it before starting any task.

## Global Constraints

- Everything lives under `bench/`; nothing in `src/`, `Cargo.toml` or CI changes. The one exception is the root `.gitignore` (Task 1) and `.gitleaks.toml` (Task 17).
- Python: stdlib only, ≥ 3.11. No `pip install`. Tests run with `python -m unittest discover -s bench/tests -t bench -v` from the repo root.
- The orchestrator has **no per-gateway code paths**. Gateway differences live only in `bench/gateways/<name>/`.
- Container names: `fbb-upstream`, `fbb-gateway`, `fbb-loadgen`, `fbb-dep-<name>`. Every container gets the label `featherbit-bench=1`.
- Fixed ports: the upstream serves on `8080`. Gateways listen on `8000` (plain) and `8443` (TLS). On the `local` topology these are published to host ports `18000`, `18443` and `18080` (upstream).
- Fixed wire contract, identical for every gateway:
  - request paths `/bench/1k`, `/bench/64k`, `/bench/1m`
  - `routes-1k` paths `/r000/…` to `/r999/…`
  - rate-limit probe route `/limited/…`
  - API key header `apikey: bench-api-key`
  - JWT in `Authorization: Bearer <HS256 token>`
  - request headers added: `X-Bench-Added: 1`, `X-Bench-Added2: 2`
  - response header removed: `X-Bench-Remove`
  - script: reads `X-Bench-In` and sets request header `X-Bench-Script` = upper(value) + "-" + length, so `abc` gives `ABC-3`
- SLO and parameters come only from `bench/scenarios/scenarios.toml` `[params]`: p99 ≤ 10 ms, error rate < 0.1%, 64 connections, 5 reps, and so on. Don't hard-code them elsewhere.
- Text printed to the console is ASCII only (Windows consoles are cp1252). Use `>=` and `~`, never `≥` or `≈`. The HTML report is UTF-8 and may use any character.
- File names never contain `|` or `:` (illegal on Windows). A cell key `gw|scenario|profile` becomes `gw__scenario__profile` on disk.
- Image pins: every third-party image is pinned as `name:tag@sha256:digest`. The tag is each project's **latest stable release at implementation time**, resolved in the owning task with:
  ```bash
  docker pull <repo>:<tag>
  docker image inspect --format '{{index .RepoDigests 0}}' <repo>:<tag>
  ```
  Write the tag and digest you resolved; record the date in that gateway's `TUNING.md`.
- **Commits:** Francesco commits only on their explicit go-ahead (auto-memory "delivery-workflow"). Each task ends with a *Commit* step that shows the message to use; run it only if commits were authorized for this execution. Otherwise leave the changes uncommitted and move on. Messages follow Conventional Commits and carry **no** `Co-Authored-By`, session link or Claude footer (project `CLAUDE.md` and memory override the harness default).
- Work on branch `feature/benchmark-suite` (already created off `develop`).
- After the last code task, run `graphify update .` (project `CLAUDE.md`).

## Review Focus

1. **An interrupted run (Ctrl+C, crash, reboot) resumed with `--resume`.** Completed repetitions must be skipped and never duplicated, and a half-written `run.json` must never be left behind. Pinned by `test_resume_skips_done_reps` and `test_save_is_atomic` (Task 4).
2. **Stale `fbb-*` containers left by a previous crashed run.** Boot must remove them before creating new ones, not fail with "name already in use". Pinned by `test_boot_removes_stale_containers_first` (Task 9).
3. **A target that answers nothing:** connection refused, a zero-request tool output, a zero flood ceiling. The result must be a cell `error`, or a `no-ceiling` flag, never a division by zero or a crash of the whole run. Pinned by `test_parse_wrk_unreachable_raises`, `test_no_ceiling` (Tasks 2, 3) and `test_load_error_marks_rep_error` (Task 8).
4. **A Windows checkout with CRLF line endings.** Rendered configs must come out LF-only. Otherwise nginx/YAML/Lua inside Linux containers can misparse. Pinned by `test_render_dir_normalizes_crlf` (Task 5), plus `bench/.gitattributes` (Task 1).
5. **Windows paths handed to `docker cp`** (`C:\Users\...`). The copy source must be a form the Docker CLI accepts on Windows (`C:/Users/.../.`). Pinned by `test_cp_into_uses_posix_source` (Task 6).

## File Structure

```
bench/
  .gitattributes              # LF for every text file (Task 1)
  README.md                   # methodology + usage (Task 17)
  bench.py                    # CLI shim -> benchlib.cli.main (Task 9)
  benchlib/
    __init__.py
    config.py                 # TOML loading/validation: Params, Scenario, Probe, Gateway, Topology (Task 1)
    parsers.py                # wrk / wrk2 / oha output -> LoadResult (Task 2)
    search.py                 # Step, bisection, ladder selection (Task 3)
    classify.py               # statuses, saturation, summaries (Task 3)
    matrix.py                 # Cell, selection globs, expansion, scheduling (Task 4)
    results.py                # RunStore: run dir, atomic run.json, resume, log files (Task 4)
    templating.py             # @@TOKEN@@ / @@REPEAT@@ / @@EACH_n@@ rendering (Task 5)
    tokens.py                 # API key, JWT, JWKS, static template values (Task 5)
    docker.py                 # Docker CLI wrapper, ContainerSpec (Task 6)
    sampler.py                # docker stats streaming sampler (Task 6)
    probes.py                 # health wait, HTTP/TLS/ALPN probes (Task 7)
    certs.py                  # self-signed ECDSA cert via the loadgen image (Task 8)
    runner.py                 # CellRunner: the per-repetition procedure (Task 8)
    fingerprint.py            # host fingerprint, git info, publish blockers (Task 8)
    driver.py                 # DockerDriver: boots containers, runs tools (Task 9)
    cli.py                    # plan / run / validate / report / clean (Task 9)
  report/
    __init__.py
    render.py                 # run.json -> report.html (Task 16)
  scenarios/scenarios.toml    # params + 12 scenarios + probes (Task 1)
  topology/local.toml, remote.example.toml (Task 1)
  upstream/Dockerfile, nginx.conf (Task 7)
  loadgen/Dockerfile (Task 7)
  gateways/
    direct/ nginx/ (Task 9)   featherbit/ (Task 10)   apisix/ (Task 11)   kong/ (Task 12)
    envoy/ (Task 13)          tyk/ (Task 14)           krakend/ + traefik/ (Task 15)
  tests/
    __init__.py, test_config.py, test_parsers.py, test_search.py, test_matrix_results.py,
    test_templating.py, test_docker.py, test_probes.py, test_runner.py, test_driver.py,
    test_cli.py, test_report.py, fixtures/ (real tool output, Task 7)
  results/                    # gitignored
```

---
### Task 1: Scaffold, scenario catalog, topologies, config loading

**Files:**
- Create: `bench/.gitattributes`, `bench/benchlib/__init__.py`, `bench/benchlib/config.py`, `bench/scenarios/scenarios.toml`, `bench/topology/local.toml`, `bench/topology/remote.example.toml`, `bench/tests/__init__.py`, `bench/tests/test_config.py`
- Modify: `.gitignore` (append two lines)

**Interfaces:**
- Consumes: nothing.
- Produces (`benchlib.config`):
  - `ConfigError(Exception)`
  - `Params` (frozen dataclass, fields listed below) with `.quick() -> Params`; `params_from_dict(d: dict) -> Params`
  - `Probe(path: str, kind: str = "http", headers: dict, expect_status: tuple[int,...], expect_headers: dict[str,str], expect_absent: tuple[str,...], expect_body_bytes: int|None)`
  - `Scenario(id, family, tool, scheme, path, body_bytes, headers: dict, probes: tuple[Probe,...])`
  - `Dependency(name, image, port, command: tuple)`, `SetupCall(method, path, scenarios: tuple, headers: dict, body_file: str|None, expect_status: tuple)`
  - `Gateway(name, kind, dir: Path, configs: dict[str,str], na: dict[str,str], image, build: Path|None, build_args, build_requires: tuple, build_hint, config_dir, common: str|None, entrypoint: str|None, command: tuple, workdir: str|None, env: dict, ports: dict[str,int], dependencies: tuple[Dependency,...], setup: tuple[SetupCall,...])` with `.supports(scenario_id) -> bool`
  - `Role(address: str, context: str|None)`, `Profile(cores: int, gateway: str, upstream: str, loadgen: str)`, `Topology(name, kind, network, roles: dict[str,Role], profiles: dict[int,Profile], published_ports: dict[str,int], probe_host: str)`
  - `ROLES = ("loadgen", "gateway", "upstream")`
  - `load_scenarios(path) -> tuple[Params, dict[str, Scenario]]` (dict keeps file order)
  - `load_gateway(directory, scenarios) -> Gateway`, `load_gateways(root, scenarios) -> dict[str, Gateway]`
  - `load_topology(path) -> Topology`, `cpuset_members(spec) -> set[int]`, `cpuset_size(spec) -> int`

- [ ] **Step 1: Scaffold and ignore rules**

`bench/.gitattributes`:
```
# Configs are copied into Linux containers: keep LF even on Windows checkouts.
* text=auto eol=lf
```

`bench/benchlib/__init__.py` and `bench/tests/__init__.py`: empty files.

Append to the root `.gitignore`:
```
# Benchmark suite run output (bench/README.md)
bench/results/
```

- [ ] **Step 2: Write the scenario catalog**

`bench/scenarios/scenarios.toml`:
```toml
# Scenario catalog for the competitive benchmark (spec §5, §6).
# @@API_KEY@@ / @@JWT@@ / @@JWT_TAMPERED@@ are filled in at run time by
# benchlib/tokens.py. Probes run before any measurement; a failing probe marks
# the cell `invalid` and it is never measured.

[params]
slo_p99_ms = 10.0
max_error_rate = 0.001
connections = 64
ceiling_seconds = 10
warmup_seconds = 30
warmup_fraction = 0.2
search_step_seconds = 20
search_low = 0.1
search_high = 1.2
search_tolerance = 0.02
ladder_seconds = 60
ladder = [1000, 5000, 10000, 25000, 50000, 100000, 200000]
reps = 5
saturation_threshold = 0.85
boot_timeout_seconds = 60

[[scenario]]
id = "core.proxy"
family = "core"
tool = "wrk2"
scheme = "http"
path = "/bench/1k"
body_bytes = 1024
  [[scenario.probe]]
  expect_headers = { "x-bench-remove" = "1" }
  expect_body_bytes = 1024

[[scenario]]
id = "core.routes-1k"
family = "core"
tool = "wrk2"
scheme = "http"
path = "/r999/1k"
body_bytes = 1024
  [[scenario.probe]]
  expect_body_bytes = 1024
  [[scenario.probe]]
  path = "/r000/1k"
  expect_body_bytes = 1024
  [[scenario.probe]]
  path = "/r500/1k"
  expect_body_bytes = 1024
  [[scenario.probe]]
  path = "/nomatch/1k"
  expect_status = [404]

[[scenario]]
id = "plugin.key-auth"
family = "plugins"
tool = "wrk2"
scheme = "http"
path = "/bench/1k"
body_bytes = 1024
headers = { "apikey" = "@@API_KEY@@" }
  [[scenario.probe]]
  headers = { "apikey" = "@@API_KEY@@" }
  expect_body_bytes = 1024
  [[scenario.probe]]
  expect_status = [401, 403]
  [[scenario.probe]]
  headers = { "apikey" = "wrong-key" }
  expect_status = [401, 403]

[[scenario]]
id = "plugin.jwt"
family = "plugins"
tool = "wrk2"
scheme = "http"
path = "/bench/1k"
body_bytes = 1024
headers = { "Authorization" = "Bearer @@JWT@@" }
  [[scenario.probe]]
  headers = { "Authorization" = "Bearer @@JWT@@" }
  expect_body_bytes = 1024
  [[scenario.probe]]
  expect_status = [401, 403]
  [[scenario.probe]]
  headers = { "Authorization" = "Bearer @@JWT_TAMPERED@@" }
  expect_status = [401, 403]

[[scenario]]
id = "plugin.rate-limit"
family = "plugins"
tool = "wrk2"
scheme = "http"
path = "/bench/1k"
body_bytes = 1024
  [[scenario.probe]]
  expect_body_bytes = 1024
  [[scenario.probe]]
  path = "/limited/1k"
  expect_body_bytes = 1024
  [[scenario.probe]]
  path = "/limited/1k"
  expect_status = [429]

[[scenario]]
id = "plugin.header-rewrite"
family = "plugins"
tool = "wrk2"
scheme = "http"
path = "/bench/1k"
body_bytes = 1024
  [[scenario.probe]]
  expect_headers = { "x-bench-echo-added" = "1", "x-bench-echo-added2" = "2" }
  expect_absent = ["x-bench-remove"]
  expect_body_bytes = 1024

[[scenario]]
id = "plugin.chain"
family = "plugins"
tool = "wrk2"
scheme = "http"
path = "/bench/1k"
body_bytes = 1024
headers = { "apikey" = "@@API_KEY@@" }
  [[scenario.probe]]
  headers = { "apikey" = "@@API_KEY@@" }
  expect_headers = { "x-bench-echo-added" = "1", "x-bench-echo-added2" = "2" }
  expect_absent = ["x-bench-remove"]
  expect_body_bytes = 1024
  [[scenario.probe]]
  expect_status = [401, 403]
  [[scenario.probe]]
  path = "/limited/1k"
  headers = { "apikey" = "@@API_KEY@@" }
  expect_body_bytes = 1024
  [[scenario.probe]]
  path = "/limited/1k"
  headers = { "apikey" = "@@API_KEY@@" }
  expect_status = [429]

[[scenario]]
id = "payload.64k"
family = "payload"
tool = "wrk2"
scheme = "http"
path = "/bench/64k"
body_bytes = 65536
  [[scenario.probe]]
  expect_body_bytes = 65536

[[scenario]]
id = "payload.1m"
family = "payload"
tool = "wrk2"
scheme = "http"
path = "/bench/1m"
body_bytes = 1048576
  [[scenario.probe]]
  expect_body_bytes = 1048576

[[scenario]]
id = "proto.tls"
family = "protocol"
tool = "wrk2"
scheme = "https"
path = "/bench/1k"
body_bytes = 1024
  [[scenario.probe]]
  kind = "tls13"
  [[scenario.probe]]
  expect_body_bytes = 1024

[[scenario]]
id = "proto.h2"
family = "protocol"
tool = "oha"
scheme = "https"
path = "/bench/1k"
body_bytes = 1024
  [[scenario.probe]]
  kind = "alpn-h2"
  [[scenario.probe]]
  expect_body_bytes = 1024

[[scenario]]
id = "script.header"
family = "scripting"
tool = "wrk2"
scheme = "http"
path = "/bench/1k"
body_bytes = 1024
headers = { "X-Bench-In" = "abc" }
  [[scenario.probe]]
  headers = { "X-Bench-In" = "abc" }
  expect_headers = { "x-bench-echo-script" = "ABC-3" }
  expect_body_bytes = 1024
```

- [ ] **Step 3: Write the topologies**

`bench/topology/local.toml`:
```toml
# One host (laptop or a single Linux box). Each role is pinned to disjoint CPUs
# with `docker --cpuset-cpus`; on Docker Desktop the ids are the WSL2 VM's CPUs.
# Results from this topology are never publishable (bench/README.md).
kind = "local"
network = "featherbit-bench"

[published_ports]   # host ports the orchestrator probes through
plain = 18000
tls = 18443
upstream = 18080

[profiles.1]
gateway = "0"
upstream = "6-7"
loadgen = "1-5"

[profiles.4]
gateway = "0-3"
upstream = "4-5"
loadgen = "6-7"
```

`bench/topology/remote.example.toml`:
```toml
# Three Linux hosts, one per role, reached through docker contexts, e.g.
#   docker context create bench-gateway --docker host=ssh://bench@10.0.0.11
# All containers use host networking. Copy to remote.toml and edit. This is the
# only topology `bench.py run --publish` accepts.
kind = "remote"

[roles.loadgen]
context = "bench-loadgen"
address = "10.0.0.10"

[roles.gateway]
context = "bench-gateway"
address = "10.0.0.11"

[roles.upstream]
context = "bench-upstream"
address = "10.0.0.12"

[profiles.1]
gateway = "0"
upstream = "0-15"
loadgen = "0-15"

[profiles.4]
gateway = "0-3"
upstream = "0-15"
loadgen = "0-15"
```

- [ ] **Step 4: Write the failing tests**

`bench/tests/test_config.py`:
```python
import tempfile
import textwrap
import unittest
from pathlib import Path

from benchlib.config import (
    ConfigError, Params, cpuset_members, cpuset_size, load_gateway, load_gateways,
    load_scenarios, load_topology, params_from_dict,
)

BENCH = Path(__file__).resolve().parent.parent
SCENARIO_IDS = [
    "core.proxy", "core.routes-1k", "plugin.key-auth", "plugin.jwt", "plugin.rate-limit",
    "plugin.header-rewrite", "plugin.chain", "payload.64k", "payload.1m", "proto.tls",
    "proto.h2", "script.header",
]


def write(root: Path, rel: str, text: str) -> Path:
    p = root / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(textwrap.dedent(text), encoding="utf-8")
    return p


MINI_SCENARIOS = """
    [[scenario]]
    id = "a"
    family = "core"
    tool = "wrk2"
    scheme = "http"
    path = "/bench/1k"
    body_bytes = 1024
      [[scenario.probe]]
      expect_body_bytes = 1024

    [[scenario]]
    id = "b"
    family = "protocol"
    tool = "wrk2"
    scheme = "https"
    path = "/bench/1k"
    body_bytes = 1024
      [[scenario.probe]]
      kind = "tls13"
"""


class ScenarioTests(unittest.TestCase):
    def test_repo_catalog_loads_all_twelve_in_order(self):
        params, scenarios = load_scenarios(BENCH / "scenarios" / "scenarios.toml")
        self.assertEqual(list(scenarios), SCENARIO_IDS)
        self.assertEqual(params, Params())
        self.assertEqual(scenarios["proto.h2"].tool, "oha")
        self.assertEqual(scenarios["plugin.rate-limit"].probes[2].expect_status, (429,))
        self.assertEqual(scenarios["plugin.header-rewrite"].probes[0].expect_absent, ("x-bench-remove",))

    def test_probe_path_defaults_to_scenario_path(self):
        _, scenarios = load_scenarios(BENCH / "scenarios" / "scenarios.toml")
        self.assertEqual(scenarios["core.proxy"].probes[0].path, "/bench/1k")

    def test_quick_params(self):
        q = Params().quick()
        self.assertEqual((q.reps, q.search_step_seconds, q.ladder), (1, 5, (1000, 5000, 10000)))

    def test_params_round_trip(self):
        import dataclasses, json
        p = Params()
        self.assertEqual(params_from_dict(json.loads(json.dumps(dataclasses.asdict(p)))), p)

    def test_unknown_param_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            f = write(Path(d), "s.toml", "[params]\nslo = 5\n" + MINI_SCENARIOS)
            with self.assertRaisesRegex(ConfigError, "unknown params"):
                load_scenarios(f)

    def test_scenario_without_probe_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            f = write(Path(d), "s.toml", """
                [[scenario]]
                id = "a"
                family = "core"
                tool = "wrk2"
                scheme = "http"
                path = "/x"
                body_bytes = 1
            """)
            with self.assertRaisesRegex(ConfigError, "at least one probe"):
                load_scenarios(f)

    def test_bad_tool_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            f = write(Path(d), "s.toml", MINI_SCENARIOS.replace('tool = "wrk2"', 'tool = "ab"', 1))
            with self.assertRaisesRegex(ConfigError, "tool"):
                load_scenarios(f)

    def test_missing_key_is_config_error(self):
        with tempfile.TemporaryDirectory() as d:
            f = write(Path(d), "s.toml", MINI_SCENARIOS.replace('family = "core"', "", 1))
            with self.assertRaisesRegex(ConfigError, "family"):
                load_scenarios(f)


class GatewayTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        f = write(self.root, "s.toml", MINI_SCENARIOS)
        _, self.scenarios = load_scenarios(f)

    def tearDown(self):
        self.tmp.cleanup()

    def gw(self, body: str, dirs=("proxy",)):
        d = self.root / "gateways" / "g"
        for sub in dirs:
            (d / sub).mkdir(parents=True, exist_ok=True)
        write(d, "gateway.toml", body)
        return d

    def test_valid_container_gateway(self):
        d = self.gw("""
            name = "g"
            image = "x:1"
            config_dir = "/etc/g"
            ports = { plain = 8000, tls = 8443 }
            [env]
            WORKERS = "@@CORES@@"
            [configs]
            a = "proxy"
            b = "proxy"
        """)
        g = load_gateway(d, self.scenarios)
        self.assertTrue(g.supports("a"))
        self.assertEqual(g.env, {"WORKERS": "@@CORES@@"})
        self.assertEqual(g.kind, "container")

    def test_every_scenario_must_be_accounted_for(self):
        d = self.gw("""
            name = "g"
            image = "x:1"
            config_dir = "/etc/g"
            ports = { plain = 8000 }
            [configs]
            a = "proxy"
        """)
        with self.assertRaisesRegex(ConfigError, "neither a config nor an n/a reason.*'b'"):
            load_gateway(d, self.scenarios)

    def test_config_and_na_both_rejected(self):
        d = self.gw("""
            name = "g"
            image = "x:1"
            config_dir = "/etc/g"
            ports = { plain = 8000, tls = 8443 }
            [configs]
            a = "proxy"
            b = "proxy"
            [na]
            a = "nope"
        """)
        with self.assertRaisesRegex(ConfigError, "both"):
            load_gateway(d, self.scenarios)

    def test_missing_config_dir_rejected(self):
        d = self.gw("""
            name = "g"
            image = "x:1"
            config_dir = "/etc/g"
            ports = { plain = 8000, tls = 8443 }
            [configs]
            a = "proxy"
            b = "tls"
        """)
        with self.assertRaisesRegex(ConfigError, "'tls'"):
            load_gateway(d, self.scenarios)

    def test_https_scenario_needs_tls_port(self):
        d = self.gw("""
            name = "g"
            image = "x:1"
            config_dir = "/etc/g"
            ports = { plain = 8000 }
            [configs]
            a = "proxy"
            b = "proxy"
        """)
        with self.assertRaisesRegex(ConfigError, "tls"):
            load_gateway(d, self.scenarios)

    def test_name_must_match_directory(self):
        d = self.gw("""
            name = "other"
            kind = "direct"
            [configs]
            a = ""
            [na]
            b = "x"
        """)
        with self.assertRaisesRegex(ConfigError, "directory name"):
            load_gateway(d, self.scenarios)

    def test_direct_gateway_needs_no_image(self):
        d = self.gw("""
            name = "g"
            kind = "direct"
            [configs]
            a = ""
            [na]
            b = "Baseline covers plain proxying only."
        """, dirs=())
        g = load_gateway(d, self.scenarios)
        self.assertEqual((g.kind, g.image), ("direct", ""))

    def test_setup_for_unsupported_scenario_rejected(self):
        d = self.gw("""
            name = "g"
            image = "x:1"
            config_dir = "/etc/g"
            ports = { plain = 8000 }
            [configs]
            a = "proxy"
            [na]
            b = "no tls"
            [[setup]]
            method = "post"
            path = "/keys"
            scenarios = ["b"]
        """)
        with self.assertRaisesRegex(ConfigError, "unsupported"):
            load_gateway(d, self.scenarios)

    def test_load_gateways_skips_dirs_without_toml(self):
        self.gw("""
            name = "g"
            kind = "direct"
            [configs]
            a = ""
            [na]
            b = "x"
        """, dirs=())
        (self.root / "gateways" / "notes").mkdir()
        self.assertEqual(list(load_gateways(self.root / "gateways", self.scenarios)), ["g"])


class RepoGatewayTests(unittest.TestCase):
    def test_every_committed_gateway_loads(self):
        _, scenarios = load_scenarios(BENCH / "scenarios" / "scenarios.toml")
        root = BENCH / "gateways"
        if root.is_dir():
            load_gateways(root, scenarios)  # raises on any drift


class TopologyTests(unittest.TestCase):
    def test_cpusets(self):
        self.assertEqual(cpuset_members("0,2-3"), {0, 2, 3})
        self.assertEqual(cpuset_size("1-5"), 5)
        for bad in ("", "a", "3-1", "1,,2"):
            with self.assertRaises(ConfigError):
                cpuset_members(bad)

    def test_repo_local_topology(self):
        t = load_topology(BENCH / "topology" / "local.toml")
        self.assertEqual((t.kind, t.network, t.probe_host), ("local", "featherbit-bench", "127.0.0.1"))
        self.assertEqual(sorted(t.profiles), [1, 4])
        self.assertEqual(t.roles["upstream"].address, "upstream")
        self.assertEqual(t.published_ports["upstream"], 18080)

    def test_repo_remote_example(self):
        t = load_topology(BENCH / "topology" / "remote.example.toml")
        self.assertEqual((t.kind, t.network), ("remote", "host"))
        self.assertEqual(t.roles["gateway"].context, "bench-gateway")
        self.assertEqual(t.probe_host, "10.0.0.11")

    def test_local_overlap_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            f = write(Path(d), "t.toml", """
                kind = "local"
                [profiles.1]
                gateway = "0"
                upstream = "0-1"
                loadgen = "2-3"
            """)
            with self.assertRaisesRegex(ConfigError, "overlap"):
                load_topology(f)

    def test_gateway_cpuset_must_match_core_count(self):
        with tempfile.TemporaryDirectory() as d:
            f = write(Path(d), "t.toml", """
                kind = "local"
                [profiles.2]
                gateway = "0"
                upstream = "1"
                loadgen = "2"
            """)
            with self.assertRaisesRegex(ConfigError, "expected 2"):
                load_topology(f)

    def test_remote_needs_every_role(self):
        with tempfile.TemporaryDirectory() as d:
            f = write(Path(d), "t.toml", """
                kind = "remote"
                [roles.gateway]
                context = "g"
                address = "10.0.0.1"
                [profiles.1]
                gateway = "0"
                upstream = "0"
                loadgen = "0"
            """)
            with self.assertRaisesRegex(ConfigError, "loadgen"):
                load_topology(f)
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `ModuleNotFoundError: No module named 'benchlib.config'`.

- [ ] **Step 6: Implement `benchlib/config.py`**

```python
"""Load and validate the benchmark's TOML inputs: scenarios, gateway adapters, topologies."""
from __future__ import annotations

import dataclasses
import tomllib
from dataclasses import dataclass, field, replace
from pathlib import Path


class ConfigError(Exception):
    """An input file is missing, malformed or inconsistent."""


ROLES = ("loadgen", "gateway", "upstream")
TOOLS = {"wrk2", "oha"}
SCHEMES = {"http", "https"}
PROBE_KINDS = {"http", "tls13", "alpn-h2"}


@dataclass(frozen=True)
class Params:
    slo_p99_ms: float = 10.0
    max_error_rate: float = 0.001
    connections: int = 64
    ceiling_seconds: int = 10
    warmup_seconds: int = 30
    warmup_fraction: float = 0.2
    search_step_seconds: int = 20
    search_low: float = 0.1
    search_high: float = 1.2
    search_tolerance: float = 0.02
    ladder_seconds: int = 60
    ladder: tuple[int, ...] = (1000, 5000, 10000, 25000, 50000, 100000, 200000)
    reps: int = 5
    saturation_threshold: float = 0.85
    boot_timeout_seconds: int = 60

    def quick(self) -> "Params":
        """Short everything, one repetition: for local iteration, never for results."""
        return replace(self, reps=1, ceiling_seconds=5, warmup_seconds=5,
                       search_step_seconds=5, ladder_seconds=5, ladder=self.ladder[:3])


def params_from_dict(d: dict) -> Params:
    known = {f.name for f in dataclasses.fields(Params)}
    unknown = set(d) - known
    if unknown:
        raise ConfigError(f"unknown params {sorted(unknown)}")
    if "ladder" in d:
        d = {**d, "ladder": tuple(int(x) for x in d["ladder"])}
    return Params(**d)


@dataclass(frozen=True)
class Probe:
    path: str
    kind: str = "http"
    headers: dict[str, str] = field(default_factory=dict)
    expect_status: tuple[int, ...] = (200,)
    expect_headers: dict[str, str] = field(default_factory=dict)
    expect_absent: tuple[str, ...] = ()
    expect_body_bytes: int | None = None


@dataclass(frozen=True)
class Scenario:
    id: str
    family: str
    tool: str
    scheme: str
    path: str
    body_bytes: int
    headers: dict[str, str] = field(default_factory=dict)
    probes: tuple[Probe, ...] = ()


@dataclass(frozen=True)
class Dependency:
    name: str
    image: str
    port: int
    command: tuple[str, ...] = ()


@dataclass(frozen=True)
class SetupCall:
    method: str
    path: str
    scenarios: tuple[str, ...]
    headers: dict[str, str] = field(default_factory=dict)
    body_file: str | None = None
    expect_status: tuple[int, ...] = (200,)


@dataclass(frozen=True)
class Gateway:
    name: str
    kind: str
    dir: Path
    configs: dict[str, str]
    na: dict[str, str]
    image: str = ""
    build: Path | None = None
    build_args: dict[str, str] = field(default_factory=dict)
    build_requires: tuple[str, ...] = ()
    build_hint: str = ""
    config_dir: str = ""
    common: str | None = None
    entrypoint: str | None = None
    command: tuple[str, ...] = ()
    workdir: str | None = None
    env: dict[str, str] = field(default_factory=dict)
    ports: dict[str, int] = field(default_factory=dict)
    dependencies: tuple[Dependency, ...] = ()
    setup: tuple[SetupCall, ...] = ()

    def supports(self, scenario_id: str) -> bool:
        return scenario_id in self.configs


@dataclass(frozen=True)
class Role:
    address: str
    context: str | None = None


@dataclass(frozen=True)
class Profile:
    cores: int
    gateway: str
    upstream: str
    loadgen: str


@dataclass(frozen=True)
class Topology:
    name: str
    kind: str
    network: str
    roles: dict[str, Role]
    profiles: dict[int, Profile]
    published_ports: dict[str, int] = field(default_factory=dict)
    probe_host: str = ""


def _read(path: Path) -> dict:
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except FileNotFoundError:
        raise ConfigError(f"{path}: file not found") from None
    except tomllib.TOMLDecodeError as e:
        raise ConfigError(f"{path}: {e}") from None


def _probe(pr: dict, default_path: str, where: str) -> Probe:
    kind = pr.get("kind", "http")
    if kind not in PROBE_KINDS:
        raise ConfigError(f"{where}: probe kind must be one of {sorted(PROBE_KINDS)}")
    return Probe(
        path=pr.get("path", default_path),
        kind=kind,
        headers={str(k): str(v) for k, v in pr.get("headers", {}).items()},
        expect_status=tuple(int(s) for s in pr.get("expect_status", [200])),
        expect_headers={str(k).lower(): str(v) for k, v in pr.get("expect_headers", {}).items()},
        expect_absent=tuple(str(h).lower() for h in pr.get("expect_absent", [])),
        expect_body_bytes=pr.get("expect_body_bytes"),
    )


def load_scenarios(path: Path) -> tuple[Params, dict[str, Scenario]]:
    raw = _read(path)
    try:
        params = params_from_dict(raw.get("params", {}))
    except ConfigError as e:
        raise ConfigError(f"{path}: {e}") from None
    scenarios: dict[str, Scenario] = {}
    for s in raw.get("scenario", []):
        sid = s.get("id")
        if not sid:
            raise ConfigError(f"{path}: scenario without id")
        where = f"{path}: scenario {sid!r}"
        if sid in scenarios:
            raise ConfigError(f"{where}: duplicate id")
        try:
            if s["tool"] not in TOOLS:
                raise ConfigError(f"{where}: tool must be one of {sorted(TOOLS)}")
            if s["scheme"] not in SCHEMES:
                raise ConfigError(f"{where}: scheme must be one of {sorted(SCHEMES)}")
            probes = tuple(_probe(pr, s["path"], where) for pr in s.get("probe", []))
            if not probes:
                raise ConfigError(f"{where}: at least one probe is required")
            scenarios[sid] = Scenario(
                id=sid, family=s["family"], tool=s["tool"], scheme=s["scheme"], path=s["path"],
                body_bytes=int(s["body_bytes"]),
                headers={str(k): str(v) for k, v in s.get("headers", {}).items()},
                probes=probes,
            )
        except KeyError as e:
            raise ConfigError(f"{where}: missing key {e}") from None
    if not scenarios:
        raise ConfigError(f"{path}: no scenarios")
    return params, scenarios


def load_gateway(directory: Path, scenarios: dict[str, Scenario]) -> Gateway:
    path = directory / "gateway.toml"
    raw = _read(path)
    where = str(path)
    name = raw.get("name", directory.name)
    if name != directory.name:
        raise ConfigError(f"{where}: name {name!r} must match the directory name {directory.name!r}")
    kind = raw.get("kind", "container")
    if kind not in ("container", "direct"):
        raise ConfigError(f"{where}: kind must be 'container' or 'direct'")
    configs = {str(k): str(v) for k, v in raw.get("configs", {}).items()}
    na = {str(k): str(v) for k, v in raw.get("na", {}).items()}
    for sid in [*configs, *na]:
        if sid not in scenarios:
            raise ConfigError(f"{where}: unknown scenario {sid!r}")
    both = sorted(set(configs) & set(na))
    if both:
        raise ConfigError(f"{where}: {both} listed under both [configs] and [na]")
    missing = [s for s in scenarios if s not in configs and s not in na]
    if missing:
        raise ConfigError(f"{where}: scenarios with neither a config nor an n/a reason: {missing}")
    blank = [s for s, reason in na.items() if not reason.strip()]
    if blank:
        raise ConfigError(f"{where}: empty n/a reason for {blank}")
    try:
        deps = tuple(
            Dependency(name=d["name"], image=d["image"], port=int(d["port"]),
                       command=tuple(d.get("command", [])))
            for d in raw.get("dependency", [])
        )
        setup = tuple(
            SetupCall(method=s["method"].upper(), path=s["path"], scenarios=tuple(s["scenarios"]),
                      headers={str(k): str(v) for k, v in s.get("headers", {}).items()},
                      body_file=s.get("body_file"),
                      expect_status=tuple(int(x) for x in s.get("expect_status", [200])))
            for s in raw.get("setup", [])
        )
    except KeyError as e:
        raise ConfigError(f"{where}: missing key {e}") from None
    for call in setup:
        bad = [s for s in call.scenarios if s not in configs]
        if bad:
            raise ConfigError(f"{where}: setup {call.method} {call.path} targets unsupported scenarios {bad}")
        if call.body_file and not (directory / call.body_file).is_file():
            raise ConfigError(f"{where}: setup body_file {call.body_file!r} not found")
    common = raw.get("common")
    ports = {str(k): int(v) for k, v in raw.get("ports", {}).items()}
    if kind == "container":
        for key in ("image", "config_dir"):
            if not raw.get(key):
                raise ConfigError(f"{where}: {key!r} is required")
        if "plain" not in ports:
            raise ConfigError(f"{where}: ports.plain is required")
        if any(scenarios[s].scheme == "https" for s in configs) and "tls" not in ports:
            raise ConfigError(f"{where}: an https scenario is configured but ports.tls is missing")
        for sid, sub in configs.items():
            if not sub or not (directory / sub).is_dir():
                raise ConfigError(f"{where}: config directory {sub!r} for {sid} not found")
        if common and not (directory / common).is_dir():
            raise ConfigError(f"{where}: common directory {common!r} not found")
    elif any(configs.values()):
        raise ConfigError(f"{where}: a direct gateway has no config directories")
    build = raw.get("build")
    return Gateway(
        name=name, kind=kind, dir=directory, configs=configs, na=na,
        image=raw.get("image", ""),
        build=(directory / build).resolve() if build else None,
        build_args={str(k): str(v) for k, v in raw.get("build_args", {}).items()},
        build_requires=tuple(raw.get("build_requires", [])),
        build_hint=raw.get("build_hint", ""),
        config_dir=raw.get("config_dir", ""),
        common=common,
        entrypoint=raw.get("entrypoint"),
        command=tuple(raw.get("command", [])),
        workdir=raw.get("workdir"),
        env={str(k): str(v) for k, v in raw.get("env", {}).items()},
        ports=ports, dependencies=deps, setup=setup,
    )


def load_gateways(root: Path, scenarios: dict[str, Scenario]) -> dict[str, Gateway]:
    return {
        d.name: load_gateway(d, scenarios)
        for d in sorted(root.iterdir())
        if d.is_dir() and (d / "gateway.toml").is_file()
    }


def cpuset_members(spec: str) -> set[int]:
    members: set[int] = set()
    try:
        for part in spec.split(","):
            part = part.strip()
            if not part:
                raise ValueError
            if "-" in part:
                a, b = part.split("-", 1)
                lo, hi = int(a), int(b)
                if hi < lo:
                    raise ValueError
                members.update(range(lo, hi + 1))
            else:
                members.add(int(part))
    except ValueError:
        raise ConfigError(f"bad cpuset {spec!r} (use forms like '0', '0-3', '0,2-3')") from None
    return members


def cpuset_size(spec: str) -> int:
    return len(cpuset_members(spec))


def load_topology(path: Path) -> Topology:
    raw = _read(path)
    kind = raw.get("kind")
    if kind == "local":
        network = raw.get("network", "featherbit-bench")
        roles = {r: Role(address=r) for r in ROLES}
        published = {"plain": 18000, "tls": 18443, "upstream": 18080,
                     **{str(k): int(v) for k, v in raw.get("published_ports", {}).items()}}
        probe_host = "127.0.0.1"
    elif kind == "remote":
        network = "host"
        roles = {}
        for r in ROLES:
            rr = raw.get("roles", {}).get(r)
            if not rr or "address" not in rr or "context" not in rr:
                raise ConfigError(f"{path}: roles.{r} needs 'context' and 'address'")
            roles[r] = Role(address=rr["address"], context=rr["context"])
        published = {}
        probe_host = roles["gateway"].address
    else:
        raise ConfigError(f"{path}: kind must be 'local' or 'remote'")
    profiles: dict[int, Profile] = {}
    for key, pr in raw.get("profiles", {}).items():
        try:
            cores = int(key)
            prof = Profile(cores=cores, gateway=pr["gateway"], upstream=pr["upstream"], loadgen=pr["loadgen"])
        except (ValueError, KeyError) as e:
            raise ConfigError(f"{path}: profile {key!r}: {e}") from None
        sets = {r: cpuset_members(getattr(prof, r)) for r in ROLES}
        if len(sets["gateway"]) != cores:
            raise ConfigError(f"{path}: profile {key}: gateway cpuset {prof.gateway!r} has "
                              f"{len(sets['gateway'])} cpus, expected {cores}")
        if kind == "local":
            for a, b in (("gateway", "upstream"), ("gateway", "loadgen"), ("upstream", "loadgen")):
                if sets[a] & sets[b]:
                    raise ConfigError(f"{path}: profile {key}: {a} and {b} cpusets overlap")
        profiles[cores] = prof
    if not profiles:
        raise ConfigError(f"{path}: no profiles")
    return Topology(name=path.stem, kind=kind, network=network, roles=roles, profiles=profiles,
                    published_ports=published, probe_host=probe_host)
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all `test_config` tests PASS.

- [ ] **Step 8: Commit** (only if authorized, see Global Constraints)

```bash
git add .gitignore bench/.gitattributes bench/benchlib bench/scenarios bench/topology bench/tests
git commit -m "feat(bench): scenario catalog, topologies and config loading"
```

---
### Task 2: Load-tool output parsers

**Files:**
- Create: `bench/benchlib/parsers.py`, `bench/tests/test_parsers.py`

**Interfaces:**
- Consumes: nothing.
- Produces (`benchlib.parsers`):
  - `ParseError(Exception)`
  - `LoadResult(requests: int, duration_s: float, rps: float, errors: int, non2xx: int, p50_ms, p90_ms, p99_ms, p999_ms, max_ms)`: the latency fields are `float | None`. Methods: `.error_rate() -> float` = (errors + non2xx) / max(requests, 1), and `.to_dict()`.
  - `parse_wrk(text) -> LoadResult`, `parse_wrk2(text) -> LoadResult`, `parse_oha(text) -> LoadResult`

The formats below are those of wrk 4.2.0, wrk2 (giltene) and oha's JSON output. Task 7 adds fixtures captured from the real pinned binaries. If they differ from these samples, fix the parser, not the fixture.

- [ ] **Step 1: Write the failing tests**

`bench/tests/test_parsers.py`:
```python
import json
import unittest

from benchlib.parsers import ParseError, parse_oha, parse_wrk, parse_wrk2

WRK = """Running 10s test @ http://gateway:8000/bench/1k
  4 threads and 64 connections
  Thread Stats   Avg      Stdev     Max   +/- Stdev
    Latency     1.05ms  452.11us  12.34ms   85.12%
    Req/Sec    15.12k     1.02k   17.89k    70.25%
  Latency Distribution
     50%    0.98ms
     75%    1.21ms
     90%    1.55ms
     99%    2.89ms
  602345 requests in 10.00s, 650.12MB read
  Socket errors: connect 0, read 3, write 0, timeout 1
  Non-2xx or 3xx responses: 12
Requests/sec:  60213.45
Transfer/sec:     64.99MB
"""

WRK2 = """Running 20s test @ http://gateway:8000/bench/1k
  4 threads and 64 connections
  Thread calibration: mean lat.: 1.234ms, rate sampling interval: 10ms
  Thread Stats   Avg      Stdev     Max   +/- Stdev
    Latency     1.12ms  510.33us   9.87ms   78.00%
    Req/Sec     2.64k   210.00     3.44k    80.00%
  Latency Distribution (HdrHistogram - Recorded Latency)
 50.000%    1.05ms
 75.000%    1.40ms
 90.000%    1.80ms
 99.000%    3.10ms
 99.900%    6.20ms
 99.990%    8.90ms
 99.999%    9.80ms
100.000%    9.87ms

  Detailed Percentile spectrum:
       Value   Percentile   TotalCount 1/(1-Percentile)

       0.123     0.000000            1         1.00
       1.050     0.500000        99900         2.00
#[Mean    =        1.120, StdDeviation   =        0.510]
#[Max     =        9.872, Total count    =       199800]
----------------------------------------------------------
  200020 requests in 20.00s, 215.99MB read
Requests/sec:  10001.02
Transfer/sec:     10.80MB
"""

OHA = {
    "summary": {"successRate": 1.0, "total": 20.001, "slowest": 0.0123, "fastest": 0.0002,
                "average": 0.0011, "requestsPerSec": 9999.5},
    "latencyPercentiles": {"p10": 0.0005, "p50": 0.001, "p90": 0.0015, "p99": 0.003,
                           "p99.9": 0.006, "p99.99": 0.009},
    "statusCodeDistribution": {"200": 199990, "502": 8},
    "errorDistribution": {"connection closed": 2},
}


class WrkTests(unittest.TestCase):
    def test_parse_wrk(self):
        r = parse_wrk(WRK)
        self.assertEqual((r.requests, r.errors, r.non2xx), (602345, 4, 12))
        self.assertAlmostEqual(r.rps, 60213.45)
        self.assertAlmostEqual(r.duration_s, 10.0)
        self.assertAlmostEqual(r.p50_ms, 0.98)
        self.assertAlmostEqual(r.p99_ms, 2.89)
        self.assertIsNone(r.p999_ms)
        self.assertAlmostEqual(r.max_ms, 12.34)
        self.assertAlmostEqual(r.error_rate(), 16 / 602345)

    def test_parse_wrk_unreachable_raises(self):
        with self.assertRaises(ParseError):
            parse_wrk("unable to connect to gateway:8000 Connection refused\n")

    def test_units(self):
        text = WRK.replace("     99%    2.89ms", "     99%    1.50s").replace("12.34ms", "2.00m")
        r = parse_wrk(text)
        self.assertAlmostEqual(r.p99_ms, 1500.0)
        self.assertAlmostEqual(r.max_ms, 120000.0)

    def test_no_errors_lines_means_zero(self):
        text = "\n".join(l for l in WRK.splitlines() if "Socket errors" not in l and "Non-2xx" not in l)
        r = parse_wrk(text)
        self.assertEqual((r.errors, r.non2xx), (0, 0))


class Wrk2Tests(unittest.TestCase):
    def test_parse_wrk2(self):
        r = parse_wrk2(WRK2)
        self.assertEqual(r.requests, 200020)
        self.assertAlmostEqual(r.rps, 10001.02)
        self.assertAlmostEqual(r.p50_ms, 1.05)
        self.assertAlmostEqual(r.p90_ms, 1.80)
        self.assertAlmostEqual(r.p99_ms, 3.10)
        self.assertAlmostEqual(r.p999_ms, 6.20)
        self.assertAlmostEqual(r.max_ms, 9.87)
        self.assertEqual(r.error_rate(), 0.0)

    def test_wrk2_without_distribution_raises(self):
        with self.assertRaises(ParseError):
            parse_wrk2(WRK)  # wrk output has no HdrHistogram section


class OhaTests(unittest.TestCase):
    def test_parse_oha(self):
        r = parse_oha(json.dumps(OHA))
        self.assertEqual((r.requests, r.errors, r.non2xx), (200000, 2, 8))
        self.assertAlmostEqual(r.p99_ms, 3.0)
        self.assertAlmostEqual(r.p999_ms, 6.0)
        self.assertAlmostEqual(r.max_ms, 12.3)
        self.assertAlmostEqual(r.duration_s, 20.001)

    def test_parse_oha_bad_json(self):
        with self.assertRaises(ParseError):
            parse_oha("Error: connection refused")

    def test_to_dict_round_trips_json(self):
        d = parse_oha(json.dumps(OHA)).to_dict()
        self.assertEqual(json.loads(json.dumps(d))["requests"], 200000)
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'benchlib.parsers'`.

- [ ] **Step 3: Implement `benchlib/parsers.py`**

```python
"""Parse wrk, wrk2 and oha output into one LoadResult shape."""
from __future__ import annotations

import dataclasses
import json
import re
from dataclasses import dataclass


class ParseError(Exception):
    """Tool output did not have the expected shape (tool failed, target unreachable...)."""


@dataclass(frozen=True)
class LoadResult:
    requests: int
    duration_s: float
    rps: float
    errors: int
    non2xx: int
    p50_ms: float | None = None
    p90_ms: float | None = None
    p99_ms: float | None = None
    p999_ms: float | None = None
    max_ms: float | None = None

    def error_rate(self) -> float:
        return (self.errors + self.non2xx) / max(self.requests, 1)

    def to_dict(self) -> dict:
        return dataclasses.asdict(self)


_UNIT_MS = {"us": 0.001, "ms": 1.0, "s": 1000.0, "m": 60000.0}
_VALUE = re.compile(r"^([\d.]+)(us|ms|s|m)$")
_REQUESTS = re.compile(r"^\s*(\d+) requests in ([\d.]+(?:us|ms|s|m)),", re.M)
_RPS = re.compile(r"^Requests/sec:\s+([\d.]+)", re.M)
_NON2XX = re.compile(r"Non-2xx or 3xx responses:\s+(\d+)")
_SOCKET = re.compile(r"Socket errors: connect (\d+), read (\d+), write (\d+), timeout (\d+)")
_THREAD_LAT = re.compile(r"^\s+Latency\s+(\S+)\s+(\S+)\s+(\S+)", re.M)
_WRK_PCT = re.compile(r"^\s+(50|75|90|99)%\s+(\S+)\s*$", re.M)
_WRK2_PCT = re.compile(r"^\s*(\d+\.\d+)%\s+(\S+)\s*$", re.M)


def _ms(token: str) -> float:
    m = _VALUE.match(token.strip())
    if not m:
        raise ParseError(f"unrecognised latency value {token!r}")
    return float(m.group(1)) * _UNIT_MS[m.group(2)]


def _common(text: str) -> tuple[int, float, float, int, int, float | None]:
    req = _REQUESTS.search(text)
    rps = _RPS.search(text)
    if not req or not rps:
        first = text.strip().splitlines()[:3]
        raise ParseError(f"no request summary in tool output: {' | '.join(first) or '<empty>'}")
    requests = int(req.group(1))
    duration_s = _ms(req.group(2)) / 1000.0
    non2xx = int(m.group(1)) if (m := _NON2XX.search(text)) else 0
    errors = sum(int(x) for x in m.groups()) if (m := _SOCKET.search(text)) else 0
    max_ms = _ms(m.group(3)) if (m := _THREAD_LAT.search(text)) else None
    return requests, duration_s, float(rps.group(1)), errors, non2xx, max_ms


def parse_wrk(text: str) -> LoadResult:
    requests, duration_s, rps, errors, non2xx, max_ms = _common(text)
    pct = {int(p): _ms(v) for p, v in _WRK_PCT.findall(text)}
    return LoadResult(requests, duration_s, rps, errors, non2xx,
                      p50_ms=pct.get(50), p90_ms=pct.get(90), p99_ms=pct.get(99),
                      p999_ms=None, max_ms=max_ms)


def parse_wrk2(text: str) -> LoadResult:
    requests, duration_s, rps, errors, non2xx, _ = _common(text)
    start = text.find("Latency Distribution (HdrHistogram")
    if start < 0:
        raise ParseError("no HdrHistogram latency distribution (was --latency passed?)")
    section = text[start:].split("\n\n", 1)[0]
    pct = {p: _ms(v) for p, v in _WRK2_PCT.findall(section)}
    return LoadResult(requests, duration_s, rps, errors, non2xx,
                      p50_ms=pct.get("50.000"), p90_ms=pct.get("90.000"), p99_ms=pct.get("99.000"),
                      p999_ms=pct.get("99.900"), max_ms=pct.get("100.000"))


def parse_oha(text: str) -> LoadResult:
    try:
        d = json.loads(text)
        summary = d["summary"]
    except (json.JSONDecodeError, KeyError, TypeError) as e:
        raise ParseError(f"oha output is not the expected JSON ({e}): {text.strip()[:200]!r}") from None
    codes = {int(k): int(v) for k, v in d.get("statusCodeDistribution", {}).items()}
    errors = sum(int(v) for v in d.get("errorDistribution", {}).values())
    non2xx = sum(v for k, v in codes.items() if not 200 <= k < 300)
    lp = d.get("latencyPercentiles") or {}

    def ms(key: str) -> float | None:
        v = lp.get(key)
        return None if v is None else float(v) * 1000.0

    slowest = summary.get("slowest")
    return LoadResult(
        requests=sum(codes.values()) + errors,
        duration_s=float(summary["total"]),
        rps=float(summary["requestsPerSec"]),
        errors=errors, non2xx=non2xx,
        p50_ms=ms("p50"), p90_ms=ms("p90"), p99_ms=ms("p99"), p999_ms=ms("p99.9"),
        max_ms=None if slowest is None else float(slowest) * 1000.0,
    )
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all parser tests PASS.

- [ ] **Step 5: Commit** (only if authorized)

```bash
git add bench/benchlib/parsers.py bench/tests/test_parsers.py
git commit -m "feat(bench): wrk, wrk2 and oha output parsers"
```

---
### Task 3: Throughput search, ladder selection, status classification

**Files:**
- Create: `bench/benchlib/search.py`, `bench/benchlib/classify.py`, `bench/tests/test_search.py`

**Interfaces:**
- Consumes: `LoadResult` (Task 2).
- Produces:
  - `benchlib.search`:
    - `Step(rate: int, result: LoadResult, passed: bool, t_start: float, t_end: float)`
    - `SearchOutcome(max_sustainable: int, steps: list[Step], flags: list[str])`
    - `passes(result, slo_p99_ms, max_error_rate) -> bool`
    - `find_max_sustainable(measure: Callable[[int], Step], ceiling: float, low: float, high: float, tolerance: float) -> SearchOutcome`
    - `ladder_rates(ladder: tuple[int,...], max_sustainable: int) -> list[int]`
    - Flag names: `"no-ceiling"`, `"slo-unmet-at-floor"`, `"search-ceiling-hit"`.
  - `benchlib.classify`:
    - constants `OK="ok"`, `INVALID="invalid"`, `ERROR="error"`, `NA="n/a"`, `PENDING="pending"`, `LOADGEN_BOUND="loadgen-bound"`, `UPSTREAM_BOUND="upstream-bound"`
    - `saturation_flags(loadgen_frac, upstream_frac, threshold) -> list[str]`
    - `rep_status(flags: list[str]) -> str`
    - `cell_status(rep_statuses: list[str]) -> str`
    - `summarize(values: list[float]) -> dict | None`, returning `{"median","min","max","n"}`
    - `summarize_cell(reps: list[dict]) -> dict`
    - `LADDER_METRICS`: the tuple of per-point keys that get summarized

A repetition result dict (built in Task 8) has these keys:
- `status`
- `max_sustainable_rps` (optional)
- `ladder`: a list of point dicts, each with `rate`, `p50_ms`, `p90_ms`, `p99_ms`, `p999_ms`, `rps_per_core`, `gateway_rss_peak_mb` and `gateway_cpu_cores`

- [ ] **Step 1: Write the failing tests**

`bench/tests/test_search.py`:
```python
import unittest

from benchlib.classify import (
    ERROR, INVALID, LOADGEN_BOUND, OK, PENDING, UPSTREAM_BOUND,
    cell_status, rep_status, saturation_flags, summarize, summarize_cell,
)
from benchlib.parsers import LoadResult
from benchlib.search import Step, find_max_sustainable, ladder_rates, passes


def result(p99=1.0, requests=1000, errors=0, non2xx=0):
    return LoadResult(requests, 1.0, float(requests), errors, non2xx, 0.5, 0.8, p99, p99, p99)


def capacity_model(capacity):
    calls = []

    def measure(rate):
        calls.append(rate)
        return Step(rate, result(p99=2.0 if rate <= capacity else 80.0), rate <= capacity, 0.0, 1.0)
    return measure, calls


class PassesTests(unittest.TestCase):
    def test_passes(self):
        self.assertTrue(passes(result(p99=9.9), 10.0, 0.001))
        self.assertFalse(passes(result(p99=10.1), 10.0, 0.001))
        self.assertFalse(passes(result(requests=1000, non2xx=1), 10.0, 0.001))
        self.assertFalse(passes(LoadResult(0, 1.0, 0.0, 0, 0), 10.0, 0.001))  # no p99


class SearchTests(unittest.TestCase):
    def test_converges_within_tolerance_below_capacity(self):
        measure, calls = capacity_model(37_000)
        out = find_max_sustainable(measure, 40_000, 0.1, 1.2, 0.02)
        self.assertLessEqual(out.max_sustainable, 37_000)
        self.assertGreaterEqual(out.max_sustainable, 37_000 * 0.97)
        self.assertLessEqual(len(calls), 10)
        self.assertEqual(out.flags, [])
        self.assertEqual(calls[:2], [4000, 48000])

    def test_floor_failure(self):
        measure, calls = capacity_model(100)
        out = find_max_sustainable(measure, 40_000, 0.1, 1.2, 0.02)
        self.assertEqual((out.max_sustainable, out.flags, len(calls)), (0, ["slo-unmet-at-floor"], 1))

    def test_ceiling_hit(self):
        measure, _ = capacity_model(10**9)
        out = find_max_sustainable(measure, 40_000, 0.1, 1.2, 0.02)
        self.assertEqual((out.max_sustainable, out.flags), (48_000, ["search-ceiling-hit"]))

    def test_no_ceiling(self):
        measure, calls = capacity_model(10)
        out = find_max_sustainable(measure, 0.0, 0.1, 1.2, 0.02)
        self.assertEqual((out.max_sustainable, out.flags, calls), (0, ["no-ceiling"], []))

    def test_tiny_ceiling_terminates(self):
        measure, calls = capacity_model(3)
        out = find_max_sustainable(measure, 5.0, 0.1, 1.2, 0.02)
        self.assertLessEqual(out.max_sustainable, 3)
        self.assertLess(len(calls), 10)


class LadderTests(unittest.TestCase):
    def test_ladder_rates(self):
        ladder = (1000, 5000, 10000, 25000)
        self.assertEqual(ladder_rates(ladder, 12_000), [1000, 5000, 10000])
        self.assertEqual(ladder_rates(ladder, 10_000), [1000, 5000, 10000])
        self.assertEqual(ladder_rates(ladder, 500), [1000])  # always at least the lowest rung


class ClassifyTests(unittest.TestCase):
    def test_saturation(self):
        self.assertEqual(saturation_flags(0.90, 0.10, 0.85), [LOADGEN_BOUND])
        self.assertEqual(saturation_flags(0.10, 0.95, 0.85), [UPSTREAM_BOUND])
        self.assertEqual(saturation_flags(None, None, 0.85), [])

    def test_rep_status(self):
        self.assertEqual(rep_status([]), OK)
        self.assertEqual(rep_status(["search-ceiling-hit"]), OK)
        self.assertEqual(rep_status([UPSTREAM_BOUND, LOADGEN_BOUND]), LOADGEN_BOUND)

    def test_cell_status_precedence(self):
        self.assertEqual(cell_status([]), PENDING)
        self.assertEqual(cell_status([OK, INVALID]), INVALID)
        self.assertEqual(cell_status([ERROR, ERROR]), ERROR)
        self.assertEqual(cell_status([OK, ERROR]), OK)
        self.assertEqual(cell_status([OK, UPSTREAM_BOUND]), UPSTREAM_BOUND)
        self.assertEqual(cell_status([UPSTREAM_BOUND, LOADGEN_BOUND]), LOADGEN_BOUND)

    def test_summarize(self):
        self.assertIsNone(summarize([]))
        self.assertEqual(summarize([3, 1, 2]), {"median": 2, "min": 1, "max": 3, "n": 3})

    def test_summarize_cell_uses_good_reps_only(self):
        point = lambda rate, p99: {"rate": rate, "p50_ms": 1.0, "p90_ms": 1.5, "p99_ms": p99,
                                   "p999_ms": None, "rps_per_core": 900.0,
                                   "gateway_rss_peak_mb": 40.0, "gateway_cpu_cores": 1.0}
        reps = [
            {"rep": 0, "status": OK, "max_sustainable_rps": 10_000, "ladder": [point(1000, 2.0)]},
            {"rep": 1, "status": LOADGEN_BOUND, "max_sustainable_rps": 12_000, "ladder": [point(1000, 4.0)]},
            {"rep": 2, "status": ERROR, "error": "boom"},
        ]
        s = summarize_cell(reps)
        self.assertEqual((s["reps_ok"], s["reps_total"]), (2, 3))
        self.assertEqual(s["max_sustainable_rps"]["median"], 11_000)
        self.assertEqual(s["ladder"][0]["rate"], 1000)
        self.assertEqual(s["ladder"][0]["p99_ms"]["median"], 3.0)
        self.assertIsNone(s["ladder"][0]["p999_ms"])

    def test_summarize_cell_validate_only_reps(self):
        s = summarize_cell([{"rep": 0, "status": OK, "probes": []}])
        self.assertIsNone(s["max_sustainable_rps"])
        self.assertEqual(s["ladder"], [])
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'benchlib.classify'`.

- [ ] **Step 3: Implement `benchlib/search.py`**

```python
"""Find the max sustainable rate (spec §6 step 4) and pick latency-ladder rates (step 5)."""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable

from .parsers import LoadResult


@dataclass(frozen=True)
class Step:
    rate: int
    result: LoadResult
    passed: bool
    t_start: float
    t_end: float


@dataclass
class SearchOutcome:
    max_sustainable: int
    steps: list[Step] = field(default_factory=list)
    flags: list[str] = field(default_factory=list)


def passes(result: LoadResult, slo_p99_ms: float, max_error_rate: float) -> bool:
    if result.p99_ms is None:
        return False
    return result.p99_ms <= slo_p99_ms and result.error_rate() < max_error_rate


def find_max_sustainable(measure: Callable[[int], Step], ceiling: float,
                         low: float, high: float, tolerance: float) -> SearchOutcome:
    """Bisect between low*ceiling and high*ceiling for the highest passing rate.

    `measure(rate)` runs one fixed-rate step and reports whether it met the SLO.
    """
    out = SearchOutcome(0)
    if ceiling <= 0:
        out.flags.append("no-ceiling")
        return out
    lo = max(1, round(ceiling * low))
    hi = max(lo + 1, round(ceiling * high))
    step = measure(lo)
    out.steps.append(step)
    if not step.passed:
        out.flags.append("slo-unmet-at-floor")
        return out
    step = measure(hi)
    out.steps.append(step)
    if step.passed:
        out.max_sustainable = hi
        out.flags.append("search-ceiling-hit")
        return out
    while hi - lo > 1 and (hi - lo) / hi > tolerance:
        mid = (lo + hi) // 2
        step = measure(mid)
        out.steps.append(step)
        if step.passed:
            lo = mid
        else:
            hi = mid
    out.max_sustainable = lo
    return out


def ladder_rates(ladder: tuple[int, ...], max_sustainable: int) -> list[int]:
    """Every ladder rate up to the max sustainable one; at least the lowest rung."""
    rates = [r for r in ladder if r <= max_sustainable]
    return rates or [ladder[0]]
```

- [ ] **Step 4: Implement `benchlib/classify.py`**

```python
"""Repetition/cell statuses (spec §6 steps 2, 7, 8) and median/min/max summaries."""
from __future__ import annotations

import statistics

OK = "ok"
INVALID = "invalid"
ERROR = "error"
NA = "n/a"
PENDING = "pending"
LOADGEN_BOUND = "loadgen-bound"
UPSTREAM_BOUND = "upstream-bound"
MEASURED = (OK, LOADGEN_BOUND, UPSTREAM_BOUND)
LADDER_METRICS = ("p50_ms", "p90_ms", "p99_ms", "p999_ms", "rps_per_core",
                  "gateway_rss_peak_mb", "gateway_cpu_cores")


def saturation_flags(loadgen_frac: float | None, upstream_frac: float | None,
                     threshold: float) -> list[str]:
    flags = []
    if loadgen_frac is not None and loadgen_frac > threshold:
        flags.append(LOADGEN_BOUND)
    if upstream_frac is not None and upstream_frac > threshold:
        flags.append(UPSTREAM_BOUND)
    return flags


def rep_status(flags: list[str]) -> str:
    if LOADGEN_BOUND in flags:
        return LOADGEN_BOUND
    if UPSTREAM_BOUND in flags:
        return UPSTREAM_BOUND
    return OK


def cell_status(rep_statuses: list[str]) -> str:
    if not rep_statuses:
        return PENDING
    if INVALID in rep_statuses:
        return INVALID
    if all(s == ERROR for s in rep_statuses):
        return ERROR
    if LOADGEN_BOUND in rep_statuses:
        return LOADGEN_BOUND
    if UPSTREAM_BOUND in rep_statuses:
        return UPSTREAM_BOUND
    return OK


def summarize(values: list[float]) -> dict | None:
    if not values:
        return None
    return {"median": statistics.median(values), "min": min(values), "max": max(values),
            "n": len(values)}


def summarize_cell(reps: list[dict]) -> dict:
    good = [r for r in reps if r.get("status") in MEASURED]
    by_rate: dict[int, list[dict]] = {}
    for r in good:
        for point in r.get("ladder", []):
            by_rate.setdefault(point["rate"], []).append(point)
    ladder = []
    for rate, points in sorted(by_rate.items()):
        entry = {"rate": rate, "reps": len(points)}
        for key in LADDER_METRICS:
            entry[key] = summarize([p[key] for p in points if p.get(key) is not None])
        ladder.append(entry)
    return {
        "reps_ok": len(good),
        "reps_total": len(reps),
        "max_sustainable_rps": summarize(
            [r["max_sustainable_rps"] for r in good if r.get("max_sustainable_rps") is not None]),
        "ladder": ladder,
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all search/classify tests PASS.

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/benchlib/search.py bench/benchlib/classify.py bench/tests/test_search.py
git commit -m "feat(bench): max-sustainable-rate search and status classification"
```

---
### Task 4: Matrix expansion, scheduling and the run store

**Files:**
- Create: `bench/benchlib/matrix.py`, `bench/benchlib/results.py`, `bench/tests/test_matrix_results.py`

**Interfaces:**
- Consumes: `ConfigError`, `Gateway` (Task 1); `cell_status`, `summarize_cell` (Task 3).
- Produces:
  - `benchlib.matrix`:
    - `Cell(gateway: str, scenario: str, profile: int)` (frozen), with `.key` = `"gw|scenario|profile"` and `.slug` = `"gw__scenario__profile"`
    - `select(patterns: list[str]|None, names: list[str], what: str) -> list[str]`: fnmatch globs, keeps catalog order, raises `ConfigError` for a pattern that matches nothing
    - `gateway_order(names) -> list[str]`: `"direct"` first, then alphabetical
    - `expand(gateway_names, scenario_ids, profiles) -> list[Cell]`: order is scenario, then profile, then gateway
    - `schedule(cells, gateways, reps) -> list[tuple[Cell, int]]`: repetitions outermost, supported cells only
  - `benchlib.results.RunStore`:
    - `RunStore.create(root: Path, label: str, meta: dict, now: datetime | None = None) -> RunStore`: creates `<root>/<YYYYmmddTHHMMSSZ>-<label>/` with `raw/`, `configs/`, `logs/`
    - `RunStore.open(path) -> RunStore`
    - attributes `.path`, `.data` (the run.json dict), `.raw_dir`, `.configs_dir`, `.logs_dir`
    - `.done(cell, rep) -> bool`, `.record_na(cell, reason)`, `.record_rep(cell, rep, result: dict)`, `.save()`
    - `record_rep` pops `result["logs"]` (a `{container: text}` dict), writes each entry to `logs/<slug>__rep<N>__<container>.log`, and stores the file names under `result["log_files"]`

- [ ] **Step 1: Write the failing tests**

`bench/tests/test_matrix_results.py`:
```python
import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from benchlib.config import ConfigError, Gateway
from benchlib.matrix import Cell, expand, gateway_order, schedule, select
from benchlib.results import RunStore


def gw(name, supported):
    return Gateway(name=name, kind="direct", dir=Path("."), configs={s: "" for s in supported}, na={})


class MatrixTests(unittest.TestCase):
    def test_select_globs_keep_catalog_order(self):
        names = ["core.proxy", "core.routes-1k", "plugin.jwt"]
        self.assertEqual(select(["plugin.*", "core.proxy"], names, "scenario"), ["core.proxy", "plugin.jwt"])
        self.assertEqual(select(None, names, "scenario"), names)
        with self.assertRaisesRegex(ConfigError, "no scenario matches 'nope'"):
            select(["nope"], names, "scenario")

    def test_gateway_order(self):
        self.assertEqual(gateway_order(["nginx", "direct", "apisix"]), ["direct", "apisix", "nginx"])

    def test_expand_order(self):
        cells = expand(["a", "b"], ["s1", "s2"], [4, 1])
        self.assertEqual([c.key for c in cells[:3]], ["a|s1|1", "b|s1|1", "a|s1|4"])
        self.assertEqual(len(cells), 8)

    def test_schedule_interleaves_reps_and_skips_unsupported(self):
        gws = {"a": gw("a", ["s1"]), "b": gw("b", ["s1", "s2"])}
        cells = expand(["a", "b"], ["s1", "s2"], [1])
        plan = schedule(cells, gws, 2)
        self.assertEqual([(c.key, r) for c, r in plan], [
            ("a|s1|1", 0), ("b|s1|1", 0), ("b|s2|1", 0),
            ("a|s1|1", 1), ("b|s1|1", 1), ("b|s2|1", 1),
        ])

    def test_slug_is_windows_safe(self):
        self.assertEqual(Cell("kong", "plugin.jwt", 4).slug, "kong__plugin.jwt__4")


class RunStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.now = datetime(2026, 9, 26, 10, 0, 0, tzinfo=timezone.utc)

    def tearDown(self):
        self.tmp.cleanup()

    def test_create_layout(self):
        s = RunStore.create(self.root, "local", {"publish": False}, now=self.now)
        self.assertEqual(s.path.name, "20260926T100000Z-local")
        for sub in ("raw", "configs", "logs"):
            self.assertTrue((s.path / sub).is_dir())
        data = json.loads((s.path / "run.json").read_text("utf-8"))
        self.assertEqual((data["schema"], data["publish"], data["cells"]), (1, False, {}))

    def test_record_rep_and_summary(self):
        s = RunStore.create(self.root, "local", {}, now=self.now)
        c = Cell("featherbit", "core.proxy", 1)
        s.record_rep(c, 0, {"status": "ok", "max_sustainable_rps": 1000, "ladder": []})
        cell = s.data["cells"][c.key]
        self.assertEqual((cell["status"], cell["summary"]["max_sustainable_rps"]["median"]), ("ok", 1000))
        self.assertTrue(s.done(c, 0))
        self.assertFalse(s.done(c, 1))

    def test_resume_skips_done_reps(self):
        s = RunStore.create(self.root, "local", {}, now=self.now)
        c = Cell("featherbit", "core.proxy", 1)
        s.record_rep(c, 0, {"status": "ok", "max_sustainable_rps": 1000, "ladder": []})
        reopened = RunStore.open(s.path)
        self.assertTrue(reopened.done(c, 0))
        reopened.record_rep(c, 0, {"status": "ok", "max_sustainable_rps": 2000, "ladder": []})
        self.assertEqual(len(reopened.data["cells"][c.key]["reps"]), 1)  # replaced, not duplicated

    def test_save_is_atomic(self):
        s = RunStore.create(self.root, "local", {}, now=self.now)
        (s.path / "run.json.tmp").write_text("{garbage", "utf-8")  # leftover from a crash
        s.record_na(Cell("nginx", "plugin.jwt", 1), "Plus only")
        self.assertEqual(RunStore.open(s.path).data["cells"]["nginx|plugin.jwt|1"]["status"], "n/a")
        self.assertFalse((s.path / "run.json.tmp").exists())

    def test_logs_go_to_files(self):
        s = RunStore.create(self.root, "local", {}, now=self.now)
        c = Cell("kong", "core.proxy", 4)
        s.record_rep(c, 2, {"status": "error", "error": "boot", "logs": {"fbb-gateway": "panic!"}})
        rep = s.data["cells"][c.key]["reps"][0]
        self.assertNotIn("logs", rep)
        self.assertEqual(rep["log_files"], ["kong__core.proxy__4__rep2__fbb-gateway.log"])
        self.assertEqual((s.logs_dir / rep["log_files"][0]).read_text("utf-8"), "panic!")

    def test_open_rejects_other_schema(self):
        s = RunStore.create(self.root, "local", {}, now=self.now)
        s.data["schema"] = 99
        s.save()
        with self.assertRaisesRegex(ConfigError, "schema"):
            RunStore.open(s.path)
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'benchlib.matrix'`.

- [ ] **Step 3: Implement `benchlib/matrix.py`**

```python
"""The benchmark matrix: which (gateway, scenario, profile) cells exist and in what order they run."""
from __future__ import annotations

import fnmatch
from dataclasses import dataclass

from .config import ConfigError, Gateway


@dataclass(frozen=True)
class Cell:
    gateway: str
    scenario: str
    profile: int

    @property
    def key(self) -> str:
        return f"{self.gateway}|{self.scenario}|{self.profile}"

    @property
    def slug(self) -> str:
        return f"{self.gateway}__{self.scenario}__{self.profile}"


def select(patterns: list[str] | None, names: list[str], what: str) -> list[str]:
    if not patterns:
        return list(names)
    chosen: set[str] = set()
    for pat in patterns:
        hits = [n for n in names if fnmatch.fnmatchcase(n, pat)]
        if not hits:
            raise ConfigError(f"no {what} matches {pat!r} (known: {', '.join(names)})")
        chosen.update(hits)
    return [n for n in names if n in chosen]


def gateway_order(names) -> list[str]:
    return sorted(names, key=lambda n: (n != "direct", n))


def expand(gateway_names: list[str], scenario_ids: list[str], profiles: list[int]) -> list[Cell]:
    return [Cell(g, s, p) for s in scenario_ids for p in sorted(profiles) for g in gateway_names]


def schedule(cells: list[Cell], gateways: dict[str, Gateway], reps: int) -> list[tuple[Cell, int]]:
    """Repetitions outermost, so drift spreads across gateways instead of hitting the last one."""
    runnable = [c for c in cells if gateways[c.gateway].supports(c.scenario)]
    return [(c, rep) for rep in range(reps) for c in runnable]
```

- [ ] **Step 4: Implement `benchlib/results.py`**

```python
"""A run directory: run.json (written atomically after every repetition) plus raw/, configs/, logs/."""
from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path

from .classify import NA, PENDING, cell_status, summarize_cell
from .config import ConfigError
from .matrix import Cell

SCHEMA = 1


class RunStore:
    def __init__(self, path: Path, data: dict):
        self.path = path
        self.data = data

    @classmethod
    def create(cls, root: Path, label: str, meta: dict, now: datetime | None = None) -> "RunStore":
        now = now or datetime.now(timezone.utc)
        path = root / f"{now:%Y%m%dT%H%M%SZ}-{label}"
        path.mkdir(parents=True, exist_ok=False)
        for sub in ("raw", "configs", "logs"):
            (path / sub).mkdir()
        store = cls(path, {"schema": SCHEMA, "started": now.isoformat(), **meta, "cells": {}})
        store.save()
        return store

    @classmethod
    def open(cls, path: Path) -> "RunStore":
        try:
            data = json.loads((path / "run.json").read_text("utf-8"))
        except FileNotFoundError:
            raise ConfigError(f"{path}: no run.json") from None
        if data.get("schema") != SCHEMA:
            raise ConfigError(f"{path}: run.json schema {data.get('schema')!r}, expected {SCHEMA}")
        return cls(path, data)

    @property
    def raw_dir(self) -> Path:
        return self.path / "raw"

    @property
    def configs_dir(self) -> Path:
        return self.path / "configs"

    @property
    def logs_dir(self) -> Path:
        return self.path / "logs"

    def _cell(self, cell: Cell) -> dict:
        return self.data["cells"].setdefault(cell.key, {
            "gateway": cell.gateway, "scenario": cell.scenario, "profile": cell.profile,
            "status": PENDING, "reps": [],
        })

    def done(self, cell: Cell, rep: int) -> bool:
        return any(r["rep"] == rep for r in self.data["cells"].get(cell.key, {}).get("reps", []))

    def record_na(self, cell: Cell, reason: str) -> None:
        c = self._cell(cell)
        c["status"] = NA
        c["na_reason"] = reason
        self.save()

    def record_rep(self, cell: Cell, rep: int, result: dict) -> None:
        result = dict(result)
        logs = result.pop("logs", None) or {}
        files = []
        for container, text in logs.items():
            name = f"{cell.slug}__rep{rep}__{container}.log"
            (self.logs_dir / name).write_text(text, encoding="utf-8")
            files.append(name)
        if files:
            result["log_files"] = files
        c = self._cell(cell)
        c["reps"] = sorted([r for r in c["reps"] if r["rep"] != rep] + [{"rep": rep, **result}],
                           key=lambda r: r["rep"])
        c["status"] = cell_status([r["status"] for r in c["reps"]])
        c["summary"] = summarize_cell(c["reps"])
        self.save()

    def save(self) -> None:
        tmp = self.path / "run.json.tmp"
        tmp.write_text(json.dumps(self.data, indent=1, sort_keys=False), encoding="utf-8")
        os.replace(tmp, self.path / "run.json")
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all matrix/results tests PASS.

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/benchlib/matrix.py bench/benchlib/results.py bench/tests/test_matrix_results.py
git commit -m "feat(bench): benchmark matrix, scheduling and resumable run store"
```

---
### Task 5: Config templating and test credentials

**Files:**
- Create: `bench/benchlib/templating.py`, `bench/benchlib/tokens.py`, `bench/tests/test_templating.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `benchlib.templating`:
    - `TemplateError(Exception)`
    - `expand_repeats(text) -> str`
    - `render_text(text, values: dict[str,str]) -> str`
    - `render_dir(src: Path, dst: Path, values) -> list[Path]`

    Template syntax, used by every gateway config:
    - `@@NAME@@` is replaced by `values["NAME"]`. An unknown name is a `TemplateError`.
    - A token standing **alone on its line** whose value spans several lines is inserted with every line carrying that line's indentation. This is how PEM blocks go into YAML.
    - `@@REPEAT n@@` … `@@END@@` (each on its own line) repeats the enclosed lines n times, with `@@I@@` set to the zero-padded index (`000`–`999` for n=1000). The form `@@REPEAT n SEP ,@@` appends `,` to every copy except the last (for JSON arrays). Nesting is not supported.
    - A file **name** containing `@@EACH_n@@` becomes n files, and `@@I@@` inside each file gets the same index.
    - Input is normalized to LF; output is always LF.
  - `benchlib.tokens`:
    - constants `API_KEY`, `JWT_SECRET`, `JWT_ISSUER`, `JWT_KID`, `JWT_EXP`, `TYK_SECRET`
    - `make_jwt(secret=JWT_SECRET, claims=None) -> str`, `tamper(token) -> str`, `jwks_json(secret=JWT_SECRET) -> str`
    - `static_values() -> dict[str,str]` with keys `API_KEY, JWT, JWT_TAMPERED, JWT_SECRET, JWT_SECRET_B64, JWT_ISSUER, JWT_KID, JWKS_JSON, TYK_SECRET`

The run-time keys added by the driver (Task 9) are `UPSTREAM_HOST, UPSTREAM_PORT, CORES, CONFIG_DIR, TLS_CERT_PEM, TLS_KEY_PEM`, plus `DEP_<NAME>_HOST` and `DEP_<NAME>_PORT` per dependency.

- [ ] **Step 1: Write the failing tests**

`bench/tests/test_templating.py`:
```python
import base64
import hashlib
import hmac
import json
import tempfile
import unittest
from pathlib import Path

from benchlib.templating import TemplateError, expand_repeats, render_dir, render_text
from benchlib.tokens import JWT_SECRET, jwks_json, make_jwt, static_values, tamper


def b64url_decode(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


class RepeatTests(unittest.TestCase):
    def test_repeat_pads_index(self):
        out = expand_repeats("a\n@@REPEAT 12@@\n- r@@I@@\n@@END@@\nb\n")
        lines = out.splitlines()
        self.assertEqual(lines[0], "a")
        self.assertEqual(lines[1], "- r00")
        self.assertEqual(lines[12], "- r11")
        self.assertEqual(lines[13], "b")

    def test_repeat_1000_uses_three_digits(self):
        out = expand_repeats("@@REPEAT 1000@@\nr@@I@@\n@@END@@\n").splitlines()
        self.assertEqual((out[0], out[-1], len(out)), ("r000", "r999", 1000))

    def test_separator(self):
        out = expand_repeats("[\n@@REPEAT 3 SEP ,@@\n  {\"i\": \"@@I@@\"}\n@@END@@\n]\n")
        self.assertEqual(json.loads(out), [{"i": "0"}, {"i": "1"}, {"i": "2"}])

    def test_errors(self):
        for bad in ("@@REPEAT 2@@\nx\n", "x\n@@END@@\n", "@@REPEAT 2@@\n@@REPEAT 2@@\n@@END@@\n@@END@@\n"):
            with self.assertRaises(TemplateError):
                expand_repeats(bad)


class RenderTests(unittest.TestCase):
    def test_inline_tokens(self):
        self.assertEqual(render_text("host: @@H@@:@@P@@\n", {"H": "up", "P": "8080"}), "host: up:8080\n")

    def test_block_token_is_indented(self):
        out = render_text("cert: |\n      @@PEM@@\nnext: 1\n", {"PEM": "-----BEGIN-----\nAAA\n-----END-----\n"})
        self.assertEqual(out, "cert: |\n      -----BEGIN-----\n      AAA\n      -----END-----\nnext: 1\n")

    def test_multiline_token_inline_is_an_error(self):
        with self.assertRaisesRegex(TemplateError, "alone on its line"):
            render_text("x: @@PEM@@ tail\n", {"PEM": "a\nb"})

    def test_unknown_token(self):
        with self.assertRaisesRegex(TemplateError, "@@NOPE@@"):
            render_text("@@NOPE@@\n", {})

    def test_non_token_at_signs_pass_through(self):
        self.assertEqual(render_text("user@@host and {{error.code}}\n", {}), "user@@host and {{error.code}}\n")

    def test_render_dir_each_and_binary(self):
        with tempfile.TemporaryDirectory() as d:
            src, dst = Path(d, "src"), Path(d, "dst")
            (src / "apps").mkdir(parents=True)
            (src / "apps" / "r@@EACH_3@@.json").write_text('{"id": "r@@I@@", "up": "@@H@@"}', "utf-8")
            (src / "blob.bin").write_bytes(b"\xff\xfe\x00")
            render_dir(src, dst, {"H": "upstream"})
            self.assertEqual(json.loads((dst / "apps" / "r2.json").read_text("utf-8")),
                             {"id": "r2", "up": "upstream"})
            self.assertEqual(sorted(p.name for p in (dst / "apps").iterdir()), ["r0.json", "r1.json", "r2.json"])
            self.assertEqual((dst / "blob.bin").read_bytes(), b"\xff\xfe\x00")

    def test_render_dir_normalizes_crlf(self):
        with tempfile.TemporaryDirectory() as d:
            src, dst = Path(d, "src"), Path(d, "dst")
            src.mkdir()
            (src / "nginx.conf").write_bytes(b"a @@H@@;\r\nb;\r\n")
            render_dir(src, dst, {"H": "x"})
            self.assertEqual((dst / "nginx.conf").read_bytes(), b"a x;\nb;\n")

    def test_render_dir_names_failing_file(self):
        with tempfile.TemporaryDirectory() as d:
            src = Path(d, "src")
            src.mkdir()
            (src / "bad.yaml").write_text("@@MISSING@@\n", "utf-8")
            with self.assertRaisesRegex(TemplateError, "bad.yaml"):
                render_dir(src, Path(d, "dst"), {})


class TokenTests(unittest.TestCase):
    def test_jwt_verifies(self):
        tok = make_jwt()
        head, payload, sig = tok.split(".")
        expected = hmac.new(JWT_SECRET.encode(), f"{head}.{payload}".encode(), hashlib.sha256).digest()
        self.assertEqual(b64url_decode(sig), expected)
        self.assertEqual(json.loads(b64url_decode(head))["kid"], "bench")
        claims = json.loads(b64url_decode(payload))
        self.assertEqual((claims["iss"], claims["key"], claims["sub"]), ("bench-jwt", "bench-jwt", "bench"))

    def test_tamper_breaks_signature(self):
        tok = make_jwt()
        bad = tamper(tok)
        self.assertNotEqual(bad, tok)
        self.assertEqual(bad.rsplit(".", 1)[0], tok.rsplit(".", 1)[0])
        self.assertNotEqual(b64url_decode(bad.rsplit(".", 1)[1]), b64url_decode(tok.rsplit(".", 1)[1]))

    def test_jwks_key_is_the_secret(self):
        key = json.loads(jwks_json())["keys"][0]
        self.assertEqual((key["kty"], key["alg"], key["kid"]), ("oct", "HS256", "bench"))
        self.assertEqual(b64url_decode(key["k"]).decode(), JWT_SECRET)

    def test_static_values_are_single_line(self):
        for k, v in static_values().items():
            self.assertNotIn("\n", v, k)
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'benchlib.templating'`.

- [ ] **Step 3: Implement `benchlib/templating.py`**

```python
"""Render gateway configs: @@TOKEN@@ substitution, @@REPEAT@@ blocks, @@EACH_n@@ file fan-out."""
from __future__ import annotations

import re
from pathlib import Path

TOKEN = re.compile(r"@@([A-Z][A-Z0-9_]*)@@")
REPEAT = re.compile(r"^\s*@@REPEAT (\d+)(?: SEP (\S+))?@@\s*$")
END = re.compile(r"^\s*@@END@@\s*$")
EACH = re.compile(r"@@EACH_(\d+)@@")


class TemplateError(Exception):
    """A config template is malformed or references an unknown token."""


def expand_repeats(text: str) -> str:
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    i = 0
    while i < len(lines):
        m = REPEAT.match(lines[i])
        if not m:
            if END.match(lines[i]):
                raise TemplateError(f"line {i + 1}: @@END@@ without @@REPEAT@@")
            out.append(lines[i])
            i += 1
            continue
        n, sep = int(m.group(1)), m.group(2) or ""
        j = i + 1
        while j < len(lines) and not END.match(lines[j]):
            if REPEAT.match(lines[j]):
                raise TemplateError(f"line {j + 1}: nested @@REPEAT@@ is not supported")
            j += 1
        if j == len(lines):
            raise TemplateError(f"line {i + 1}: @@REPEAT@@ without @@END@@")
        body = "".join(lines[i + 1:j])
        width = len(str(n - 1))
        copies = [body.replace("@@I@@", str(k).zfill(width)) for k in range(n)]
        if sep:
            copies = [c.rstrip("\n") + (sep if k < n - 1 else "") + "\n" for k, c in enumerate(copies)]
        out.extend(copies)
        i = j + 1
    return "".join(out)


def render_text(text: str, values: dict[str, str]) -> str:
    text = expand_repeats(text.replace("\r\n", "\n"))
    out: list[str] = []
    for lineno, line in enumerate(text.splitlines(keepends=True), 1):
        alone = TOKEN.fullmatch(line.strip())
        if alone and "\n" in values.get(alone.group(1), ""):
            indent = line[: len(line) - len(line.lstrip())]
            block = values[alone.group(1)].rstrip("\n").split("\n")
            out.append("".join(f"{indent}{b}\n" for b in block))
            continue

        def sub(m: re.Match) -> str:
            name = m.group(1)
            if name not in values:
                raise TemplateError(f"line {lineno}: unknown token @@{name}@@")
            if "\n" in values[name]:
                raise TemplateError(f"line {lineno}: multi-line @@{name}@@ must stand alone on its line")
            return values[name]

        out.append(TOKEN.sub(sub, line))
    return "".join(out)


def render_dir(src: Path, dst: Path, values: dict[str, str]) -> list[Path]:
    """Render every file under src into dst (merging into whatever dst already holds)."""
    written: list[Path] = []
    for f in sorted(src.rglob("*")):
        if f.is_dir():
            continue
        rel = f.relative_to(src).as_posix()
        m = EACH.search(rel)
        if m:
            n = int(m.group(1))
            width = len(str(n - 1))
            variants = [(EACH.sub(str(k).zfill(width), rel), {**values, "I": str(k).zfill(width)})
                        for k in range(n)]
        else:
            variants = [(rel, values)]
        raw = f.read_bytes()
        for relpath, vals in variants:
            out = dst / relpath
            out.parent.mkdir(parents=True, exist_ok=True)
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                out.write_bytes(raw)
            else:
                try:
                    rendered = render_text(text, vals)
                except TemplateError as e:
                    raise TemplateError(f"{src.name}/{rel}: {e}") from None
                out.write_bytes(rendered.encode("utf-8"))
            written.append(out)
    return written
```

- [ ] **Step 4: Implement `benchlib/tokens.py`**

```python
"""Fixed benchmark credentials. These are public test fixtures, not secrets: they only
ever authenticate against throwaway gateways inside a benchmark run."""
from __future__ import annotations

import base64
import hashlib
import hmac
import json

API_KEY = "bench-api-key"
JWT_SECRET = "featherbit-bench-hs256-secret-0123456789"
JWT_ISSUER = "bench-jwt"
JWT_KID = "bench"
JWT_EXP = 4102444800  # 2100-01-01T00:00:00Z
TYK_SECRET = "bench-tyk-control-secret"


def _b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _compact(obj: dict) -> bytes:
    return json.dumps(obj, separators=(",", ":")).encode()


def make_jwt(secret: str = JWT_SECRET, claims: dict | None = None) -> str:
    # `key` is what APISIX/Featherbit consumer lookup reads; `iss` is what Kong/Envoy/KrakenD read.
    claims = claims or {"key": JWT_ISSUER, "iss": JWT_ISSUER, "sub": "bench", "exp": JWT_EXP}
    signing = f"{_b64url(_compact({'alg': 'HS256', 'typ': 'JWT', 'kid': JWT_KID}))}.{_b64url(_compact(claims))}"
    sig = hmac.new(secret.encode(), signing.encode(), hashlib.sha256).digest()
    return f"{signing}.{_b64url(sig)}"


def tamper(token: str) -> str:
    # Flip the first signature character: the last one may only carry padding bits.
    head, _, sig = token.rpartition(".")
    return f"{head}.{'A' if sig[0] != 'A' else 'B'}{sig[1:]}"


def jwks_json(secret: str = JWT_SECRET) -> str:
    return json.dumps({"keys": [{"kty": "oct", "kid": JWT_KID, "alg": "HS256", "use": "sig",
                                 "k": _b64url(secret.encode())}]}, separators=(",", ":"))


def static_values() -> dict[str, str]:
    token = make_jwt()
    return {
        "API_KEY": API_KEY,
        "JWT": token,
        "JWT_TAMPERED": tamper(token),
        "JWT_SECRET": JWT_SECRET,
        "JWT_SECRET_B64": base64.b64encode(JWT_SECRET.encode()).decode(),
        "JWT_ISSUER": JWT_ISSUER,
        "JWT_KID": JWT_KID,
        "JWKS_JSON": jwks_json(),
        "TYK_SECRET": TYK_SECRET,
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all templating/token tests PASS.

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/benchlib/templating.py bench/benchlib/tokens.py bench/tests/test_templating.py
git commit -m "feat(bench): config templating and fixed test credentials"
```

---
### Task 6: Docker CLI wrapper and resource sampler

**Files:**
- Create: `bench/benchlib/docker.py`, `bench/benchlib/sampler.py`, `bench/tests/test_docker.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `benchlib.docker`:
    - `DockerError(Exception)`, `LABEL_KEY = "featherbit-bench"`
    - `ContainerSpec(name, image, cpuset=None, network=None, aliases=(), ports=(), env=(), command=(), entrypoint=None, workdir=None, nofile=1048576)`: frozen; `ports` is a tuple of `(host, container)` and `env` a tuple of `(key, value)`
    - `Docker(context: str | None = None, runner=subprocess.run)` with:
      - `.base()`, `.run(args, *, check=True, timeout=None)`
      - `Docker.create_args(spec)` (static, pure), `.create(spec)`, `.cp_into(name, src: Path, dest: str)`
      - `.start(name)`, `.rm(name)`, `.exec(name, argv, timeout=None) -> str`, `.logs(name, tail=500) -> str`
      - `.ensure_network(name)`, `.has_image(image) -> bool`, `.ensure_image(image, build=None, build_args=None)`, `.image_id(image) -> str`
      - `.info() -> dict`, `.remove_labelled() -> list[str]`, `.stats_command(names) -> list[str]`
  - `benchlib.sampler`:
    - `Sample(t, name, cpu_cores, mem_mb)`
    - `parse_mem_mb(text) -> float`, `parse_stats_line(line, t) -> Sample | None`
    - `median(values) -> float | None`
    - `Sampler(targets: list[tuple[Docker, str]], popen=subprocess.Popen, clock=time.time)` with `.start()` and `.stop() -> list[Sample]`

- [ ] **Step 1: Write the failing tests**

`bench/tests/test_docker.py`:
```python
import subprocess
import unittest
from pathlib import PureWindowsPath

from benchlib.docker import ContainerSpec, Docker, DockerError
from benchlib.sampler import Sampler, median, parse_mem_mb, parse_stats_line


class Recorder:
    def __init__(self, returncode=0, stdout="", stderr=""):
        self.calls = []
        self.result = (returncode, stdout, stderr)

    def __call__(self, argv, **kwargs):
        self.calls.append(argv)
        code, out, err = self.result
        return subprocess.CompletedProcess(argv, code, out, err)


class DockerTests(unittest.TestCase):
    def test_context_flag(self):
        self.assertEqual(Docker("bench-gw").base(), ["docker", "--context", "bench-gw"])
        self.assertEqual(Docker().base(), ["docker"])

    def test_create_args_bridge(self):
        spec = ContainerSpec(name="fbb-gateway", image="kong:3", cpuset="0-3", network="featherbit-bench",
                             aliases=("gateway",), ports=((18000, 8000),), env=(("A", "1"),),
                             command=("kong", "start"), entrypoint="/bin/sh", workdir="/w")
        self.assertEqual(Docker.create_args(spec), [
            "create", "--name", "fbb-gateway", "--label", "featherbit-bench=1",
            "--ulimit", "nofile=1048576:1048576", "--cpuset-cpus", "0-3",
            "--network", "featherbit-bench", "--network-alias", "gateway",
            "-p", "18000:8000", "-e", "A=1", "--entrypoint", "/bin/sh", "-w", "/w",
            "kong:3", "kong", "start",
        ])

    def test_host_network_drops_aliases_and_ports(self):
        spec = ContainerSpec(name="x", image="i", network="host", aliases=("gateway",), ports=((1, 2),))
        args = Docker.create_args(spec)
        self.assertNotIn("--network-alias", args)
        self.assertNotIn("-p", args)

    def test_cp_into_uses_posix_source(self):
        rec = Recorder()
        Docker(runner=rec).cp_into("fbb-gateway", PureWindowsPath(r"C:\Users\me\run\configs\kong\proxy"), "/kong/bench")
        self.assertEqual(rec.calls[0], ["docker", "cp", "C:/Users/me/run/configs/kong/proxy/.", "fbb-gateway:/kong/bench"])

    def test_failure_raises_with_stderr(self):
        with self.assertRaisesRegex(DockerError, "no such image"):
            Docker(runner=Recorder(1, "", "Error: no such image")).run(["pull", "x"])

    def test_rm_ignores_missing(self):
        Docker(runner=Recorder(1, "", "No such container")).rm("fbb-gateway")

    def test_image_id_prefers_repo_digest(self):
        d = Docker(runner=Recorder(0, '["kong@sha256:abc"]|sha256:local\n'))
        self.assertEqual(d.image_id("kong:3"), "kong@sha256:abc")
        d = Docker(runner=Recorder(0, "[]|sha256:local\n"))
        self.assertEqual(d.image_id("mine:dev"), "sha256:local")

    def test_missing_cli(self):
        def runner(argv, **kw):
            raise FileNotFoundError
        with self.assertRaisesRegex(DockerError, "not found"):
            Docker(runner=runner).run(["info"])


class StatsParsingTests(unittest.TestCase):
    def test_mem_units(self):
        self.assertAlmostEqual(parse_mem_mb("12.5MiB / 7.6GiB"), 12.5)
        self.assertAlmostEqual(parse_mem_mb("1.5GiB / 7.6GiB"), 1536.0)
        self.assertAlmostEqual(parse_mem_mb("512KiB / 1GiB"), 0.5)
        self.assertEqual(parse_mem_mb("garbage"), 0.0)

    def test_stats_line_with_ansi_prefix(self):
        line = '\x1b[2J\x1b[H{"Name":"fbb-gateway","CPUPerc":"187.50%","MemUsage":"40MiB / 7.6GiB"}'
        s = parse_stats_line(line, 12.0)
        self.assertEqual((s.name, s.cpu_cores, s.mem_mb, s.t), ("fbb-gateway", 1.875, 40.0, 12.0))

    def test_placeholder_reading_skipped(self):
        self.assertIsNone(parse_stats_line('{"Name":"x","CPUPerc":"--","MemUsage":"-- / --"}', 0))
        self.assertIsNone(parse_stats_line("not json", 0))

    def test_median(self):
        self.assertIsNone(median([]))
        self.assertEqual(median([1.0, 3.0, 2.0]), 2.0)


class FakeProc:
    def __init__(self, lines):
        self.stdout = iter(lines)
        self.terminated = False

    def terminate(self):
        self.terminated = True

    def wait(self, timeout=None):
        return 0


class SamplerTests(unittest.TestCase):
    def test_one_stream_per_context_and_collects_samples(self):
        procs = []

        def popen(argv, **kw):
            names = argv[argv.index("{{json .}}") + 1:]
            p = FakeProc([f'{{"Name":"{n}","CPUPerc":"50.00%","MemUsage":"10MiB / 1GiB"}}\n' for n in names])
            procs.append((argv, p))
            return p

        local, remote = Docker(), Docker("bench-gw")
        s = Sampler([(local, "fbb-upstream"), (local, "fbb-loadgen"), (remote, "fbb-gateway")],
                    popen=popen, clock=lambda: 5.0)
        s.start()
        samples = s.stop()
        self.assertEqual(len(procs), 2)
        self.assertTrue(all(p.terminated for _, p in procs))
        self.assertEqual(sorted(x.name for x in samples), ["fbb-gateway", "fbb-loadgen", "fbb-upstream"])
        self.assertTrue(all(x.cpu_cores == 0.5 and x.t == 5.0 for x in samples))
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'benchlib.docker'`.

- [ ] **Step 3: Implement `benchlib/docker.py`**

```python
"""Thin wrapper over the docker CLI. Every call can target a docker context (remote topology)."""
from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import PurePath

LABEL_KEY = "featherbit-bench"


class DockerError(Exception):
    """A docker CLI call failed."""


@dataclass(frozen=True)
class ContainerSpec:
    name: str
    image: str
    cpuset: str | None = None
    network: str | None = None
    aliases: tuple[str, ...] = ()
    ports: tuple[tuple[int, int], ...] = ()
    env: tuple[tuple[str, str], ...] = ()
    command: tuple[str, ...] = ()
    entrypoint: str | None = None
    workdir: str | None = None
    nofile: int = 1048576


class Docker:
    def __init__(self, context: str | None = None, runner=subprocess.run):
        self.context = context
        self._runner = runner

    def base(self) -> list[str]:
        return ["docker"] + (["--context", self.context] if self.context else [])

    def run(self, args, *, check: bool = True, timeout: float | None = None) -> subprocess.CompletedProcess:
        argv = self.base() + list(args)
        try:
            proc = self._runner(argv, capture_output=True, text=True, encoding="utf-8",
                                errors="replace", timeout=timeout)
        except FileNotFoundError:
            raise DockerError("docker CLI not found on PATH") from None
        except subprocess.TimeoutExpired:
            raise DockerError(f"timed out after {timeout}s: {' '.join(argv)}") from None
        if check and proc.returncode != 0:
            raise DockerError(f"{' '.join(argv)} exited {proc.returncode}: {(proc.stderr or '').strip()[-2000:]}")
        return proc

    @staticmethod
    def create_args(spec: ContainerSpec) -> list[str]:
        a = ["create", "--name", spec.name, "--label", f"{LABEL_KEY}=1",
             "--ulimit", f"nofile={spec.nofile}:{spec.nofile}"]
        if spec.cpuset:
            a += ["--cpuset-cpus", spec.cpuset]
        if spec.network:
            a += ["--network", spec.network]
        if spec.network != "host":
            for alias in spec.aliases:
                a += ["--network-alias", alias]
            for host, container in spec.ports:
                a += ["-p", f"{host}:{container}"]
        for k, v in spec.env:
            a += ["-e", f"{k}={v}"]
        if spec.entrypoint is not None:
            a += ["--entrypoint", spec.entrypoint]
        if spec.workdir:
            a += ["-w", spec.workdir]
        return a + [spec.image, *spec.command]

    def create(self, spec: ContainerSpec) -> None:
        self.run(self.create_args(spec))

    def cp_into(self, name: str, src: PurePath, dest: str) -> None:
        # `<dir>/.` copies the directory's contents; forward slashes work on every platform.
        self.run(["cp", f"{src.as_posix()}/.", f"{name}:{dest}"])

    def start(self, name: str) -> None:
        self.run(["start", name])

    def rm(self, name: str) -> None:
        self.run(["rm", "-f", "-v", name], check=False)

    def exec(self, name: str, argv: list[str], timeout: float | None = None) -> str:
        return self.run(["exec", name, *argv], timeout=timeout).stdout

    def logs(self, name: str, tail: int = 500) -> str:
        p = self.run(["logs", "--tail", str(tail), name], check=False)
        return (p.stdout or "") + (p.stderr or "")

    def ensure_network(self, name: str) -> None:
        if name == "host":
            return
        if self.run(["network", "inspect", name], check=False).returncode != 0:
            self.run(["network", "create", name])

    def has_image(self, image: str) -> bool:
        return self.run(["image", "inspect", image], check=False).returncode == 0

    def ensure_image(self, image: str, build: PurePath | None = None,
                     build_args: dict[str, str] | None = None) -> None:
        if build is not None:
            args = ["build", "-t", image]
            for k, v in (build_args or {}).items():
                args += ["--build-arg", f"{k}={v}"]
            self.run([*args, str(build)], timeout=3600)
        elif not self.has_image(image):
            self.run(["pull", image], timeout=1800)

    def image_id(self, image: str) -> str:
        out = self.run(["image", "inspect", "--format", "{{json .RepoDigests}}|{{.Id}}", image]).stdout.strip()
        digests, _, local_id = out.partition("|")
        repo = json.loads(digests or "[]")
        return repo[0] if repo else local_id

    def info(self) -> dict:
        return json.loads(self.run(["info", "--format", "{{json .}}"]).stdout)

    def remove_labelled(self) -> list[str]:
        ids = self.run(["ps", "-aq", "--filter", f"label={LABEL_KEY}=1"]).stdout.split()
        if ids:
            self.run(["rm", "-f", "-v", *ids])
        return ids

    def stats_command(self, names: list[str]) -> list[str]:
        return self.base() + ["stats", "--format", "{{json .}}", *names]
```

- [ ] **Step 4: Implement `benchlib/sampler.py`**

```python
"""Sample container CPU and memory through streaming `docker stats` (spec §6 step 6)."""
from __future__ import annotations

import json
import re
import statistics
import subprocess
import threading
import time
from dataclasses import dataclass

from .docker import Docker

ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
_MEM = re.compile(r"^\s*([\d.]+)\s*([A-Za-z]+)")
_TO_MB = {"B": 1 / 1048576, "KiB": 1 / 1024, "MiB": 1.0, "GiB": 1024.0, "TiB": 1048576.0,
          "kB": 1e3 / 1048576, "KB": 1e3 / 1048576, "MB": 1e6 / 1048576, "GB": 1e9 / 1048576}


@dataclass(frozen=True)
class Sample:
    t: float
    name: str
    cpu_cores: float
    mem_mb: float


def parse_mem_mb(text: str) -> float:
    m = _MEM.match(text or "")
    if not m or m.group(2) not in _TO_MB:
        return 0.0
    return float(m.group(1)) * _TO_MB[m.group(2)]


def parse_stats_line(line: str, t: float) -> Sample | None:
    line = ANSI.sub("", line).strip()
    start = line.find("{")
    if start < 0:
        return None
    try:
        d = json.loads(line[start:])
        cpu = float(str(d["CPUPerc"]).rstrip("%")) / 100.0
    except (json.JSONDecodeError, KeyError, ValueError):
        return None
    return Sample(t=t, name=d.get("Name", ""), cpu_cores=cpu, mem_mb=parse_mem_mb(d.get("MemUsage", "")))


def median(values) -> float | None:
    values = list(values)
    return statistics.median(values) if values else None


class Sampler:
    def __init__(self, targets: list[tuple[Docker, str]], popen=subprocess.Popen, clock=time.time):
        self._groups: dict[str | None, tuple[Docker, list[str]]] = {}
        for docker, name in targets:
            self._groups.setdefault(docker.context, (docker, []))[1].append(name)
        self._popen = popen
        self._clock = clock
        self._lock = threading.Lock()
        self._samples: list[Sample] = []
        self._procs = []
        self._threads: list[threading.Thread] = []

    def _pump(self, proc) -> None:
        for line in proc.stdout:
            sample = parse_stats_line(line, self._clock())
            if sample:
                with self._lock:
                    self._samples.append(sample)

    def start(self) -> None:
        for docker, names in self._groups.values():
            proc = self._popen(docker.stats_command(names), stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, text=True, encoding="utf-8", errors="replace")
            thread = threading.Thread(target=self._pump, args=(proc,), daemon=True)
            thread.start()
            self._procs.append(proc)
            self._threads.append(thread)

    def stop(self) -> list[Sample]:
        for proc in self._procs:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        for thread in self._threads:
            thread.join(timeout=5)
        self._procs, self._threads = [], []
        with self._lock:
            samples, self._samples = self._samples, []
        return samples
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all docker/sampler tests PASS.

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/benchlib/docker.py bench/benchlib/sampler.py bench/tests/test_docker.py
git commit -m "feat(bench): docker CLI wrapper and stats sampler"
```

---
### Task 7: Probes, the upstream image, the load-generator image

**Files:**
- Create: `bench/benchlib/probes.py`, `bench/tests/test_probes.py`, `bench/upstream/Dockerfile`, `bench/upstream/nginx.conf`, `bench/loadgen/Dockerfile`, `bench/tests/fixtures/wrk_real.txt`, `bench/tests/fixtures/wrk2_real.txt`, `bench/tests/fixtures/oha_real.json`
- Modify: `bench/tests/test_parsers.py` (add real-fixture tests)

**Interfaces:**
- Consumes: `Probe` (Task 1), `render_text` (Task 5), the parsers (Task 2).
- Produces (`benchlib.probes`):
  - `SNI = "bench.local"`
  - `ProbeResult(probe: Probe, ok: bool, detail: str)`
  - `http_call(method, scheme, host, port, path, headers=None, body=None, timeout=5.0) -> tuple[int, dict[str,str], bytes]`: header names come back lowercased
  - `tls_info(host, port, alpn=None, timeout=5.0) -> tuple[str, str | None]`, returning (TLS version, ALPN)
  - `run_probe(probe, scheme, host, port, values, timeout=5.0) -> ProbeResult`, `run_probes(probes, scheme, host, port, values) -> list[ProbeResult]`
  - `wait_healthy(scheme, host, port, path, timeout_s, sleep=time.sleep, clock=time.monotonic) -> bool`: true on any HTTP status < 500
- Produces (images): `featherbit-bench/upstream:dev`, which serves `/…/1k|64k|1m` on 8080 and echoes the X-Bench-* headers; `featherbit-bench/loadgen:dev`, which carries `wrk`, `wrk2`, `oha`, `curl` and `openssl`, with `sleep infinity` as its default command.

- [ ] **Step 1: Write the failing probe tests**

`bench/tests/test_probes.py`:
```python
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from benchlib.config import Probe
from benchlib.probes import run_probe, tls_info, wait_healthy


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/boom":
            self.send_response(503)
            self.end_headers()
            return
        body = b"a" * 1024
        self.send_response(401 if self.path == "/deny" else 200)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Bench-Remove", "1")
        self.send_header("X-Bench-Echo-Key", self.headers.get("apikey", ""))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class ProbeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.port = cls.server.server_address[1]
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def probe(self, p, values=None):
        return run_probe(p, "http", "127.0.0.1", self.port, values or {})

    def test_passing_probe(self):
        r = self.probe(Probe(path="/bench/1k", expect_headers={"x-bench-remove": "1"}, expect_body_bytes=1024))
        self.assertTrue(r.ok, r.detail)

    def test_status_mismatch(self):
        r = self.probe(Probe(path="/deny"))
        self.assertFalse(r.ok)
        self.assertIn("status 401", r.detail)

    def test_reject_status_list(self):
        self.assertTrue(self.probe(Probe(path="/deny", expect_status=(401, 403))).ok)

    def test_absent_header_and_body_size(self):
        r = self.probe(Probe(path="/x", expect_absent=("x-bench-remove",), expect_body_bytes=10))
        self.assertFalse(r.ok)
        self.assertIn("x-bench-remove present", r.detail)
        self.assertIn("body 1024 bytes", r.detail)

    def test_header_tokens_rendered(self):
        r = self.probe(Probe(path="/x", headers={"apikey": "@@API_KEY@@"},
                             expect_headers={"x-bench-echo-key": "k-123"}), {"API_KEY": "k-123"})
        self.assertTrue(r.ok, r.detail)

    def test_refused_connection_fails_cleanly(self):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            free = s.getsockname()[1]
        r = run_probe(Probe(path="/"), "http", "127.0.0.1", free, {})
        self.assertFalse(r.ok)
        self.assertIn("Error", r.detail)

    def test_wait_healthy(self):
        self.assertTrue(wait_healthy("http", "127.0.0.1", self.port, "/", 2))
        t = iter(range(100))
        self.assertFalse(wait_healthy("http", "127.0.0.1", self.port, "/boom", 3,
                                      sleep=lambda s: None, clock=lambda: next(t)))


@unittest.skipUnless(shutil.which("openssl"), "openssl not on PATH")
class TlsProbeTests(unittest.TestCase):
    def test_tls13_and_alpn(self):
        with tempfile.TemporaryDirectory() as d:
            cert, key = Path(d, "c.pem"), Path(d, "k.pem")
            subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt",
                            "ec_paramgen_curve:prime256v1", "-nodes", "-keyout", str(key), "-out", str(cert),
                            "-days", "1", "-subj", "/CN=bench.local"], check=True, capture_output=True)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.minimum_version = ssl.TLSVersion.TLSv1_3
            ctx.load_cert_chain(cert, key)
            ctx.set_alpn_protocols(["h2", "http/1.1"])
            srv = socket.socket()
            srv.bind(("127.0.0.1", 0))
            srv.listen(4)
            port = srv.getsockname()[1]

            def serve():
                for _ in range(2):
                    conn, _ = srv.accept()
                    try:
                        with ctx.wrap_socket(conn, server_side=True):
                            pass
                    except (ssl.SSLError, OSError):
                        pass

            threading.Thread(target=serve, daemon=True).start()
            self.assertEqual(tls_info("127.0.0.1", port)[0], "TLSv1.3")
            self.assertEqual(tls_info("127.0.0.1", port, alpn=["h2", "http/1.1"])[1], "h2")
            srv.close()
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'benchlib.probes'`.

- [ ] **Step 3: Implement `benchlib/probes.py`**

```python
"""Correctness probes (spec §6 step 2) and the boot health wait (step 1)."""
from __future__ import annotations

import http.client
import socket
import ssl
import time
from dataclasses import dataclass

from .config import Probe
from .templating import render_text

SNI = "bench.local"


@dataclass(frozen=True)
class ProbeResult:
    probe: Probe
    ok: bool
    detail: str


def _ctx(alpn: list[str] | None = None) -> ssl.SSLContext:
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    if alpn:
        ctx.set_alpn_protocols(alpn)
    return ctx


def http_call(method: str, scheme: str, host: str, port: int, path: str,
              headers: dict[str, str] | None = None, body: bytes | None = None,
              timeout: float = 5.0) -> tuple[int, dict[str, str], bytes]:
    if scheme == "https":
        conn = http.client.HTTPSConnection(host, port, timeout=timeout, context=_ctx())
    else:
        conn = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        conn.request(method, path, body=body, headers={"Host": SNI, **(headers or {})})
        resp = conn.getresponse()
        data = resp.read()
        return resp.status, {k.lower(): v for k, v in resp.getheaders()}, data
    finally:
        conn.close()


def tls_info(host: str, port: int, alpn: list[str] | None = None,
             timeout: float = 5.0) -> tuple[str, str | None]:
    with socket.create_connection((host, port), timeout=timeout) as raw:
        with _ctx(alpn).wrap_socket(raw, server_hostname=SNI) as s:
            return s.version(), s.selected_alpn_protocol()


def run_probe(probe: Probe, scheme: str, host: str, port: int, values: dict[str, str],
              timeout: float = 5.0) -> ProbeResult:
    try:
        if probe.kind == "tls13":
            version, _ = tls_info(host, port, timeout=timeout)
            return ProbeResult(probe, version == "TLSv1.3", f"negotiated {version}")
        if probe.kind == "alpn-h2":
            _, alpn = tls_info(host, port, alpn=["h2", "http/1.1"], timeout=timeout)
            return ProbeResult(probe, alpn == "h2", f"ALPN {alpn}")
        headers = {k: render_text(v, values) for k, v in probe.headers.items()}
        status, got, body = http_call("GET", scheme, host, port, probe.path, headers, timeout=timeout)
    except (OSError, http.client.HTTPException) as e:
        return ProbeResult(probe, False, f"{type(e).__name__}: {e}")
    problems = []
    if status not in probe.expect_status:
        problems.append(f"status {status}, expected {list(probe.expect_status)}")
    for name, want in probe.expect_headers.items():
        if got.get(name) != want:
            problems.append(f"header {name}={got.get(name)!r}, expected {want!r}")
    for name in probe.expect_absent:
        if name in got:
            problems.append(f"header {name} present, expected absent")
    if probe.expect_body_bytes is not None and len(body) != probe.expect_body_bytes:
        problems.append(f"body {len(body)} bytes, expected {probe.expect_body_bytes}")
    return ProbeResult(probe, not problems, "; ".join(problems) or f"status {status}")


def run_probes(probes, scheme: str, host: str, port: int, values: dict[str, str]) -> list[ProbeResult]:
    return [run_probe(p, scheme, host, port, values) for p in probes]


def wait_healthy(scheme: str, host: str, port: int, path: str, timeout_s: float,
                 sleep=time.sleep, clock=time.monotonic) -> bool:
    """True once the listener answers anything below 500 (404 counts: the process is up)."""
    deadline = clock() + timeout_s
    while True:
        try:
            status, _, _ = http_call("GET", scheme, host, port, path, timeout=2.0)
            if status < 500:
                return True
        except (OSError, http.client.HTTPException):
            pass
        if clock() >= deadline:
            return False
        sleep(0.5)
```
(`ssl.SSLError` subclasses `OSError`, so it's already covered.)

- [ ] **Step 4: Run the probe tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: probe tests PASS. `TlsProbeTests` is skipped only if `openssl` is not on PATH; Git for Windows ships it.

- [ ] **Step 5: Pin nginx and write the upstream image**

Resolve the current stable nginx alpine tag. It's the `stable-alpine` line, e.g. `1.28.x-alpine`; check hub.docker.com/_/nginx. Then:
```bash
docker pull nginx:<TAG>-alpine
docker image inspect --format '{{index .RepoDigests 0}}' nginx:<TAG>-alpine
```

`bench/upstream/Dockerfile` (substitute the resolved tag and digest):
```dockerfile
# Shared benchmark upstream: stock nginx serving fixed-size bodies from memory
# and echoing the X-Bench-* request headers back, so probes can observe
# request-side rewrites from the client. Worker count is set at run time
# (`-g "worker_processes N;"`) to the upstream's cpuset size.
FROM nginx:<TAG>-alpine@sha256:<DIGEST>
RUN mkdir -p /srv/bench \
 && head -c 1024 /dev/zero | tr '\0' 'a' > /srv/bench/1k \
 && head -c 65536 /dev/zero | tr '\0' 'a' > /srv/bench/64k \
 && head -c 1048576 /dev/zero | tr '\0' 'a' > /srv/bench/1m
COPY nginx.conf /etc/nginx/nginx.conf
EXPOSE 8080
```

`bench/upstream/nginx.conf`:
```nginx
# worker_processes is passed on the command line by the orchestrator.
worker_rlimit_nofile 1048576;
error_log /dev/stderr warn;

events {
    worker_connections 65535;
    multi_accept on;
}

http {
    access_log off;
    sendfile on;
    tcp_nopush on;
    tcp_nodelay on;
    keepalive_timeout 300s;
    keepalive_requests 1000000;
    open_file_cache max=16 inactive=600s;
    open_file_cache_valid 600s;

    server {
        listen 8080 reuseport backlog=65535;
        root /srv/bench;

        add_header X-Bench-Remove 1 always;
        add_header X-Bench-Echo-Added $http_x_bench_added always;
        add_header X-Bench-Echo-Added2 $http_x_bench_added2 always;
        add_header X-Bench-Echo-Script $http_x_bench_script always;

        # Any path ending in /1k, /64k or /1m: /bench/1k, /r999/1k, /limited/1k...
        location ~ /(1k|64k|1m)$ {
            default_type application/octet-stream;
            try_files /$1 =404;
        }
    }
}
```
nginx drops `add_header` lines whose value is empty, so the echo headers only appear when the gateway sent the request header.

- [ ] **Step 6: Build and check the upstream**

```bash
docker build -t featherbit-bench/upstream:dev bench/upstream
docker run -d --rm --name fbb-upstream-check -p 18080:8080 featherbit-bench/upstream:dev nginx -g "daemon off; worker_processes 2;"
curl -s -o /dev/null -w "%{http_code} %{size_download}\n" http://127.0.0.1:18080/bench/64k
curl -s -D - -o /dev/null -H "X-Bench-Added: 1" http://127.0.0.1:18080/r999/1k
docker rm -f fbb-upstream-check
```
Expected: `200 65536`. The second command's headers include `X-Bench-Remove: 1` and `X-Bench-Echo-Added: 1`, and **no** `X-Bench-Echo-Script`.

- [ ] **Step 7: Write the load-generator image**

Pin the three tools:
- **wrk**: tag `4.2.0`.
- **wrk2**: the `giltene/wrk2` master commit. Confirm it with `git ls-remote https://github.com/giltene/wrk2 HEAD`; at the time of writing it was `44a94c17d8e6a0bac8559b53da76848e430cb7a7`.
- **oha**: the latest release from github.com/hatoo/oha/releases. Confirm its asset is named `oha-linux-amd64`.

Pin the Debian base the same way as nginx:
```bash
docker pull debian:bookworm-slim
docker image inspect --format '{{index .RepoDigests 0}}' debian:bookworm-slim
```

`bench/loadgen/Dockerfile`:
```dockerfile
# Load generators, pinned: wrk (throughput ceiling), wrk2 (fixed-rate,
# coordinated-omission-corrected HdrHistogram latency), oha (HTTP/2, with
# --latency-correction). Also curl (upstream readiness) and openssl (bench
# certificates). linux/amd64 only: wrk2's bundled LuaJIT does not build on arm64.
FROM debian:bookworm-slim@sha256:<DIGEST> AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential git ca-certificates libssl-dev zlib1g-dev unzip \
 && rm -rf /var/lib/apt/lists/*
ARG WRK_REF=4.2.0
ARG WRK2_REF=44a94c17d8e6a0bac8559b53da76848e430cb7a7
RUN git clone https://github.com/wg/wrk.git /src/wrk \
 && git -C /src/wrk checkout "${WRK_REF}" \
 && make -C /src/wrk -j"$(nproc)" WITH_OPENSSL=/usr \
 && cp /src/wrk/wrk /usr/local/bin/wrk
RUN git clone https://github.com/giltene/wrk2.git /src/wrk2 \
 && git -C /src/wrk2 checkout "${WRK2_REF}" \
 && make -C /src/wrk2 -j"$(nproc)" \
 && cp /src/wrk2/wrk /usr/local/bin/wrk2

FROM debian:bookworm-slim@sha256:<DIGEST>
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl libssl3 openssl zlib1g \
 && rm -rf /var/lib/apt/lists/*
ARG OHA_VERSION=<OHA_VERSION>
ADD https://github.com/hatoo/oha/releases/download/v${OHA_VERSION}/oha-linux-amd64 /usr/local/bin/oha
RUN chmod 0755 /usr/local/bin/oha
COPY --from=build /usr/local/bin/wrk /usr/local/bin/wrk2 /usr/local/bin/
CMD ["sleep", "infinity"]
```
If the wrk2 build fails against OpenSSL 3 or a modern GCC, fix it in this Dockerfile, for example with `CFLAGS=-fcommon`, or a `sed` on the Makefile. Keep the pin, and note the fix in a comment.

- [ ] **Step 8: Build and check the tools; confirm oha's flags**

```bash
docker build -t featherbit-bench/loadgen:dev bench/loadgen
docker run --rm featherbit-bench/loadgen:dev sh -c "wrk -v; wrk2 -v; oha --version; oha --help | grep -E -- '--latency-correction|--output-format|--http2|-p,|--no-tui'"
```
Expected: all three tools print a version, and each flag in the grep appears. The driver (Task 9) calls oha as `-z <s>s -c <n> -p 4 --http2 --insecure --no-tui --output-format json [-q RATE --latency-correction]`. If the pinned oha spells a flag differently (older releases used `-j`/`--json`), write down the actual spelling now; Task 9's `load()` must use it.

- [ ] **Step 9: Capture real tool output as parser fixtures**

```bash
docker network create fbb-check
docker run -d --rm --name fbb-up --network fbb-check --network-alias upstream featherbit-bench/upstream:dev nginx -g "daemon off; worker_processes 2;"
docker run --rm --network fbb-check featherbit-bench/loadgen:dev wrk -t2 -c16 -d3s --latency http://upstream:8080/bench/1k > bench/tests/fixtures/wrk_real.txt
docker run --rm --network fbb-check featherbit-bench/loadgen:dev wrk2 -t2 -c16 -d5s -R2000 --latency http://upstream:8080/bench/1k > bench/tests/fixtures/wrk2_real.txt
docker run --rm --network fbb-check featherbit-bench/loadgen:dev oha -z 3s -c 16 -q 500 --latency-correction --no-tui --output-format json http://upstream:8080/bench/1k > bench/tests/fixtures/oha_real.json
docker rm -f fbb-up && docker network rm fbb-check
```
(oha runs over plain HTTP/1.1 here because the upstream has no TLS; the JSON shape is the same.)

Append to `bench/tests/test_parsers.py`:
```python
from pathlib import Path

FIXTURES = Path(__file__).resolve().parent / "fixtures"


class RealOutputTests(unittest.TestCase):
    """Output captured from the pinned tools (Task 7). If a tool is re-pinned, re-capture."""

    def check(self, r):
        self.assertGreater(r.requests, 0)
        self.assertGreater(r.rps, 0)
        self.assertIsNotNone(r.p50_ms)
        self.assertIsNotNone(r.p99_ms)
        self.assertLessEqual(r.p50_ms, r.p99_ms)

    def test_wrk_real(self):
        self.check(parse_wrk((FIXTURES / "wrk_real.txt").read_text("utf-8")))

    def test_wrk2_real(self):
        r = parse_wrk2((FIXTURES / "wrk2_real.txt").read_text("utf-8"))
        self.check(r)
        self.assertIsNotNone(r.p999_ms)

    def test_oha_real(self):
        self.check(parse_oha((FIXTURES / "oha_real.json").read_text("utf-8")))
```

- [ ] **Step 10: Run all tests**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all PASS. If a `RealOutputTests` case fails, the Task 2 parser disagrees with the real tool. Fix the parser (and its synthetic sample) until both pass.

- [ ] **Step 11: Commit** (only if authorized)

```bash
git add bench/benchlib/probes.py bench/tests/test_probes.py bench/tests/test_parsers.py bench/tests/fixtures bench/upstream bench/loadgen
git commit -m "feat(bench): probes, shared upstream image and pinned load-generator image"
```

---
### Task 8: Cell runner, certificates, host fingerprint

**Files:**
- Create: `bench/benchlib/runner.py`, `bench/benchlib/certs.py`, `bench/benchlib/fingerprint.py`, `bench/tests/test_runner.py`

**Interfaces:**
- Consumes:
  - `Params`, `Topology`, `Role`, `Profile`, `Probe` (Task 1)
  - `LoadResult` (Task 2)
  - `Step`, `find_max_sustainable`, `ladder_rates`, and the status helpers (Task 3)
  - `Cell` (Task 4)
  - `Docker`, `DockerError`, `Sample`, `median` (Task 6)
  - `ProbeResult` (Task 7)
- Produces:
  - `benchlib.runner`:
    - `BootError(Exception)`, `LoadError(Exception)`
    - the `Driver` protocol, which Task 9's `DockerDriver` implements exactly:
      - `boot(cell, rep) -> None`, which raises `BootError`
      - `setup_and_probe(cell) -> list[ProbeResult]`
      - `load(cell, rate: int | None, seconds: int) -> LoadResult`, which raises `LoadError`; `rate=None` means an unpaced flood
      - `start_sampling(cell) -> None`
      - `stop_sampling() -> dict[str, list[Sample]]`, keyed by role (`gateway`, `upstream`, `loadgen`)
      - `cores(cell, role) -> int`
      - `logs() -> dict[str, str]`
      - `teardown() -> None`
    - `CellRunner(driver, params, clock=time.time)` with `.run_rep(cell, rep) -> dict` and `.validate(cell) -> dict`
  - `benchlib.certs`: `Certs(cert_pem, key_pem)`, `split_pem(text) -> Certs`, `generate_certs(docker, image) -> Certs`
  - `benchlib.fingerprint`: `parse_host_probe(text) -> dict`, `host_fingerprint(docker, image) -> dict`, `git_info(repo, runner=subprocess.run) -> dict`, `publish_blockers(topology, hosts: dict[str, dict]) -> list[str]`

The repetition result dict, as written into `run.json` by `RunStore.record_rep`, has these keys:
- `status`, `probes` (a list of `{path, kind, ok, detail}`)
- `ceiling_rps`, `max_sustainable_rps`
- `search` and `ladder`, both lists of points; each point holds `rate, passed, achieved_rps, p50_ms, p90_ms, p99_ms, p999_ms, max_ms, requests, errors, gateway_cpu_cores, gateway_rss_peak_mb, loadgen_cpu_frac, upstream_cpu_frac, rps_per_core`
- `flags`
- On failure: `error` and `logs`

- [ ] **Step 1: Write the failing tests**

`bench/tests/test_runner.py`:
```python
import subprocess
import unittest
from pathlib import Path

from benchlib.certs import generate_certs, split_pem
from benchlib.classify import ERROR, INVALID, LOADGEN_BOUND, OK
from benchlib.config import Params, Probe, Profile, Role, Topology
from benchlib.docker import Docker, DockerError
from benchlib.fingerprint import git_info, parse_host_probe, publish_blockers
from benchlib.matrix import Cell
from benchlib.parsers import LoadResult
from benchlib.probes import ProbeResult
from benchlib.runner import BootError, CellRunner, LoadError
from benchlib.sampler import Sample

CELL = Cell("featherbit", "core.proxy", 1)


class FakeDriver:
    def __init__(self, capacity=20_000, boot_error=None, probe_ok=True, load_error=False, loadgen_cpu=0.5):
        self.capacity = capacity
        self.boot_error = boot_error
        self.probe_ok = probe_ok
        self.load_error = load_error
        self.loadgen_cpu = loadgen_cpu
        self.now = 0.0
        self.events = []
        self.samples = {"gateway": [], "upstream": [], "loadgen": []}

    def boot(self, cell, rep):
        self.events.append("boot")
        if self.boot_error:
            raise BootError(self.boot_error)

    def setup_and_probe(self, cell):
        self.events.append("probe")
        return [ProbeResult(Probe(path="/bench/1k"), self.probe_ok, "status 200" if self.probe_ok else "status 404")]

    def load(self, cell, rate, seconds):
        self.events.append(("load", rate))
        if self.load_error and rate is not None:
            raise LoadError("wrk2 exited 1")
        t0 = self.now
        self.now += seconds
        for role, cpu in (("gateway", 0.8), ("upstream", 0.4), ("loadgen", self.loadgen_cpu)):
            self.samples[role].append(Sample(t0 + seconds / 2, role, cpu, 40.0))
        if rate is None:
            return LoadResult(10_000, seconds, self.capacity * 1.05, 0, 0, 1.0, 1.0, 1.0, 1.0, 1.0)
        ok = rate <= self.capacity
        return LoadResult(rate * seconds, seconds, float(min(rate, self.capacity)), 0, 0,
                          1.0, 1.5, 2.0 if ok else 80.0, 3.0, 5.0)

    def start_sampling(self, cell):
        self.events.append("sample-start")

    def stop_sampling(self):
        self.events.append("sample-stop")
        return self.samples

    def cores(self, cell, role):
        return 2

    def logs(self):
        return {"fbb-gateway": "log text"}

    def teardown(self):
        self.events.append("teardown")


def runner(driver):
    return CellRunner(driver, Params(), clock=lambda: driver.now)


class RunRepTests(unittest.TestCase):
    def test_happy_path(self):
        d = FakeDriver()
        r = runner(d).run_rep(CELL, 0)
        self.assertEqual(r["status"], OK)
        self.assertTrue(19_600 <= r["max_sustainable_rps"] <= 20_000)
        self.assertEqual([p["rate"] for p in r["ladder"]], [1000, 5000, 10000])
        self.assertAlmostEqual(r["ladder"][0]["rps_per_core"], 1000 / 0.8)
        self.assertAlmostEqual(r["ladder"][0]["loadgen_cpu_frac"], 0.25)
        self.assertEqual(r["ladder"][0]["gateway_rss_peak_mb"], 40.0)
        self.assertEqual(d.events[0], "boot")
        self.assertEqual(d.events[-1], "teardown")
        # flood (None), then warm-up at 20% of the flood ceiling, then sampling starts
        self.assertEqual(d.events[2:5], [("load", None), ("load", round(21_000 * 0.2)), "sample-start"])

    def test_boot_error(self):
        d = FakeDriver(boot_error="not answering")
        r = runner(d).run_rep(CELL, 0)
        self.assertEqual(r["status"], ERROR)
        self.assertEqual(r["error"], "boot: not answering")
        self.assertEqual(r["logs"], {"fbb-gateway": "log text"})
        self.assertEqual(d.events, ["boot", "teardown"])

    def test_probe_failure_is_invalid_and_never_measured(self):
        d = FakeDriver(probe_ok=False)
        r = runner(d).run_rep(CELL, 0)
        self.assertEqual(r["status"], INVALID)
        self.assertFalse(r["probes"][0]["ok"])
        self.assertFalse(any(isinstance(e, tuple) for e in d.events))
        self.assertEqual(d.events[-1], "teardown")

    def test_loadgen_bound(self):
        r = runner(FakeDriver(loadgen_cpu=1.9)).run_rep(CELL, 0)
        self.assertEqual(r["status"], LOADGEN_BOUND)
        self.assertIn(LOADGEN_BOUND, r["flags"])

    def test_load_error_marks_rep_error(self):
        d = FakeDriver(load_error=True)
        r = runner(d).run_rep(CELL, 0)
        self.assertEqual(r["status"], ERROR)
        self.assertTrue(r["error"].startswith("load: "))
        self.assertIn("sample-stop", d.events)
        self.assertEqual(d.events[-1], "teardown")

    def test_validate(self):
        d = FakeDriver()
        self.assertEqual(runner(d).validate(CELL)["status"], OK)
        self.assertEqual(d.events, ["boot", "probe", "teardown"])
        self.assertEqual(runner(FakeDriver(probe_ok=False)).validate(CELL)["status"], INVALID)


PEM = """-----BEGIN CERTIFICATE-----
MIIB
-----END CERTIFICATE-----
-----BEGIN PRIVATE KEY-----
MIGH
-----END PRIVATE KEY-----
"""


class CertTests(unittest.TestCase):
    def test_split_pem(self):
        c = split_pem(PEM)
        self.assertTrue(c.cert_pem.startswith("-----BEGIN CERTIFICATE-----"))
        self.assertTrue(c.key_pem.rstrip().endswith("-----END PRIVATE KEY-----"))

    def test_split_pem_missing_key(self):
        with self.assertRaises(DockerError):
            split_pem(PEM.split("-----BEGIN PRIVATE")[0])

    def test_generate_runs_openssl_in_image(self):
        calls = []

        def run(argv, **kw):
            calls.append(argv)
            return subprocess.CompletedProcess(argv, 0, PEM, "")

        generate_certs(Docker(runner=run), "featherbit-bench/loadgen:dev")
        self.assertEqual(calls[0][:6], ["docker", "run", "--rm", "--entrypoint", "sh", "featherbit-bench/loadgen:dev"])
        self.assertIn("prime256v1", calls[0][-1])


def topo(kind, network):
    roles = {r: Role(address=r) for r in ("loadgen", "gateway", "upstream")}
    return Topology(name="t", kind=kind, network=network, roles=roles,
                    profiles={1: Profile(1, "0", "1", "2")})


class FingerprintTests(unittest.TestCase):
    def test_parse_host_probe(self):
        fp = parse_host_probe(" AMD EPYC 9R14\nperformance\n0\n0.10 0.20 0.30\n")
        self.assertEqual(fp, {"cpu_model": "AMD EPYC 9R14", "governor": "performance",
                              "turbo": "0", "loadavg": "0.10 0.20 0.30"})
        self.assertEqual(parse_host_probe("x\n")["governor"], "unknown")

    def test_publish_blockers(self):
        good = {"os_type": "linux", "os": "Ubuntu 24.04", "governor": "performance"}
        self.assertEqual(publish_blockers(topo("remote", "host"), {"gw": good}), [])
        laptop = {"os_type": "linux", "os": "Docker Desktop", "governor": "unknown"}
        blockers = publish_blockers(topo("local", "featherbit-bench"), {"local": laptop})
        self.assertEqual(len(blockers), 4)

    def test_git_info(self):
        def run(argv, **kw):
            out = "abc123\n" if "rev-parse" in argv else " M src/main.rs\n"
            return subprocess.CompletedProcess(argv, 0, out, "")
        self.assertEqual(git_info(Path("."), runner=run), {"sha": "abc123", "dirty": True})
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'benchlib.certs'`.

- [ ] **Step 3: Implement `benchlib/runner.py`**

```python
"""One benchmark repetition of one cell: the measurement procedure of spec §6."""
from __future__ import annotations

import time
from typing import Protocol

from .classify import ERROR, INVALID, OK, rep_status, saturation_flags
from .config import Params
from .matrix import Cell
from .parsers import LoadResult
from .probes import ProbeResult
from .sampler import Sample, median
from .search import Step, find_max_sustainable, ladder_rates, passes


class BootError(Exception):
    """The gateway (or the upstream) did not come up."""


class LoadError(Exception):
    """A load tool failed or produced unparseable output."""


class Driver(Protocol):
    def boot(self, cell: Cell, rep: int) -> None: ...
    def setup_and_probe(self, cell: Cell) -> list[ProbeResult]: ...
    def load(self, cell: Cell, rate: int | None, seconds: int) -> LoadResult: ...
    def start_sampling(self, cell: Cell) -> None: ...
    def stop_sampling(self) -> dict[str, list[Sample]]: ...
    def cores(self, cell: Cell, role: str) -> int: ...
    def logs(self) -> dict[str, str]: ...
    def teardown(self) -> None: ...


def _probe_dict(r: ProbeResult) -> dict:
    return {"path": r.probe.path, "kind": r.probe.kind, "ok": r.ok, "detail": r.detail}


class CellRunner:
    def __init__(self, driver: Driver, params: Params, clock=time.time):
        self.driver = driver
        self.params = params
        self.clock = clock

    def _fail(self, message: str) -> dict:
        return {"status": ERROR, "error": message, "logs": self.driver.logs()}

    def validate(self, cell: Cell) -> dict:
        """Boot + probes only (bench.py validate)."""
        try:
            self.driver.boot(cell, 0)
        except BootError as e:
            result = self._fail(f"boot: {e}")
            self.driver.teardown()
            return result
        try:
            probes = self.driver.setup_and_probe(cell)
            return {"status": OK if all(r.ok for r in probes) else INVALID,
                    "probes": [_probe_dict(r) for r in probes]}
        finally:
            self.driver.teardown()

    def run_rep(self, cell: Cell, rep: int) -> dict:
        p = self.params
        try:
            self.driver.boot(cell, rep)
        except BootError as e:
            result = self._fail(f"boot: {e}")
            self.driver.teardown()
            return result
        try:
            probes = self.driver.setup_and_probe(cell)
            probe_list = [_probe_dict(r) for r in probes]
            if not all(r.ok for r in probes):
                return {"status": INVALID, "probes": probe_list}
            ceiling = self.driver.load(cell, None, p.ceiling_seconds).rps
            if ceiling > 0:
                self.driver.load(cell, max(1, round(ceiling * p.warmup_fraction)), p.warmup_seconds)
            self.driver.start_sampling(cell)
            try:
                search = find_max_sustainable(
                    lambda rate: self._step(cell, rate, p.search_step_seconds),
                    ceiling, p.search_low, p.search_high, p.search_tolerance)
                ladder = ([self._step(cell, rate, p.ladder_seconds)
                           for rate in ladder_rates(p.ladder, search.max_sustainable)]
                          if search.max_sustainable > 0 else [])
            finally:
                samples = self.driver.stop_sampling()
            search_points = [self._point(cell, s, samples) for s in search.steps]
            ladder_points = [self._point(cell, s, samples) for s in ladder]
            final = [pt for pt in search_points
                     if pt["passed"] and pt["rate"] == search.max_sustainable][-1:]
            flags = set(search.flags)
            for pt in final + ladder_points:
                flags.update(saturation_flags(pt["loadgen_cpu_frac"], pt["upstream_cpu_frac"],
                                              p.saturation_threshold))
            return {
                "status": rep_status(sorted(flags)),
                "probes": probe_list,
                "ceiling_rps": ceiling,
                "max_sustainable_rps": search.max_sustainable,
                "search": search_points,
                "ladder": ladder_points,
                "flags": sorted(flags),
            }
        except LoadError as e:
            return self._fail(f"load: {e}")
        finally:
            self.driver.teardown()

    def _step(self, cell: Cell, rate: int, seconds: int) -> Step:
        t0 = self.clock()
        result = self.driver.load(cell, rate, seconds)
        t1 = self.clock()
        return Step(rate=rate, result=result,
                    passed=passes(result, self.params.slo_p99_ms, self.params.max_error_rate),
                    t_start=t0, t_end=t1)

    def _point(self, cell: Cell, step: Step, samples: dict[str, list[Sample]]) -> dict:
        def window(role: str) -> list[Sample]:
            return [s for s in samples.get(role, []) if step.t_start <= s.t <= step.t_end]

        gw = window("gateway")
        gw_cpu = median(s.cpu_cores for s in gw)
        lg_cpu = median(s.cpu_cores for s in window("loadgen"))
        up_cpu = median(s.cpu_cores for s in window("upstream"))
        r = step.result
        return {
            "rate": step.rate, "passed": step.passed, "achieved_rps": r.rps,
            "p50_ms": r.p50_ms, "p90_ms": r.p90_ms, "p99_ms": r.p99_ms, "p999_ms": r.p999_ms,
            "max_ms": r.max_ms, "requests": r.requests, "errors": r.errors + r.non2xx,
            "gateway_cpu_cores": gw_cpu,
            "gateway_rss_peak_mb": max((s.mem_mb for s in gw), default=None),
            "loadgen_cpu_frac": None if lg_cpu is None else lg_cpu / self.driver.cores(cell, "loadgen"),
            "upstream_cpu_frac": None if up_cpu is None else up_cpu / self.driver.cores(cell, "upstream"),
            "rps_per_core": r.rps / gw_cpu if gw_cpu else None,
        }
```

- [ ] **Step 4: Implement `benchlib/certs.py`**

```python
"""A throwaway self-signed ECDSA P-256 certificate per run, generated inside the loadgen image."""
from __future__ import annotations

import re
from dataclasses import dataclass

from .docker import Docker, DockerError

_CERT = re.compile(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----\n?", re.S)
_KEY = re.compile(r"-----BEGIN (?:EC )?PRIVATE KEY-----.*?-----END (?:EC )?PRIVATE KEY-----\n?", re.S)
CERT_SCRIPT = (
    "openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes "
    "-keyout /tmp/key.pem -out /tmp/cert.pem -days 30 -subj /CN=bench.local "
    "-addext subjectAltName=DNS:bench.local,DNS:gateway,IP:127.0.0.1 2>/dev/null "
    "&& cat /tmp/cert.pem /tmp/key.pem"
)


@dataclass(frozen=True)
class Certs:
    cert_pem: str
    key_pem: str


def split_pem(text: str) -> Certs:
    cert, key = _CERT.search(text), _KEY.search(text)
    if not cert or not key:
        raise DockerError("certificate generation did not print a certificate and a private key")
    return Certs(cert_pem=cert.group(0), key_pem=key.group(0))


def generate_certs(docker: Docker, image: str) -> Certs:
    return split_pem(docker.run(["run", "--rm", "--entrypoint", "sh", image, "-c", CERT_SCRIPT]).stdout)
```

- [ ] **Step 5: Implement `benchlib/fingerprint.py`**

```python
"""Environment fingerprint recorded in run.json (spec §6), and the --publish gate (spec §4.4)."""
from __future__ import annotations

import subprocess
from pathlib import Path

from .config import Topology
from .docker import Docker

FP_SCRIPT = (
    "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2; "
    "cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown; "
    "cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null "
    "|| cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null || echo unknown; "
    "cut -d' ' -f1-3 /proc/loadavg"
)
_FIELDS = ("cpu_model", "governor", "turbo", "loadavg")


def parse_host_probe(text: str) -> dict:
    lines = [l.strip() for l in text.strip().splitlines()]
    lines += ["unknown"] * (len(_FIELDS) - len(lines))
    return dict(zip(_FIELDS, lines))


def host_fingerprint(docker: Docker, image: str) -> dict:
    info = docker.info()
    probe = docker.run(["run", "--rm", "--entrypoint", "sh", image, "-c", FP_SCRIPT]).stdout
    return {
        "docker": info.get("ServerVersion"), "os": info.get("OperatingSystem"),
        "os_type": info.get("OSType"), "kernel": info.get("KernelVersion"),
        "arch": info.get("Architecture"), "ncpu": info.get("NCPU"), "mem_bytes": info.get("MemTotal"),
        **parse_host_probe(probe),
    }


def git_info(repo: Path, runner=subprocess.run) -> dict:
    def git(*args: str) -> str:
        return runner(["git", "-C", str(repo), *args], capture_output=True, text=True).stdout.strip()
    return {"sha": git("rev-parse", "HEAD"), "dirty": bool(git("status", "--porcelain", "--untracked-files=no"))}


def publish_blockers(topology: Topology, hosts: dict[str, dict]) -> list[str]:
    blockers = []
    if topology.kind != "remote":
        blockers.append("topology is not 'remote' (one Linux host per role)")
    if topology.network != "host":
        blockers.append("containers do not use host networking")
    for name, fp in hosts.items():
        if fp.get("os_type") != "linux":
            blockers.append(f"{name}: not a Linux docker host ({fp.get('os_type')})")
        if "Docker Desktop" in str(fp.get("os", "")):
            blockers.append(f"{name}: Docker Desktop runs in a VM, not on bare Linux")
        if fp.get("governor") != "performance":
            blockers.append(f"{name}: CPU governor is {fp.get('governor')!r}, not 'performance'")
    return blockers
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all runner/cert/fingerprint tests PASS.

- [ ] **Step 7: Commit** (only if authorized)

```bash
git add bench/benchlib/runner.py bench/benchlib/certs.py bench/benchlib/fingerprint.py bench/tests/test_runner.py
git commit -m "feat(bench): per-repetition cell runner, run certificates, host fingerprint"
```

---
### Task 9: Docker driver, CLI, the `direct` baseline and the `nginx` ceiling, first smoke run

**Files:**
- Create: `bench/benchlib/driver.py`, `bench/benchlib/cli.py`, `bench/bench.py`, `bench/tests/test_driver.py`, `bench/tests/test_cli.py`
- Create: `bench/gateways/direct/gateway.toml`
- Create: `bench/gateways/nginx/gateway.toml`, `bench/gateways/nginx/TUNING.md`, `bench/gateways/nginx/proxy/nginx.conf`, `bench/gateways/nginx/routes-1k/nginx.conf`, `bench/gateways/nginx/rate-limit/nginx.conf`, `bench/gateways/nginx/header-rewrite/nginx.conf`

**Interfaces:**
- Consumes: everything from Tasks 1–8. `report.render.write_report` is imported lazily by `cmd_report` and lands in Task 16. Until then, `bench.py report` fails with an ImportError, and that's expected.
- Produces:
  - `benchlib.driver`:
    - `Images(loadgen="featherbit-bench/loadgen:dev", upstream="featherbit-bench/upstream:dev")`
    - `NAMES = {"gateway": "fbb-gateway", "upstream": "fbb-upstream", "loadgen": "fbb-loadgen"}`
    - `DockerDriver(topology, gateways, scenarios, params, images, gateway_images: dict[str,str], certs, store, docker_factory=Docker, health=wait_healthy, probes=run_probes, http=http_call, sampler_factory=Sampler, sleep=time.sleep)`, which implements the Task 8 `Driver` protocol. It also exposes `.values(cell) -> dict[str,str]`, `.load_url(cell) -> str` and `.probe_target(cell) -> tuple[str, int]`.
  - `benchlib.cli`:
    - `main(argv) -> int`
    - `estimate_rep_seconds(params) -> int`
    - `parse_image_overrides(items: list[str], gateways) -> dict[str,str]`
    - `topology_path(arg) -> Path`
  - `bench/bench.py`: the executable shim.

- [ ] **Step 1: Write the failing driver tests**

`bench/tests/test_driver.py`:
```python
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from benchlib.certs import Certs
from benchlib.config import Params, load_scenarios, load_topology, load_gateways
from benchlib.driver import DockerDriver, Images
from benchlib.matrix import Cell
from benchlib.results import RunStore

BENCH = Path(__file__).resolve().parent.parent


class FakeDocker:
    log = []

    def __init__(self, context=None):
        self.context = context

    def _rec(self, *call):
        FakeDocker.log.append((self.context, *call))

    def rm(self, name): self._rec("rm", name)
    def ensure_network(self, name): self._rec("network", name)
    def create(self, spec): self._rec("create", spec)
    def cp_into(self, name, src, dest): self._rec("cp", name, Path(src), dest)
    def start(self, name): self._rec("start", name)
    def logs(self, name, tail=500): return f"logs of {name}"

    def exec(self, name, argv, timeout=None):
        self._rec("exec", name, tuple(argv))
        if argv[0] == "curl":
            return "200"
        return WRK2_OUT


WRK2_OUT = """  Latency Distribution (HdrHistogram - Recorded Latency)
 50.000%    1.00ms
 90.000%    1.50ms
 99.000%    2.00ms
 99.900%    3.00ms
100.000%    4.00ms

  1000 requests in 1.00s, 1.00MB read
Requests/sec:   1000.00
"""


class DriverTests(unittest.TestCase):
    def setUp(self):
        FakeDocker.log = []
        self.tmp = tempfile.TemporaryDirectory()
        _, self.scenarios = load_scenarios(BENCH / "scenarios" / "scenarios.toml")
        self.gateways = load_gateways(BENCH / "gateways", self.scenarios)
        self.topology = load_topology(BENCH / "topology" / "local.toml")
        self.store = RunStore.create(Path(self.tmp.name), "local", {},
                                     now=datetime(2026, 9, 26, tzinfo=timezone.utc))
        self.driver = DockerDriver(
            self.topology, self.gateways, self.scenarios, Params(), Images(),
            {"nginx": "nginx:test"}, Certs("CERT\n", "KEY\n"), self.store,
            docker_factory=FakeDocker, health=lambda *a, **k: True, sleep=lambda s: None)

    def tearDown(self):
        self.tmp.cleanup()

    def test_boot_removes_stale_containers_first(self):
        self.driver.boot(Cell("nginx", "core.proxy", 1), 0)
        first_creates = next(i for i, c in enumerate(FakeDocker.log) if c[1] == "create")
        removed = {c[2] for c in FakeDocker.log[:first_creates] if c[1] == "rm"}
        self.assertTrue({"fbb-gateway", "fbb-upstream", "fbb-loadgen"} <= removed)

    def test_boot_pins_cpusets_and_renders_config(self):
        self.driver.boot(Cell("nginx", "core.proxy", 4), 0)
        specs = {c[2].name: c[2] for c in FakeDocker.log if c[1] == "create"}
        self.assertEqual(specs["fbb-gateway"].cpuset, "0-3")
        self.assertEqual(specs["fbb-upstream"].cpuset, "4-5")
        self.assertEqual(specs["fbb-loadgen"].cpuset, "6-7")
        self.assertIn("worker_processes 2;", specs["fbb-upstream"].command[-1])
        self.assertEqual(specs["fbb-gateway"].ports, ((18000, 8000), (18443, 8443)))
        conf = (self.store.configs_dir / "nginx" / "proxy" / "nginx.conf").read_text("utf-8")
        self.assertIn("worker_processes 4;", conf)
        self.assertIn("server upstream:8080;", conf)
        self.assertEqual((self.store.configs_dir / "nginx" / "proxy" / "tls" / "cert.pem").read_text("utf-8"), "CERT\n")
        cp = next(c for c in FakeDocker.log if c[1] == "cp")
        self.assertEqual(cp[4], "/etc/nginx/bench")

    def test_direct_boots_no_gateway_and_targets_upstream(self):
        cell = Cell("direct", "core.proxy", 1)
        self.driver.boot(cell, 0)
        names = [c[2].name for c in FakeDocker.log if c[1] == "create"]
        self.assertNotIn("fbb-gateway", names)
        self.assertEqual(self.driver.load_url(cell), "http://upstream:8080/bench/1k")
        self.assertEqual(self.driver.probe_target(cell), ("127.0.0.1", 18080))

    def test_load_builds_wrk2_command_with_headers(self):
        cell = Cell("nginx", "core.proxy", 1)
        self.driver.boot(cell, 0)
        r = self.driver.load(cell, 5000, 20)
        self.assertEqual(r.requests, 1000)
        argv = next(c[3] for c in FakeDocker.log if c[1] == "exec" and c[3][0] == "wrk2")
        self.assertEqual(argv, ("wrk2", "-t5", "-c64", "-d20s", "-R5000", "--latency", "http://gateway:8000/bench/1k"))
        raw = list((self.store.raw_dir / cell.slug).iterdir())
        self.assertEqual(len(raw), 1)

    def test_values_for_tls_scenario(self):
        v = self.driver.values(Cell("nginx", "proto.tls", 4))
        self.assertEqual((v["UPSTREAM_HOST"], v["UPSTREAM_PORT"], v["CORES"]), ("upstream", "8080", "4"))
        self.assertEqual(v["CONFIG_DIR"], "/etc/nginx/bench")
        self.assertEqual(self.driver.load_url(Cell("nginx", "proto.tls", 4)), "https://gateway:8443/bench/1k")
```

- [ ] **Step 2: Write the failing CLI tests**

`bench/tests/test_cli.py`:
```python
import contextlib
import io
import unittest

from benchlib.cli import estimate_rep_seconds, main, parse_image_overrides
from benchlib.config import ConfigError, Params


class CliTests(unittest.TestCase):
    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = main(list(argv))
        return code, out.getvalue(), err.getvalue()

    def test_estimate(self):
        self.assertEqual(estimate_rep_seconds(Params()), 640)
        self.assertEqual(estimate_rep_seconds(Params().quick()), 85)

    def test_plan(self):
        code, out, _ = self.run_cli("plan", "--gateways", "direct,nginx", "--scenarios", "core.proxy,plugin.jwt",
                                    "--profiles", "1", "--quick")
        self.assertEqual(code, 0)
        self.assertIn("2 runnable cells, 2 n/a; 2 repetitions", out)
        self.assertIn("n/a  nginx|plugin.jwt|1", out)
        out.encode("ascii")  # console output stays ASCII (Windows cp1252)

    def test_unknown_profile_is_an_error(self):
        code, _, err = self.run_cli("plan", "--profiles", "3")
        self.assertEqual(code, 2)
        self.assertIn("profile", err)

    def test_resume_rejects_filters(self):
        code, _, err = self.run_cli("run", "--resume", "somewhere", "--gateways", "nginx")
        self.assertEqual(code, 2)
        self.assertIn("--resume", err)

    def test_image_overrides(self):
        gws = {"featherbit": object(), "nginx": object()}
        self.assertEqual(parse_image_overrides(["featherbit=featherbit/featherbit:0.12.1"], gws),
                         {"featherbit": "featherbit/featherbit:0.12.1"})
        with self.assertRaises(ConfigError):
            parse_image_overrides(["kong=kong:3"], gws)
        with self.assertRaises(ConfigError):
            parse_image_overrides(["featherbit"], gws)
```

- [ ] **Step 3: Write the `direct` and `nginx` adapters**

These are needed by the tests above.

`bench/gateways/direct/gateway.toml`:
```toml
# Baseline: the load generator hits the upstream directly, with no gateway in
# between. It proves the upstream is not the bottleneck and anchors the
# report's overhead view. Expect it to be flagged `upstream-bound`: its
# ceiling IS the upstream.
name = "direct"
kind = "direct"

[configs]
"core.proxy" = ""
"payload.64k" = ""
"payload.1m" = ""

[na]
"core.routes-1k" = "Baseline: no gateway, no routing."
"plugin.key-auth" = "Baseline: no gateway, no plugins."
"plugin.jwt" = "Baseline: no gateway, no plugins."
"plugin.rate-limit" = "Baseline: no gateway, no plugins."
"plugin.header-rewrite" = "Baseline: no gateway, no plugins."
"plugin.chain" = "Baseline: no gateway, no plugins."
"proto.tls" = "Baseline: the upstream speaks plaintext only."
"proto.h2" = "Baseline: the upstream speaks plaintext only."
"script.header" = "Baseline: no gateway, no scripting."
```

`bench/gateways/nginx/gateway.toml`. Use the **same** nginx tag and digest as `bench/upstream/Dockerfile` (Task 7):
```toml
# Zero-feature ceiling: stock nginx as a plain reverse proxy.
name = "nginx"
image = "nginx:<TAG>-alpine@sha256:<DIGEST>"
config_dir = "/etc/nginx/bench"
command = ["nginx", "-c", "/etc/nginx/bench/nginx.conf", "-g", "daemon off;"]
ports = { plain = 8000, tls = 8443 }

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "proxy"
"proto.h2" = "proxy"
"core.routes-1k" = "routes-1k"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"

[na]
"plugin.key-auth" = "Open-source nginx has no API-key authentication (only via njs/Lua scripting)."
"plugin.jwt" = "JWT validation (auth_jwt) is NGINX Plus only."
"plugin.chain" = "Needs key-auth, which open-source nginx lacks."
"script.header" = "Kept a zero-feature ceiling: njs scripting is out of scope for the baseline."
```

`bench/gateways/nginx/proxy/nginx.conf`:
```nginx
# Plain reverse proxy: the ceiling every gateway is compared against.
worker_processes @@CORES@@;
worker_rlimit_nofile 1048576;
error_log /dev/stderr warn;

events {
    worker_connections 65535;
    multi_accept on;
}

http {
    access_log off;
    sendfile on;
    tcp_nopush on;
    tcp_nodelay on;
    keepalive_timeout 300s;
    keepalive_requests 1000000;

    upstream backend {
        server @@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@;
        keepalive 512;
        keepalive_requests 1000000;
        keepalive_timeout 300s;
    }

    server {
        listen 8000 reuseport backlog=65535;
        location /bench/ {
            proxy_pass http://backend;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
        }
    }

    server {
        listen 8443 ssl reuseport backlog=65535;
        http2 on;
        ssl_protocols TLSv1.3;
        ssl_session_cache shared:SSL:10m;
        ssl_certificate @@CONFIG_DIR@@/tls/cert.pem;
        ssl_certificate_key @@CONFIG_DIR@@/tls/key.pem;
        location /bench/ {
            proxy_pass http://backend;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
        }
    }
}
```

`bench/gateways/nginx/routes-1k/nginx.conf` is the proxy config minus the TLS server, with the plain server's single location replaced by:
```nginx
    server {
        listen 8000 reuseport backlog=65535;
@@REPEAT 1000@@
        location /r@@I@@/ {
            proxy_pass http://backend;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
        }
@@END@@
    }
```
Write the full file: the `worker_processes` through `upstream backend { … }` block exactly as in `proxy/nginx.conf`, then the server above, then the closing `}` of `http`.

`bench/gateways/nginx/rate-limit/nginx.conf`: the same header and upstream block as `proxy/nginx.conf`, and this plain server (no TLS server):
```nginx
    limit_req_zone $binary_remote_addr zone=bench:10m rate=10000000r/s;
    limit_req_zone $binary_remote_addr zone=probe:1m rate=1r/m;
    limit_req_status 429;

    server {
        listen 8000 reuseport backlog=65535;
        location /bench/ {
            limit_req zone=bench burst=10000000 nodelay;
            proxy_pass http://backend;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
        }
        location /limited/ {
            limit_req zone=probe;
            proxy_pass http://backend;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
        }
    }
```
The `limit_req_*` lines go inside `http { }`, before the server.

`bench/gateways/nginx/header-rewrite/nginx.conf`: the same header and upstream block, and:
```nginx
    server {
        listen 8000 reuseport backlog=65535;
        location /bench/ {
            proxy_pass http://backend;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header X-Bench-Added 1;
            proxy_set_header X-Bench-Added2 2;
            proxy_hide_header X-Bench-Remove;
        }
    }
```

`bench/gateways/nginx/TUNING.md`:
```markdown
# nginx (baseline ceiling): tuning

Image: `nginx:<TAG>-alpine` (pinned by digest in gateway.toml), resolved <DATE>.

| Setting | Value | Why / source |
|---|---|---|
| `worker_processes` | profile cores | One worker per core: nginx docs, "Core functionality: worker_processes". |
| `worker_connections` / `worker_rlimit_nofile` | 65535 / 1048576 | Never the limit at 64 client connections. |
| `access_log` | off | Same for every gateway (spec §8). |
| upstream `keepalive` | 512, `proxy_http_version 1.1`, `Connection ""` | Upstream connection reuse: nginx docs, "ngx_http_upstream_module: keepalive". Every gateway keeps upstream connections alive. |
| `reuseport` | on | nginx's own guidance for multi-worker accept balancing ("Socket Sharding in NGINX", nginx blog). |
| TLS | TLS 1.3 only, session cache on, ECDSA P-256 | Same cert/protocol for every gateway. |
| rate limit | `limit_req` at 10,000,000 r/s, burst 10,000,000 nodelay | Never trips at benchmark load; the probe route uses 1 r/m. |

Not applicable here: key-auth, JWT, chain, scripting (see gateway.toml `[na]`).
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: `test_driver`/`test_cli` ERROR with `No module named 'benchlib.driver'`. `test_every_committed_gateway_loads` should already PASS with the two new adapters. If it fails, fix the adapter it names.

- [ ] **Step 5: Implement `benchlib/driver.py`**

```python
"""DockerDriver: boots a cell's containers, runs the load tools, samples resources (spec §4, §6)."""
from __future__ import annotations

import http.client
import shutil
import time
from dataclasses import dataclass
from pathlib import Path

from .certs import Certs
from .config import ROLES, Gateway, Params, Probe, Scenario, Topology, cpuset_size
from .docker import ContainerSpec, Docker, DockerError
from .matrix import Cell
from .parsers import LoadResult, ParseError, parse_oha, parse_wrk, parse_wrk2
from .probes import ProbeResult, http_call, run_probes, wait_healthy
from .results import RunStore
from .runner import BootError, LoadError
from .sampler import Sample, Sampler
from .templating import TemplateError, render_dir, render_text
from .tokens import static_values

NAMES = {"gateway": "fbb-gateway", "upstream": "fbb-upstream", "loadgen": "fbb-loadgen"}
UPSTREAM_PORT = 8080


@dataclass(frozen=True)
class Images:
    loadgen: str = "featherbit-bench/loadgen:dev"
    upstream: str = "featherbit-bench/upstream:dev"


class DockerDriver:
    def __init__(self, topology: Topology, gateways: dict[str, Gateway], scenarios: dict[str, Scenario],
                 params: Params, images: Images, gateway_images: dict[str, str], certs: Certs,
                 store: RunStore, docker_factory=Docker, health=wait_healthy, probes=run_probes,
                 http=http_call, sampler_factory=Sampler, sleep=time.sleep):
        self.t = topology
        self.gateways = gateways
        self.scenarios = scenarios
        self.params = params
        self.images = images
        self.gateway_images = gateway_images
        self.certs = certs
        self.store = store
        self.health = health
        self.probes = probes
        self.http = http
        self.sampler_factory = sampler_factory
        self.sleep = sleep
        self.local = topology.kind == "local"
        self.dk = {role: docker_factory(topology.roles[role].context) for role in ROLES}
        self._dep_names = sorted({f"fbb-dep-{d.name}" for g in gateways.values() for d in g.dependencies})
        self._sampler = None
        self._sampling_roles: dict[str, str] = {}
        self._rep = 0
        self._seq = 0

    # --- addressing -------------------------------------------------------------------

    def _host(self, role: str) -> str:
        return role if self.local else self.t.roles[role].address

    def values(self, cell: Cell) -> dict[str, str]:
        gw = self.gateways[cell.gateway]
        v = {**static_values(), "UPSTREAM_HOST": self._host("upstream"), "UPSTREAM_PORT": str(UPSTREAM_PORT),
             "CORES": str(cell.profile), "CONFIG_DIR": gw.config_dir,
             "TLS_CERT_PEM": self.certs.cert_pem, "TLS_KEY_PEM": self.certs.key_pem}
        for dep in gw.dependencies:
            key = dep.name.upper().replace("-", "_")
            v[f"DEP_{key}_HOST"] = dep.name if self.local else self.t.roles["upstream"].address
            v[f"DEP_{key}_PORT"] = str(dep.port)
        return v

    def load_url(self, cell: Cell) -> str:
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        if gw.kind == "direct":
            host, port = self._host("upstream"), UPSTREAM_PORT
        else:
            host, port = self._host("gateway"), gw.ports["tls" if sc.scheme == "https" else "plain"]
        return f"{sc.scheme}://{host}:{port}{sc.path}"

    def probe_target(self, cell: Cell) -> tuple[str, int]:
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        if gw.kind == "direct":
            if self.local:
                return self.t.probe_host, self.t.published_ports["upstream"]
            return self.t.roles["upstream"].address, UPSTREAM_PORT
        key = "tls" if sc.scheme == "https" else "plain"
        return self.t.probe_host, (self.t.published_ports[key] if self.local else gw.ports[key])

    def cores(self, cell: Cell, role: str) -> int:
        return cpuset_size(getattr(self.t.profiles[cell.profile], role))

    # --- lifecycle --------------------------------------------------------------------

    def _start(self, role: str, spec: ContainerSpec) -> None:
        try:
            self.dk[role].create(spec)
            self.dk[role].start(spec.name)
        except DockerError as e:
            raise BootError(str(e)) from None

    def _render(self, gw: Gateway, cell: Cell, values: dict[str, str]) -> Path:
        name = gw.configs[cell.scenario]
        out = self.store.configs_dir / gw.name / name
        if out.exists():
            shutil.rmtree(out)
        try:
            if gw.common:
                render_dir(gw.dir / gw.common, out, values)
            render_dir(gw.dir / name, out, values)
        except TemplateError as e:
            raise BootError(f"config template: {e}") from None
        (out / "tls").mkdir(parents=True, exist_ok=True)
        (out / "tls" / "cert.pem").write_bytes(self.certs.cert_pem.encode("utf-8"))
        (out / "tls" / "key.pem").write_bytes(self.certs.key_pem.encode("utf-8"))
        return out

    def _wait_upstream(self) -> None:
        url = f"http://{self._host('upstream')}:{UPSTREAM_PORT}/bench/1k"
        deadline = time.monotonic() + self.params.boot_timeout_seconds
        while True:
            try:
                code = self.dk["loadgen"].exec(
                    NAMES["loadgen"], ["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", url], timeout=10)
                if code.strip() == "200":
                    return
            except DockerError:
                pass
            if time.monotonic() >= deadline:
                raise BootError(f"upstream not serving {url}")
            self.sleep(0.5)

    def boot(self, cell: Cell, rep: int) -> None:
        self.teardown()  # clears stale fbb-* containers from any earlier crash
        self._rep, self._seq = rep, 0
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        prof = self.t.profiles[cell.profile]
        net = self.t.network
        try:
            for role in ROLES:
                self.dk[role].ensure_network(net)
        except DockerError as e:
            raise BootError(str(e)) from None
        up_ports = ((self.t.published_ports["upstream"], UPSTREAM_PORT),) if self.local else ()
        self._start("upstream", ContainerSpec(
            name=NAMES["upstream"], image=self.images.upstream, cpuset=prof.upstream, network=net,
            aliases=("upstream",), ports=up_ports,
            command=("nginx", "-g", f"daemon off; worker_processes {cpuset_size(prof.upstream)};")))
        for dep in gw.dependencies:
            self._start("upstream", ContainerSpec(
                name=f"fbb-dep-{dep.name}", image=dep.image, cpuset=prof.upstream, network=net,
                aliases=(dep.name,), command=dep.command))
        self._start("loadgen", ContainerSpec(
            name=NAMES["loadgen"], image=self.images.loadgen, cpuset=prof.loadgen, network=net,
            aliases=("loadgen",), command=("sleep", "infinity")))
        self._wait_upstream()
        if gw.kind == "container":
            values = self.values(cell)
            rendered = self._render(gw, cell, values)
            ports = tuple((self.t.published_ports[k], p) for k, p in gw.ports.items()) if self.local else ()
            spec = ContainerSpec(
                name=NAMES["gateway"], image=self.gateway_images[gw.name], cpuset=prof.gateway,
                network=net, aliases=("gateway",), ports=ports,
                env=tuple((k, render_text(v, values)) for k, v in gw.env.items()),
                command=tuple(render_text(c, values) for c in gw.command),
                entrypoint=gw.entrypoint, workdir=gw.workdir)
            try:
                self.dk["gateway"].create(spec)
                self.dk["gateway"].cp_into(NAMES["gateway"], rendered, gw.config_dir)
                self.dk["gateway"].start(NAMES["gateway"])
            except DockerError as e:
                raise BootError(str(e)) from None
        host, port = self.probe_target(cell)
        if not self.health(sc.scheme, host, port, "/", self.params.boot_timeout_seconds):
            raise BootError(f"{gw.name} not answering on {sc.scheme}://{host}:{port} "
                            f"within {self.params.boot_timeout_seconds}s")

    def setup_and_probe(self, cell: Cell) -> list[ProbeResult]:
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        values = self.values(cell)
        host, port = self.probe_target(cell)
        results: list[ProbeResult] = []
        for call in gw.setup:
            if cell.scenario not in call.scenarios:
                continue
            path = render_text(call.path, values)
            body = (render_text((gw.dir / call.body_file).read_text("utf-8"), values).encode("utf-8")
                    if call.body_file else None)
            headers = {k: render_text(v, values) for k, v in call.headers.items()}
            try:
                status, _, _ = self.http(call.method, sc.scheme, host, port, path, headers, body)
                detail = f"setup {call.method} {path}: status {status}"
            except (OSError, http.client.HTTPException) as e:
                status, detail = None, f"setup {call.method} {path}: {type(e).__name__}: {e}"
            ok = status in call.expect_status
            results.append(ProbeResult(Probe(path=path, kind="setup"), ok, detail))
            if not ok:
                return results
        return results + self.probes(sc.probes, sc.scheme, host, port, values)

    def load(self, cell: Cell, rate: int | None, seconds: int) -> LoadResult:
        sc = self.scenarios[cell.scenario]
        values = self.values(cell)
        url = self.load_url(cell)
        threads = self.cores(cell, "loadgen")
        conns = self.params.connections
        headers: list[str] = []
        for k, v in sc.headers.items():
            headers += ["-H", f"{k}: {render_text(v, values)}"]
        if sc.tool == "oha":
            # 16 connections x 4 streams = the same 64 in-flight requests as the HTTP/1.1 tools.
            argv = ["oha", "-z", f"{seconds}s", "-c", str(max(1, conns // 4)), "-p", "4", "--http2",
                    "--insecure", "--no-tui", "--output-format", "json", *headers]
            if rate is not None:
                argv += ["-q", str(rate), "--latency-correction"]
            argv.append(url)
            parse, tool = parse_oha, "oha"
        elif rate is None:
            argv = ["wrk", f"-t{threads}", f"-c{conns}", f"-d{seconds}s", "--latency", *headers, url]
            parse, tool = parse_wrk, "wrk"
        else:
            argv = ["wrk2", f"-t{threads}", f"-c{conns}", f"-d{seconds}s", f"-R{rate}", "--latency", *headers, url]
            parse, tool = parse_wrk2, "wrk2"
        try:
            out = self.dk["loadgen"].exec(NAMES["loadgen"], argv, timeout=seconds + 60)
        except DockerError as e:
            raise LoadError(str(e)) from None
        self._seq += 1
        raw = self.store.raw_dir / cell.slug
        raw.mkdir(parents=True, exist_ok=True)
        (raw / f"rep{self._rep}-{self._seq:03d}-{tool}-{rate or 'flood'}.txt").write_bytes(
            (" ".join(argv) + "\n\n" + out).encode("utf-8"))
        try:
            return parse(out)
        except ParseError as e:
            raise LoadError(str(e)) from None

    def start_sampling(self, cell: Cell) -> None:
        roles = ["upstream", "loadgen"] + (["gateway"] if self.gateways[cell.gateway].kind == "container" else [])
        self._sampling_roles = {NAMES[r]: r for r in roles}
        self._sampler = self.sampler_factory([(self.dk[r], NAMES[r]) for r in roles])
        self._sampler.start()

    def stop_sampling(self) -> dict[str, list[Sample]]:
        out: dict[str, list[Sample]] = {r: [] for r in ROLES}
        if self._sampler is None:
            return out
        samples, self._sampler = self._sampler.stop(), None
        for s in samples:
            role = self._sampling_roles.get(s.name)
            if role:
                out[role].append(s)
        return out

    def logs(self) -> dict[str, str]:
        out = {NAMES["gateway"]: self.dk["gateway"].logs(NAMES["gateway"]),
               NAMES["upstream"]: self.dk["upstream"].logs(NAMES["upstream"])}
        for name in self._dep_names:
            out[name] = self.dk["upstream"].logs(name)
        return out

    def teardown(self) -> None:
        if self._sampler is not None:
            self._sampler.stop()
            self._sampler = None
        self.dk["gateway"].rm(NAMES["gateway"])
        self.dk["loadgen"].rm(NAMES["loadgen"])
        for name in self._dep_names:
            self.dk["upstream"].rm(name)
        self.dk["upstream"].rm(NAMES["upstream"])
```
If Task 7 Step 8 found different oha flag spellings, use them in `load()`.

- [ ] **Step 6: Implement `benchlib/cli.py` and `bench/bench.py`**

`bench/bench.py`:
```python
#!/usr/bin/env python3
"""Featherbit competitive benchmark suite - see bench/README.md."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from benchlib.cli import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
```

`bench/benchlib/cli.py`:
```python
"""bench.py commands: plan, run, validate, report, clean."""
from __future__ import annotations

import argparse
import dataclasses
import math
import sys
from pathlib import Path

from .certs import generate_certs
from .config import (ROLES, ConfigError, Gateway, Params, load_gateways, load_scenarios,
                     load_topology, params_from_dict)
from .docker import Docker, DockerError
from .driver import DockerDriver, Images
from .fingerprint import git_info, host_fingerprint, publish_blockers
from .matrix import expand, gateway_order, schedule, select
from .results import RunStore
from .runner import CellRunner

BENCH = Path(__file__).resolve().parent.parent
SCENARIOS = BENCH / "scenarios" / "scenarios.toml"
GATEWAYS = BENCH / "gateways"
TOPOLOGIES = BENCH / "topology"
RESULTS = BENCH / "results"


def _csv(value: str | None) -> list[str] | None:
    return [x.strip() for x in value.split(",") if x.strip()] if value else None


def topology_path(arg: str) -> Path:
    p = Path(arg)
    if p.suffix == ".toml" and p.is_file():
        return p.resolve()
    candidate = TOPOLOGIES / f"{arg}.toml"
    if candidate.is_file():
        return candidate
    raise ConfigError(f"topology {arg!r} not found (looked for {p} and {candidate})")


def estimate_rep_seconds(p: Params) -> int:
    """Worst case for one repetition: ~20 s boot/probes, ceiling, warm-up, search, full ladder."""
    search_steps = 2 + math.ceil(math.log2((p.search_high - p.search_low) / (p.search_tolerance * p.search_high)))
    return (20 + p.ceiling_seconds + p.warmup_seconds + search_steps * p.search_step_seconds
            + len(p.ladder) * p.ladder_seconds)


def parse_image_overrides(items: list[str], gateways: dict[str, Gateway]) -> dict[str, str]:
    out = {}
    for item in items:
        name, sep, image = item.partition("=")
        if not sep or not image:
            raise ConfigError(f"--image expects GATEWAY=IMAGE, got {item!r}")
        if name not in gateways:
            raise ConfigError(f"--image: unknown gateway {name!r}")
        out[name] = image
    return out


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="bench.py", description="Featherbit competitive benchmark suite")
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("plan", "run", "validate"):
        sp = sub.add_parser(name)
        sp.add_argument("--topology", default="local", help="name under bench/topology/ or a .toml path")
        sp.add_argument("--gateways", help="comma-separated names or globs")
        sp.add_argument("--scenarios", help="comma-separated ids or globs, e.g. 'core.*,plugin.jwt'")
        sp.add_argument("--profiles", help="comma-separated core counts, e.g. 1,4")
        if name != "validate":
            sp.add_argument("--reps", type=int)
            sp.add_argument("--quick", action="store_true", help="1 rep, 5 s steps: iteration only")
        if name != "plan":
            sp.add_argument("--image", action="append", default=[], metavar="GATEWAY=IMAGE")
    sub.choices["run"].add_argument("--publish", action="store_true")
    sub.choices["run"].add_argument("--resume", metavar="RUN_DIR")
    sub.add_parser("report").add_argument("run_dir")
    sub.add_parser("clean").add_argument("--topology", default="local")
    return ap


def _inputs():
    params, scenarios = load_scenarios(SCENARIOS)
    return params, scenarios, load_gateways(GATEWAYS, scenarios)


def _resolve(args, scenarios, gateways, topology) -> tuple[list[str], list[str], list[int]]:
    gws = select(_csv(args.gateways), gateway_order(gateways), "gateway")
    scs = select(_csv(args.scenarios), list(scenarios), "scenario")
    profs = sorted(topology.profiles)
    if args.profiles:
        try:
            wanted = sorted(int(x) for x in _csv(args.profiles))
        except ValueError:
            raise ConfigError(f"--profiles expects core counts, got {args.profiles!r}") from None
        unknown = [w for w in wanted if w not in topology.profiles]
        if unknown:
            raise ConfigError(f"unknown profile(s) {unknown}; {topology.name} defines {profs}")
        profs = wanted
    return gws, scs, profs


def _params(params: Params, args) -> Params:
    p = params.quick() if getattr(args, "quick", False) else params
    if getattr(args, "reps", None):
        p = dataclasses.replace(p, reps=args.reps)
    return p


def _dockers(topology) -> tuple[dict[str, Docker], dict[str | None, Docker]]:
    dk = {role: Docker(topology.roles[role].context) for role in ROLES}
    contexts: dict[str | None, Docker] = {}
    for role in ROLES:
        contexts.setdefault(topology.roles[role].context, dk[role])
    return dk, contexts


def _prepare(topology, gateways, needed: list[str], overrides: dict[str, str], store: RunStore):
    """Build/pull every image, record digests, make the run certificate, fingerprint the hosts."""
    images = Images()
    dk, contexts = _dockers(topology)
    local = contexts.get(None) or Docker(None)
    for d in {id(x): x for x in [*contexts.values(), local]}.values():
        print(f"building {images.loadgen} on {d.context or 'default'} ...", flush=True)
        d.ensure_image(images.loadgen, build=BENCH / "loadgen")
    print(f"building {images.upstream} ...", flush=True)
    dk["upstream"].ensure_image(images.upstream, build=BENCH / "upstream")
    gateway_images: dict[str, str] = {}
    for name in needed:
        gw = gateways[name]
        if gw.kind != "container":
            continue
        if name in overrides:
            gateway_images[name] = overrides[name]
            dk["gateway"].ensure_image(overrides[name])
        else:
            if gw.build:
                missing = [r for r in gw.build_requires if not (gw.build / r).exists()]
                if missing:
                    raise ConfigError(f"{name}: image build needs {missing}. {gw.build_hint}")
            print(f"preparing {gw.image} ...", flush=True)
            dk["gateway"].ensure_image(gw.image, build=gw.build, build_args=gw.build_args)
            gateway_images[name] = gw.image
        for dep in gw.dependencies:
            dk["upstream"].ensure_image(dep.image)
    store.data["images"] = {
        "loadgen": dk["loadgen"].image_id(images.loadgen),
        "upstream": dk["upstream"].image_id(images.upstream),
        **{n: dk["gateway"].image_id(i) for n, i in gateway_images.items()},
    }
    certs = generate_certs(local, images.loadgen)
    hosts = {ctx or "local": host_fingerprint(d, images.loadgen) for ctx, d in contexts.items()}
    return images, gateway_images, certs, hosts


def cmd_plan(args) -> int:
    params, scenarios, gateways = _inputs()
    topology = load_topology(topology_path(args.topology))
    gws, scs, profs = _resolve(args, scenarios, gateways, topology)
    params = _params(params, args)
    cells = expand(gws, scs, profs)
    runnable = [c for c in cells if gateways[c.gateway].supports(c.scenario)]
    reps = len(runnable) * params.reps
    secs = reps * estimate_rep_seconds(params)
    print(f"{len(runnable)} runnable cells, {len(cells) - len(runnable)} n/a; {reps} repetitions")
    print(f"worst-case duration ~{secs // 3600}h {secs % 3600 // 60}m")
    for c in cells:
        print(f"  {'run' if gateways[c.gateway].supports(c.scenario) else 'n/a'}  {c.key}")
    return 0


def cmd_run(args) -> int:
    params, scenarios, gateways = _inputs()
    if args.resume:
        if any([args.gateways, args.scenarios, args.profiles, args.reps, args.quick, args.publish, args.image]):
            raise ConfigError("--resume reuses the stored selection; drop the other flags")
        store = RunStore.open(Path(args.resume))
        sel = store.data["selection"]
        topology = load_topology(Path(sel["topology_path"]))
        params = params_from_dict(store.data["params"])
        gws, scs, profs = sel["gateways"], sel["scenarios"], sel["profiles"]
        overrides = sel.get("image_overrides", {})
        publish = store.data.get("publish", False)
    else:
        tpath = topology_path(args.topology)
        topology = load_topology(tpath)
        gws, scs, profs = _resolve(args, scenarios, gateways, topology)
        params = _params(params, args)
        overrides = parse_image_overrides(args.image, gateways)
        publish = args.publish
        store = RunStore.create(RESULTS, topology.name, {
            "publish": publish,
            "topology": {"name": topology.name, "kind": topology.kind},
            "selection": {"topology_path": str(tpath), "gateways": gws, "scenarios": scs,
                          "profiles": profs, "image_overrides": overrides},
            "params": dataclasses.asdict(params), "images": {}, "fingerprint": {},
        })
    cells = expand(gws, scs, profs)
    for c in cells:
        if not gateways[c.gateway].supports(c.scenario):
            store.record_na(c, gateways[c.gateway].na[c.scenario])
    plan = schedule(cells, gateways, params.reps)
    needed = sorted({c.gateway for c, _ in plan})
    try:
        images, gateway_images, certs, hosts = _prepare(topology, gateways, needed, overrides, store)
    except DockerError as e:
        raise ConfigError(f"preparing images failed: {e}") from None
    store.data["fingerprint"] = {"hosts": hosts, "git": git_info(BENCH.parent)}
    store.save()
    if publish:
        blockers = publish_blockers(topology, hosts)
        if blockers:
            raise ConfigError("--publish refused:\n  " + "\n  ".join(blockers))
    driver = DockerDriver(topology, gateways, scenarios, params, images, gateway_images, certs, store)
    runner = CellRunner(driver, params)
    todo = [(c, r) for c, r in plan if not store.done(c, r)]
    print(f"run dir: {store.path} ({len(todo)} of {len(plan)} repetitions to go)", flush=True)
    try:
        for i, (cell, rep) in enumerate(todo, 1):
            print(f"[{i}/{len(todo)}] {cell.key} rep {rep + 1}/{params.reps} ...", flush=True)
            result = runner.run_rep(cell, rep)
            store.record_rep(cell, rep, result)
            print(f"    {result['status']}  max sustainable: {result.get('max_sustainable_rps', '-')} req/s"
                  + (f"  ({result['error']})" if "error" in result else ""), flush=True)
    except KeyboardInterrupt:
        print(f"\ninterrupted - resume with: python bench/bench.py run --resume {store.path}")
        return 130
    finally:
        driver.teardown()
    print(f"done - render the report with: python bench/bench.py report {store.path}")
    return 0


def cmd_validate(args) -> int:
    params, scenarios, gateways = _inputs()
    tpath = topology_path(args.topology)
    topology = load_topology(tpath)
    gws, scs, profs = _resolve(args, scenarios, gateways, topology)
    overrides = parse_image_overrides(args.image, gateways)
    store = RunStore.create(RESULTS, f"{topology.name}-validate", {
        "publish": False, "topology": {"name": topology.name, "kind": topology.kind},
        "selection": {"topology_path": str(tpath), "gateways": gws, "scenarios": scs,
                      "profiles": profs, "image_overrides": overrides},
        "params": dataclasses.asdict(params), "images": {}, "fingerprint": {},
    })
    cells = [c for c in expand(gws, scs, profs[:1]) if gateways[c.gateway].supports(c.scenario)]
    try:
        images, gateway_images, certs, hosts = _prepare(topology, gateways, sorted({c.gateway for c in cells}),
                                                        overrides, store)
    except DockerError as e:
        raise ConfigError(f"preparing images failed: {e}") from None
    store.data["fingerprint"] = {"hosts": hosts, "git": git_info(BENCH.parent)}
    driver = DockerDriver(topology, gateways, scenarios, params, images, gateway_images, certs, store)
    runner = CellRunner(driver, params)
    failures = 0
    try:
        for cell in cells:
            result = runner.validate(cell)
            store.record_rep(cell, 0, result)
            bad = [p["detail"] for p in result.get("probes", []) if not p["ok"]]
            detail = result.get("error") or "; ".join(bad) or "all probes passed"
            print(f"{result['status']:8} {cell.key}  {detail}", flush=True)
            failures += result["status"] != "ok"
    finally:
        driver.teardown()
    print(f"{len(cells) - failures}/{len(cells)} cells valid - details in {store.path}")
    return 1 if failures else 0


def cmd_report(args) -> int:
    from report.render import write_report  # bench/ is on sys.path via bench.py
    print(write_report(Path(args.run_dir)))
    return 0


def cmd_clean(args) -> int:
    _, contexts = _dockers(load_topology(topology_path(args.topology)))
    removed = [cid for d in contexts.values() for cid in d.remove_labelled()]
    print(f"removed {len(removed)} container(s)")
    return 0


COMMANDS = {"plan": cmd_plan, "run": cmd_run, "validate": cmd_validate, "report": cmd_report, "clean": cmd_clean}


def main(argv: list[str]) -> int:
    args = build_parser().parse_args(argv)
    try:
        return COMMANDS[args.cmd](args)
    except ConfigError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
```
`validate` checks each supported cell once, at the first selected profile only. Configs don't differ by core count apart from `@@CORES@@`.

- [ ] **Step 7: Run the unit tests**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all PASS.

- [ ] **Step 8: Smoke-run on this machine (Docker Desktop must be running)**

Pin the nginx image first. It's the same tag and digest as the upstream; write it into `bench/gateways/nginx/gateway.toml`. Then:
```bash
python bench/bench.py plan --gateways direct,nginx --scenarios core.proxy --quick
python bench/bench.py validate --gateways nginx
python bench/bench.py run --gateways direct,nginx --scenarios core.proxy --profiles 1 --quick
```
Expected:
- `plan` prints `2 runnable cells, 0 n/a` for both profiles.
- `validate` prints `ok` for every nginx cell: core.proxy, routes-1k, rate-limit, header-rewrite, 64k, 1m, tls, h2.
- `run` ends with `done - render the report ...`. `bench/results/<ts>-local/run.json` then shows both cells with a numeric `max_sustainable_rps`, `raw/` holds the tool outputs, and `configs/nginx/proxy/nginx.conf` holds the rendered config.

If a cell fails, read `logs/` and the probe details. Fix configs or driver code, not the tests.

Then check the interrupt path: start the same `run` without `--quick`, press Ctrl+C during the first cell, confirm that `docker ps -a --filter label=featherbit-bench=1` is empty, then run `python bench/bench.py run --resume <dir>` and confirm it continues.

- [ ] **Step 9: Commit** (only if authorized)

```bash
git add bench/bench.py bench/benchlib/driver.py bench/benchlib/cli.py bench/tests/test_driver.py bench/tests/test_cli.py bench/gateways/direct bench/gateways/nginx
git commit -m "feat(bench): docker driver, bench.py CLI, direct baseline and nginx ceiling"
```

---
### Task 10: Featherbit adapter

**Files:**
- Create under `bench/gateways/featherbit/`:
  - `gateway.toml`, `TUNING.md`
  - `common/system.yaml`
  - `proxy/gateway.yaml`
  - `tls/system.yaml`, `tls/gateway.yaml`
  - `routes-1k/gateway.yaml`, `key-auth/gateway.yaml`, `jwt/gateway.yaml`, `rate-limit/gateway.yaml`, `header-rewrite/gateway.yaml`, `chain/gateway.yaml`, `script/gateway.yaml`

**Interfaces:**
- Consumes: the adapter format (Task 1) and the tokens (Tasks 5, 9). Featherbit config reference: `website/docs/guides/configuration.md`, `website/docs/guides/tls.md`, and `website/docs/reference/plugins/{key-auth,jwt-auth,limit-count,proxy-rewrite,script,upstream}.md`.
- Produces: `featherbit` in the matrix, supporting all 12 scenarios.

How Featherbit is set up for the benchmark:
- **One listener per process.** With `tls:` set, the listener speaks HTTPS, so TLS/h2 use a separate `tls` config on 8443.
- **No `admin:` section**, which disables the admin server.
- **Log level `warn`.**
- Every policy ends at `client`, and every outcome port (`denied`, `limited`, `respond`) is wired, or the policy won't compile.

- [ ] **Step 1: Write `gateway.toml`**

```toml
# Featherbit. Dev runs build the current checkout; publishable runs pass the
# released image instead:  bench.py run --image featherbit=featherbit/featherbit:<tag>
name = "featherbit"
image = "featherbit-bench/featherbit:dev"
build = "../../.."
build_requires = ["ui/dist"]
build_hint = "The Dockerfile copies ui/dist: run `npm --prefix ui ci && npm --prefix ui run build` first, or pass --image featherbit=featherbit/featherbit:<tag>."
config_dir = "/etc/featherbit"
common = "common"
command = ["--system-config", "/etc/featherbit/system.yaml", "--gateway-config", "/etc/featherbit/gateway.yaml"]
ports = { plain = 8000, tls = 8443 }

[env]
TOKIO_WORKER_THREADS = "@@CORES@@"

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "tls"
"proto.h2" = "tls"
"core.routes-1k" = "routes-1k"
"plugin.key-auth" = "key-auth"
"plugin.jwt" = "jwt"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"
"plugin.chain" = "chain"
"script.header" = "script"

[na]
```

- [ ] **Step 2: Write the shared system config and the TLS override**

`common/system.yaml`:
```yaml
# Benchmark profile: data plane only (no admin section = no admin server),
# warn-level logs. HTTP/2 stays at its default (enabled).
listener:
  bind: "0.0.0.0"
  port: 8000

logging:
  level: warn
  format: json
```

`tls/system.yaml` (rendered over `common/`, so it replaces the file):
```yaml
listener:
  bind: "0.0.0.0"
  port: 8443

tls:
  cert_path: @@CONFIG_DIR@@/tls/cert.pem
  key_path: @@CONFIG_DIR@@/tls/key.pem
  min_version: "1.3"

http2:
  enabled: true

logging:
  level: warn
  format: json
```

- [ ] **Step 3: Write the proxy policy (also used by `tls`)**

`proxy/gateway.yaml`. Copy it byte for byte to `tls/gateway.yaml`:
```yaml
routes:
  - name: bench
    match:
      path: /bench/*
    policy: proxy

policies:
  - name: proxy
    nodes:
      - id: listener
        type: listener
      - id: backend
        type: upstream
        config:
          targets:
            - host: "@@UPSTREAM_HOST@@"
              port: @@UPSTREAM_PORT@@
      - id: client
        type: client
    edges:
      - from: listener.out
        to: backend.in
      - from: backend.success
        to: client.in
```

- [ ] **Step 4: Write `routes-1k/gateway.yaml`**

```yaml
routes:
@@REPEAT 1000@@
  - name: r@@I@@
    match:
      path: /r@@I@@/*
    policy: proxy
@@END@@

policies:
  - name: proxy
    nodes:
      - id: listener
        type: listener
      - id: backend
        type: upstream
        config:
          targets:
            - host: "@@UPSTREAM_HOST@@"
              port: @@UPSTREAM_PORT@@
      - id: client
        type: client
    edges:
      - from: listener.out
        to: backend.in
      - from: backend.success
        to: client.in
```

- [ ] **Step 5: Write the plugin policies**

`key-auth/gateway.yaml`:
```yaml
routes:
  - name: bench
    match:
      path: /bench/*
    policy: key-auth

policies:
  - name: key-auth
    nodes:
      - id: listener
        type: listener
      - id: auth
        type: key-auth
        config:
          keys: ["@@API_KEY@@"]
          header_name: apikey
      - id: backend
        type: upstream
        config:
          targets:
            - host: "@@UPSTREAM_HOST@@"
              port: @@UPSTREAM_PORT@@
      - id: client
        type: client
    edges:
      - from: listener.out
        to: auth.in
      - from: auth.success
        to: backend.in
      - from: auth.denied
        to: client.in
      - from: backend.success
        to: client.in
```

`jwt/gateway.yaml` is the same as `key-auth/gateway.yaml` with the policy renamed to `jwt` in both places. Its `auth` node is:
```yaml
      - id: auth
        type: jwt-auth
        config:
          secret: "@@JWT_SECRET@@"
          algorithm: HS256
          header_name: authorization
```

`rate-limit/gateway.yaml`:
```yaml
routes:
  - name: bench
    match:
      path: /bench/*
    policy: limit
  - name: limited
    match:
      path: /limited/*
    policy: limit-probe

policies:
  - name: limit
    nodes:
      - id: listener
        type: listener
      - id: limit
        type: limit-count
        config:
          count: 100000000
          time_window: 60
          key: "$remote_addr"
          policy: local
          rejected_code: 429
      - id: backend
        type: upstream
        config:
          targets:
            - host: "@@UPSTREAM_HOST@@"
              port: @@UPSTREAM_PORT@@
      - id: client
        type: client
    edges:
      - from: listener.out
        to: limit.in
      - from: limit.success
        to: backend.in
      - from: limit.limited
        to: client.in
      - from: backend.success
        to: client.in

  - name: limit-probe
    nodes:
      - id: listener
        type: listener
      - id: limit
        type: limit-count
        config:
          count: 1
          time_window: 60
          key: "$remote_addr"
          policy: local
          rejected_code: 429
      - id: backend
        type: upstream
        config:
          targets:
            - host: "@@UPSTREAM_HOST@@"
              port: @@UPSTREAM_PORT@@
      - id: client
        type: client
    edges:
      - from: listener.out
        to: limit.in
      - from: limit.success
        to: backend.in
      - from: limit.limited
        to: client.in
      - from: backend.success
        to: client.in
```

`header-rewrite/gateway.yaml`:
```yaml
routes:
  - name: bench
    match:
      path: /bench/*
    policy: headers

policies:
  - name: headers
    nodes:
      - id: listener
        type: listener
      - id: add-headers
        type: proxy-rewrite
        config:
          phase: request
          add_headers:
            x-bench-added: "1"
            x-bench-added2: "2"
      - id: backend
        type: upstream
        config:
          targets:
            - host: "@@UPSTREAM_HOST@@"
              port: @@UPSTREAM_PORT@@
      - id: strip-header
        type: proxy-rewrite
        config:
          phase: response
          remove_headers: [x-bench-remove]
      - id: client
        type: client
    edges:
      - from: listener.out
        to: add-headers.in
      - from: add-headers.success
        to: backend.in
      - from: backend.success
        to: strip-header.in
      - from: strip-header.success
        to: client.in
```

`chain/gateway.yaml` has two routes, `/bench/*` with policy `chain` and `/limited/*` with policy `chain-probe`. Both policies have the same node sequence: listener, then `auth` (key-auth as in `key-auth/`), then `limit` (limit-count), then `add-headers`, then `backend`, then `strip-header`, then client. They differ only in `limit.count`: `100000000` for `chain` and `1` for `chain-probe`. Edges for each policy:
```yaml
    edges:
      - from: listener.out
        to: auth.in
      - from: auth.success
        to: limit.in
      - from: auth.denied
        to: client.in
      - from: limit.success
        to: add-headers.in
      - from: limit.limited
        to: client.in
      - from: add-headers.success
        to: backend.in
      - from: backend.success
        to: strip-header.in
      - from: strip-header.success
        to: client.in
```
Write both policies in full. Each node's config is the same as in the single-plugin files above.

`script/gateway.yaml`:
```yaml
routes:
  - name: bench
    match:
      path: /bench/*
    policy: script

policies:
  - name: script
    nodes:
      - id: listener
        type: listener
      - id: script
        type: script
        config:
          runtime: lua
          inline: |
            function execute(ctx)
              local v = ctx.request.headers["x-bench-in"]
              v = v and v[1] or ""
              ctx.request.headers["x-bench-script"] = string.upper(v) .. "-" .. #v
              return ctx
            end
      - id: backend
        type: upstream
        config:
          targets:
            - host: "@@UPSTREAM_HOST@@"
              port: @@UPSTREAM_PORT@@
      - id: client
        type: client
    edges:
      - from: listener.out
        to: script.in
      - from: script.success
        to: backend.in
      - from: script.respond
        to: client.in
      - from: backend.success
        to: client.in
```

- [ ] **Step 6: Write `TUNING.md`**

```markdown
# Featherbit: tuning

Image: built from the checkout (`featherbit-bench/featherbit:dev`, git SHA in run.json),
or the released image via `--image featherbit=featherbit/featherbit:<tag>` for publishable runs.

| Setting | Value | Why |
|---|---|---|
| `TOKIO_WORKER_THREADS` | profile cores | One runtime worker per pinned core (tokio honours the env var in `#[tokio::main]`). |
| admin server | disabled (no `admin:` section) | Data plane only, like every competitor. |
| `logging.level` | warn | No per-request logging; same rule for every gateway. |
| TLS | `min_version: "1.3"`, ECDSA P-256, HTTP/2 enabled (default) | Same cert and protocol for every gateway. |
| limit-count | `policy: local`, count 100,000,000 / 60 s | Never trips at benchmark load; probe route uses count 1. |

Deliberately **not** tuned: no custom allocator, no build-profile changes, no plugin shortcuts. The image is what users get.

Known costs Featherbit always pays, disclosed so the numbers are read correctly:
- Per-route and per-node Prometheus metrics are recorded on every request. They can't be disabled.
- The published image is a static musl build (`FROM scratch`) that uses musl's default allocator.
- `script` runs every execution in a fresh Luau VM (docs: reference/plugins/script.md).
- Responses pass through the node graph's buffered path unless a node permits streaming (docs: reference/plugins/upstream.md).
```

- [ ] **Step 7: Check the adapter loads**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: `test_every_committed_gateway_loads` PASS.

- [ ] **Step 8: Validate against the real gateway**

```bash
npm --prefix ui ci && npm --prefix ui run build     # once, for the Dockerfile's ui/dist copy
python bench/bench.py validate --gateways featherbit
```
Expected: `12/12 cells valid`. If a probe fails, the detail line names the header, status or byte count. Fix the config, read `logs/`, and re-run. A policy compile error shows up as a boot error whose log file quotes the compiler message.

- [ ] **Step 9: Quick measurement**

```bash
python bench/bench.py run --gateways featherbit,nginx --scenarios core.proxy,script.header --profiles 1 --quick
```
Expected: `ok`, or a `-bound` status, with numeric results for every runnable cell.

- [ ] **Step 10: Commit** (only if authorized)

```bash
git add bench/gateways/featherbit
git commit -m "feat(bench): featherbit adapter"
```

---
### Task 11: Apache APISIX adapter

**Files:**
- Create under `bench/gateways/apisix/`:
  - `gateway.toml`, `TUNING.md`
  - `common/config.yaml`
  - `apisix.yaml` in each of `proxy/`, `routes-1k/`, `key-auth/`, `jwt/`, `rate-limit/`, `header-rewrite/`, `chain/` and `script/`

**Interfaces:**
- Consumes: the adapter format (Task 1) and the tokens (Tasks 5, 9).
- Produces: `apisix` in the matrix, supporting all 12 scenarios.

How APISIX is set up for the benchmark:
- **Standalone (YAML) data-plane mode.** There's no etcd and no Admin API; routes live in `conf/apisix.yaml`, which must end with `#END`.
- **Config goes into the image's own `conf/` directory.** `config_dir` is `/usr/local/apisix/conf`, and `docker cp` merges our two files into the existing directory.
- **TLS certificates live in the `ssls:` section.** Because of that, only `proxy/` (which also serves `proto.tls` and `proto.h2`) carries one.

- [ ] **Step 1: Pin the image**

Pick the latest stable APISIX release (apisix.apache.org/downloads; Docker tag `<version>-debian`):
```bash
docker pull apache/apisix:<VERSION>-debian
docker image inspect --format '{{index .RepoDigests 0}}' apache/apisix:<VERSION>-debian
```

- [ ] **Step 2: Write `gateway.toml`**

```toml
name = "apisix"
image = "apache/apisix:<VERSION>-debian@sha256:<DIGEST>"
config_dir = "/usr/local/apisix/conf"
common = "common"
ports = { plain = 8000, tls = 8443 }

[env]
APISIX_STAND_ALONE = "true"

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "proxy"
"proto.h2" = "proxy"
"core.routes-1k" = "routes-1k"
"plugin.key-auth" = "key-auth"
"plugin.jwt" = "jwt"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"
"plugin.chain" = "chain"
"script.header" = "script"

[na]
```

- [ ] **Step 3: Write `common/config.yaml`**

```yaml
# Standalone data plane: routes from conf/apisix.yaml, no etcd, no Admin API.
deployment:
  role: data_plane
  role_data_plane:
    config_provider: yaml

apisix:
  node_listen:
    - 8000
  enable_admin: false
  ssl:
    enable: true
    listen:
      - port: 8443
        enable_http2: true
    ssl_protocols: TLSv1.3
    fallback_sni: bench.local

nginx_config:
  worker_processes: @@CORES@@
  error_log_level: warn
  http:
    enable_access_log: false
    keepalive_timeout: 300s
    upstream:
      keepalive: 512
      keepalive_requests: 1000000
      keepalive_timeout: 300s
```

- [ ] **Step 4: Write the per-scenario `apisix.yaml` files**

Every file starts with this upstream and ends with a line `#END`:
```yaml
upstreams:
  - id: 1
    type: roundrobin
    scheme: http
    pass_host: pass
    nodes:
      "@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@": 1
```

`proxy/apisix.yaml`:
```yaml
upstreams:
  - id: 1
    type: roundrobin
    scheme: http
    pass_host: pass
    nodes:
      "@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@": 1
routes:
  - id: bench
    uri: /bench/*
    upstream_id: 1
ssls:
  - id: bench
    snis: ["bench.local"]
    cert: |
      @@TLS_CERT_PEM@@
    key: |
      @@TLS_KEY_PEM@@
#END
```

`routes-1k/apisix.yaml` (upstream block as above), then:
```yaml
routes:
@@REPEAT 1000@@
  - id: r@@I@@
    uri: /r@@I@@/*
    upstream_id: 1
@@END@@
#END
```

`key-auth/apisix.yaml` (upstream block), then:
```yaml
consumers:
  - username: bench
    plugins:
      key-auth:
        key: "@@API_KEY@@"
routes:
  - id: bench
    uri: /bench/*
    upstream_id: 1
    plugins:
      key-auth: {}
#END
```

`jwt/apisix.yaml` (upstream block), then:
```yaml
consumers:
  - username: bench
    plugins:
      jwt-auth:
        key: "@@JWT_ISSUER@@"
        secret: "@@JWT_SECRET@@"
        algorithm: HS256
routes:
  - id: bench
    uri: /bench/*
    upstream_id: 1
    plugins:
      jwt-auth: {}
#END
```

`rate-limit/apisix.yaml` (upstream block), then:
```yaml
routes:
  - id: bench
    uri: /bench/*
    upstream_id: 1
    plugins:
      limit-count:
        count: 100000000
        time_window: 60
        key_type: var
        key: remote_addr
        policy: local
        rejected_code: 429
  - id: limited
    uri: /limited/*
    upstream_id: 1
    plugins:
      limit-count:
        count: 1
        time_window: 60
        key_type: var
        key: remote_addr
        policy: local
        rejected_code: 429
#END
```

`header-rewrite/apisix.yaml` (upstream block), then:
```yaml
routes:
  - id: bench
    uri: /bench/*
    upstream_id: 1
    plugins:
      proxy-rewrite:
        headers:
          set:
            X-Bench-Added: "1"
            X-Bench-Added2: "2"
      response-rewrite:
        headers:
          remove:
            - X-Bench-Remove
#END
```

`chain/apisix.yaml` (upstream block), then:
```yaml
consumers:
  - username: bench
    plugins:
      key-auth:
        key: "@@API_KEY@@"
routes:
  - id: bench
    uri: /bench/*
    upstream_id: 1
    plugins:
      key-auth: {}
      limit-count:
        count: 100000000
        time_window: 60
        key_type: var
        key: remote_addr
        policy: local
        rejected_code: 429
      proxy-rewrite:
        headers:
          set:
            X-Bench-Added: "1"
            X-Bench-Added2: "2"
      response-rewrite:
        headers:
          remove:
            - X-Bench-Remove
  - id: limited
    uri: /limited/*
    upstream_id: 1
    plugins:
      key-auth: {}
      limit-count:
        count: 1
        time_window: 60
        key_type: var
        key: remote_addr
        policy: local
        rejected_code: 429
#END
```

`script/apisix.yaml` (upstream block), then:
```yaml
routes:
  - id: bench
    uri: /bench/*
    upstream_id: 1
    plugins:
      serverless-pre-function:
        phase: rewrite
        functions:
          - "return function(conf, ctx) local v = ngx.var.http_x_bench_in or '' ngx.req.set_header('X-Bench-Script', string.upper(v) .. '-' .. #v) end"
#END
```

- [ ] **Step 5: Write `TUNING.md`**

```markdown
# Apache APISIX: tuning

Image: `apache/apisix:<VERSION>-debian` (digest in gateway.toml), resolved <DATE>.

| Setting | Value | Why / source |
|---|---|---|
| Deployment | standalone YAML data plane, Admin API off | APISIX docs "Deployment modes: standalone". No etcd round trips; static config like every other gateway. |
| `nginx_config.worker_processes` | profile cores | APISIX benchmark guidance: one worker per core. |
| `enable_access_log` | false | Same for every gateway (spec §8). |
| upstream keepalive | pool 512, 1,000,000 requests, 300 s | APISIX `config-default.yaml` `nginx_config.http.upstream`, raised so pooled connections are never recycled mid-run. |
| TLS | TLS 1.3, `enable_http2` on 8443, `fallback_sni: bench.local` | Load tools connect by container name or IP; the fallback SNI selects the bench cert. |
| Plugins | APISIX default plugin list, left unchanged | Only plugins configured on a route execute. |
| limit-count | `policy: local`, 100,000,000 / 60 s | Never trips; the probe route uses count 1. |
| script | `serverless-pre-function`, rewrite phase | APISIX's first-party Lua hook. |
```

- [ ] **Step 6: Check it loads, validate, measure**

```bash
python -m unittest discover -s bench/tests -t bench
python bench/bench.py validate --gateways apisix
python bench/bench.py run --gateways apisix --scenarios core.proxy --profiles 1 --quick
```
Expected: the unit tests pass, then `12/12 cells valid`, then an `ok` (or bound) result. Consult the pinned version's docs if a key has moved. APISIX's jwt-auth consumer lookup (`key` claim) and `ssls` fields have changed across 3.x; the token carries both `key` and `iss`. Every fix is recorded in TUNING.md.

- [ ] **Step 7: Commit** (only if authorized)

```bash
git add bench/gateways/apisix
git commit -m "feat(bench): apache apisix adapter"
```

---
### Task 12: Kong (OSS) adapter

**Files:**
- Create under `bench/gateways/kong/`:
  - `gateway.toml`, `TUNING.md`
  - `kong.yml` in each of `proxy/`, `routes-1k/`, `key-auth/`, `jwt/`, `rate-limit/`, `header-rewrite/`, `chain/` and `script/`

**Interfaces:**
- Consumes: the adapter format (Task 1) and the tokens (Tasks 5, 9).
- Produces: `kong` in the matrix, supporting all 12 scenarios.

How Kong is set up for the benchmark:
- **DB-less mode** (`KONG_DATABASE=off`) with a declarative `kong.yml`.
- **Configured entirely through `KONG_*` env vars.** TLS uses the default certificate (`KONG_SSL_CERT`), which answers every SNI.

- [ ] **Step 1: Pin the image**

Use the latest **open-source** Kong Gateway image, from the official `kong` library image (hub.docker.com/_/kong). Kong stopped publishing new OSS builds after the 3.9 line. If no newer OSS tag exists, pin the newest OSS one (e.g. `3.9.1`) and say so in TUNING.md; don't use `kong/kong-gateway` (Enterprise).
```bash
docker pull kong:<VERSION>
docker image inspect --format '{{index .RepoDigests 0}}' kong:<VERSION>
```

- [ ] **Step 2: Write `gateway.toml`**

```toml
name = "kong"
image = "kong:<VERSION>@sha256:<DIGEST>"
config_dir = "/kong/bench"
ports = { plain = 8000, tls = 8443 }

[env]
KONG_DATABASE = "off"
KONG_DECLARATIVE_CONFIG = "/kong/bench/kong.yml"
KONG_PROXY_LISTEN = "0.0.0.0:8000 reuseport backlog=65535, 0.0.0.0:8443 http2 ssl reuseport backlog=65535"
KONG_ADMIN_LISTEN = "off"
KONG_STATUS_LISTEN = "off"
KONG_NGINX_WORKER_PROCESSES = "@@CORES@@"
KONG_PROXY_ACCESS_LOG = "off"
KONG_ADMIN_ACCESS_LOG = "off"
KONG_LOG_LEVEL = "warn"
KONG_SSL_CERT = "/kong/bench/tls/cert.pem"
KONG_SSL_CERT_KEY = "/kong/bench/tls/key.pem"
KONG_SSL_PROTOCOLS = "TLSv1.3"
KONG_UPSTREAM_KEEPALIVE_POOL_SIZE = "512"
KONG_UPSTREAM_KEEPALIVE_MAX_REQUESTS = "1000000"
KONG_UPSTREAM_KEEPALIVE_IDLE_TIMEOUT = "300"
KONG_NGINX_HTTP_KEEPALIVE_REQUESTS = "1000000"
KONG_NGINX_HTTP_KEEPALIVE_TIMEOUT = "300s"
KONG_ANONYMOUS_REPORTS = "off"

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "proxy"
"proto.h2" = "proxy"
"core.routes-1k" = "routes-1k"
"plugin.key-auth" = "key-auth"
"plugin.jwt" = "jwt"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"
"plugin.chain" = "chain"
"script.header" = "script"

[na]
```

- [ ] **Step 3: Write the `kong.yml` files**

`proxy/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
      - name: bench
        paths: ["/bench/"]
        strip_path: false
```

`routes-1k/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
@@REPEAT 1000@@
      - name: r@@I@@
        paths: ["/r@@I@@/"]
        strip_path: false
@@END@@
```

`key-auth/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
      - name: bench
        paths: ["/bench/"]
        strip_path: false
        plugins:
          - name: key-auth
consumers:
  - username: bench
    keyauth_credentials:
      - key: "@@API_KEY@@"
```

`jwt/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
      - name: bench
        paths: ["/bench/"]
        strip_path: false
        plugins:
          - name: jwt
            config:
              claims_to_verify: [exp]
consumers:
  - username: bench
    jwt_secrets:
      - key: "@@JWT_ISSUER@@"
        secret: "@@JWT_SECRET@@"
        algorithm: HS256
```

`rate-limit/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
      - name: bench
        paths: ["/bench/"]
        strip_path: false
        plugins:
          - name: rate-limiting
            config: { minute: 100000000, policy: local, limit_by: ip }
      - name: limited
        paths: ["/limited/"]
        strip_path: false
        plugins:
          - name: rate-limiting
            config: { minute: 1, policy: local, limit_by: ip }
```

`header-rewrite/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
      - name: bench
        paths: ["/bench/"]
        strip_path: false
        plugins:
          - name: request-transformer
            config:
              add:
                headers: ["X-Bench-Added:1", "X-Bench-Added2:2"]
          - name: response-transformer
            config:
              remove:
                headers: ["X-Bench-Remove"]
```

`chain/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
      - name: bench
        paths: ["/bench/"]
        strip_path: false
        plugins:
          - name: key-auth
          - name: rate-limiting
            config: { minute: 100000000, policy: local, limit_by: ip }
          - name: request-transformer
            config:
              add:
                headers: ["X-Bench-Added:1", "X-Bench-Added2:2"]
          - name: response-transformer
            config:
              remove:
                headers: ["X-Bench-Remove"]
      - name: limited
        paths: ["/limited/"]
        strip_path: false
        plugins:
          - name: key-auth
          - name: rate-limiting
            config: { minute: 1, policy: local, limit_by: ip }
consumers:
  - username: bench
    keyauth_credentials:
      - key: "@@API_KEY@@"
```

`script/kong.yml`:
```yaml
_format_version: "3.0"
services:
  - name: bench
    url: http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@
    routes:
      - name: bench
        paths: ["/bench/"]
        strip_path: false
        plugins:
          - name: pre-function
            config:
              access:
                - "local v = kong.request.get_header('x-bench-in') or '' kong.service.request.set_header('X-Bench-Script', string.upper(v) .. '-' .. #v)"
```

- [ ] **Step 4: Write `TUNING.md`**

```markdown
# Kong Gateway (OSS): tuning

Image: `kong:<VERSION>` (digest in gateway.toml), resolved <DATE>. <If pinned to the last OSS line, say so here.>

| Setting | Value | Why / source |
|---|---|---|
| Mode | DB-less, declarative `kong.yml`; Admin and Status listeners off | Kong docs "DB-less and declarative configuration". Static config, no database. |
| `nginx_worker_processes` | profile cores | Kong docs "Performance testing / benchmark" recommendations: workers = cores. |
| `proxy_access_log` | off | Same for every gateway (spec §8). |
| `proxy_listen` | `reuseport backlog=65535`; `http2 ssl` on 8443 | Kong's published performance-testing configuration. |
| upstream keepalive | pool 512, 1,000,000 requests, 300 s idle | `upstream_keepalive_*` in kong.conf reference. |
| TLS | TLS 1.3 only, default cert (`ssl_cert`) | Same cert for every gateway; the default cert answers any SNI. |
| jwt | `claims_to_verify: [exp]` | Featherbit, APISIX and Envoy always verify `exp`; Kong only does when asked. |
| rate-limiting | `policy: local`, 100,000,000 / minute, by IP | Never trips; the probe route uses 1/minute. |
| script | `pre-function` (access phase), default `untrusted_lua = sandbox` | Kong's first-party Lua hook. |
```

- [ ] **Step 5: Check it loads, validate, measure**

```bash
python -m unittest discover -s bench/tests -t bench
python bench/bench.py validate --gateways kong
python bench/bench.py run --gateways kong --scenarios core.proxy --profiles 1 --quick
```
Expected: `12/12 cells valid`, then a numeric result. Record every config deviation in TUNING.md.

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/gateways/kong
git commit -m "feat(bench): kong adapter"
```

---
### Task 13: Envoy adapter

**Files:**
- Create under `bench/gateways/envoy/`:
  - `gateway.toml`, `TUNING.md`
  - `envoy.yaml` in each of `proxy/`, `routes-1k/`, `key-auth/`, `jwt/`, `rate-limit/`, `header-rewrite/`, `chain/` and `script/`

**Interfaces:**
- Consumes: the adapter format (Task 1) and the tokens (Tasks 5, 9).
- Produces: `envoy` in the matrix. It supports all 12 scenarios, unless the pinned version lacks `envoy.filters.http.api_key_auth`. In that case, `plugin.key-auth` and `plugin.chain` move to `[na]` with reason "No built-in API-key filter in Envoy <version>.".

How Envoy is set up for the benchmark:
- **Static bootstrap only**: no xDS and no admin listener.
- **Worker count** comes from `--concurrency`.
- **The upstream cluster's circuit breakers are raised.** The defaults cap at 1024 pending/active requests, which a benchmark can hit; Envoy's own docs call this out.
- **Every non-proxy config has only the plain listener**, since TLS scenarios use `proxy/`.

- [ ] **Step 1: Pin the image and check the key-auth filter**

Pick the latest stable Envoy release (github.com/envoyproxy/envoy/releases; image `envoyproxy/envoy:v<X.Y.Z>`):
```bash
docker pull envoyproxy/envoy:v<X.Y.Z>
docker image inspect --format '{{index .RepoDigests 0}}' envoyproxy/envoy:v<X.Y.Z>
docker run --rm envoyproxy/envoy:v<X.Y.Z> --version
```
Check whether the release notes or docs for that version list `envoy.filters.http.api_key_auth` ("API key auth" HTTP filter). The Step 3 validate run confirms it either way.

- [ ] **Step 2: Write `gateway.toml`**

```toml
name = "envoy"
image = "envoyproxy/envoy:v<X.Y.Z>@sha256:<DIGEST>"
config_dir = "/etc/envoy/bench"
command = ["-c", "/etc/envoy/bench/envoy.yaml", "--concurrency", "@@CORES@@", "--log-level", "warn"]
ports = { plain = 8000, tls = 8443 }

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "proxy"
"proto.h2" = "proxy"
"core.routes-1k" = "routes-1k"
"plugin.key-auth" = "key-auth"
"plugin.jwt" = "jwt"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"
"plugin.chain" = "chain"
"script.header" = "script"

[na]
```
The image's entrypoint runs `envoy` when the first argument starts with `-`.

- [ ] **Step 3: Write the `envoy.yaml` files**

Every file ends with the same `clusters:` section:
```yaml
  clusters:
    - name: upstream
      type: STRICT_DNS
      connect_timeout: 1s
      circuit_breakers:
        thresholds:
          - max_connections: 1000000
            max_pending_requests: 1000000
            max_requests: 1000000
            max_retries: 1000000
      load_assignment:
        cluster_name: upstream
        endpoints:
          - lb_endpoints:
              - endpoint:
                  address:
                    socket_address: { address: "@@UPSTREAM_HOST@@", port_value: @@UPSTREAM_PORT@@ }
```

`proxy/envoy.yaml`. The YAML anchor reuses one connection manager for both listeners:
```yaml
static_resources:
  listeners:
    - name: plain
      address:
        socket_address: { address: 0.0.0.0, port_value: 8000 }
      filter_chains:
        - filters:
            - name: envoy.filters.network.http_connection_manager
              typed_config: &hcm
                "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
                stat_prefix: bench
                codec_type: AUTO
                route_config:
                  name: bench
                  virtual_hosts:
                    - name: bench
                      domains: ["*"]
                      routes:
                        - match: { prefix: "/bench/" }
                          route: { cluster: upstream }
                http_filters:
                  - name: envoy.filters.http.router
                    typed_config:
                      "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router
    - name: tls
      address:
        socket_address: { address: 0.0.0.0, port_value: 8443 }
      filter_chains:
        - transport_socket:
            name: envoy.transport_sockets.tls
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext
              common_tls_context:
                alpn_protocols: ["h2", "http/1.1"]
                tls_params: { tls_minimum_protocol_version: TLSv1_3 }
                tls_certificates:
                  - certificate_chain: { filename: "@@CONFIG_DIR@@/tls/cert.pem" }
                    private_key: { filename: "@@CONFIG_DIR@@/tls/key.pem" }
          filters:
            - name: envoy.filters.network.http_connection_manager
              typed_config: *hcm
  clusters:
    - name: upstream
      type: STRICT_DNS
      connect_timeout: 1s
      circuit_breakers:
        thresholds:
          - max_connections: 1000000
            max_pending_requests: 1000000
            max_requests: 1000000
            max_retries: 1000000
      load_assignment:
        cluster_name: upstream
        endpoints:
          - lb_endpoints:
              - endpoint:
                  address:
                    socket_address: { address: "@@UPSTREAM_HOST@@", port_value: @@UPSTREAM_PORT@@ }
```

The other seven files share one plain-listener skeleton. In it, `ROUTES` and `FILTERS` below stand for the per-scenario blocks. Write each file with those blocks inlined, plus the `clusters:` section above:
```yaml
static_resources:
  listeners:
    - name: plain
      address:
        socket_address: { address: 0.0.0.0, port_value: 8000 }
      filter_chains:
        - filters:
            - name: envoy.filters.network.http_connection_manager
              typed_config:
                "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
                stat_prefix: bench
                codec_type: AUTO
                route_config:
                  name: bench
                  virtual_hosts:
                    - name: bench
                      domains: ["*"]
                      routes:
                        # ROUTES
                http_filters:
                  # FILTERS (each file's extra filters, then always last:)
                  - name: envoy.filters.http.router
                    typed_config:
                      "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router
```

The blocks per file:
- **`routes-1k`**
  - ROUTES:
    ```yaml
    @@REPEAT 1000@@
                            - match: { prefix: "/r@@I@@/" }
                              route: { cluster: upstream }
    @@END@@
    ```
  - FILTERS: none.
- **`key-auth`**
  - ROUTES: `- match: { prefix: "/bench/" }` / `route: { cluster: upstream }`.
  - FILTERS:
    ```yaml
                      - name: envoy.filters.http.api_key_auth
                        typed_config:
                          "@type": type.googleapis.com/envoy.extensions.filters.http.api_key_auth.v3.ApiKeyAuth
                          credentials:
                            - key: "@@API_KEY@@"
                              client: bench
                          key_sources:
                            - header: apikey
    ```
- **`jwt`**
  - ROUTES: as key-auth.
  - FILTERS:
    ```yaml
                      - name: envoy.filters.http.jwt_authn
                        typed_config:
                          "@type": type.googleapis.com/envoy.extensions.filters.http.jwt_authn.v3.JwtAuthentication
                          providers:
                            bench:
                              issuer: "@@JWT_ISSUER@@"
                              forward: true
                              local_jwks:
                                inline_string: '@@JWKS_JSON@@'
                              from_headers:
                                - name: Authorization
                                  value_prefix: "Bearer "
                          rules:
                            - match: { prefix: "/bench/" }
                              requires: { provider_name: bench }
    ```
- **`rate-limit`**
  - ROUTES (the probe route first, with a per-route bucket of 1 token per minute):
    ```yaml
                            - match: { prefix: "/limited/" }
                              route: { cluster: upstream }
                              typed_per_filter_config:
                                envoy.filters.http.local_ratelimit:
                                  "@type": type.googleapis.com/envoy.extensions.filters.http.local_ratelimit.v3.LocalRateLimit
                                  stat_prefix: probe
                                  token_bucket: { max_tokens: 1, tokens_per_fill: 1, fill_interval: 60s }
                                  filter_enabled: { runtime_key: probe_enabled, default_value: { numerator: 100, denominator: HUNDRED } }
                                  filter_enforced: { runtime_key: probe_enforced, default_value: { numerator: 100, denominator: HUNDRED } }
                            - match: { prefix: "/bench/" }
                              route: { cluster: upstream }
    ```
  - FILTERS:
    ```yaml
                      - name: envoy.filters.http.local_ratelimit
                        typed_config:
                          "@type": type.googleapis.com/envoy.extensions.filters.http.local_ratelimit.v3.LocalRateLimit
                          stat_prefix: bench
                          token_bucket: { max_tokens: 100000000, tokens_per_fill: 100000000, fill_interval: 60s }
                          filter_enabled: { runtime_key: bench_enabled, default_value: { numerator: 100, denominator: HUNDRED } }
                          filter_enforced: { runtime_key: bench_enforced, default_value: { numerator: 100, denominator: HUNDRED } }
    ```
- **`header-rewrite`**
  - ROUTES:
    ```yaml
                            - match: { prefix: "/bench/" }
                              route: { cluster: upstream }
                              request_headers_to_add:
                                - header: { key: X-Bench-Added, value: "1" }
                                  append_action: OVERWRITE_IF_EXISTS_OR_ADD
                                - header: { key: X-Bench-Added2, value: "2" }
                                  append_action: OVERWRITE_IF_EXISTS_OR_ADD
                              response_headers_to_remove: ["X-Bench-Remove"]
    ```
  - FILTERS: none.
- **`chain`**
  - ROUTES: the `rate-limit` `/limited/` route, then the `header-rewrite` `/bench/` route.
  - FILTERS: the `key-auth` filter, then the `rate-limit` filter.
- **`script`**
  - ROUTES: as key-auth.
  - FILTERS:
    ```yaml
                      - name: envoy.filters.http.lua
                        typed_config:
                          "@type": type.googleapis.com/envoy.extensions.filters.http.lua.v3.Lua
                          default_source_code:
                            inline_string: |
                              function envoy_on_request(handle)
                                local v = handle:headers():get("x-bench-in") or ""
                                handle:headers():replace("x-bench-script", string.upper(v) .. "-" .. #v)
                              end
    ```

Indentation: the ROUTES items sit under `routes:` and the FILTERS items under `http_filters:`, exactly as in `proxy/envoy.yaml`. Envoy rejects a misindented file at boot, and the boot log in `logs/` quotes the offending field.

- [ ] **Step 4: Write `TUNING.md`**

```markdown
# Envoy: tuning

Image: `envoyproxy/envoy:v<X.Y.Z>` (digest in gateway.toml), resolved <DATE>.

| Setting | Value | Why / source |
|---|---|---|
| Bootstrap | static, no admin listener, no xDS | Static config like every other gateway. |
| `--concurrency` | profile cores | Envoy docs "Threading model": one worker thread per core. |
| Access log | none configured | Envoy logs no requests unless an access_log is configured (spec §8). |
| Circuit breakers | 1,000,000 on the upstream cluster | Envoy docs "Circuit breaking" / FAQ "benchmarking": the 1024 defaults throttle benchmarks. |
| TLS | TLS 1.3 minimum, ALPN h2 + http/1.1 | Same cert/protocol for every gateway. |
| jwt | `jwt_authn` with a local HS256 JWKS (oct key) | Built-in filter; verifies signature and `exp`. |
| key-auth | `api_key_auth` filter (if present in this version) | Built-in filter. |
| rate limit | `local_ratelimit` token bucket 100,000,000 / 60 s | Never trips. Envoy's local bucket is per filter config, not per client IP; with one load-generator IP it's equivalent to the others' per-IP counters. |
| script | `envoy.filters.http.lua` | Envoy's first-party scripting filter. |
```

- [ ] **Step 5: Check it loads, validate, measure**

```bash
python -m unittest discover -s bench/tests -t bench
python bench/bench.py validate --gateways envoy
python bench/bench.py run --gateways envoy --scenarios core.proxy --profiles 1 --quick
```
Expected: `12/12 cells valid`. If the api_key_auth filter doesn't exist, the key-auth/chain cells fail boot with "Didn't find a registered implementation". Move those two scenarios to `[na]` with the reason given in Interfaces, delete the two config dirs, and expect `10/10`.

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/gateways/envoy
git commit -m "feat(bench): envoy adapter"
```

---
### Task 14: Tyk (OSS) adapter

**Files:**
- Create under `bench/gateways/tyk/`:
  - `gateway.toml`, `TUNING.md`
  - `setup/key.json`, `setup/key-chain.json`
  - `common/tyk.conf`, `common/policies/policies.json`
  - `proxy/apps/bench.json`
  - `tls/tyk.conf`, `tls/apps/bench.json`
  - `routes-1k/apps/r@@EACH_1000@@.json`
  - `key-auth/apps/bench.json`, `jwt/apps/bench.json`
  - `rate-limit/apps/bench.json`, `rate-limit/apps/limited.json`
  - `header-rewrite/apps/bench.json`
  - `chain/apps/bench.json`, `chain/apps/limited.json`
  - `script/tyk.conf`, `script/apps/bench.json`, `script/middleware/bench.js`

**Interfaces:**
- Consumes: the adapter format, including `[[dependency]]` and `[[setup]]` (Task 1), the driver's setup calls and `DEP_REDIS_*` tokens (Task 9), and `@@EACH_n@@` (Task 5).
- Produces: `tyk` in the matrix, supporting all 12 scenarios.

How Tyk is set up for the benchmark:
- **Tyk OSS needs Redis for everything**, including key storage, so Redis runs as a disclosed dependency on the upstream's cores.
- **APIs are file-based definitions** in `apps/`.
- **API keys can't be file-provisioned.** They're created through the gateway's own `/tyk/keys` API by the adapter's `[[setup]]` calls before the probes run.
- **Tyk listens on one port.** With `use_ssl`, that port is HTTPS, so TLS/h2 use the `tls` config on 8443.

- [ ] **Step 1: Pin the images**

Pick the latest stable Tyk Gateway OSS release (`tykio/tyk-gateway:v<X.Y.Z>`) and the Redis version Tyk's docs list as supported (e.g. `redis:7.x-alpine`):
```bash
docker pull tykio/tyk-gateway:v<X.Y.Z>
docker image inspect --format '{{index .RepoDigests 0}}' tykio/tyk-gateway:v<X.Y.Z>
docker pull redis:<REDIS>-alpine
docker image inspect --format '{{index .RepoDigests 0}}' redis:<REDIS>-alpine
```

- [ ] **Step 2: Write `gateway.toml`**

```toml
name = "tyk"
image = "tykio/tyk-gateway:v<X.Y.Z>@sha256:<DIGEST>"
config_dir = "/opt/tyk-gateway/bench"
common = "common"
command = ["--conf=/opt/tyk-gateway/bench/tyk.conf"]
ports = { plain = 8000, tls = 8443 }

[env]
GOMAXPROCS = "@@CORES@@"

[[dependency]]
name = "redis"
image = "redis:<REDIS>-alpine@sha256:<DIGEST>"
port = 6379
command = ["redis-server", "--save", "", "--appendonly", "no", "--protected-mode", "no"]

[[setup]]
method = "POST"
path = "/tyk/keys/@@API_KEY@@"
scenarios = ["plugin.key-auth"]
headers = { "x-tyk-authorization" = "@@TYK_SECRET@@", "content-type" = "application/json" }
body_file = "setup/key.json"

[[setup]]
method = "POST"
path = "/tyk/keys/@@API_KEY@@"
scenarios = ["plugin.chain"]
headers = { "x-tyk-authorization" = "@@TYK_SECRET@@", "content-type" = "application/json" }
body_file = "setup/key-chain.json"

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "tls"
"proto.h2" = "tls"
"core.routes-1k" = "routes-1k"
"plugin.key-auth" = "key-auth"
"plugin.jwt" = "jwt"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"
"plugin.chain" = "chain"
"script.header" = "script"

[na]
```

- [ ] **Step 3: Write the shared config, policy and key bodies**

`common/tyk.conf`:
```json
{
  "listen_port": 8000,
  "secret": "@@TYK_SECRET@@",
  "template_path": "/opt/tyk-gateway/templates",
  "app_path": "@@CONFIG_DIR@@/apps",
  "use_db_app_configs": false,
  "policies": {
    "policy_source": "file",
    "policy_record_name": "@@CONFIG_DIR@@/policies/policies.json"
  },
  "storage": {
    "type": "redis",
    "host": "@@DEP_REDIS_HOST@@",
    "port": @@DEP_REDIS_PORT@@,
    "optimisation_max_idle": 2000,
    "optimisation_max_active": 4000
  },
  "enable_analytics": false,
  "health_check": { "enable_health_checks": false },
  "hash_keys": true,
  "enable_jsvm": false,
  "log_level": "warn",
  "max_idle_connections_per_host": 512,
  "close_connections": false,
  "proxy_close_connections": false,
  "enable_non_transactional_rate_limiter": true,
  "enable_sentinel_rate_limiter": false,
  "enable_redis_rolling_limiter": false
}
```

`tls/tyk.conf` is the same file with `"listen_port": 8443` and this extra key:
```json
  "http_server_options": {
    "use_ssl": true,
    "enable_http2": true,
    "min_version": 772,
    "certificates": [
      { "domain_name": "*", "cert_file": "@@CONFIG_DIR@@/tls/cert.pem", "key_file": "@@CONFIG_DIR@@/tls/key.pem" }
    ]
  },
```
(772 = TLS 1.3.) `script/tyk.conf` is `common/tyk.conf` with `"enable_jsvm": true`. Write both files in full.

`common/policies/policies.json` (used by JWT sessions):
```json
{
  "bench": {
    "id": "bench", "name": "bench", "org_id": "default", "active": true,
    "rate": 1000000000, "per": 1, "quota_max": -1,
    "access_rights": { "bench": { "api_id": "bench", "api_name": "bench", "versions": ["Default"] } }
  }
}
```

`setup/key.json`:
```json
{
  "org_id": "default", "rate": 1000000000, "per": 1, "quota_max": -1, "expires": 0,
  "access_rights": { "bench": { "api_id": "bench", "api_name": "bench", "versions": ["Default"] } }
}
```

`setup/key-chain.json` is the same with a second access right `"limited": { "api_id": "limited", "api_name": "limited", "versions": ["Default"] }`.

- [ ] **Step 4: Write the API definitions**

The base keyless definition is `proxy/apps/bench.json`. Copy it to `tls/apps/bench.json`:
```json
{
  "name": "bench", "api_id": "bench", "org_id": "default", "active": true,
  "use_keyless": true,
  "definition": { "location": "header", "key": "x-api-version" },
  "version_data": {
    "not_versioned": true, "default_version": "Default",
    "versions": { "Default": { "name": "Default", "use_extended_paths": true } }
  },
  "proxy": { "listen_path": "/bench/", "target_url": "http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@/bench/", "strip_listen_path": true },
  "disable_rate_limit": true, "disable_quota": true, "do_not_track": true
}
```

The other files are this base with the changes listed:

- `routes-1k/apps/r@@EACH_1000@@.json`:
  - `"name": "r@@I@@"`, `"api_id": "r@@I@@"`
  - `"listen_path": "/r@@I@@/"`, `"target_url": "http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@/r@@I@@/"`
- `key-auth/apps/bench.json`:
  - `"use_keyless": false`
  - add `"auth": { "auth_header_name": "apikey" }` and `"auth_configs": { "authToken": { "auth_header_name": "apikey" } }`
- `jwt/apps/bench.json`:
  - `"use_keyless": false`, `"enable_jwt": true`, `"jwt_signing_method": "hmac"`, `"jwt_source": "@@JWT_SECRET_B64@@"`
  - `"jwt_identity_base_field": "sub"`, `"jwt_policy_field_name": "pol"`, `"jwt_default_policies": ["bench"]`
  - `"auth_configs": { "jwt": { "auth_header_name": "Authorization" } }`
- `rate-limit/apps/bench.json`: `"disable_rate_limit": false` and `"global_rate_limit": { "rate": 100000000, "per": 60 }`.
- `rate-limit/apps/limited.json`:
  - `"name": "limited"`, `"api_id": "limited"`
  - `"listen_path": "/limited/"`, `"target_url": "http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@/limited/"`
  - `"disable_rate_limit": false`, `"global_rate_limit": { "rate": 1, "per": 60 }`
- `header-rewrite/apps/bench.json`:
  - inside `versions.Default`, add `"global_headers": { "X-Bench-Added": "1", "X-Bench-Added2": "2" }` and `"global_response_headers_remove": ["X-Bench-Remove"]`
  - at top level, add `"response_processors": [{ "name": "header_injector", "options": {} }]`
- `chain/apps/bench.json`: the key-auth changes, the rate-limit changes and the header-rewrite changes combined.
- `chain/apps/limited.json`: the `rate-limit/apps/limited.json` file plus the key-auth changes.
- `script/apps/bench.json`: add
  ```json
  "custom_middleware": {
    "driver": "otto",
    "pre": [{ "name": "benchScript", "path": "@@CONFIG_DIR@@/middleware/bench.js", "require_session": false }]
  }
  ```

Write every file in full, as valid JSON.

`script/middleware/bench.js`:
```javascript
var benchScript = new TykJS.TykMiddleware.NewMiddleware({});

benchScript.NewProcessRequest(function (request, session) {
  var values = request.Headers["X-Bench-In"];
  var v = values && values.length ? values[0] : "";
  request.SetHeaders["X-Bench-Script"] = v.toUpperCase() + "-" + v.length;
  return benchScript.ReturnData(request, {});
});
```

- [ ] **Step 5: Write `TUNING.md`**

```markdown
# Tyk Gateway (OSS): tuning

Images: `tykio/tyk-gateway:v<X.Y.Z>` and `redis:<REDIS>-alpine` (digests in gateway.toml), resolved <DATE>.

| Setting | Value | Why / source |
|---|---|---|
| Redis | required dependency, on the upstream's cores, persistence off | Tyk OSS stores keys, sessions and rate-limit state in Redis; it can't run without it. Disclosed: Tyk gets extra CPU the others don't use. |
| `GOMAXPROCS` | profile cores | Go scheduler threads = pinned cores. |
| `enable_analytics` | false | No analytics pipeline; same "no request logging" rule (spec §8). |
| `log_level` | warn | Same for every gateway. |
| `max_idle_connections_per_host`, `close_connections: false` | 512 / keep-alive | Tyk docs "Performance tuning": upstream keep-alive. |
| Rate limiter | non-transactional (DRL) | Tyk docs recommend it for performance; API-level `global_rate_limit`. |
| `enable_jsvm` | only in the script config | Tyk docs: disable JSVM unless used. |
| keys | created via `/tyk/keys` before probes (API keys can't be file-provisioned) | Same credential as every gateway. |
| TLS | TLS 1.3 (`min_version: 772`), HTTP/2 enabled | Same cert/protocol for every gateway. |
```

- [ ] **Step 6: Check it loads, validate, measure**

```bash
python -m unittest discover -s bench/tests -t bench
python bench/bench.py validate --gateways tyk
python bench/bench.py run --gateways tyk --scenarios core.proxy,plugin.key-auth --profiles 1 --quick
```
Expected: `12/12 cells valid`. Tyk config keys have changed across 5.x (policy file location `policy_record_name` vs `policy_path`, `auth_configs`, `enable_http2`). Where the pinned version differs, follow its docs and note it in TUNING.md.

- [ ] **Step 7: Commit** (only if authorized)

```bash
git add bench/gateways/tyk
git commit -m "feat(bench): tyk adapter"
```

---
### Task 15: KrakenD (CE) and Traefik adapters

**Files:**
- Create under `bench/gateways/krakend/`:
  - `gateway.toml`, `TUNING.md`
  - `krakend.json` in each of `proxy/`, `tls/`, `routes-1k/`, `jwt/`, `rate-limit/`, `header-rewrite/` and `script/`
  - `jwt/jwks.json`
- Create under `bench/gateways/traefik/`:
  - `gateway.toml`, `TUNING.md`
  - `common/traefik.yml`
  - `dynamic.yml` in each of `proxy/`, `routes-1k/`, `rate-limit/`, `header-rewrite/` and `script/`
  - `script/traefik.yml`
  - `script/plugins-local/src/github.com/featherbitplatform/benchscript/benchscript.go`, `go.mod` and `.traefik.yml` in that same directory

**Interfaces:**
- Consumes: the adapter format (Task 1), tokens and templating (Tasks 5, 9).
- Produces:
  - `krakend` in the matrix: 10 scenarios, with `plugin.key-auth` and `plugin.chain` n/a.
  - `traefik` in the matrix: 9 scenarios, with `plugin.key-auth`, `plugin.jwt` and `plugin.chain` n/a.

**KrakenD.** KrakenD is an API aggregator, so a few settings matter here:
- **`no-op` encoding** is set on the endpoint and the backend. That makes KrakenD a transparent proxy: status, headers and body are passed through.
- **Client headers are not forwarded by default.** The script endpoint lists `X-Bench-In` in `input_headers`.
- **One port per process.** TLS makes that port HTTPS, hence the separate `tls` config on 8443.
- **No catch-all wildcard routes in CE.** Endpoints use `/bench/{size}`.

- [ ] **Step 1: Pin KrakenD and write its `gateway.toml`**

Use the latest stable KrakenD CE (`devopsfaith/krakend:<X.Y.Z>`; the official image is also published as `krakend:<X.Y.Z>`):
```bash
docker pull devopsfaith/krakend:<X.Y.Z>
docker image inspect --format '{{index .RepoDigests 0}}' devopsfaith/krakend:<X.Y.Z>
```

```toml
name = "krakend"
image = "devopsfaith/krakend:<X.Y.Z>@sha256:<DIGEST>"
config_dir = "/etc/krakend/bench"
command = ["run", "-c", "/etc/krakend/bench/krakend.json"]
ports = { plain = 8000, tls = 8443 }

[env]
GOMAXPROCS = "@@CORES@@"

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "tls"
"proto.h2" = "tls"
"core.routes-1k" = "routes-1k"
"plugin.jwt" = "jwt"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"
"script.header" = "script"

[na]
"plugin.key-auth" = "API-key authentication (auth/api-keys) is KrakenD Enterprise only."
"plugin.chain" = "Needs key-auth, which KrakenD CE lacks."
```

- [ ] **Step 2: Write the KrakenD configs**

`proxy/krakend.json`:
```json
{
  "version": 3,
  "port": 8000,
  "timeout": "10s",
  "output_encoding": "no-op",
  "max_idle_connections_per_host": 512,
  "idle_connection_timeout": "300s",
  "extra_config": {
    "router": { "disable_access_log": true, "return_error_msg": false },
    "telemetry/logging": { "level": "WARNING", "stdout": true }
  },
  "endpoints": [
    {
      "endpoint": "/bench/{size}",
      "method": "GET",
      "output_encoding": "no-op",
      "backend": [
        { "host": ["http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@"], "url_pattern": "/bench/{size}", "encoding": "no-op" }
      ]
    }
  ]
}
```

The other files are this one with these changes:
- `tls/krakend.json`: `"port": 8443` and a top-level `"tls": { "public_key": "@@CONFIG_DIR@@/tls/cert.pem", "private_key": "@@CONFIG_DIR@@/tls/key.pem", "min_version": "TLS13" }`. If the pinned version uses the newer `"tls": { "keys": [ { "public_key": …, "private_key": … } ], "min_version": "TLS13" }` form, use that instead.
- `routes-1k/krakend.json`: the `endpoints` array becomes
  ```json
    "endpoints": [
  @@REPEAT 1000 SEP ,@@
      { "endpoint": "/r@@I@@/{size}", "method": "GET", "output_encoding": "no-op",
        "backend": [ { "host": ["http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@"], "url_pattern": "/r@@I@@/{size}", "encoding": "no-op" } ] }
  @@END@@
    ]
  ```
- `jwt/krakend.json`: the endpoint gains `"extra_config": { "auth/validator": { "alg": "HS256", "jwk_local_path": "@@CONFIG_DIR@@/jwks.json", "disable_jwk_security": true, "issuer": "@@JWT_ISSUER@@", "cache": true } }`. `jwt/jwks.json` contains exactly `@@JWKS_JSON@@`.
- `rate-limit/krakend.json` has two endpoints:
  - `/bench/{size}` with `"extra_config": { "qos/ratelimit/router": { "client_max_rate": 100000000, "client_capacity": 100000000, "every": "1m", "strategy": "ip" } }`
  - `/limited/{size}` (backend `url_pattern` `/limited/{size}`) with `client_max_rate` 1, `client_capacity` 1, `every` "1m", `strategy` "ip"
- `header-rewrite/krakend.json`: the backend gains
  ```json
  "extra_config": { "modifier/martian": { "fifo.Group": { "scope": ["request", "response"], "aggregateErrors": true, "modifiers": [
    { "header.Modifier": { "scope": ["request"], "name": "X-Bench-Added", "value": "1" } },
    { "header.Modifier": { "scope": ["request"], "name": "X-Bench-Added2", "value": "2" } },
    { "header.Blacklist": { "scope": ["response"], "names": ["X-Bench-Remove"] } }
  ] } } }
  ```
- `script/krakend.json`: the endpoint gains `"input_headers": ["X-Bench-In"]`, and the backend gains
  ```json
  "extra_config": { "modifier/lua-backend": { "allow_open_libs": true,
    "pre": "local r = request.load(); local v = r:headers('X-Bench-In') or ''; r:headers('X-Bench-Script', string.upper(v) .. '-' .. string.len(v))" } }
  ```

Write every file in full, as valid JSON (after rendering, for `routes-1k`).

- [ ] **Step 3: KrakenD `TUNING.md`**

```markdown
# KrakenD CE: tuning

Image: `devopsfaith/krakend:<X.Y.Z>` (digest in gateway.toml), resolved <DATE>.

| Setting | Value | Why / source |
|---|---|---|
| Encoding | `no-op` on endpoint and backend | KrakenD docs "No-op encoding": turns the aggregator into a transparent proxy, the only fair mode for a proxy benchmark. |
| `router.disable_access_log` | true | Same "no request logging" rule (spec §8). |
| `GOMAXPROCS` | profile cores | Go scheduler threads = pinned cores. |
| `max_idle_connections_per_host` | 512 | KrakenD docs "HTTP transport settings": upstream keep-alive pool. |
| Routes | `/bench/{size}` parameters (CE has no catch-all wildcard) | Same request paths as every gateway. |
| jwt | `auth/validator`, local HS256 JWKS | Built-in CE validator. |
| rate limit | `qos/ratelimit/router` per client IP, 100,000,000 / minute | Never trips; probe route 1/minute. |
| script | `modifier/lua-backend` with `allow_open_libs` (needed for `string`) | KrakenD's first-party Lua. |
| Header forwarding | `input_headers` only where a scenario needs one | KrakenD forwards no client headers by default (by design). |
```

**Traefik:**
- **The file provider supplies the dynamic config.** `traefik.yml` is the static config.
- **Access logs and metrics are off by default.**
- **Scripting uses a Yaegi local plugin**, loaded from `./plugins-local` relative to the working directory, which is why `workdir` is the config dir.

- [ ] **Step 4: Pin Traefik and write its `gateway.toml`**

Use the latest stable Traefik release (`traefik:v<X.Y.Z>`):
```bash
docker pull traefik:v<X.Y.Z>
docker image inspect --format '{{index .RepoDigests 0}}' traefik:v<X.Y.Z>
```

```toml
name = "traefik"
image = "traefik:v<X.Y.Z>@sha256:<DIGEST>"
config_dir = "/etc/traefik/bench"
common = "common"
workdir = "/etc/traefik/bench"
command = ["--configFile=/etc/traefik/bench/traefik.yml"]
ports = { plain = 8000, tls = 8443 }

[env]
GOMAXPROCS = "@@CORES@@"

[configs]
"core.proxy" = "proxy"
"payload.64k" = "proxy"
"payload.1m" = "proxy"
"proto.tls" = "proxy"
"proto.h2" = "proxy"
"core.routes-1k" = "routes-1k"
"plugin.rate-limit" = "rate-limit"
"plugin.header-rewrite" = "header-rewrite"
"script.header" = "script"

[na]
"plugin.key-auth" = "Traefik has no built-in API-key middleware (only third-party plugins or forwardAuth to an external service)."
"plugin.jwt" = "JWT validation is a Traefik Hub (commercial) feature."
"plugin.chain" = "Needs key-auth, which Traefik lacks."
```

- [ ] **Step 5: Write the Traefik configs**

`common/traefik.yml`:
```yaml
entryPoints:
  web:
    address: ":8000"
  websecure:
    address: ":8443"
providers:
  file:
    filename: /etc/traefik/bench/dynamic.yml
log:
  level: WARN
serversTransport:
  maxIdleConnsPerHost: 512
global:
  checkNewVersion: false
  sendAnonymousUsage: false
```

`script/traefik.yml` is `common/traefik.yml` plus:
```yaml
experimental:
  localPlugins:
    benchscript:
      moduleName: github.com/featherbitplatform/benchscript
```

`proxy/dynamic.yml`:
```yaml
http:
  routers:
    bench:
      rule: "PathPrefix(`/bench/`)"
      entryPoints: [web]
      service: upstream
    bench-tls:
      rule: "PathPrefix(`/bench/`)"
      entryPoints: [websecure]
      service: upstream
      tls: {}
  services:
    upstream:
      loadBalancer:
        servers:
          - url: "http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@"
tls:
  options:
    default:
      minVersion: VersionTLS13
  stores:
    default:
      defaultCertificate:
        certFile: "@@CONFIG_DIR@@/tls/cert.pem"
        keyFile: "@@CONFIG_DIR@@/tls/key.pem"
```

`routes-1k/dynamic.yml`:
```yaml
http:
  routers:
@@REPEAT 1000@@
    r@@I@@:
      rule: "PathPrefix(`/r@@I@@/`)"
      entryPoints: [web]
      service: upstream
@@END@@
  services:
    upstream:
      loadBalancer:
        servers:
          - url: "http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@"
```

`rate-limit/dynamic.yml`:
```yaml
http:
  routers:
    bench:
      rule: "PathPrefix(`/bench/`)"
      entryPoints: [web]
      service: upstream
      middlewares: [limit]
    limited:
      rule: "PathPrefix(`/limited/`)"
      entryPoints: [web]
      service: upstream
      middlewares: [limit-probe]
  middlewares:
    limit:
      rateLimit: { average: 100000000, period: 1m, burst: 100000000 }
    limit-probe:
      rateLimit: { average: 1, period: 1m, burst: 1 }
  services:
    upstream:
      loadBalancer:
        servers:
          - url: "http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@"
```

`header-rewrite/dynamic.yml`:
```yaml
http:
  routers:
    bench:
      rule: "PathPrefix(`/bench/`)"
      entryPoints: [web]
      service: upstream
      middlewares: [rewrite]
  middlewares:
    rewrite:
      headers:
        customRequestHeaders:
          X-Bench-Added: "1"
          X-Bench-Added2: "2"
        customResponseHeaders:
          X-Bench-Remove: ""
  services:
    upstream:
      loadBalancer:
        servers:
          - url: "http://@@UPSTREAM_HOST@@:@@UPSTREAM_PORT@@"
```

`script/dynamic.yml`: the header-rewrite file with the middleware replaced by:
```yaml
  middlewares:
    script:
      plugin:
        benchscript: {}
```
The router's `middlewares` becomes `[script]`.

The plugin lives in `script/plugins-local/src/github.com/featherbitplatform/benchscript/`.

`go.mod`:
```
module github.com/featherbitplatform/benchscript

go 1.22
```

`.traefik.yml`:
```yaml
displayName: Bench script
type: middleware
import: github.com/featherbitplatform/benchscript
summary: Benchmark scripting scenario - derives X-Bench-Script from X-Bench-In.
testData: {}
```

`benchscript.go`:
```go
// Package benchscript is the benchmark's scripting scenario for Traefik: a Yaegi-interpreted
// middleware that derives X-Bench-Script (upper-cased value + "-" + length) from X-Bench-In.
package benchscript

import (
	"context"
	"net/http"
	"strconv"
	"strings"
)

// Config is empty: the plugin takes no options.
type Config struct{}

// CreateConfig returns the (empty) plugin configuration.
func CreateConfig() *Config { return &Config{} }

// Plugin is the middleware handler.
type Plugin struct{ next http.Handler }

// New builds the middleware.
func New(ctx context.Context, next http.Handler, config *Config, name string) (http.Handler, error) {
	return &Plugin{next: next}, nil
}

func (p *Plugin) ServeHTTP(rw http.ResponseWriter, req *http.Request) {
	v := req.Header.Get("X-Bench-In")
	req.Header.Set("X-Bench-Script", strings.ToUpper(v)+"-"+strconv.Itoa(len(v)))
	p.next.ServeHTTP(rw, req)
}
```

- [ ] **Step 6: Traefik `TUNING.md`**

```markdown
# Traefik: tuning

Image: `traefik:v<X.Y.Z>` (digest in gateway.toml), resolved <DATE>.

| Setting | Value | Why / source |
|---|---|---|
| Providers | file provider only; no Docker provider, dashboard or API | Static config like every other gateway. |
| Access log / metrics | not configured (off by default) | Same "no request logging" rule (spec §8). |
| `GOMAXPROCS` | profile cores | Go scheduler threads = pinned cores. |
| `serversTransport.maxIdleConnsPerHost` | 512 | Traefik docs "ServersTransport": default 200 upstream idle connections, raised. |
| TLS | `minVersion: VersionTLS13`, default certificate store | Same cert/protocol for every gateway; HTTP/2 negotiated by default. |
| rate limit | `rateLimit` middleware, 100,000,000 / minute | Never trips; probe route 1/minute. |
| script | Yaegi local plugin (`experimental.localPlugins`) | Traefik's first-party extension mechanism; interpreted Go. |
```

- [ ] **Step 7: Check both load, validate, measure**

```bash
python -m unittest discover -s bench/tests -t bench
python bench/bench.py validate --gateways krakend,traefik
python bench/bench.py run --gateways krakend,traefik --scenarios core.proxy --profiles 1 --quick
```
Expected: `19/19 cells valid` (10 KrakenD + 9 Traefik), then numeric results. KrakenD's martian and lua keys, and Traefik's local-plugin loading, are the likeliest to need adjusting to the pinned version. Record any adjustment in the TUNING.md files.

- [ ] **Step 8: Commit** (only if authorized)

```bash
git add bench/gateways/krakend bench/gateways/traefik
git commit -m "feat(bench): krakend and traefik adapters"
```

---
### Task 16: HTML report

**Files:**
- Create: `bench/report/__init__.py` (empty), `bench/report/render.py`, `bench/tests/test_report.py`

**Interfaces:**
- Consumes: the `run.json` shape written by `RunStore` (Task 4) and `cli.cmd_run` (Task 9). Its keys are:
  - `publish`, `topology.{name,kind}`, `selection.{scenarios,…}`, `params`
  - `fingerprint.{hosts,git}`, `images`
  - `cells[key].{gateway,scenario,profile,status,na_reason,summary}`; `summary` holds `max_sustainable_rps` and `ladder[]`, whose points carry `rate` and per-metric summaries (`median/min/max/n`)
- Produces: `report.render.render(run: dict) -> str` and `report.render.write_report(run_dir: Path) -> Path` (writes `<run_dir>/report.html`).

Design rules come from the dataviz method (spec §7):
- **Fixed colors per gateway:** featherbit→1 … nginx→8, from the validated reference palette, in light and dark. The `direct` baseline is a neutral dashed line, not a categorical color.
- **One y-axis per chart**, log-scaled for latency.
- **A legend on every multi-series chart.**
- **Every chart has a table view.** This is also required for the three light-mode slots under 3:1 contrast.
- **A crosshair tooltip on line charts, and a per-bar tooltip.**
- **Tooltips enhance, never gate:** n/a reasons are also listed in text under the grid.

- [ ] **Step 1: Write the failing tests**

`bench/tests/test_report.py`:
```python
import tempfile
import unittest
from html.parser import HTMLParser
from pathlib import Path

from benchlib.config import Params
from report.render import render, write_report


def s(v):
    return {"median": v, "min": v, "max": v, "n": 1}


def ladder(base):
    return [{"rate": r, "reps": 1, "p50_ms": s(base), "p90_ms": s(base * 1.5), "p99_ms": s(base * 3),
             "p999_ms": s(base * 5), "rps_per_core": s(r / 0.8), "gateway_rss_peak_mb": s(40.0),
             "gateway_cpu_cores": s(0.8)} for r in (1000, 5000)]


def cell(gw, sc, status, max_rps=None, base=None, reason=None):
    c = {"gateway": gw, "scenario": sc, "profile": 1, "status": status, "reps": []}
    if reason:
        c["na_reason"] = reason
    if max_rps is not None:
        c["summary"] = {"reps_ok": 1, "reps_total": 1, "max_sustainable_rps": s(max_rps), "ladder": ladder(base)}
    return c


def make_run(publish=False):
    import dataclasses
    cells = [
        cell("direct", "core.proxy", "upstream-bound", 90_000, 0.3),
        cell("featherbit", "core.proxy", "ok", 40_000, 0.5),
        cell("nginx", "core.proxy", "loadgen-bound", 60_000, 0.4),
        cell("featherbit", "plugin.jwt", "invalid"),
        cell("nginx", "plugin.jwt", "n/a", reason="JWT is <b>Plus</b> only"),
    ]
    return {
        "schema": 1, "started": "2026-09-26T10:00:00+00:00", "publish": publish,
        "topology": {"name": "local", "kind": "local"},
        "selection": {"gateways": ["direct", "featherbit", "nginx"], "scenarios": ["core.proxy", "plugin.jwt"],
                      "profiles": [1]},
        "params": dataclasses.asdict(Params()),
        "fingerprint": {"hosts": {"local": {"cpu_model": "Test CPU", "os": "Docker Desktop", "kernel": "6.6",
                                            "docker": "29.8.0", "governor": "unknown", "turbo": "unknown",
                                            "loadavg": "0.1 0.2 0.3", "ncpu": 8}},
                        "git": {"sha": "abc123", "dirty": False}},
        "images": {"featherbit": "featherbit-bench/featherbit:dev@sha256:1"},
        "cells": {f"{c['gateway']}|{c['scenario']}|1": c for c in cells},
    }


class Strict(HTMLParser):
    def __init__(self):
        super().__init__()
        self.figures = 0

    def handle_starttag(self, tag, attrs):
        self.figures += tag == "figure"


class ReportTests(unittest.TestCase):
    def test_banner_only_when_not_published(self):
        self.assertIn("NOT FOR PUBLICATION", render(make_run(publish=False)))
        self.assertNotIn("NOT FOR PUBLICATION", render(make_run(publish=True)))

    def test_statuses_and_bound_marker(self):
        html = render(make_run())
        self.assertIn("≥ 60.0k", html)       # loadgen-bound nginx shown as ">= X"
        self.assertIn(">invalid<", html)
        self.assertIn(">N/A<", html)

    def test_reasons_are_escaped_and_listed(self):
        html = render(make_run())
        self.assertIn("JWT is &lt;b&gt;Plus&lt;/b&gt; only", html)
        self.assertNotIn("<b>Plus</b>", html)

    def test_series_colors_follow_the_entity(self):
        html = render(make_run())
        self.assertIn("var(--series-1)", html)     # featherbit
        self.assertIn("var(--series-8)", html)     # nginx
        self.assertIn("var(--baseline)", html)     # direct

    def test_charts_have_table_views_and_parse(self):
        html = render(make_run())
        self.assertIn("<svg", html)
        self.assertIn("Table view", html)
        p = Strict()
        p.feed(html)
        self.assertGreater(p.figures, 0)

    def test_methodology_from_params(self):
        self.assertIn("p99 ≤ 10.0 ms", render(make_run()))

    def test_write_report(self):
        import json
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "run.json").write_text(json.dumps(make_run()), "utf-8")
            out = write_report(Path(d))
            self.assertEqual(out.name, "report.html")
            self.assertIn("<!doctype html>", out.read_text("utf-8").lower())
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: ERROR, `No module named 'report'`.

- [ ] **Step 3: Implement `bench/report/render.py`**

```python
"""Render a run's run.json into one self-contained report.html (inline SVG, no external requests)."""
from __future__ import annotations

import html
import json
import math
from pathlib import Path

GATEWAY_SLOTS = ["featherbit", "apisix", "kong", "envoy", "tyk", "krakend", "traefik", "nginx"]
BASELINE = "direct"
STATUS_LABEL = {"n/a": "N/A", "invalid": "invalid", "error": "error", "pending": "not run"}
BOUND = ("loadgen-bound", "upstream-bound")

CSS = """
:root { color-scheme: light;
  --surface-1: #fcfcfb; --surface-2: #f3f2ef; --grid: #e4e3df; --text-primary: #0b0b0b;
  --text-secondary: #52514e; --text-muted: #7a7973; --baseline: #7a7973; --warn-bg: #fdf1dc;
  --series-1: #2a78d6; --series-2: #eb6834; --series-3: #1baf7a; --series-4: #eda100;
  --series-5: #e87ba4; --series-6: #008300; --series-7: #4a3aa7; --series-8: #e34948; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { color-scheme: dark;
  --surface-1: #1a1a19; --surface-2: #242423; --grid: #383835; --text-primary: #ffffff;
  --text-secondary: #c3c2b7; --text-muted: #9a998f; --baseline: #9a998f; --warn-bg: #3a2e14;
  --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --series-4: #c98500;
  --series-5: #d55181; --series-6: #008300; --series-7: #9085e9; --series-8: #e66767; } }
:root[data-theme="dark"] { color-scheme: dark;
  --surface-1: #1a1a19; --surface-2: #242423; --grid: #383835; --text-primary: #ffffff;
  --text-secondary: #c3c2b7; --text-muted: #9a998f; --baseline: #9a998f; --warn-bg: #3a2e14;
  --series-1: #3987e5; --series-2: #d95926; --series-3: #199e70; --series-4: #c98500;
  --series-5: #d55181; --series-6: #008300; --series-7: #9085e9; --series-8: #e66767; }
* { box-sizing: border-box; }
body { margin: 0; background: var(--surface-1); color: var(--text-primary);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 1100px; margin: 0 auto; padding: 24px 16px 64px; }
h1 { font-size: 26px; margin: 0 0 4px; } h2 { font-size: 20px; margin: 40px 0 8px; }
h3 { font-size: 16px; margin: 24px 0 8px; color: var(--text-secondary); }
.banner { background: var(--warn-bg); border-radius: 8px; padding: 12px 16px; margin: 16px 0; font-weight: 600; }
.muted { color: var(--text-muted); }
.scroll { overflow-x: auto; }
table { border-collapse: collapse; font-variant-numeric: tabular-nums; font-size: 14px; }
th, td { padding: 6px 10px; border-bottom: 1px solid var(--grid); text-align: right; white-space: nowrap; }
th:first-child, td:first-child { text-align: left; }
td.status { color: var(--text-muted); } td.bound { color: var(--text-secondary); font-style: italic; }
.swatch { display: inline-block; width: 10px; height: 10px; border-radius: 2px; margin-right: 6px; }
.legend { display: flex; flex-wrap: wrap; gap: 4px 16px; font-size: 13px; color: var(--text-secondary); margin: 4px 0; }
.legend .dash { display: inline-block; width: 14px; border-top: 2px dashed var(--baseline); margin-right: 6px; vertical-align: middle; }
figure { margin: 16px 0; } figcaption { font-weight: 600; margin-bottom: 4px; }
svg { width: 100%; height: auto; display: block; }
svg text { fill: var(--text-muted); font-size: 11px; }
.grid line { stroke: var(--grid); stroke-width: 1; }
.crosshair { stroke: var(--text-muted); stroke-width: 1; visibility: hidden; }
details { margin-top: 4px; } summary { cursor: pointer; color: var(--text-secondary); font-size: 13px; }
.tip { position: fixed; pointer-events: none; background: var(--surface-2); color: var(--text-primary);
  border-radius: 6px; padding: 6px 10px; font-size: 13px; box-shadow: 0 2px 8px rgb(0 0 0 / .2); z-index: 10; }
.charts { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 0 24px; }
"""

JS = """
(() => {
  const tip = document.createElement("div"); tip.className = "tip"; tip.hidden = true; document.body.appendChild(tip);
  function show(evt, lines) {
    tip.replaceChildren(...lines.map(([value, label]) => {
      const row = document.createElement("div"); const b = document.createElement("strong");
      b.textContent = value; row.append(b, document.createTextNode(label ? " " + label : "")); return row;
    }));
    tip.hidden = false;
    const pad = 12, r = tip.getBoundingClientRect();
    let x = evt.clientX + pad, y = evt.clientY + pad;
    if (x + r.width > innerWidth) x = evt.clientX - r.width - pad;
    if (y + r.height > innerHeight) y = evt.clientY - r.height - pad;
    tip.style.left = x + "px"; tip.style.top = y + "px";
  }
  document.querySelectorAll("figure[data-chart]").forEach(fig => {
    const data = JSON.parse(fig.dataset.chart), svg = fig.querySelector("svg"), hair = svg.querySelector(".crosshair");
    svg.addEventListener("pointermove", e => {
      const pt = svg.createSVGPoint(); pt.x = e.clientX; pt.y = e.clientY;
      const p = pt.matrixTransform(svg.getScreenCTM().inverse());
      let best = null;
      for (const [rate, px] of data.xpos) if (!best || Math.abs(px - p.x) < Math.abs(best[1] - p.x)) best = [rate, px];
      if (!best) return;
      hair.setAttribute("x1", best[1]); hair.setAttribute("x2", best[1]); hair.style.visibility = "visible";
      const lines = [[data.rateLabels[best[0]], "req/s offered"]];
      for (const s of data.series) { const v = s.values[best[0]]; if (v !== undefined) lines.push([v, s.name]); }
      show(e, lines);
    });
    svg.addEventListener("pointerleave", () => { hair.style.visibility = "hidden"; tip.hidden = true; });
  });
  document.querySelectorAll("[data-tip]").forEach(el => {
    el.addEventListener("pointermove", e => show(e, [[el.dataset.tip, el.dataset.tipLabel || ""]]));
    el.addEventListener("pointerleave", () => { tip.hidden = true; });
  });
})();
"""


def esc(v) -> str:
    return html.escape(str(v), quote=True)


def color(gw: str) -> str:
    return f"var(--series-{GATEWAY_SLOTS.index(gw) + 1})" if gw in GATEWAY_SLOTS else "var(--baseline)"


def fmt_rps(v) -> str:
    if v is None:
        return "–"
    return f"{v / 1000:.1f}k" if v >= 10_000 else f"{v:,.0f}"


def fmt_ms(v) -> str:
    if v is None:
        return "–"
    return f"{v:.2f} ms" if v < 10 else f"{v:.1f} ms"


def med(summary) -> float | None:
    return summary["median"] if summary else None


def ordered_gateways(run: dict) -> list[str]:
    names = {c["gateway"] for c in run["cells"].values()}
    ranked = [BASELINE] if BASELINE in names else []
    ranked += [g for g in GATEWAY_SLOTS if g in names]
    return ranked + sorted(names - set(ranked))


def ordered_scenarios(run: dict) -> list[str]:
    present = {c["scenario"] for c in run["cells"].values()}
    order = [s for s in run.get("selection", {}).get("scenarios", []) if s in present]
    return order + sorted(present - set(order))


def profiles(run: dict) -> list[int]:
    return sorted({c["profile"] for c in run["cells"].values()})


def get(run: dict, gw: str, sc: str, prof: int) -> dict | None:
    return run["cells"].get(f"{gw}|{sc}|{prof}")


def summary_cell(c: dict | None) -> tuple[str, str, str]:
    """(text, css class, tooltip)"""
    if c is None:
        return "–", "status", ""
    st = c["status"]
    if st in STATUS_LABEL:
        return STATUS_LABEL[st], "status", c.get("na_reason", st)
    m = (c.get("summary") or {}).get("max_sustainable_rps")
    if not m:
        return "no result", "status", st
    text = fmt_rps(m["median"])
    if m["n"] > 1:
        text += f" ({fmt_rps(m['min'])}–{fmt_rps(m['max'])})"
    if st in BOUND:
        return "≥ " + text, "bound", f"{st}: the {st.split('-')[0]} saturated first; the gateway sustained at least this"
    return text, "ok", ""


def ladder_map(c: dict | None, metric: str) -> dict[int, float]:
    if not c or c["status"] not in ("ok", *BOUND):
        return {}
    out = {}
    for pt in (c.get("summary") or {}).get("ladder", []):
        v = med(pt.get(metric))
        if v is not None:
            out[pt["rate"]] = v
    return out


def line_chart(title: str, series: list[tuple[str, dict[int, float]]]) -> str:
    W, H, L, R, T, B = 640, 280, 60, 16, 12, 40
    pts = [(x, y) for _, d in series for x, y in d.items() if y > 0]
    if not pts:
        return f'<p class="muted">{esc(title)}: no measured data.</p>'
    xs = sorted({x for x, _ in pts})
    lx0, lx1 = math.log10(xs[0]), math.log10(xs[-1])
    if lx0 == lx1:
        lx0, lx1 = lx0 - 0.5, lx1 + 0.5
    ys = [y for _, y in pts]
    ly0, ly1 = math.floor(math.log10(min(ys))), math.ceil(math.log10(max(ys)))
    if ly0 == ly1:
        ly1 += 1

    def sx(x):
        return L + (math.log10(x) - lx0) / (lx1 - lx0) * (W - L - R)

    def sy(y):
        return H - B - (math.log10(y) - ly0) / (ly1 - ly0) * (H - T - B)

    parts = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="{esc(title)}">', '<g class="grid">']
    for e in range(ly0, ly1 + 1):
        y = sy(10 ** e)
        parts.append(f'<line x1="{L}" x2="{W - R}" y1="{y:.1f}" y2="{y:.1f}"/>')
        parts.append(f'<text x="{L - 6}" y="{y + 4:.1f}" text-anchor="end">{esc(fmt_ms(10 ** e))}</text>')
    for x in xs:
        parts.append(f'<text x="{sx(x):.1f}" y="{H - B + 16}" text-anchor="middle">{esc(fmt_rps(x))}</text>')
    parts.append(f'<text x="{(L + W - R) / 2:.0f}" y="{H - 6}" text-anchor="middle">offered load (req/s)</text></g>')
    for name, d in series:
        if not d:
            continue
        coords = [(sx(x), sy(y)) for x, y in sorted(d.items()) if y > 0]
        dash = ' stroke-dasharray="5 4"' if name == BASELINE else ""
        path = " ".join(f"{'M' if i == 0 else 'L'}{px:.1f},{py:.1f}" for i, (px, py) in enumerate(coords))
        parts.append(f'<path d="{path}" fill="none" stroke="{color(name)}" stroke-width="2"{dash}/>')
        for px, py in coords:
            parts.append(f'<circle cx="{px:.1f}" cy="{py:.1f}" r="4" fill="{color(name)}" '
                         f'stroke="var(--surface-1)" stroke-width="2"/>')
    parts.append(f'<line class="crosshair" x1="0" x2="0" y1="{T}" y2="{H - B}"/></svg>')
    payload = {
        "xpos": [[x, round(sx(x), 1)] for x in xs],
        "rateLabels": {x: fmt_rps(x) for x in xs},
        "series": [{"name": n, "values": {x: fmt_ms(y) for x, y in d.items()}} for n, d in series if d],
    }
    legend = "".join(
        (f'<span><span class="dash"></span>{esc(n)} (no gateway)</span>' if n == BASELINE else
         f'<span><span class="swatch" style="background:{color(n)}"></span>{esc(n)}</span>')
        for n, d in series if d)
    head = "".join(f"<th>{esc(fmt_rps(x))}</th>" for x in xs)
    rows = "".join(f"<tr><td>{esc(n)}</td>" + "".join(f"<td>{esc(fmt_ms(d.get(x)))}</td>" for x in xs) + "</tr>"
                   for n, d in series if d)
    return (f'<figure data-chart="{esc(json.dumps(payload))}"><figcaption>{esc(title)}</figcaption>'
            f'<div class="legend">{legend}</div>{"".join(parts)}'
            f'<details><summary>Table view</summary><div class="scroll"><table><tr><th>gateway \\ req/s</th>{head}</tr>'
            f'{rows}</table></div></details></figure>')


def bar_chart(title: str, rows: list[tuple[str, float]], fmt, unit: str) -> str:
    rows = [(n, v) for n, v in rows if v is not None]
    if not rows:
        return f'<p class="muted">{esc(title)}: no measured data.</p>'
    W, L, R, ROW = 640, 110, 90, 30
    H = ROW * len(rows) + 8
    top = max(v for _, v in rows) or 1
    parts = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="{esc(title)}">']
    for i, (name, v) in enumerate(rows):
        y = 4 + i * ROW
        w = max(2.0, (W - L - R) * v / top)
        parts.append(f'<text x="{L - 8}" y="{y + 18}" text-anchor="end">{esc(name)}</text>')
        # Rounded data end, square baseline end.
        r = min(4.0, w / 2)
        d = (f"M{L},{y + 4} H{L + w - r:.1f} Q{L + w:.1f},{y + 4} {L + w:.1f},{y + 4 + r:.1f} "
             f"V{y + 22 - r:.1f} Q{L + w:.1f},{y + 22} {L + w - r:.1f},{y + 22} H{L} Z")
        dash = ' stroke="var(--baseline)" stroke-dasharray="4 3" fill-opacity="0.25"' if name == BASELINE else ""
        parts.append(f'<path d="{d}" fill="{color(name)}"{dash} data-tip="{esc(fmt(v))} {esc(unit)}" '
                     f'data-tip-label="{esc(name)}"/>')
        parts.append(f'<text x="{L + w + 6:.1f}" y="{y + 18}">{esc(fmt(v))}</text>')
    parts.append("</svg>")
    table = "".join(f"<tr><td>{esc(n)}</td><td>{esc(fmt(v))} {esc(unit)}</td></tr>" for n, v in rows)
    return (f'<figure><figcaption>{esc(title)}</figcaption>{"".join(parts)}'
            f'<details><summary>Table view</summary><table>{table}</table></details></figure>')


def common_rate(run: dict, gws: list[str], sc: str, prof: int) -> int | None:
    """Highest ladder rate every measured gateway in this scenario/profile sustained."""
    sets = [set(ladder_map(get(run, g, sc, prof), "p50_ms")) for g in gws]
    sets = [s for s in sets if s]
    if not sets:
        return None
    shared = set.intersection(*sets)
    return max(shared) if shared else None


def baseline_cell(run: dict, sc: str, prof: int) -> dict | None:
    return get(run, BASELINE, sc, prof) or get(run, BASELINE, "core.proxy", prof)


def section_summary(run: dict, gws: list[str], scs: list[str]) -> str:
    out = ['<h2>Max sustainable throughput</h2>',
           f'<p class="muted">Median over repetitions of the highest offered rate with p99 ≤ '
           f'{esc(run["params"]["slo_p99_ms"])} ms and &lt; {esc(run["params"]["max_error_rate"] * 100)}% errors '
           f'(min–max in brackets). “≥”: the load generator or upstream saturated first.</p>']
    reasons = []
    for prof in profiles(run):
        out.append(f'<h3>{prof} core{"s" if prof > 1 else ""} for the gateway</h3><div class="scroll"><table><tr><th>scenario</th>')
        out += [f'<th><span class="swatch" style="background:{color(g)}"></span>{esc(g)}</th>' for g in gws]
        out.append("</tr>")
        for sc in scs:
            out.append(f"<tr><td>{esc(sc)}</td>")
            for g in gws:
                c = get(run, g, sc, prof)
                text, cls, tip = summary_cell(c)
                out.append(f'<td class="{cls}" title="{esc(tip)}">{esc(text)}</td>')
                if c and c["status"] == "n/a" and (g, c.get("na_reason")) not in reasons:
                    reasons.append((g, c.get("na_reason")))
            out.append("</tr>")
        out.append("</table></div>")
    if reasons:
        out.append("<h3>Not applicable</h3><ul>")
        out += [f"<li><strong>{esc(g)}</strong>: {esc(r)}</li>" for g, r in reasons]
        out.append("</ul>")
    return "".join(out)


def section_scenario(run: dict, gws: list[str], sc: str) -> str:
    out = [f'<h2>{esc(sc)}</h2>']
    for prof in profiles(run):
        cells = {g: get(run, g, sc, prof) for g in gws}
        if not any(ladder_map(c, "p50_ms") for c in cells.values()):
            continue
        out.append(f'<h3>{prof} core{"s" if prof > 1 else ""}</h3><div class="charts">')
        for metric, label in (("p50_ms", "p50"), ("p99_ms", "p99")):
            out.append(line_chart(f"{label} latency vs offered load",
                                  [(g, ladder_map(c, metric)) for g, c in cells.items()]))
        out.append("</div>")
        rate = common_rate(run, [g for g in gws if g != BASELINE], sc, prof)
        if rate is None:
            out.append('<p class="muted">No ladder rate was sustained by every gateway; per-core and overhead views skipped.</p>')
            continue
        at = lambda g, m: ladder_map(cells[g], m).get(rate)
        out.append('<div class="charts">')
        out.append(bar_chart(f"Throughput per gateway core at {fmt_rps(rate)} req/s",
                             [(g, at(g, "rps_per_core")) for g in gws if g != BASELINE],
                             fmt_rps, "req/s per core"))
        out.append(bar_chart(f"Peak memory at {fmt_rps(rate)} req/s",
                             [(g, at(g, "gateway_rss_peak_mb")) for g in gws if g != BASELINE],
                             lambda v: f"{v:.0f}", "MiB"))
        out.append("</div>")
        base = baseline_cell(run, sc, prof)
        b50, b99 = ladder_map(base, "p50_ms").get(rate), ladder_map(base, "p99_ms").get(rate)
        if b50 is not None:
            note = "" if base and base["scenario"] == sc else " (baseline: direct on core.proxy)"
            out.append(f'<h3>Latency added over the direct baseline at {fmt_rps(rate)} req/s{esc(note)}</h3>'
                       '<table><tr><th>gateway</th><th>added p50</th><th>added p99</th></tr>')
            for g in gws:
                if g == BASELINE or at(g, "p50_ms") is None:
                    continue
                out.append(f"<tr><td>{esc(g)}</td><td>{esc(fmt_ms(at(g, 'p50_ms') - b50))}</td>"
                           f"<td>{esc(fmt_ms(at(g, 'p99_ms') - b99) if b99 is not None and at(g, 'p99_ms') is not None else chr(8211))}</td></tr>")
            out.append("</table>")
    return "".join(out)


def section_environment(run: dict) -> str:
    fp = run.get("fingerprint", {})
    rows = "".join(
        f"<tr><td>{esc(name)}</td><td>{esc(h.get('cpu_model'))}</td><td>{esc(h.get('ncpu'))}</td>"
        f"<td>{esc(h.get('os'))}</td><td>{esc(h.get('kernel'))}</td><td>{esc(h.get('docker'))}</td>"
        f"<td>{esc(h.get('governor'))}</td><td>{esc(h.get('turbo'))}</td><td>{esc(h.get('loadavg'))}</td></tr>"
        for name, h in fp.get("hosts", {}).items())
    images = "".join(f"<tr><td>{esc(k)}</td><td>{esc(v)}</td></tr>" for k, v in run.get("images", {}).items())
    git = fp.get("git", {})
    return ('<h2>Environment</h2><div class="scroll"><table><tr><th>host</th><th>CPU</th><th>cpus</th><th>OS</th>'
            f'<th>kernel</th><th>docker</th><th>governor</th><th>turbo</th><th>load</th></tr>{rows}</table></div>'
            f'<p>Git: <code>{esc(git.get("sha"))}</code>{" (dirty)" if git.get("dirty") else ""} · '
            f'topology <code>{esc(run["topology"]["name"])}</code> ({esc(run["topology"]["kind"])})</p>'
            f'<h3>Images</h3><div class="scroll"><table>{images}</table></div>')


def section_methodology(run: dict) -> str:
    p = run["params"]
    links = "".join(f'<li><a href="configs/{esc(g)}/">configs/{esc(g)}/</a></li>'
                    for g in ordered_gateways(run) if g != BASELINE)
    return (
        "<h2>Methodology</h2><ul>"
        f"<li>Every cell first passes correctness probes; a failing cell is <em>invalid</em> and never measured.</li>"
        f"<li>Ceiling: {esc(p['ceiling_seconds'])} s unpaced flood (wrk / oha); warm-up {esc(p['warmup_seconds'])} s "
        f"at {esc(round(p['warmup_fraction'] * 100))}% of it, discarded.</li>"
        f"<li>Max sustainable rate: fixed-rate steps of {esc(p['search_step_seconds'])} s (wrk2 / oha, "
        f"coordinated-omission corrected), bisected between {esc(p['search_low'])}× and {esc(p['search_high'])}× "
        f"the ceiling to within {esc(p['search_tolerance'] * 100)}%; a step passes with p99 ≤ {esc(p['slo_p99_ms'])} ms "
        f"and &lt; {esc(p['max_error_rate'] * 100)}% errors.</li>"
        f"<li>Latency ladder: {esc(p['ladder_seconds'])} s at each of "
        f"{esc(', '.join(fmt_rps(r) for r in p['ladder']))} req/s up to the max sustainable rate.</li>"
        f"<li>{esc(p['connections'])} connections; {esc(p['reps'])} repetitions with a full restart each; "
        f"cells interleaved across gateways.</li>"
        f"<li>Saturation: a load-generator or upstream median CPU above {esc(round(p['saturation_threshold'] * 100))}% "
        f"of its cores marks the result “≥”.</li>"
        f"<li>Exact configs used:</li></ul><ul>{links}</ul>")


def render(run: dict) -> str:
    gws, scs = ordered_gateways(run), ordered_scenarios(run)
    banner = ("" if run.get("publish") else
              '<div class="banner">NOT FOR PUBLICATION — this run did not use the publish gate '
              '(dedicated Linux hosts, host networking, performance governor). Indicative only.</div>')
    body = [
        f'<h1>Gateway benchmark — {esc(run.get("started", ""))}</h1>',
        banner,
        section_summary(run, gws, scs),
        *[section_scenario(run, gws, sc) for sc in scs],
        section_environment(run),
        section_methodology(run),
    ]
    return ('<!doctype html><html lang="en"><head><meta charset="utf-8">'
            '<meta name="viewport" content="width=device-width, initial-scale=1">'
            f'<title>Gateway Benchmark Report</title><style>{CSS}</style></head>'
            f'<body><main>{"".join(body)}</main><script>{JS}</script></body></html>')


def write_report(run_dir: Path) -> Path:
    run = json.loads((run_dir / "run.json").read_text("utf-8"))
    out = run_dir / "report.html"
    out.write_text(render(run), encoding="utf-8")
    return out
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m unittest discover -s bench/tests -t bench -v`
Expected: all report tests PASS.

- [ ] **Step 5: Render a real run and look at it**

```bash
python bench/bench.py report bench/results/<the run dir from Task 15>
```
Open the printed `report.html` in a browser and check it by eye:
- Grid cells never collide at phone width (the table scrolls sideways inside its box, not the page).
- Line charts show one line per measured gateway, and the direct baseline is dashed.
- Hovering shows the crosshair tooltip with every series at that rate.
- Bar tooltips work.
- Switching the OS to dark mode swaps the palette.
- The "Table view" disclosure opens.

Fix layout problems in CSS, not by dropping content.

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/report bench/tests/test_report.py
git commit -m "feat(bench): self-contained HTML report"
```

---
### Task 17: README, security scans, full-matrix validation

**Files:**
- Create: `bench/README.md`
- Modify: `.gitleaks.toml` (allowlist the benchmark fixtures, only if gitleaks flags them)
- Modify: `CLAUDE.md` (one line under Build Commands)

**Interfaces:**
- Consumes: everything.
- Produces: the documented, verified suite.

- [ ] **Step 1: Write `bench/README.md`**

````markdown
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

## Adding a gateway

Create `gateways/<name>/gateway.toml` (see the existing ones), one native config directory per distinct config, and a `TUNING.md`. Every scenario must appear under `[configs]` or `[na]` (with a reason). Configs use `@@TOKEN@@` placeholders (`benchlib/templating.py`, `benchlib/tokens.py`). Then run `python bench/bench.py validate --gateways <name>`.

## Tests

```bash
python -m unittest discover -s bench/tests -t bench -v
```
````

- [ ] **Step 2: Add the suite to `CLAUDE.md`**

In `CLAUDE.md` under "## Build Commands", after the SAST block, add:
````markdown
Competitive benchmark suite (Docker + Python >= 3.11, stdlib only; see `bench/README.md`):
```bash
python bench/bench.py validate                     # correctness probes, all gateways
python bench/bench.py run --quick --gateways featherbit,nginx --scenarios core.proxy
python -m unittest discover -s bench/tests -t bench   # harness unit tests
```
````

- [ ] **Step 3: Run the SAST pipeline and allowlist the fixture credentials**

```powershell
./dev/sast.ps1
```
Expected: no new findings. The two most likely:
- **gitleaks** may flag the benchmark's fixed JWT secret or API key in `bench/benchlib/tokens.py` or the adapters. These are public test fixtures, so add `bench/` paths to the fixture allowlist already used in `.gitleaks.toml`, following its existing `[[allowlists]]` form with a comment ("benchmark fixture credentials, never real").
- **hadolint** may flag unpinned apt packages in `bench/loadgen/Dockerfile`. Add a `# hadolint ignore=DL3008` with a one-line justification, the way the root `Dockerfile` does for DL3018.

Fix anything else properly.

- [ ] **Step 4: Full verification**

```bash
python -m unittest discover -s bench/tests -t bench -v
python bench/bench.py plan
python bench/bench.py validate
cargo test
```
Expected:
- **Unit tests:** all PASS.
- **`plan`:** lists every cell. Runnable cells: 12 each for featherbit, apisix, kong, envoy and tyk, 10 for krakend, 9 for traefik, 8 for nginx and 3 for direct. That's 90 per profile (fewer if Envoy's key-auth became n/a).
- **`validate`:** every runnable cell `ok`.
- **`cargo test`:** unchanged and passing. Nothing in the crate changed; this proves it.

Paste the `validate` summary line into the final report to Francesco.

- [ ] **Step 5: Keep the knowledge graph current**

```bash
graphify update .
```

- [ ] **Step 6: Commit** (only if authorized)

```bash
git add bench/README.md CLAUDE.md .gitleaks.toml bench/loadgen/Dockerfile
git commit -m "docs(bench): benchmark suite README, CLAUDE.md entry, scanner allowlists"
```

---

## Self-review notes (for the executor)

- Every spec section maps to a task:
  - §3 layout → all tasks
  - §4.1 upstream → Task 7
  - §4.2 adapters → Tasks 1, 9–15
  - §4.3 load generator → Task 7
  - §4.4 orchestrator → Tasks 8, 9
  - §5 scenarios and N/A rule → Tasks 1, 9–15
  - §6 procedure → Tasks 3, 8, 9
  - §7 results/report → Tasks 4, 16
  - §8 fairness → the TUNING.md files, README
  - §9 testing → every task and Task 17
  - §10 out of scope → nothing added
- Names used across tasks: `Cell.key` / `Cell.slug`, `RunStore.record_rep`, the `Driver` protocol methods, `Images`, `NAMES`, `static_values()` keys, and `run.json` keys. The Interfaces blocks are authoritative; if you rename anything, update every task that consumes it.
