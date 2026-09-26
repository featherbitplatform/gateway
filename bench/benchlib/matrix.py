"""The benchmark matrix: which (gateway, scenario, profile) cells exist and in what order they run."""
from __future__ import annotations

import fnmatch
from dataclasses import dataclass

from .config import ConfigError, Gateway


@dataclass(frozen=True)
class Cell:
    gateway: str
    scenario: str
    profile: int

    @property
    def key(self) -> str:
        return f"{self.gateway}|{self.scenario}|{self.profile}"

    @property
    def slug(self) -> str:
        return f"{self.gateway}__{self.scenario}__{self.profile}"


def select(patterns: list[str] | None, names: list[str], what: str) -> list[str]:
    if not patterns:
        return list(names)
    chosen: set[str] = set()
    for pat in patterns:
        hits = [n for n in names if fnmatch.fnmatchcase(n, pat)]
        if not hits:
            raise ConfigError(f"no {what} matches {pat!r} (known: {', '.join(names)})")
        chosen.update(hits)
    return [n for n in names if n in chosen]


def gateway_order(names) -> list[str]:
    return sorted(names, key=lambda n: (n != "direct", n))


def expand(gateway_names: list[str], scenario_ids: list[str], profiles: list[int]) -> list[Cell]:
    return [Cell(g, s, p) for s in scenario_ids for p in sorted(profiles) for g in gateway_names]


def schedule(cells: list[Cell], gateways: dict[str, Gateway], reps: int) -> list[tuple[Cell, int]]:
    """Repetitions outermost, so drift spreads across gateways instead of hitting the last one."""
    runnable = [c for c in cells if gateways[c.gateway].supports(c.scenario)]
    return [(c, rep) for rep in range(reps) for c in runnable]
