"""A run directory: run.json (written atomically after every repetition) plus raw/, configs/, logs/."""
from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path

from .classify import ERROR, NA, PENDING, cell_status, summarize_cell
from .config import ConfigError
from .matrix import Cell

SCHEMA = 1


class RunStore:
    def __init__(self, path: Path, data: dict):
        self.path = path
        self.data = data

    @classmethod
    def create(cls, root: Path, label: str, meta: dict, now: datetime | None = None) -> "RunStore":
        now = now or datetime.now(timezone.utc)
        path = root / f"{now:%Y%m%dT%H%M%SZ}-{label}"
        path.mkdir(parents=True, exist_ok=False)
        for sub in ("raw", "configs", "logs"):
            (path / sub).mkdir()
        store = cls(path, {"schema": SCHEMA, "started": now.isoformat(), **meta, "cells": {}})
        store.save()
        return store

    @classmethod
    def open(cls, path: Path) -> "RunStore":
        try:
            data = json.loads((path / "run.json").read_text("utf-8"))
        except FileNotFoundError:
            raise ConfigError(f"{path}: no run.json") from None
        if data.get("schema") != SCHEMA:
            raise ConfigError(f"{path}: run.json schema {data.get('schema')!r}, expected {SCHEMA}")
        return cls(path, data)

    @property
    def raw_dir(self) -> Path:
        return self.path / "raw"

    @property
    def configs_dir(self) -> Path:
        return self.path / "configs"

    @property
    def logs_dir(self) -> Path:
        return self.path / "logs"

    def _cell(self, cell: Cell) -> dict:
        return self.data["cells"].setdefault(cell.key, {
            "gateway": cell.gateway, "scenario": cell.scenario, "profile": cell.profile,
            "status": PENDING, "reps": [],
        })

    def done(self, cell: Cell, rep: int) -> bool:
        """True once the rep has a result; `error` reps stay retryable (a crashed daemon, a
        suspended laptop) so --resume runs them again."""
        return any(r["rep"] == rep and r["status"] != ERROR
                   for r in self.data["cells"].get(cell.key, {}).get("reps", []))

    def record_na(self, cell: Cell, reason: str) -> None:
        c = self._cell(cell)
        c["status"] = NA
        c["na_reason"] = reason
        self.save()

    def record_rep(self, cell: Cell, rep: int, result: dict) -> None:
        result = dict(result)
        logs = result.pop("logs", None) or {}
        files = []
        for container, text in logs.items():
            name = f"{cell.slug}__rep{rep}__{container}.log"
            (self.logs_dir / name).write_text(text, encoding="utf-8")
            files.append(name)
        if files:
            result["log_files"] = files
        c = self._cell(cell)
        c["reps"] = sorted([r for r in c["reps"] if r["rep"] != rep] + [{"rep": rep, **result}],
                           key=lambda r: r["rep"])
        c["status"] = cell_status([r["status"] for r in c["reps"]])
        c["summary"] = summarize_cell(c["reps"])
        self.save()

    def save(self) -> None:
        tmp = self.path / "run.json.tmp"
        tmp.write_text(json.dumps(self.data, indent=1, sort_keys=False), encoding="utf-8")
        os.replace(tmp, self.path / "run.json")
