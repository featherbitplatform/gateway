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
_THREAD_LAT = re.compile(r"^\s+Latency\s+([\d.]+[a-z]+)\s+([\d.]+[a-z]+)\s+([\d.]+[a-z]+)", re.M)
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


def parse_oha(text: str, inflight: int | None = None) -> LoadResult:
    """`inflight`: requests that can legitimately be cut off when -z expires (connections x
    streams). Deadline aborts beyond it are stalls and count as errors; None excuses them all."""
    try:
        d = json.loads(text)
        summary = d["summary"]
    except (json.JSONDecodeError, KeyError, TypeError) as e:
        raise ParseError(f"oha output is not the expected JSON ({e}): {text.strip()[:200]!r}") from None
    codes = {int(k): int(v) for k, v in d.get("statusCodeDistribution", {}).items()}
    dist = {k: int(v) for k, v in d.get("errorDistribution", {}).items()}
    # Requests still in flight when -z expires are aborted by oha itself, not failed by the target.
    aborted = dist.pop("aborted due to deadline", 0)
    errors = sum(dist.values()) + (0 if inflight is None else max(0, aborted - inflight))
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
