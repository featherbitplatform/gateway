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


class StateTests(unittest.TestCase):
    def test_state_parses_inspect(self):
        d = Docker(runner=Recorder(0, '{"Running":false,"OOMKilled":true,"ExitCode":137}|0\n'))
        self.assertEqual(d.state("fbb-gateway"), {"Running": False, "OOMKilled": True, "ExitCode": 137, "RestartCount": 0})

    def test_state_of_missing_container(self):
        self.assertEqual(Docker(runner=Recorder(1, "", "No such object")).state("x"), {})
