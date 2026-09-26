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


class DeliveryTests(unittest.TestCase):
    def test_step_must_deliver_its_rate(self):
        slow = LoadResult(8000, 10.0, 800.0, 0, 0, 0.5, 0.8, 2.0, 2.0, 2.0)
        self.assertFalse(passes(slow, 10.0, 0.001, rate=1000))
        self.assertTrue(passes(slow, 10.0, 0.001, rate=820))

    def test_zero_requests_never_pass(self):
        empty = LoadResult(0, 5.0, 0.0, 0, 0, 0.0, 0.0, 0.0, 0.0, 0.0)
        self.assertFalse(passes(empty, 10.0, 0.001, rate=1000))
        self.assertFalse(passes(empty, 10.0, 0.001))


class CellFlagTests(unittest.TestCase):
    def test_summary_carries_the_union_of_measured_rep_flags(self):
        reps = [{"rep": 0, "status": OK, "max_sustainable_rps": 10, "ladder": [], "flags": ["search-ceiling-hit"]},
                {"rep": 1, "status": OK, "max_sustainable_rps": 12, "ladder": [], "flags": []},
                {"rep": 2, "status": ERROR, "error": "x", "flags": ["ignored"]}]
        self.assertEqual(summarize_cell(reps)["flags"], ["search-ceiling-hit"])
