# Kong Gateway (OSS): tuning

Image: `kong:3.9.3` (digest in gateway.toml), resolved 2026-09-26. Kong stopped publishing open-source images after the 3.9 line; 3.9.3 is the newest OSS build (newer tags are Enterprise-only `kong/kong-gateway`).

| Setting | Value | Why / source |
|---|---|---|
| Mode | DB-less, declarative `kong.yml`; Admin and Status listeners off | Kong docs "DB-less and declarative configuration". Static config, no database. |
| `nginx_worker_processes` | profile cores | Kong docs "Performance testing / benchmark" recommendations: workers = cores. |
| `proxy_access_log` | off | Same for every gateway (spec §8). |
| `proxy_listen` | `reuseport backlog=65535`; `http2 ssl` on 8443 | Kong's published performance-testing configuration. |
| upstream keepalive | pool 512, 1,000,000 requests, 300 s idle | `upstream_keepalive_*` in kong.conf reference. |
| TLS | TLS 1.3 only, default cert (`ssl_cert`) | Same cert for every gateway; the default cert answers any SNI. |
| jwt | `claims_to_verify: [exp]` | Featherbit, APISIX and Envoy always verify `exp`; Kong only does when asked. |
| rate-limiting | `policy: local`, 100,000,000 / minute, by IP | Never trips; the probe route uses 1/minute. |
| script | `pre-function` (access phase), default `untrusted_lua = sandbox` | Kong's first-party Lua hook. |
