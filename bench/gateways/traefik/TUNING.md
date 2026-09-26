# Traefik: tuning

Image: `traefik:v3.7.13` (digest in gateway.toml), resolved 2026-09-26.

| Setting | Value | Why / source |
|---|---|---|
| Providers | file provider only; no Docker provider, dashboard or API | Static config like every other gateway. |
| Access log / metrics | not configured (off by default) | Same "no request logging" rule (spec §8). |
| `GOMAXPROCS` | profile cores | Go scheduler threads = pinned cores. |
| `serversTransport.maxIdleConnsPerHost` | 512 | Traefik docs "ServersTransport": default 200 upstream idle connections, raised. |
| TLS | `minVersion: VersionTLS13`, default certificate store | Same cert/protocol for every gateway; HTTP/2 negotiated by default. |
| rate limit | `rateLimit` middleware, 100,000,000 / minute | Never trips; probe route 1/minute. |
| script | Yaegi local plugin (`experimental.localPlugins`) | Traefik's first-party extension mechanism; interpreted Go. |
