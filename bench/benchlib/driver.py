"""DockerDriver: boots a cell's containers, runs the load tools, samples resources (spec §4, §6)."""
from __future__ import annotations

import http.client
from urllib.parse import urlsplit
import shutil
import time
from dataclasses import dataclass
from pathlib import Path

from .certs import Certs
from .config import ROLES, Gateway, Params, Probe, Scenario, Topology, cpuset_size
from .docker import ContainerSpec, Docker, DockerError
from .matrix import Cell
from .parsers import LoadResult, ParseError, parse_oha, parse_wrk, parse_wrk2
from .probes import ProbeResult, http_call, run_probes, wait_healthy
from .results import RunStore
from .runner import BootError, LoadError
from .sampler import Sample, Sampler
from .templating import TemplateError, render_dir, render_text
from .tokens import static_values

NAMES = {"gateway": "fbb-gateway", "upstream": "fbb-upstream", "loadgen": "fbb-loadgen"}
UPSTREAM_PORT = 8080


@dataclass(frozen=True)
class Images:
    loadgen: str = "featherbit-bench/loadgen:dev"
    upstream: str = "featherbit-bench/upstream:dev"


class DockerDriver:
    def __init__(self, topology: Topology, gateways: dict[str, Gateway], scenarios: dict[str, Scenario],
                 params: Params, images: Images, gateway_images: dict[str, str], certs: Certs,
                 store: RunStore, docker_factory=Docker, health=wait_healthy, probes=run_probes,
                 http=http_call, sampler_factory=Sampler, sleep=time.sleep):
        self.t = topology
        self.gateways = gateways
        self.scenarios = scenarios
        self.params = params
        self.images = images
        self.gateway_images = gateway_images
        self.certs = certs
        self.store = store
        self.health = health
        self.probes = probes
        self.http = http
        self.sampler_factory = sampler_factory
        self.sleep = sleep
        self.local = topology.kind == "local"
        self.dk = {role: docker_factory(topology.roles[role].context) for role in ROLES}
        self._dep_names = sorted({f"fbb-dep-{d.name}" for g in gateways.values() for d in g.dependencies})
        self._sampler = None
        self._sampling_roles: dict[str, str] = {}
        self._rep = 0
        self._seq = 0

    # --- addressing -------------------------------------------------------------------

    def _host(self, role: str) -> str:
        return role if self.local else self.t.roles[role].address

    def values(self, cell: Cell) -> dict[str, str]:
        gw = self.gateways[cell.gateway]
        v = {**static_values(), "UPSTREAM_HOST": self._host("upstream"), "UPSTREAM_PORT": str(UPSTREAM_PORT),
             "CORES": str(cell.profile), "CONFIG_DIR": gw.config_dir,
             "TLS_CERT_PEM": self.certs.cert_pem, "TLS_KEY_PEM": self.certs.key_pem}
        for dep in gw.dependencies:
            key = dep.name.upper().replace("-", "_")
            v[f"DEP_{key}_HOST"] = dep.name if self.local else self.t.roles["upstream"].address
            v[f"DEP_{key}_PORT"] = str(dep.port)
        return v

    def load_url(self, cell: Cell) -> str:
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        if gw.kind == "direct":
            host, port = self._host("upstream"), UPSTREAM_PORT
        else:
            host, port = self._host("gateway"), gw.ports["tls" if sc.scheme == "https" else "plain"]
        return f"{sc.scheme}://{host}:{port}{sc.path}"

    def probe_target(self, cell: Cell) -> tuple[str, int]:
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        if gw.kind == "direct":
            if self.local:
                return self.t.probe_host, self.t.published_ports["upstream"]
            return self.t.roles["upstream"].address, UPSTREAM_PORT
        key = "tls" if sc.scheme == "https" else "plain"
        return self.t.probe_host, (self.t.published_ports[key] if self.local else gw.ports[key])

    def gateway_health(self, cell: Cell) -> tuple[bool, str]:
        if self.gateways[cell.gateway].kind != "container":
            return True, "no gateway container"
        st = self.dk["gateway"].state(NAMES["gateway"])
        if not st:
            return False, "container is gone"
        if not st.get("Running") or st.get("RestartCount", 0) > 0:
            return False, (f"exited {st.get('ExitCode')}" + (" (OOMKilled)" if st.get("OOMKilled") else "")
                           + (f", restarted {st['RestartCount']}x" if st.get("RestartCount") else ""))
        return True, "running"

    def sni(self, cell: Cell) -> str:
        """The TLS server name the load generator sends (its URL host): probes and the
        health check present the same one, so a gateway that cannot serve it fails
        validation instead of every TLS measurement."""
        return urlsplit(self.load_url(cell)).hostname or ""

    def cores(self, cell: Cell, role: str) -> int:
        return cpuset_size(getattr(self.t.profiles[cell.profile], role))

    # --- lifecycle --------------------------------------------------------------------

    def _start(self, role: str, spec: ContainerSpec) -> None:
        try:
            self.dk[role].create(spec)
            self.dk[role].start(spec.name)
        except DockerError as e:
            raise BootError(str(e)) from None

    def _render(self, gw: Gateway, cell: Cell, values: dict[str, str]) -> Path:
        name = gw.configs[cell.scenario]
        out = self.store.configs_dir / gw.name / name
        if out.exists():
            shutil.rmtree(out)
        try:
            if gw.common:
                render_dir(gw.dir / gw.common, out, values)
            render_dir(gw.dir / name, out, values)
        except TemplateError as e:
            raise BootError(f"config template: {e}") from None
        (out / "tls").mkdir(parents=True, exist_ok=True)
        (out / "tls" / "cert.pem").write_bytes(self.certs.cert_pem.encode("utf-8"))
        (out / "tls" / "key.pem").write_bytes(self.certs.key_pem.encode("utf-8"))
        return out

    def _wait_upstream(self) -> None:
        url = f"http://{self._host('upstream')}:{UPSTREAM_PORT}/bench/1k"
        deadline = time.monotonic() + self.params.boot_timeout_seconds
        while True:
            try:
                code = self.dk["loadgen"].exec(
                    NAMES["loadgen"], ["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", url], timeout=10)
                if code.strip() == "200":
                    return
            except DockerError:
                pass
            if time.monotonic() >= deadline:
                raise BootError(f"upstream not serving {url}")
            self.sleep(0.5)

    def boot(self, cell: Cell, rep: int) -> None:
        self.teardown()  # clears stale fbb-* containers from any earlier crash
        self._rep, self._seq = rep, 0
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        prof = self.t.profiles[cell.profile]
        net = self.t.network
        try:
            for role in ROLES:
                self.dk[role].ensure_network(net)
        except DockerError as e:
            raise BootError(str(e)) from None
        up_ports = ((self.t.published_ports["upstream"], UPSTREAM_PORT),) if self.local else ()
        self._start("upstream", ContainerSpec(
            name=NAMES["upstream"], image=self.images.upstream, cpuset=prof.upstream, network=net,
            aliases=("upstream",), ports=up_ports,
            command=("nginx", "-g", f"daemon off; worker_processes {cpuset_size(prof.upstream)};")))
        for dep in gw.dependencies:
            self._start("upstream", ContainerSpec(
                name=f"fbb-dep-{dep.name}", image=dep.image, cpuset=prof.upstream, network=net,
                aliases=(dep.name,), command=dep.command))
        self._start("loadgen", ContainerSpec(
            name=NAMES["loadgen"], image=self.images.loadgen, cpuset=prof.loadgen, network=net,
            aliases=("loadgen",), command=("sleep", "infinity")))
        self._wait_upstream()
        if gw.kind == "container":
            values = self.values(cell)
            rendered = self._render(gw, cell, values)
            ports = tuple((self.t.published_ports[k], p) for k, p in gw.ports.items()) if self.local else ()
            spec = ContainerSpec(
                name=NAMES["gateway"], image=self.gateway_images[gw.name], cpuset=prof.gateway,
                network=net, aliases=("gateway",), ports=ports,
                env=tuple((k, render_text(v, values)) for k, v in gw.env.items()),
                command=tuple(render_text(c, values) for c in gw.command),
                entrypoint=gw.entrypoint, workdir=gw.workdir)
            try:
                self.dk["gateway"].create(spec)
                self.dk["gateway"].cp_into(NAMES["gateway"], rendered, gw.config_dir)
                self.dk["gateway"].start(NAMES["gateway"])
            except DockerError as e:
                raise BootError(str(e)) from None
        host, port = self.probe_target(cell)
        if not self.health(sc.scheme, host, port, "/", self.params.boot_timeout_seconds, sni=self.sni(cell)):
            raise BootError(f"{gw.name} not answering on {sc.scheme}://{host}:{port} "
                            f"within {self.params.boot_timeout_seconds}s")

    def setup_and_probe(self, cell: Cell) -> list[ProbeResult]:
        gw, sc = self.gateways[cell.gateway], self.scenarios[cell.scenario]
        values = self.values(cell)
        host, port = self.probe_target(cell)
        results: list[ProbeResult] = []
        for call in gw.setup:
            if cell.scenario not in call.scenarios:
                continue
            path = render_text(call.path, values)
            body = (render_text((gw.dir / call.body_file).read_text("utf-8"), values).encode("utf-8")
                    if call.body_file else None)
            headers = {k: render_text(v, values) for k, v in call.headers.items()}
            try:
                status, _, _ = self.http(call.method, sc.scheme, host, port, path, headers, body, sni=self.sni(cell))
                detail = f"setup {call.method} {path}: status {status}"
            except (OSError, http.client.HTTPException) as e:
                status, detail = None, f"setup {call.method} {path}: {type(e).__name__}: {e}"
            ok = status in call.expect_status
            results.append(ProbeResult(Probe(path=path, kind="setup"), ok, detail))
            if not ok:
                return results
        return results + self.probes(sc.probes, sc.scheme, host, port, values, sni=self.sni(cell))

    def load(self, cell: Cell, rate: int | None, seconds: int) -> LoadResult:
        sc = self.scenarios[cell.scenario]
        values = self.values(cell)
        url = self.load_url(cell)
        threads = self.cores(cell, "loadgen")
        conns = self.params.connections
        headers: list[str] = []
        for k, v in sc.headers.items():
            headers += ["-H", f"{k}: {render_text(v, values)}"]
        if sc.tool == "oha":
            # 16 connections x 4 streams = the same 64 in-flight requests as the HTTP/1.1 tools.
            argv = ["oha", "-z", f"{seconds}s", "-c", str(max(1, conns // 4)), "-p", "4", "--http2",
                    "--insecure", "--no-tui", "--output-format", "json", *headers]
            if rate is not None:
                argv += ["-q", str(rate), "--latency-correction"]
            argv.append(url)
            parse, tool = (lambda out: parse_oha(out, inflight=max(1, conns // 4) * 4)), "oha"
        elif rate is None:
            argv = ["wrk", f"-t{threads}", f"-c{conns}", f"-d{seconds}s", "--latency", *headers, url]
            parse, tool = parse_wrk, "wrk"
        else:
            argv = ["wrk2", f"-t{threads}", f"-c{conns}", f"-d{seconds}s", f"-R{rate}", "--latency", *headers, url]
            parse, tool = parse_wrk2, "wrk2"
        try:
            out = self.dk["loadgen"].exec(NAMES["loadgen"], argv, timeout=seconds + 60)
        except DockerError as e:
            raise LoadError(str(e)) from None
        self._seq += 1
        raw = self.store.raw_dir / cell.slug
        raw.mkdir(parents=True, exist_ok=True)
        (raw / f"rep{self._rep}-{self._seq:03d}-{tool}-{rate or 'flood'}.txt").write_bytes(
            (" ".join(argv) + "\n\n" + out).encode("utf-8"))
        try:
            return parse(out)
        except ParseError as e:
            raise LoadError(str(e)) from None

    def start_sampling(self, cell: Cell) -> None:
        roles = ["upstream", "loadgen"] + (["gateway"] if self.gateways[cell.gateway].kind == "container" else [])
        self._sampling_roles = {NAMES[r]: r for r in roles}
        self._sampler = self.sampler_factory([(self.dk[r], NAMES[r]) for r in roles])
        self._sampler.start()

    def stop_sampling(self) -> dict[str, list[Sample]]:
        out: dict[str, list[Sample]] = {r: [] for r in ROLES}
        if self._sampler is None:
            return out
        samples, self._sampler = self._sampler.stop(), None
        for s in samples:
            role = self._sampling_roles.get(s.name)
            if role:
                out[role].append(s)
        return out

    def logs(self) -> dict[str, str]:
        out = {NAMES["gateway"]: self.dk["gateway"].logs(NAMES["gateway"]),
               NAMES["upstream"]: self.dk["upstream"].logs(NAMES["upstream"])}
        for name in self._dep_names:
            out[name] = self.dk["upstream"].logs(name)
        return out

    def teardown(self) -> None:
        if self._sampler is not None:
            self._sampler.stop()
            self._sampler = None
        self.dk["gateway"].rm(NAMES["gateway"])
        self.dk["loadgen"].rm(NAMES["loadgen"])
        for name in self._dep_names:
            self.dk["upstream"].rm(name)
        self.dk["upstream"].rm(NAMES["upstream"])
