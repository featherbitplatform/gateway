"""Thin wrapper over the docker CLI. Every call can target a docker context (remote topology)."""
from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from pathlib import PurePath

LABEL_KEY = "featherbit-bench"


class DockerError(Exception):
    """A docker CLI call failed."""


@dataclass(frozen=True)
class ContainerSpec:
    name: str
    image: str
    cpuset: str | None = None
    network: str | None = None
    aliases: tuple[str, ...] = ()
    ports: tuple[tuple[int, int], ...] = ()
    env: tuple[tuple[str, str], ...] = ()
    command: tuple[str, ...] = ()
    entrypoint: str | None = None
    workdir: str | None = None
    nofile: int = 1048576


class Docker:
    def __init__(self, context: str | None = None, runner=subprocess.run):
        self.context = context
        self._runner = runner

    def base(self) -> list[str]:
        return ["docker"] + (["--context", self.context] if self.context else [])

    def run(self, args, *, check: bool = True, timeout: float | None = None) -> subprocess.CompletedProcess:
        argv = self.base() + list(args)
        try:
            proc = self._runner(argv, capture_output=True, text=True, encoding="utf-8",
                                errors="replace", timeout=timeout)
        except FileNotFoundError:
            raise DockerError("docker CLI not found on PATH") from None
        except subprocess.TimeoutExpired:
            raise DockerError(f"timed out after {timeout}s: {' '.join(argv)}") from None
        if check and proc.returncode != 0:
            raise DockerError(f"{' '.join(argv)} exited {proc.returncode}: {(proc.stderr or '').strip()[-2000:]}")
        return proc

    @staticmethod
    def create_args(spec: ContainerSpec) -> list[str]:
        a = ["create", "--name", spec.name, "--label", f"{LABEL_KEY}=1",
             "--ulimit", f"nofile={spec.nofile}:{spec.nofile}"]
        if spec.cpuset:
            a += ["--cpuset-cpus", spec.cpuset]
        if spec.network:
            a += ["--network", spec.network]
        if spec.network != "host":
            for alias in spec.aliases:
                a += ["--network-alias", alias]
            for host, container in spec.ports:
                a += ["-p", f"{host}:{container}"]
        for k, v in spec.env:
            a += ["-e", f"{k}={v}"]
        if spec.entrypoint is not None:
            a += ["--entrypoint", spec.entrypoint]
        if spec.workdir:
            a += ["-w", spec.workdir]
        return a + [spec.image, *spec.command]

    def create(self, spec: ContainerSpec) -> None:
        self.run(self.create_args(spec))

    def cp_into(self, name: str, src: PurePath, dest: str) -> None:
        # `<dir>/.` copies the directory's contents; forward slashes work on every platform.
        self.run(["cp", f"{src.as_posix()}/.", f"{name}:{dest}"])

    def start(self, name: str) -> None:
        self.run(["start", name])

    def rm(self, name: str) -> None:
        self.run(["rm", "-f", "-v", name], check=False)

    def exec(self, name: str, argv: list[str], timeout: float | None = None) -> str:
        return self.run(["exec", name, *argv], timeout=timeout).stdout

    def logs(self, name: str, tail: int = 500) -> str:
        p = self.run(["logs", "--tail", str(tail), name], check=False)
        return (p.stdout or "") + (p.stderr or "")

    def state(self, name: str) -> dict:
        """Running/OOMKilled/ExitCode/RestartCount of a container ({} if it does not exist)."""
        p = self.run(["inspect", "--format", "{{json .State}}|{{.RestartCount}}", name], check=False)
        if p.returncode != 0:
            return {}
        state, _, restarts = p.stdout.strip().rpartition("|")
        return {**json.loads(state), "RestartCount": int(restarts or 0)}

    def ensure_network(self, name: str) -> None:
        if name == "host":
            return
        if self.run(["network", "inspect", name], check=False).returncode != 0:
            self.run(["network", "create", name])

    def has_image(self, image: str) -> bool:
        return self.run(["image", "inspect", image], check=False).returncode == 0

    def ensure_image(self, image: str, build: PurePath | None = None,
                     build_args: dict[str, str] | None = None) -> None:
        if build is not None:
            args = ["build", "-t", image]
            for k, v in (build_args or {}).items():
                args += ["--build-arg", f"{k}={v}"]
            self.run([*args, str(build)], timeout=3600)
        elif not self.has_image(image):
            self.run(["pull", image], timeout=1800)

    def image_id(self, image: str) -> str:
        out = self.run(["image", "inspect", "--format", "{{json .RepoDigests}}|{{.Id}}", image]).stdout.strip()
        digests, _, local_id = out.partition("|")
        repo = json.loads(digests or "[]")
        return repo[0] if repo else local_id

    def info(self) -> dict:
        return json.loads(self.run(["info", "--format", "{{json .}}"]).stdout)

    def remove_labelled(self) -> list[str]:
        ids = self.run(["ps", "-aq", "--filter", f"label={LABEL_KEY}=1"]).stdout.split()
        if ids:
            self.run(["rm", "-f", "-v", *ids])
        return ids

    def stats_command(self, names: list[str]) -> list[str]:
        return self.base() + ["stats", "--format", "{{json .}}", *names]
