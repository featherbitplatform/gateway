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


class RetryTests(unittest.TestCase):
    def test_error_reps_are_not_done_so_resume_retries_them(self):
        with tempfile.TemporaryDirectory() as d:
            s = RunStore.create(Path(d), "local", {}, now=datetime(2026, 9, 26, tzinfo=timezone.utc))
            c = Cell("kong", "core.proxy", 1)
            s.record_rep(c, 0, {"status": "error", "error": "boot: docker daemon not running"})
            self.assertFalse(s.done(c, 0))
            s.record_rep(c, 1, {"status": "invalid", "probes": []})
            self.assertTrue(s.done(c, 1))
