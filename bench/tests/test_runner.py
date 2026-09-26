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
    def __init__(self, capacity=20_000, boot_error=None, probe_ok=True, load_error=False, loadgen_cpu=0.5,
                 alive=True, flood_rps=None, loadgen_cpu_over=None):
        self.loadgen_cpu_over = loadgen_cpu_over  # loadgen CPU on steps above capacity
        self.alive = alive
        self.flood_rps = flood_rps
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
        if self.load_error and "sample-start" in self.events:  # fail mid-measurement
            raise LoadError("wrk2 exited 1")
        t0 = self.now
        self.now += seconds
        lg = self.loadgen_cpu
        if self.loadgen_cpu_over is not None and rate is not None and rate > self.capacity:
            lg = self.loadgen_cpu_over
        for role, cpu in (("gateway", 0.8), ("upstream", 0.4), ("loadgen", lg)):
            self.samples[role].append(Sample(t0 + seconds / 2, role, cpu, 40.0))
        if rate is None:
            rps = self.capacity * 1.05 if self.flood_rps is None else self.flood_rps
            return LoadResult(10_000, seconds, rps, 0, 0, 1.0, 1.0, 1.0, 1.0, 1.0)
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

    def gateway_health(self, cell):
        return (True, "running") if self.alive else (False, "exited 137 (OOMKilled)")

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

    def test_loadgen_saturated_on_the_failing_bound_step(self):
        # The step just above the result failed because the load generator ran out of CPU:
        # the gateway sustained at least the result, not exactly it.
        r = runner(FakeDriver(loadgen_cpu_over=1.9)).run_rep(CELL, 0)
        self.assertEqual(r["status"], LOADGEN_BOUND)

    def test_load_error_marks_rep_error(self):
        d = FakeDriver(load_error=True)
        r = runner(d).run_rep(CELL, 0)
        self.assertEqual(r["status"], ERROR)
        self.assertTrue(r["error"].startswith("load: "))
        self.assertIn("sample-stop", d.events)
        self.assertEqual(d.events[-1], "teardown")

    def test_gateway_died_during_measurement_is_an_error(self):
        d = FakeDriver(alive=False)
        r = runner(d).run_rep(CELL, 0)
        self.assertEqual(r["status"], ERROR)
        self.assertIn("OOMKilled", r["error"])
        self.assertEqual(r["logs"], {"fbb-gateway": "log text"})
        self.assertEqual(d.events[-1], "teardown")

    def test_no_ceiling_is_an_error_not_ok(self):
        r = runner(FakeDriver(flood_rps=0.0)).run_rep(CELL, 0)
        self.assertEqual(r["status"], ERROR)
        self.assertIn("no requests", r["error"])

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


class ProvenanceTests(unittest.TestCase):
    def test_resume_refuses_a_different_build(self):
        from benchlib.fingerprint import provenance_mismatches
        stored = {"images": {"featherbit": "sha256:a", "loadgen": "sha256:l"},
                  "fingerprint": {"git": {"sha": "abc", "dirty": False}}}
        same = provenance_mismatches(stored, {"featherbit": "sha256:a", "loadgen": "sha256:l"},
                                     {"sha": "abc", "dirty": False})
        self.assertEqual(same, [])
        diff = provenance_mismatches(stored, {"featherbit": "sha256:b", "loadgen": "sha256:l"},
                                     {"sha": "def", "dirty": False})
        self.assertEqual(len(diff), 2)
        self.assertTrue(any("featherbit" in d for d in diff))
        self.assertTrue(any("git" in d for d in diff))

    def test_first_session_has_nothing_to_compare(self):
        from benchlib.fingerprint import provenance_mismatches
        self.assertEqual(provenance_mismatches({"images": {}, "fingerprint": {}}, {"x": "1"}, {"sha": "a"}), [])


class PublishParamsTests(unittest.TestCase):
    def test_publish_refuses_non_catalog_params(self):
        good = {"os_type": "linux", "os": "Ubuntu 24.04", "governor": "performance"}
        t = topo("remote", "host")
        self.assertEqual(publish_blockers(t, {"gw": good}, Params(), Params()), [])
        quick = publish_blockers(t, {"gw": good}, Params().quick(), Params())
        self.assertEqual(len(quick), 1)
        self.assertIn("reps", quick[0])
