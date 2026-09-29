"""Repetition/cell statuses (spec §6 steps 2, 7, 8) and median/min/max summaries."""
from __future__ import annotations

import statistics

OK = "ok"
INVALID = "invalid"
ERROR = "error"
NA = "n/a"
PENDING = "pending"
LOADGEN_BOUND = "loadgen-bound"
UPSTREAM_BOUND = "upstream-bound"
MEASURED = (OK, LOADGEN_BOUND, UPSTREAM_BOUND)
LADDER_METRICS = ("p50_ms", "p90_ms", "p99_ms", "p999_ms", "rps_per_core",
                  "gateway_rss_peak_mb", "gateway_cpu_cores")


def saturation_flags(loadgen_frac: float | None, upstream_frac: float | None,
                     threshold: float) -> list[str]:
    flags = []
    if loadgen_frac is not None and loadgen_frac > threshold:
        flags.append(LOADGEN_BOUND)
    if upstream_frac is not None and upstream_frac > threshold:
        flags.append(UPSTREAM_BOUND)
    return flags


def rep_status(flags: list[str]) -> str:
    if LOADGEN_BOUND in flags:
        return LOADGEN_BOUND
    if UPSTREAM_BOUND in flags:
        return UPSTREAM_BOUND
    return OK


def cell_status(rep_statuses: list[str]) -> str:
    if not rep_statuses:
        return PENDING
    if INVALID in rep_statuses:
        return INVALID
    if all(s == ERROR for s in rep_statuses):
        return ERROR
    if LOADGEN_BOUND in rep_statuses:
        return LOADGEN_BOUND
    if UPSTREAM_BOUND in rep_statuses:
        return UPSTREAM_BOUND
    return OK


def summarize(values: list[float]) -> dict | None:
    if not values:
        return None
    return {"median": statistics.median(values), "min": min(values), "max": max(values),
            "n": len(values)}


def summarize_cell(reps: list[dict]) -> dict:
    good = [r for r in reps if r.get("status") in MEASURED]
    by_rate: dict[int, list[dict]] = {}
    for r in good:
        for point in r.get("ladder", []):
            by_rate.setdefault(point["rate"], []).append(point)
    ladder = []
    for rate, points in sorted(by_rate.items()):
        entry = {"rate": rate, "reps": len(points)}
        for key in LADDER_METRICS:
            entry[key] = summarize([p[key] for p in points if p.get(key) is not None])
        ladder.append(entry)
    return {
        "reps_ok": len(good),
        "reps_total": len(reps),
        "max_sustainable_rps": summarize(
            [r["max_sustainable_rps"] for r in good if r.get("max_sustainable_rps") is not None]),
        "ladder": ladder,
        "flags": sorted({f for r in good for f in r.get("flags", [])}),
    }
