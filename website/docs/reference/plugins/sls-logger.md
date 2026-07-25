---
title: sls-logger
description: Batches access-log entries and ships them to Alibaba Cloud SLS via the signed PutLogs REST API.
---

<span className="plugin-chip" style={{'--chip-color': '#f97316'}}>sls-logger</span>

Builds one access-log entry per request and hands it to a fire-and-forget batching sink; a background task POSTs batches to Alibaba Cloud Simple Log Service (SLS) using its signed `PutLogs` REST API. Place it in the response pipeline, after the `upstream` node. The node passes the context through unchanged and never fails.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | — | SLS endpoint, e.g. `cn-hangzhou.log.aliyuncs.com`. **Required**. |
| `port` | integer | — | Endpoint port, e.g. `443`. **Required**. |
| `project` | string | — | SLS project; prefixed to `host` as the request authority `<project>.<host>`. **Required**. |
| `logstore` | string | — | Destination logstore. **Required**. |
| `access_key_id` | string | — | RAM access-key id. **Required**. |
| `access_key_secret` | string | — | RAM access-key secret (used to sign each request). **Required**. |
| `timeout` | integer (ms) | `5000` | Per-flush HTTP deadline. |
| `include_req_body` | bool | `false` | Include the request body in the default entry. |
| `include_resp_body` | bool | `false` | Include the response body in the default entry. |
| `log_format` | object | — | Custom flat entry of `name -> "$var template"`. |
| `batch_max_size` | integer | `1000` | Entries per batch before an immediate flush. |
| `inactive_timeout` | integer (s) | `5` | Flush after this much idle time. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed flush. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped (with a warning) when full. |

```yaml
- id: sls-log
  type: sls-logger
  config:
    host: cn-hangzhou.log.aliyuncs.com
    port: 443
    project: my-project
    logstore: gateway
    access_key_id: ${SLS_KEY_ID}
    access_key_secret: ${SLS_KEY_SECRET}
```

## Behavior

Each flush POSTs `https://<project>.<host>:<port>/logstores/<logstore>/shards/lb` with a JSON body `{"__topic__","__source__","__logs__":[…]}`. SLS requires string log values, so non-string entry fields are JSON-encoded. The request is signed per the SLS spec: `Content-MD5`, `Date`, the sorted `x-log-*` canonical headers, and the canonical resource are HMAC-SHA1 signed with the access-key secret, base64-encoded, and sent as `Authorization: LOG <access_key_id>:<signature>`. A non-`200` response fails the batch, which is retried per the batch settings.

## Behavior notes

featherbit's shared logging infrastructure is HTTP-only — there is no raw TLS/TCP syslog delivery path to the SLS syslog ingress. This node targets the documented SLS [`PutLogs` REST API](https://www.alibabacloud.com/help/en/sls/developer-reference/api-putlogs):

- The batch is POSTed as JSON to `/logstores/<logstore>/shards/lb`.
- Requests are signed with the SLS HMAC-SHA1 `Authorization: LOG <id>:<signature>` scheme (with a self-contained MD5 for `Content-MD5`).

The signing helper and its primitives (MD5, HMAC-SHA1) are unit-tested against fixed vectors.
