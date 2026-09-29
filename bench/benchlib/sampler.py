"""Sample container CPU and memory through streaming `docker stats` (spec §6 step 6)."""
from __future__ import annotations

import json
import re
import statistics
import subprocess
import threading
import time
from dataclasses import dataclass

from .docker import Docker

ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
_MEM = re.compile(r"^\s*([\d.]+)\s*([A-Za-z]+)")
_TO_MB = {"B": 1 / 1048576, "KiB": 1 / 1024, "MiB": 1.0, "GiB": 1024.0, "TiB": 1048576.0,
          "kB": 1e3 / 1048576, "KB": 1e3 / 1048576, "MB": 1e6 / 1048576, "GB": 1e9 / 1048576}


@dataclass(frozen=True)
class Sample:
    t: float
    name: str
    cpu_cores: float
    mem_mb: float


def parse_mem_mb(text: str) -> float:
    m = _MEM.match(text or "")
    if not m or m.group(2) not in _TO_MB:
        return 0.0
    return float(m.group(1)) * _TO_MB[m.group(2)]


def parse_stats_line(line: str, t: float) -> Sample | None:
    line = ANSI.sub("", line).strip()
    start = line.find("{")
    if start < 0:
        return None
    try:
        d = json.loads(line[start:])
        cpu = float(str(d["CPUPerc"]).rstrip("%")) / 100.0
    except (json.JSONDecodeError, KeyError, ValueError):
        return None
    return Sample(t=t, name=d.get("Name", ""), cpu_cores=cpu, mem_mb=parse_mem_mb(d.get("MemUsage", "")))


def median(values) -> float | None:
    values = list(values)
    return statistics.median(values) if values else None


class Sampler:
    def __init__(self, targets: list[tuple[Docker, str]], popen=subprocess.Popen, clock=time.time):
        self._groups: dict[str | None, tuple[Docker, list[str]]] = {}
        for docker, name in targets:
            self._groups.setdefault(docker.context, (docker, []))[1].append(name)
        self._popen = popen
        self._clock = clock
        self._lock = threading.Lock()
        self._samples: list[Sample] = []
        self._procs = []
        self._threads: list[threading.Thread] = []

    def _pump(self, proc) -> None:
        for line in proc.stdout:
            sample = parse_stats_line(line, self._clock())
            if sample:
                with self._lock:
                    self._samples.append(sample)

    def start(self) -> None:
        for docker, names in self._groups.values():
            proc = self._popen(docker.stats_command(names), stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, text=True, encoding="utf-8", errors="replace")
            thread = threading.Thread(target=self._pump, args=(proc,), daemon=True)
            thread.start()
            self._procs.append(proc)
            self._threads.append(thread)

    def stop(self) -> list[Sample]:
        for proc in self._procs:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        for thread in self._threads:
            thread.join(timeout=5)
        self._procs, self._threads = [], []
        with self._lock:
            samples, self._samples = self._samples, []
        return samples
