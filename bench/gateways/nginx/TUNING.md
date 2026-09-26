# nginx (baseline ceiling): tuning

Image: `nginx:1.30.5-alpine` (pinned by digest in gateway.toml), resolved 2026-09-26.

| Setting | Value | Why / source |
|---|---|---|
| `worker_processes` | profile cores | One worker per core: nginx docs, "Core functionality: worker_processes". |
| `worker_connections` / `worker_rlimit_nofile` | 65535 / 1048576 | Never the limit at 64 client connections. |
| `access_log` | off | Same for every gateway (spec §8). |
| upstream `keepalive` | 512, `proxy_http_version 1.1`, `Connection ""` | Upstream connection reuse: nginx docs, "ngx_http_upstream_module: keepalive". Every gateway keeps upstream connections alive. |
| `reuseport` | on | nginx's own guidance for multi-worker accept balancing ("Socket Sharding in NGINX", nginx blog). |
| TLS | TLS 1.3 only, session cache on, ECDSA P-256 | Same cert/protocol for every gateway. |
| rate limit | `limit_req` at 10,000,000 r/s, burst 10,000,000 nodelay | Never trips at benchmark load; the probe route uses 1 r/m. |

Not applicable here: key-auth, JWT, chain, scripting (see gateway.toml `[na]`).
