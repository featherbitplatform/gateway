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


class ReportPolishTests(unittest.TestCase):
    def test_zero_sustainable_rate_reads_as_slo_unmet_not_zero(self):
        run = make_run()
        run["cells"]["krakend|core.proxy|1"] = cell("krakend", "core.proxy", "ok", 0, 0.5)
        html = render(run)
        self.assertIn(">SLO unmet<", html)
        self.assertNotIn(">0<", html)

    def test_title_uses_a_readable_utc_timestamp(self):
        html = render(make_run())
        self.assertIn("26 Sep 2026, 10:00 UTC", html)
        self.assertNotIn("2026-09-26T10:00:00+00:00</h1>", html)


class CaveatTests(unittest.TestCase):
    def test_ceiling_hit_renders_as_lower_bound(self):
        run = make_run()
        run["cells"]["featherbit|core.proxy|1"]["summary"]["flags"] = ["search-ceiling-hit"]
        self.assertIn("≥ 40.0k", render(run))

    def test_partial_reps_are_shown(self):
        run = make_run()
        run["cells"]["featherbit|core.proxy|1"]["summary"].update({"reps_ok": 1, "reps_total": 5})
        self.assertIn("40.0k · 1/5 reps", render(run))

    def test_slo_unmet_tooltip_does_not_claim_a_single_cause(self):
        run = make_run()
        run["cells"]["krakend|core.proxy|1"] = cell("krakend", "core.proxy", "ok", 0, 0.5)
        self.assertIn("p99 or error rate", render(run))
