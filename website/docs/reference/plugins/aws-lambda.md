---
title: aws-lambda
description: Invoke an AWS Lambda function (via its function URL or API endpoint) with optional AWS SigV4 signing, and return its reply as the gateway response.
---

<span className="plugin-chip" style={{'--chip-color': '#ff9900'}}>aws-lambda</span>

Invokes an AWS Lambda function through its function URL (or an API Gateway endpoint) and returns the function's status, headers, and body as the gateway response. It **replaces the upstream**: place it where an `upstream` node would go and wire its `success` port straight to `client.in`.

Two authorization modes are supported. With `authorization.apikey` the key is sent as the `x-api-key` header (no signing). With `authorization.iam` the request is signed with **AWS Signature Version 4** (`AWS4-HMAC-SHA256`).

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `function_uri` | string | **required** | Lambda function URL or API endpoint to invoke. |
| `authorization` | object | — | One of `apikey` or `iam` (see below). Omit for an unauthenticated call. |
| `authorization.apikey` | string | — | Sent as the `x-api-key` header (no signing). |
| `authorization.iam.accesskey` | string | **required** (with `iam`) | AWS access key id. |
| `authorization.iam.secretkey` | string | **required** (with `iam`) | AWS secret access key. |
| `authorization.iam.aws_region` | string | `us-east-1` | Signing region. |
| `authorization.iam.aws_service` | string | `lambda` | Signing service name. |
| `authorization.iam.session_token` | string | — | Optional STS token, added as `x-amz-security-token` and covered by the signature. |
| `ssl_verify` | bool | `false` | Verify TLS certificates on the callout. |
| `timeout` | integer (ms) | `3000` | Whole-call deadline (connect + request + response body). |

```yaml
type: aws-lambda
config:
  function_uri: https://xyz.lambda-url.us-east-1.on.aws/
  authorization:
    iam:
      accesskey: AKIDEXAMPLE
      secretkey: ${AWS_SECRET_KEY}
      aws_region: us-east-1
      aws_service: lambda
  ssl_verify: false
  timeout: 3000
```

Config load fails if `function_uri` is missing/empty, or if an `iam` block is present without `accesskey`/`secretkey`.

## Behavior

The plugin forwards the client's method, headers, query string, and body to `function_uri`, overriding `Host` with the endpoint's authority. For `iam` auth it computes an AWS SigV4 signature and adds the `x-amz-date`, optional `x-amz-security-token`, and `Authorization` headers. On success it populates `context.response` with the function's status, headers, and body and exits through the `success` port — which should be wired to `client.in`, since this node stands in for the upstream. The function's status is passed through as-is.

A callout failure returns the Context along with an error so the graph engine routes through the `error` port; the error is appended to `context.errors`:

| Code | Status | When |
|---|---|---|
| `AWS_LAMBDA_CALLOUT_ERROR` | 504 | The callout exceeded `timeout`. |
| `AWS_LAMBDA_CALLOUT_ERROR` | 503 | Connecting to or exchanging with the endpoint failed. |
| `AWS_LAMBDA_CALLOUT_ERROR` | 502 | The request could not be built (e.g. invalid `function_uri`). |

## SigV4 signing

The signature covers a minimal, gateway-controlled header set: `host`, `x-amz-date`, and `x-amz-security-token` (when a session token is set). AWS permits signing a subset of headers, so the remaining forwarded headers are sent unsigned. The signing routine is verified against the published AWS SigV4 `get-vanilla` test vector.

The plugin does not read or write `context.message`.
