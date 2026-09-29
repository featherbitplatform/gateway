"""Load and validate the benchmark's TOML inputs: scenarios, gateway adapters, topologies."""
from __future__ import annotations

import dataclasses
import tomllib
from dataclasses import dataclass, field, replace
from pathlib import Path


class ConfigError(Exception):
    """An input file is missing, malformed or inconsistent."""


ROLES = ("loadgen", "gateway", "upstream")
TOOLS = {"wrk2", "oha"}
SCHEMES = {"http", "https"}
PROBE_KINDS = {"http", "tls13", "alpn-h2"}


@dataclass(frozen=True)
class Params:
    slo_p99_ms: float = 10.0
    max_error_rate: float = 0.001
    connections: int = 64
    ceiling_seconds: int = 10
    warmup_seconds: int = 30
    warmup_fraction: float = 0.2
    search_step_seconds: int = 20
    search_low: float = 0.1
    search_high: float = 1.2
    search_tolerance: float = 0.02
    ladder_seconds: int = 60
    ladder: tuple[int, ...] = (1000, 5000, 10000, 25000, 50000, 100000, 200000)
    reps: int = 5
    saturation_threshold: float = 0.85
    boot_timeout_seconds: int = 60

    def quick(self) -> "Params":
        """Short everything, one repetition: for local iteration, never for results."""
        return replace(self, reps=1, ceiling_seconds=5, warmup_seconds=5,
                       search_step_seconds=5, ladder_seconds=5, ladder=self.ladder[:3])


def params_from_dict(d: dict) -> Params:
    known = {f.name for f in dataclasses.fields(Params)}
    unknown = set(d) - known
    if unknown:
        raise ConfigError(f"unknown params {sorted(unknown)}")
    if "ladder" in d:
        d = {**d, "ladder": tuple(int(x) for x in d["ladder"])}
    return Params(**d)


@dataclass(frozen=True)
class Probe:
    path: str
    kind: str = "http"
    headers: dict[str, str] = field(default_factory=dict)
    expect_status: tuple[int, ...] = (200,)
    expect_headers: dict[str, str] = field(default_factory=dict)
    expect_absent: tuple[str, ...] = ()
    expect_body_bytes: int | None = None


@dataclass(frozen=True)
class Scenario:
    id: str
    family: str
    tool: str
    scheme: str
    path: str
    body_bytes: int
    headers: dict[str, str] = field(default_factory=dict)
    probes: tuple[Probe, ...] = ()


@dataclass(frozen=True)
class Dependency:
    name: str
    image: str
    port: int
    command: tuple[str, ...] = ()


@dataclass(frozen=True)
class SetupCall:
    method: str
    path: str
    scenarios: tuple[str, ...]
    headers: dict[str, str] = field(default_factory=dict)
    body_file: str | None = None
    expect_status: tuple[int, ...] = (200,)


@dataclass(frozen=True)
class Gateway:
    name: str
    kind: str
    dir: Path
    configs: dict[str, str]
    na: dict[str, str]
    image: str = ""
    build: Path | None = None
    build_args: dict[str, str] = field(default_factory=dict)
    build_requires: tuple[str, ...] = ()
    build_hint: str = ""
    config_dir: str = ""
    common: str | None = None
    entrypoint: str | None = None
    command: tuple[str, ...] = ()
    workdir: str | None = None
    env: dict[str, str] = field(default_factory=dict)
    ports: dict[str, int] = field(default_factory=dict)
    dependencies: tuple[Dependency, ...] = ()
    setup: tuple[SetupCall, ...] = ()

    def supports(self, scenario_id: str) -> bool:
        return scenario_id in self.configs


@dataclass(frozen=True)
class Role:
    address: str
    context: str | None = None


@dataclass(frozen=True)
class Profile:
    cores: int
    gateway: str
    upstream: str
    loadgen: str


@dataclass(frozen=True)
class Topology:
    name: str
    kind: str
    network: str
    roles: dict[str, Role]
    profiles: dict[int, Profile]
    published_ports: dict[str, int] = field(default_factory=dict)
    probe_host: str = ""


def _read(path: Path) -> dict:
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except FileNotFoundError:
        raise ConfigError(f"{path}: file not found") from None
    except tomllib.TOMLDecodeError as e:
        raise ConfigError(f"{path}: {e}") from None


