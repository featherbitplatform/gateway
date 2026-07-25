---
title: attach-consumer-label
description: Copy the attached consumer's labels into upstream request headers so backends can see who is calling.
---

<span className="plugin-chip" style={{'--chip-color': '#eab308'}}>attach-consumer-label</span>

Copies the attached consumer's labels into upstream request headers so backends can identify the caller without re-reading credentials. It does **not** authenticate — an upstream auth node (e.g. `key-auth`) must have attached the consumer identity first, writing `consumer.labels` into `context.message`. Each label `k=v` becomes a request header `<header_prefix><k>: v`. Place it after auth and before the upstream node.

## Configuration

The constructor never fails.

| Key | Type | Default | Description |
|---|---|---|---|
| `header_prefix` | string | `X-Consumer-` | Prefix prepended to each label key to form the upstream header name. The combined name is lowercased, so a `tier` label with the default prefix becomes the header `x-consumer-tier`. |

```yaml
type: attach-consumer-label
config:
  header_prefix: "X-Consumer-"
```

## Behavior

- **With a consumer attached** — for every entry in `consumer.labels`, the request header `<header_prefix><label_key>` (lowercased) is set to the label value, overwriting any client-supplied copy.
- **With no consumer attached** — the request passes through untouched.

The plugin always routes through the `success` port; it never rejects and never writes to `context.response`.

:::note Behavior notes
The plugin takes a single `header_prefix` and copies *all* of the consumer's labels — there is no per-label header mapping. This matches featherbit's consumer model, where labels are a flat string→string map.
:::
