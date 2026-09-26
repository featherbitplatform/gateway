# KrakenD CE: tuning

Image: `krakend:2.13.11` (official image; `devopsfaith/krakend` stopped at 2.9) (digest in gateway.toml), resolved 2026-09-26.

| Setting | Value | Why / source |
|---|---|---|
| Encoding | `no-op` on endpoint and backend | KrakenD docs "No-op encoding": turns the aggregator into a transparent proxy, the only fair mode for a proxy benchmark. |
| `router.disable_access_log` | true | Same "no request logging" rule (spec §8). |
| `GOMAXPROCS` | profile cores | Go scheduler threads = pinned cores. |
| `max_idle_connections_per_host` | 512 | KrakenD docs "HTTP transport settings": upstream keep-alive pool. |
| Routes | `/bench/{size}` parameters (CE has no catch-all wildcard) | Same request paths as every gateway. |
| jwt |  `auth/validator`, local HS256 JWKS (no `cache`: KrakenD rejects it with `jwk_local_path`) | Built-in CE validator. |
| rate limit | `qos/ratelimit/router` per client IP, 100,000,000 / minute | Never trips; probe route 1/minute. |
| script | `modifier/lua-backend` with `allow_open_libs` (needed for `string`) | KrakenD's first-party Lua. |
| Header forwarding | `input_headers` only where a scenario needs one | KrakenD forwards no client headers by default (by design). |
