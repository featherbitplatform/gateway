"""A throwaway self-signed ECDSA P-256 certificate per run, generated inside the loadgen image."""
from __future__ import annotations

import re
from dataclasses import dataclass

from .docker import Docker, DockerError

_CERT = re.compile(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----\n?", re.S)
_KEY = re.compile(r"-----BEGIN (?:EC )?PRIVATE KEY-----.*?-----END (?:EC )?PRIVATE KEY-----\n?", re.S)
CERT_SCRIPT = (
    "openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes "
    "-keyout /tmp/key.pem -out /tmp/cert.pem -days 30 -subj /CN=bench.local "
    "-addext subjectAltName=DNS:bench.local,DNS:gateway,IP:127.0.0.1 2>/dev/null "
    "&& cat /tmp/cert.pem /tmp/key.pem"
)


@dataclass(frozen=True)
class Certs:
    cert_pem: str
    key_pem: str


def split_pem(text: str) -> Certs:
    cert, key = _CERT.search(text), _KEY.search(text)
    if not cert or not key:
        raise DockerError("certificate generation did not print a certificate and a private key")
    return Certs(cert_pem=cert.group(0), key_pem=key.group(0))


def generate_certs(docker: Docker, image: str) -> Certs:
    return split_pem(docker.run(["run", "--rm", "--entrypoint", "sh", image, "-c", CERT_SCRIPT]).stdout)
