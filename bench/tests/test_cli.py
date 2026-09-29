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


class ScheduleLoopTests(unittest.TestCase):
    def test_stops_after_consecutive_errors(self):
        from benchlib.cli import run_schedule
        from benchlib.matrix import Cell

        class Runner:
            calls = 0
            def run_rep(self, cell, rep):
                Runner.calls += 1
                return {"status": "error", "error": "boot: Cannot connect to the Docker daemon"}

        class Store:
            recorded = []
            def record_rep(self, cell, rep, result):
                Store.recorded.append(result["status"])

        todo = [(Cell("g", "s", 1), r) for r in range(10)]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = run_schedule(todo, Runner(), Store(), reps=10, max_consecutive_errors=3)
        self.assertEqual((code, Runner.calls, len(Store.recorded)), (3, 3, 3))
        self.assertIn("3 consecutive errors", out.getvalue())
