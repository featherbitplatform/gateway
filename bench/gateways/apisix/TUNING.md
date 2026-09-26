# Apache APISIX: tuning

Image: `apache/apisix:3.18.0-debian` (digest in gateway.toml), resolved 2026-09-26.

| Setting | Value | Why / source |
|---|---|---|
| Deployment | standalone YAML data plane, Admin API off | APISIX docs "Deployment modes: standalone". No etcd round trips; static config like every other gateway. |
| `nginx_config.worker_processes` | profile cores | APISIX benchmark guidance: one worker per core. |
| `enable_access_log` | false | Same for every gateway (spec §8). |
| upstream keepalive | pool 512, 1,000,000 requests, 300 s | APISIX `config-default.yaml` `nginx_config.http.upstream`, raised so pooled connections are never recycled mid-run. |
| TLS | TLS 1.3, `enable_http2` on 8443, `fallback_sni: bench.local` | Load tools connect by container name or IP; the fallback SNI selects the bench cert. |
| Plugins | APISIX default plugin list, left unchanged | Only plugins configured on a route execute. |
| limit-count | `policy: local`, 100,000,000 / 60 s | Never trips; the probe route uses count 1. |
| script | `serverless-pre-function`, rewrite phase | APISIX's first-party Lua hook. |
