---
title: google-cloud-logging
description: Ships access-log entries to Google Cloud Logging in batches over the entries:write API.
---

<span className="plugin-chip" style={{'--chip-color': '#4285F4'}}>google-cloud-logging</span>

Ships a structured access-log entry for each request to [Google Cloud Logging](https://cloud.google.com/logging). Entries are buffered by a batch processor and delivered in the background to `https://logging.googleapis.com/v2/entries:write`, authenticated with a short-lived OAuth2 access token minted from the service account's private key. The node passes the context through unchanged, so place it in the response pipeline after the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `auth_config` | object | — | Inline service account. Requires `client_email`, `private_key`, `project_id`; optional `token_uri` (default `https://oauth2.googleapis.com/token`) and `scopes` (array). |
| `auth_file` | string | — | Path to a service-account JSON file, used when `auth_config` is absent. |
| `resource` | object | `{"type":"global"}` | [MonitoredResource](https://cloud.google.com/logging/docs/reference/v2/rest/v2/MonitoredResource) attached to each entry. |
| `log_id` | string | `featherbit%2Flogs` | Log id; the entry's `logName` becomes `projects/<project_id>/logs/<log_id>`. |
| `ssl_verify` | bool | `true` | Verify Google's TLS certificate. |
| `timeout` | int (seconds) | `10` | Per-call HTTP timeout (token fetch and write). |
| `log_format` | object | — | Custom `name -> "$var template"` map. When set, its interpolated result is the `jsonPayload` instead of the default entry. |
| `include_req_body` / `include_resp_body` | bool | `false` | Include the (lossy UTF-8) request/response body in the default entry. |

Either `auth_config` (with all three required fields) or `auth_file` must be provided, or config load fails. Batch-processor keys (`batch_max_size`, `inactive_timeout`, `buffer_duration`, `max_retry_count`, `retry_delay`, `max_pending_entries`) are also accepted.

```yaml
- id: gcp-log
  type: google-cloud-logging
  config:
    auth_config:
      client_email: logger@my-project.iam.gserviceaccount.com
      private_key: ${GCP_PRIVATE_KEY}
      project_id: my-project
    resource:
      type: global
    log_id: featherbit%2Flogs
    batch_max_size: 100
```

## Behavior

For each request the node builds a log entry (the shared default entry, or a `log_format` custom entry) and pushes it to the batch sink. A background task, on each flush:

1. Ensures a valid OAuth2 access token, minting one when absent or near expiry: it RS256-signs a JWT (`iss` = `client_email`, `scope` = space-joined scopes, `aud` = `token_uri`, 1h lifetime) with the service account `private_key`, POSTs it to `token_uri` as a `urn:ietf:params:oauth:grant-type:jwt-bearer` grant, and caches the returned token until ~60s before its `expires_in`.
2. Wraps each buffered entry into a Cloud Logging `LogEntry` (`logName`, `resource`, `jsonPayload`, RFC3339 `timestamp`, and a `labels.source` of `featherbit-google-cloud-logging`).
3. POSTs `{ "entries": [...], "partialSuccess": false }` with `Authorization: Bearer <token>`.

The node is a pure passthrough: it never modifies the context and only its **success** port is taken. Delivery is best-effort — a full queue drops entries with a warning and failed flushes are retried per the batch config.

## Behavior notes

- The shared log entry is used as the `jsonPayload`; per-entry `httpRequest`/`insertId` fields are not derived and a `log_format_extra` map is not supported — shape the payload with `log_format`.
- `log_id` defaults to `featherbit%2Flogs`; `resource` defaults to `{"type":"global"}`.
- The OAuth token flow is implemented natively (jsonwebtoken RS256 + the JWT-bearer grant) with an in-process token cache.
