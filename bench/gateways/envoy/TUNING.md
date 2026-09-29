# Envoy: tuning

Image: `envoyproxy/envoy:v1.39.1` (digest in gateway.toml), resolved 2026-09-26.

| Setting | Value | Why / source |
|---|---|---|
| Bootstrap | static, no admin listener, no xDS | Static config like every other gateway. |
| `--concurrency` | profile cores | Envoy docs "Threading model": one worker thread per core. |
| Access log | none configured | Envoy logs no requests unless an access_log is configured (spec §8). |
| Circuit breakers | 1,000,000 on the upstream cluster | Envoy docs "Circuit breaking" / FAQ "benchmarking": the 1024 defaults throttle benchmarks. |
| TLS | TLS 1.3 minimum, ALPN h2 + http/1.1 | Same cert/protocol for every gateway. |
| jwt | `jwt_authn` with a local HS256 JWKS (oct key) | Built-in filter; verifies signature and `exp`. |
| key-auth | `api_key_auth` filter (if present in this version) | Built-in filter. |
| rate limit | `local_ratelimit` token bucket 100,000,000 / 60 s | Never trips. Envoy's local bucket is per filter config, not per client IP; with one load-generator IP it's equivalent to the others' per-IP counters. |
| script | `envoy.filters.http.lua` | Envoy's first-party scripting filter. |
