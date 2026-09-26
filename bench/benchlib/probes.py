"""Correctness probes (spec §6 step 2) and the boot health wait (step 1)."""
from __future__ import annotations

import http.client
import socket
import ssl
import time
from dataclasses import dataclass

from .config import Probe
from .templating import render_text

SNI = "bench.local"


@dataclass(frozen=True)
class ProbeResult:
    probe: Probe
    ok: bool
    detail: str


def _ctx(alpn: list[str] | None = None) -> ssl.SSLContext:
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    if alpn:
        ctx.set_alpn_protocols(alpn)
    return ctx


def http_call(method: str, scheme: str, host: str, port: int, path: str,
              headers: dict[str, str] | None = None, body: bytes | None = None,
              timeout: float = 5.0) -> tuple[int, dict[str, str], bytes]:
    if scheme == "https":
        conn = http.client.HTTPSConnection(host, port, timeout=timeout, context=_ctx())
    else:
        conn = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        conn.request(method, path, body=body, headers={"Host": SNI, **(headers or {})})
        resp = conn.getresponse()
        data = resp.read()
        return resp.status, {k.lower(): v for k, v in resp.getheaders()}, data
    finally:
        conn.close()


def tls_info(host: str, port: int, alpn: list[str] | None = None,
             timeout: float = 5.0) -> tuple[str, str | None]:
    with socket.create_connection((host, port), timeout=timeout) as raw:
        with _ctx(alpn).wrap_socket(raw, server_hostname=SNI) as s:
            return s.version(), s.selected_alpn_protocol()


def run_probe(probe: Probe, scheme: str, host: str, port: int, values: dict[str, str],
              timeout: float = 5.0) -> ProbeResult:
    try:
        if probe.kind == "tls13":
            version, _ = tls_info(host, port, timeout=timeout)
            return ProbeResult(probe, version == "TLSv1.3", f"negotiated {version}")
        if probe.kind == "alpn-h2":
            _, alpn = tls_info(host, port, alpn=["h2", "http/1.1"], timeout=timeout)
            return ProbeResult(probe, alpn == "h2", f"ALPN {alpn}")
        headers = {k: render_text(v, values) for k, v in probe.headers.items()}
        status, got, body = http_call("GET", scheme, host, port, probe.path, headers, timeout=timeout)
    except (OSError, http.client.HTTPException) as e:
        return ProbeResult(probe, False, f"{type(e).__name__}: {e}")
    problems = []
    if status not in probe.expect_status:
        problems.append(f"status {status}, expected {list(probe.expect_status)}")
    for name, want in probe.expect_headers.items():
        if got.get(name) != want:
            problems.append(f"header {name}={got.get(name)!r}, expected {want!r}")
    for name in probe.expect_absent:
        if name in got:
            problems.append(f"header {name} present, expected absent")
    if probe.expect_body_bytes is not None and len(body) != probe.expect_body_bytes:
        problems.append(f"body {len(body)} bytes, expected {probe.expect_body_bytes}")
    return ProbeResult(probe, not problems, "; ".join(problems) or f"status {status}")


def run_probes(probes, scheme: str, host: str, port: int, values: dict[str, str]) -> list[ProbeResult]:
    return [run_probe(p, scheme, host, port, values) for p in probes]


def wait_healthy(scheme: str, host: str, port: int, path: str, timeout_s: float,
                 sleep=time.sleep, clock=time.monotonic) -> bool:
    """True once the listener answers anything below 500 (404 counts: the process is up)."""
    deadline = clock() + timeout_s
    while True:
        try:
            status, _, _ = http_call("GET", scheme, host, port, path, timeout=2.0)
            if status < 500:
                return True
        except (OSError, http.client.HTTPException):
            pass
        if clock() >= deadline:
            return False
        sleep(0.5)
