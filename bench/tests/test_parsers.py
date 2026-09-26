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

    def test_oha_deadline_aborts_are_not_errors(self):
        # oha aborts the requests still in flight when -z expires; that is the
        # tool stopping, not the gateway failing.
        r = parse_oha((FIXTURES / "oha_real.json").read_text("utf-8"))
        self.assertEqual(r.errors, 0)
        self.assertEqual(r.requests, 1500)

    def test_oha_deadline_aborts_beyond_inflight_bound_are_errors(self):
        doc = json.loads(json.dumps(OHA))
        doc["errorDistribution"] = {"aborted due to deadline": 100}
        r = parse_oha(json.dumps(doc), inflight=64)
        self.assertEqual(r.errors, 36)