def _probe(pr: dict, default_path: str, where: str) -> Probe:
    kind = pr.get("kind", "http")
    if kind not in PROBE_KINDS:
        raise ConfigError(f"{where}: probe kind must be one of {sorted(PROBE_KINDS)}")
    return Probe(
        path=pr.get("path", default_path),
        kind=kind,
        headers={str(k): str(v) for k, v in pr.get("headers", {}).items()},
        expect_status=tuple(int(s) for s in pr.get("expect_status", [200])),
        expect_headers={str(k).lower(): str(v) for k, v in pr.get("expect_headers", {}).items()},
        expect_absent=tuple(str(h).lower() for h in pr.get("expect_absent", [])),
        expect_body_bytes=pr.get("expect_body_bytes"),
    )


def load_scenarios(path: Path) -> tuple[Params, dict[str, Scenario]]:
    raw = _read(path)
    try:
        params = params_from_dict(raw.get("params", {}))
    except ConfigError as e:
        raise ConfigError(f"{path}: {e}") from None
    scenarios: dict[str, Scenario] = {}
    for s in raw.get("scenario", []):
        sid = s.get("id")
        if not sid:
            raise ConfigError(f"{path}: scenario without id")
        where = f"{path}: scenario {sid!r}"
        if sid in scenarios:
            raise ConfigError(f"{where}: duplicate id")
        try:
            if s["tool"] not in TOOLS:
                raise ConfigError(f"{where}: tool must be one of {sorted(TOOLS)}")
            if s["scheme"] not in SCHEMES:
                raise ConfigError(f"{where}: scheme must be one of {sorted(SCHEMES)}")
            probes = tuple(_probe(pr, s["path"], where) for pr in s.get("probe", []))
            if not probes:
                raise ConfigError(f"{where}: at least one probe is required")
            scenarios[sid] = Scenario(
                id=sid, family=s["family"], tool=s["tool"], scheme=s["scheme"], path=s["path"],
                body_bytes=int(s["body_bytes"]),
                headers={str(k): str(v) for k, v in s.get("headers", {}).items()},
                probes=probes,
            )
        except KeyError as e:
            raise ConfigError(f"{where}: missing key {e}") from None
    if not scenarios:
        raise ConfigError(f"{path}: no scenarios")
    return params, scenarios


def load_gateway(directory: Path, scenarios: dict[str, Scenario]) -> Gateway:
    path = directory / "gateway.toml"
    raw = _read(path)
    where = str(path)
    name = raw.get("name", directory.name)
    if name != directory.name:
        raise ConfigError(f"{where}: name {name!r} must match the directory name {directory.name!r}")
    kind = raw.get("kind", "container")
    if kind not in ("container", "direct"):
        raise ConfigError(f"{where}: kind must be 'container' or 'direct'")
    configs = {str(k): str(v) for k, v in raw.get("configs", {}).items()}
    na = {str(k): str(v) for k, v in raw.get("na", {}).items()}
    for sid in [*configs, *na]:
        if sid not in scenarios:
            raise ConfigError(f"{where}: unknown scenario {sid!r}")
    both = sorted(set(configs) & set(na))
    if both:
        raise ConfigError(f"{where}: {both} listed under both [configs] and [na]")
    missing = [s for s in scenarios if s not in configs and s not in na]
    if missing:
        raise ConfigError(f"{where}: scenarios with neither a config nor an n/a reason: {missing}")
    blank = [s for s, reason in na.items() if not reason.strip()]
    if blank:
        raise ConfigError(f"{where}: empty n/a reason for {blank}")
    try:
        deps = tuple(
            Dependency(name=d["name"], image=d["image"], port=int(d["port"]),
                       command=tuple(d.get("command", [])))
            for d in raw.get("dependency", [])
        )
        setup = tuple(
            SetupCall(method=s["method"].upper(), path=s["path"], scenarios=tuple(s["scenarios"]),
                      headers={str(k): str(v) for k, v in s.get("headers", {}).items()},
                      body_file=s.get("body_file"),
                      expect_status=tuple(int(x) for x in s.get("expect_status", [200])))
            for s in raw.get("setup", [])
        )
    except KeyError as e:
        raise ConfigError(f"{where}: missing key {e}") from None
    for call in setup:
        bad = [s for s in call.scenarios if s not in configs]
        if bad:
            raise ConfigError(f"{where}: setup {call.method} {call.path} targets unsupported scenarios {bad}")
        if call.body_file and not (directory / call.body_file).is_file():
            raise ConfigError(f"{where}: setup body_file {call.body_file!r} not found")
    common = raw.get("common")
    ports = {str(k): int(v) for k, v in raw.get("ports", {}).items()}
    if kind == "container":
        for key in ("image", "config_dir"):
            if not raw.get(key):
                raise ConfigError(f"{where}: {key!r} is required")
        if "plain" not in ports:
            raise ConfigError(f"{where}: ports.plain is required")
        if any(scenarios[s].scheme == "https" for s in configs) and "tls" not in ports:
            raise ConfigError(f"{where}: an https scenario is configured but ports.tls is missing")
        for sid, sub in configs.items():
            if not sub or not (directory / sub).is_dir():
                raise ConfigError(f"{where}: config directory {sub!r} for {sid} not found")
        if common and not (directory / common).is_dir():
            raise ConfigError(f"{where}: common directory {common!r} not found")
    elif any(configs.values()):
        raise ConfigError(f"{where}: a direct gateway has no config directories")
    build = raw.get("build")
    return Gateway(
        name=name, kind=kind, dir=directory, configs=configs, na=na,
        image=raw.get("image", ""),
        build=(directory / build).resolve() if build else None,
        build_args={str(k): str(v) for k, v in raw.get("build_args", {}).items()},
        build_requires=tuple(raw.get("build_requires", [])),
        build_hint=raw.get("build_hint", ""),
        config_dir=raw.get("config_dir", ""),
        common=common,
        entrypoint=raw.get("entrypoint"),
        command=tuple(raw.get("command", [])),
        workdir=raw.get("workdir"),
        env={str(k): str(v) for k, v in raw.get("env", {}).items()},
        ports=ports, dependencies=deps, setup=setup,
    )


