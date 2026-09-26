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


class HealthTests(DriverTests):
    def test_dead_gateway_reported_with_reason(self):
        FakeDocker.state = lambda self, name: {"Running": False, "OOMKilled": True, "ExitCode": 137, "RestartCount": 0}
        try:
            self.assertEqual(self.driver.gateway_health(Cell("nginx", "core.proxy", 1)), (False, "exited 137 (OOMKilled)"))
            self.assertEqual(self.driver.gateway_health(Cell("direct", "core.proxy", 1))[0], True)
        finally:
            del FakeDocker.state
