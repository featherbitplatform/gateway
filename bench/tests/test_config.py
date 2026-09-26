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


class ScriptProbeTests(unittest.TestCase):
    def test_script_probe_uses_two_inputs_so_a_static_header_cannot_pass(self):
        _, scenarios = load_scenarios(BENCH / "scenarios" / "scenarios.toml")
        expected = [p.expect_headers.get("x-bench-echo-script") for p in scenarios["script.header"].probes]
        self.assertEqual(expected, ["ABC-3", "HELLO-5"])
