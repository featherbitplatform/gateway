"""Environment fingerprint recorded in run.json (spec §6), and the --publish gate (spec §4.4)."""
from __future__ import annotations

import dataclasses
import subprocess
from pathlib import Path

from .config import Topology
from .docker import Docker

FP_SCRIPT = (
    "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2; "
    "cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown; "
    "cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null "
    "|| cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null || echo unknown; "
    "cut -d' ' -f1-3 /proc/loadavg"
)
_FIELDS = ("cpu_model", "governor", "turbo", "loadavg")


def parse_host_probe(text: str) -> dict:
    lines = [l.strip() for l in text.strip().splitlines()]
    lines += ["unknown"] * (len(_FIELDS) - len(lines))
    return dict(zip(_FIELDS, lines))


def host_fingerprint(docker: Docker, image: str) -> dict:
    info = docker.info()
    probe = docker.run(["run", "--rm", "--entrypoint", "sh", image, "-c", FP_SCRIPT]).stdout
    return {
        "docker": info.get("ServerVersion"), "os": info.get("OperatingSystem"),
        "os_type": info.get("OSType"), "kernel": info.get("KernelVersion"),
        "arch": info.get("Architecture"), "ncpu": info.get("NCPU"), "mem_bytes": info.get("MemTotal"),
        **parse_host_probe(probe),
    }


def git_info(repo: Path, runner=subprocess.run) -> dict:
    def git(*args: str) -> str:
        return runner(["git", "-C", str(repo), *args], capture_output=True, text=True).stdout.strip()
    return {"sha": git("rev-parse", "HEAD"), "dirty": bool(git("status", "--porcelain", "--untracked-files=no"))}


def publish_blockers(topology: Topology, hosts: dict[str, dict], params=None, catalog=None) -> list[str]:
    blockers = []
    if params is not None and catalog is not None and params != catalog:
        changed = [f.name for f in dataclasses.fields(catalog) if getattr(params, f.name) != getattr(catalog, f.name)]
        blockers.append(f"parameters differ from scenarios.toml ({', '.join(changed)}): no --quick/--reps for publishable runs")
    if topology.kind != "remote":
        blockers.append("topology is not 'remote' (one Linux host per role)")
    if topology.network != "host":
        blockers.append("containers do not use host networking")
    for name, fp in hosts.items():
        if fp.get("os_type") != "linux":
            blockers.append(f"{name}: not a Linux docker host ({fp.get('os_type')})")
        if "Docker Desktop" in str(fp.get("os", "")):
            blockers.append(f"{name}: Docker Desktop runs in a VM, not on bare Linux")
        if fp.get("governor") != "performance":
            blockers.append(f"{name}: CPU governor is {fp.get('governor')!r}, not 'performance'")
    return blockers


def provenance_mismatches(stored: dict, images: dict, git: dict) -> list[str]:
    """Differences between the build a run started with and the one a --resume would use.

    Image ids cover the gateway builds (a rebuilt checkout gets a new id); the git SHA
    covers the harness itself. A first session has nothing stored, so nothing to compare.
    """
    problems = []
    for name, old in stored.get("images", {}).items():
        new = images.get(name)
        if new is not None and new != old:
            problems.append(f"image {name}: run started with {old}, now {new}")
    old_sha = stored.get("fingerprint", {}).get("git", {}).get("sha")
    if old_sha and git.get("sha") != old_sha:
        problems.append(f"git: run started at {old_sha}, checkout is now {git.get('sha')}")
    return problems
