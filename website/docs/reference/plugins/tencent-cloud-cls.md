---
title: tencent-cloud-cls
description: Batches access-log entries and ships them to Tencent Cloud CLS via the signed structured-log API.
---

<span className="plugin-chip" style={{'--chip-color': '#3b82f6'}}>tencent-cloud-cls</span>

Builds one access-log entry per request and hands it to a fire-and-forget batching sink; a background task POSTs batches to Tencent Cloud Log Service (CLS) at its `/structuredlog` upload endpoint, signed with the CLS/COS-style `q-sign-algorithm=sha1` `Authorization` header. Place it in the response pipeline, after the `upstream` node. The node passes the context through unchanged and never fails.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `cls_host` | string | — | CLS upload host, e.g. `ap-guangzhou.cls.tencentcs.com`. Alias `endpoint`. **Required**. |
| `cls_topic` | string | — | Destination topic id, sent as the `topic_id` query parameter. Alias `topic_id`. **Required**. |
| `secret_id` | string | — | API secret id. **Required**. |
| `secret_key` | string | — | API secret key (used to sign each request). **Required**. |
| `scheme` | string | `https` | `http` or `https`. |
| `ssl_verify` | bool | `true` | Verify TLS certificates. |
| `timeout` | integer (ms) | `10000` | Per-flush HTTP deadline. |
| `global_tag` | object | — | Fields merged into every entry before batching. |
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
- id: cls-log
  type: tencent-cloud-cls
  config:
    cls_host: ap-guangzhou.cls.tencentcs.com
    cls_topic: xxxxxxxx-xxxx-xxxx
    secret_id: ${CLS_SECRET_ID}
    secret_key: ${CLS_SECRET_KEY}
    scheme: https
```

## Behavior

Each flush POSTs `<scheme>://<cls_host>/structuredlog?topic_id=<cls_topic>`. The signature follows the CLS scheme: `sign_key = hex(hmac_sha1(secret_key, sign_time))`, then `signature = hex(hmac_sha1(sign_key, string_to_sign))`, where `string_to_sign = "sha1\n<sign_time>\n<sha1(http_request_info)>\n"` and `http_request_info = "post\n/structuredlog\n\n\n"`. Each entry is normalized into a list of `{key, value}` `contents` (non-string values JSON-encoded), grouped into one `LogGroup`. Response statuses `413`/`404`/`401`/`403` are non-retryable (the batch is dropped); other non-`200` responses fail the batch and are retried per the batch settings.

## Limitations

- **JSON body, not protobuf.** The CLS SDK serializes the `LogGroupList` with protobuf and sends `application/x-protobuf`. featherbit has no protobuf codec, so it sends the equivalent structured-log payload as JSON (`application/json`). The signature, endpoint, topic query parameter, and log normalization are otherwise faithful. Against a live CLS endpoint the protobuf content type would be required; this is a documented subset.
- **`source` omitted.** The SDK sets each `LogGroup.source` to the host IP; featherbit does not resolve its own IP and leaves it empty.

The signing helper and its primitives (SHA-1, HMAC-SHA1) are unit-tested against fixed vectors.
