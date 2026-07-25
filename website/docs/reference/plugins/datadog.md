---
title: datadog
description: Ships request metrics to a Datadog agent over DogStatsD (UDP) in batches.
---

<span className="plugin-chip" style={{'--chip-color': '#7c3aed'}}>datadog</span>

Emits DogStatsD metrics (`request.counter`, `request.latency`, `ingress.size`, `egress.size`) for each request and ships accumulated batches to a Datadog agent over UDP. Place this node in the response pipeline, **after the upstream node**, so latency and status are captured.

:::note Deviation from other loggers
Unlike the HTTP loggers, `datadog` does **not** ship JSON logs to an HTTP endpoint. It renders each buffered entry into DogStatsD metric lines and sends them as UDP datagrams via a `tokio::net::UdpSocket` (not the shared outbound HTTP client). Agent settings (`host`, `port`, `namespace`, `constant_tags`) live directly in this node's config. Because featherbit's shared log entry carries no route/service identifiers, `prefer_name` is accepted but has no effect and no `route_name`/`service_name` tags are emitted.
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `127.0.0.1` | Datadog agent address. |
| `port` | integer | `8125` | DogStatsD UDP port. |
| `namespace` | string | `featherbit` | Metric name prefix. |
| `constant_tags` | array | `[]` | Tags added to every metric. |
| `include_path` | bool | `false` | Add a `path:` tag. |
| `include_method` | bool | `false` | Add a `method:` tag. |
| `prefer_name` | bool | `true` | Accepted for config compatibility; no effect. |
| `batch_max_size` | integer | `1000` | Flush when the buffer reaches this many entries. |
| `inactive_timeout` | integer (s) | `5` | Flush after this idle period. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed flush before dropping the batch. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped with a warning when full. |

```yaml
- id: metrics
  type: datadog
  config:
    host: 127.0.0.1
    port: 8125
    namespace: featherbit
    constant_tags: [source:featherbit]
    include_method: true
```

## Behavior

The node is a pure passthrough: it never modifies the context and never fails, so only its **success** port is ever taken. `push` is fire-and-forget and never blocks the request path — when the queue is full, entries are dropped with a `tracing::warn!`. Each entry yields metric lines of the form `namespace.metric:value|type|#tags`; the entry's metrics are coalesced into one datagram (or split when they exceed the 8192-byte DogStatsD buffer). Sending runs on a background task; a mid-batch send failure retries only the undelivered tail.
