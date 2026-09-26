# Tyk Gateway (OSS): tuning

Images: `tykio/tyk-gateway:v5.14.0` and `redis:7.4.11-alpine` (digests in gateway.toml), resolved 2026-09-26.

| Setting | Value | Why / source |
|---|---|---|
| Redis | required dependency, on the upstream's cores, persistence off | Tyk OSS stores keys, sessions and rate-limit state in Redis; it can't run without it. Disclosed: Tyk gets extra CPU the others don't use. |
| `GOMAXPROCS` | profile cores | Go scheduler threads = pinned cores. |
| `enable_analytics` | false | No analytics pipeline; same "no request logging" rule (spec §8). |
| `log_level` | warn | Same for every gateway. |
| `max_idle_connections_per_host`, `close_connections: false` | 512 / keep-alive | Tyk docs "Performance tuning": upstream keep-alive. |
| Rate limiter | non-transactional (DRL) | Tyk docs recommend it for performance; API-level `global_rate_limit`. |
| `enable_jsvm` | only in the script config | Tyk docs: disable JSVM unless used. |
| keys | created via `/tyk/keys` before probes (API keys can't be file-provisioned) | Same credential as every gateway. |
| TLS | TLS 1.3 (`min_version: 772`), HTTP/2 enabled | Same cert/protocol for every gateway. |
