"""Find the max sustainable rate (spec §6 step 4) and pick latency-ladder rates (step 5)."""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable

from .parsers import LoadResult


@dataclass(frozen=True)
class Step:
    rate: int
    result: LoadResult
    passed: bool
    t_start: float
    t_end: float


@dataclass
class SearchOutcome:
    max_sustainable: int
    steps: list[Step] = field(default_factory=list)
    flags: list[str] = field(default_factory=list)


MIN_DELIVERY = 0.95  # a fixed-rate step must actually serve >= 95% of the offered rate


def passes(result: LoadResult, slo_p99_ms: float, max_error_rate: float, rate: int | None = None) -> bool:
    if result.p99_ms is None or result.requests <= 0:
        return False
    if rate is not None and result.rps < MIN_DELIVERY * rate:
        return False
    return result.p99_ms <= slo_p99_ms and result.error_rate() < max_error_rate


def find_max_sustainable(measure: Callable[[int], Step], ceiling: float,
                         low: float, high: float, tolerance: float) -> SearchOutcome:
    """Bisect between low*ceiling and high*ceiling for the highest passing rate.

    `measure(rate)` runs one fixed-rate step and reports whether it met the SLO.
    """
    out = SearchOutcome(0)
    if ceiling <= 0:
        out.flags.append("no-ceiling")
        return out
    lo = max(1, round(ceiling * low))
    hi = max(lo + 1, round(ceiling * high))
    step = measure(lo)
    out.steps.append(step)
    if not step.passed:
        out.flags.append("slo-unmet-at-floor")
        return out
    step = measure(hi)
    out.steps.append(step)
    if step.passed:
        out.max_sustainable = hi
        out.flags.append("search-ceiling-hit")
        return out
    while hi - lo > 1 and (hi - lo) / hi > tolerance:
        mid = (lo + hi) // 2
        step = measure(mid)
        out.steps.append(step)
        if step.passed:
            lo = mid
        else:
            hi = mid
    out.max_sustainable = lo
    return out


def ladder_rates(ladder: tuple[int, ...], max_sustainable: int) -> list[int]:
    """Every ladder rate up to the max sustainable one; at least the lowest rung."""
    rates = [r for r in ladder if r <= max_sustainable]
    return rates or [ladder[0]]
