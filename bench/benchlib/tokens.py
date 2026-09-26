"""Fixed benchmark credentials. These are public test fixtures, not secrets: they only
ever authenticate against throwaway gateways inside a benchmark run."""
from __future__ import annotations

import base64
import hashlib
import hmac
import json

API_KEY = "bench-api-key"
JWT_SECRET = "featherbit-bench-hs256-secret-0123456789"
JWT_ISSUER = "bench-jwt"
JWT_KID = "bench"
JWT_EXP = 4102444800  # 2100-01-01T00:00:00Z
TYK_SECRET = "bench-tyk-control-secret"


def _b64url(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _compact(obj: dict) -> bytes:
    return json.dumps(obj, separators=(",", ":")).encode()


def make_jwt(secret: str = JWT_SECRET, claims: dict | None = None) -> str:
    # `key` is what APISIX/Featherbit consumer lookup reads; `iss` is what Kong/Envoy/KrakenD read.
    claims = claims or {"key": JWT_ISSUER, "iss": JWT_ISSUER, "sub": "bench", "exp": JWT_EXP}
    signing = f"{_b64url(_compact({'alg': 'HS256', 'typ': 'JWT', 'kid': JWT_KID}))}.{_b64url(_compact(claims))}"
    sig = hmac.new(secret.encode(), signing.encode(), hashlib.sha256).digest()
    return f"{signing}.{_b64url(sig)}"


def tamper(token: str) -> str:
    # Flip the first signature character: the last one may only carry padding bits.
    head, _, sig = token.rpartition(".")
    return f"{head}.{'A' if sig[0] != 'A' else 'B'}{sig[1:]}"


def jwks_json(secret: str = JWT_SECRET) -> str:
    return json.dumps({"keys": [{"kty": "oct", "kid": JWT_KID, "alg": "HS256", "use": "sig",
                                 "k": _b64url(secret.encode())}]}, separators=(",", ":"))


def static_values() -> dict[str, str]:
    token = make_jwt()
    return {
        "API_KEY": API_KEY,
        "JWT": token,
        "JWT_TAMPERED": tamper(token),
        "JWT_SECRET": JWT_SECRET,
        "JWT_SECRET_B64": base64.b64encode(JWT_SECRET.encode()).decode(),
        "JWT_ISSUER": JWT_ISSUER,
        "JWT_KID": JWT_KID,
        "JWKS_JSON": jwks_json(),
        "TYK_SECRET": TYK_SECRET,
    }
