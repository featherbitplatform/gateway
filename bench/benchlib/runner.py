"""One benchmark repetition of one cell: the measurement procedure of spec §6."""
from __future__ import annotations

import time
from typing import Protocol

from .classify import ERROR, INVALID, OK, rep_status, saturation_flags
from .config import Params
from .matrix import Cell
from .parsers import LoadResult
from .probes import ProbeResult
from .sampler import Sample, median
from .search import Step, find_max_sustainable, ladder_rates, passes


class BootError(Exception):
    """The gateway (or the upstream) did not come up."""


class LoadError(Exception):
    """A load tool failed or produced unparseable output."""


class Driver(Protocol):
    def boot(self, cell: Cell, rep: int) -> None: ...
    def setup_and_probe(self, cell: Cell) -> list[ProbeResult]: ...
    def load(self, cell: Cell, rate: int | None, seconds: int) -> LoadResult: ...
    def start_sampling(self, cell: Cell) -> None: ...
    def stop_sampling(self) -> dict[str, list[Sample]]: ...
    def cores(self, cell: Cell, role: str) -> int: ...
    def gateway_health(self, cell: Cell) -> tuple[bool, str]: ...
    def logs(self) -> dict[str, str]: ...
    def teardown(self) -> None: ...


def _probe_dict(r: ProbeResult) -> dict:
    return {"path": r.probe.path, "kind": r.probe.kind, "ok": r.ok, "detail": r.detail}


class CellRunner:
    def __init__(self, driver: Driver, params: Params, clock=time.time):
        self.driver = driver
        self.params = params
        self.clock = clock

    def _fail(self, message: str) -> dict:
        return {"status": ERROR, "error": message, "logs": self.driver.logs()}

    def validate(self, cell: Cell) -> dict:
        """Boot + probes only (bench.py validate)."""
        try:
            self.driver.boot(cell, 0)
        except BootError as e:
            result = self._fail(f"boot: {e}")
            self.driver.teardown()
            return result
        try:
            probes = self.driver.setup_and_probe(cell)
            return {"status": OK if all(r.ok for r in probes) else INVALID,
                    "probes": [_probe_dict(r) for r in probes]}
        finally:
            self.driver.teardown()

    def run_rep(self, cell: Cell, rep: int) -> dict:
        p = self.params
        try:
            self.driver.boot(cell, rep)
        except BootError as e:
            result = self._fail(f"boot: {e}")
            self.driver.teardown()
            return result
        try:
            probes = self.driver.setup_and_probe(cell)
            probe_list = [_probe_dict(r) for r in probes]
            if not all(r.ok for r in probes):
                return {"status": INVALID, "probes": probe_list}
            ceiling = self.driver.load(cell, None, p.ceiling_seconds).rps
            if ceiling <= 0:
                return self._fail("load: the unpaced flood got no requests back (no-ceiling)")
            self.driver.load(cell, max(1, round(ceiling * p.warmup_fraction)), p.warmup_seconds)
            self.driver.start_sampling(cell)
            try:
                search = find_max_sustainable(
                    lambda rate: self._step(cell, rate, p.search_step_seconds),
                    ceiling, p.search_low, p.search_high, p.search_tolerance)
                ladder = ([self._step(cell, rate, p.ladder_seconds)
                           for rate in ladder_rates(p.ladder, search.max_sustainable)]
                          if search.max_sustainable > 0 else [])
            finally:
                samples = self.driver.stop_sampling()
            alive, detail = self.driver.gateway_health(cell)
            if not alive:  # a crash mid-search would otherwise read as a low max rate
                return self._fail(f"gateway died during measurement: {detail}")
            search_points = [self._point(cell, s, samples) for s in search.steps]
            ladder_points = [self._point(cell, s, samples) for s in ladder]
            final = [pt for pt in search_points
                     if pt["passed"] and pt["rate"] == search.max_sustainable][-1:]
            # The lowest failing step above the result set the upper bound: if the load
            # generator or upstream saturated there, the result is only a lower bound.
            bound = sorted((pt for pt in search_points
                            if not pt["passed"] and pt["rate"] > search.max_sustainable),
                           key=lambda pt: pt["rate"])[:1]
            flags = set(search.flags)
            for pt in final + bound + ladder_points:
                flags.update(saturation_flags(pt["loadgen_cpu_frac"], pt["upstream_cpu_frac"],
                                              p.saturation_threshold))
            return {
                "status": rep_status(sorted(flags)),
                "probes": probe_list,
                "ceiling_rps": ceiling,
                "max_sustainable_rps": search.max_sustainable,
                "search": search_points,
                "ladder": ladder_points,
                "flags": sorted(flags),
            }
        except LoadError as e:
            return self._fail(f"load: {e}")
        finally:
            self.driver.teardown()

    def _step(self, cell: Cell, rate: int, seconds: int) -> Step:
        t0 = self.clock()
        result = self.driver.load(cell, rate, seconds)
        t1 = self.clock()
        return Step(rate=rate, result=result,
                    passed=passes(result, self.params.slo_p99_ms, self.params.max_error_rate, rate),
                    t_start=t0, t_end=t1)

    def _point(self, cell: Cell, step: Step, samples: dict[str, list[Sample]]) -> dict:
        def window(role: str) -> list[Sample]:
            return [s for s in samples.get(role, []) if step.t_start <= s.t <= step.t_end]

        gw = window("gateway")
        gw_cpu = median(s.cpu_cores for s in gw)
        lg_cpu = median(s.cpu_cores for s in window("loadgen"))
        up_cpu = median(s.cpu_cores for s in window("upstream"))
        r = step.result
        return {
            "rate": step.rate, "passed": step.passed, "achieved_rps": r.rps,
            "p50_ms": r.p50_ms, "p90_ms": r.p90_ms, "p99_ms": r.p99_ms, "p999_ms": r.p999_ms,
            "max_ms": r.max_ms, "requests": r.requests, "errors": r.errors + r.non2xx,
            "gateway_cpu_cores": gw_cpu,
            "gateway_rss_peak_mb": max((s.mem_mb for s in gw), default=None),
            "loadgen_cpu_frac": None if lg_cpu is None else lg_cpu / self.driver.cores(cell, "loadgen"),
            "upstream_cpu_frac": None if up_cpu is None else up_cpu / self.driver.cores(cell, "upstream"),
            "rps_per_core": r.rps / gw_cpu if gw_cpu else None,
        }