def load_gateways(root: Path, scenarios: dict[str, Scenario]) -> dict[str, Gateway]:
    return {
        d.name: load_gateway(d, scenarios)
        for d in sorted(root.iterdir())
        if d.is_dir() and (d / "gateway.toml").is_file()
    }


def cpuset_members(spec: str) -> set[int]:
    members: set[int] = set()
    try:
        for part in spec.split(","):
            part = part.strip()
            if not part:
                raise ValueError
            if "-" in part:
                a, b = part.split("-", 1)
                lo, hi = int(a), int(b)
                if hi < lo:
                    raise ValueError
                members.update(range(lo, hi + 1))
            else:
                members.add(int(part))
    except ValueError:
        raise ConfigError(f"bad cpuset {spec!r} (use forms like '0', '0-3', '0,2-3')") from None
    return members


def cpuset_size(spec: str) -> int:
    return len(cpuset_members(spec))


def load_topology(path: Path) -> Topology:
    raw = _read(path)
    kind = raw.get("kind")
    if kind == "local":
        network = raw.get("network", "featherbit-bench")
        roles = {r: Role(address=r) for r in ROLES}
        published = {"plain": 18000, "tls": 18443, "upstream": 18080,
                     **{str(k): int(v) for k, v in raw.get("published_ports", {}).items()}}
        probe_host = "127.0.0.1"
    elif kind == "remote":
        network = "host"
        roles = {}
        for r in ROLES:
            rr = raw.get("roles", {}).get(r)
            if not rr or "address" not in rr or "context" not in rr:
                raise ConfigError(f"{path}: roles.{r} needs 'context' and 'address'")
            roles[r] = Role(address=rr["address"], context=rr["context"])
        published = {}
        probe_host = roles["gateway"].address
    else:
        raise ConfigError(f"{path}: kind must be 'local' or 'remote'")
    profiles: dict[int, Profile] = {}
    for key, pr in raw.get("profiles", {}).items():
        try:
            cores = int(key)
            prof = Profile(cores=cores, gateway=pr["gateway"], upstream=pr["upstream"], loadgen=pr["loadgen"])
        except (ValueError, KeyError) as e:
            raise ConfigError(f"{path}: profile {key!r}: {e}") from None
        sets = {r: cpuset_members(getattr(prof, r)) for r in ROLES}
        if len(sets["gateway"]) != cores:
            raise ConfigError(f"{path}: profile {key}: gateway cpuset {prof.gateway!r} has "
                              f"{len(sets['gateway'])} cpus, expected {cores}")
        if kind == "local":
            for a, b in (("gateway", "upstream"), ("gateway", "loadgen"), ("upstream", "loadgen")):
                if sets[a] & sets[b]:
                    raise ConfigError(f"{path}: profile {key}: {a} and {b} cpusets overlap")
        profiles[cores] = prof
    if not profiles:
        raise ConfigError(f"{path}: no profiles")
    return Topology(name=path.stem, kind=kind, network=network, roles=roles, profiles=profiles,
                    published_ports=published, probe_host=probe_host)
