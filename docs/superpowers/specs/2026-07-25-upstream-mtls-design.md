# Design: mTLS to upstreams

**Date:** 2026-07-25
**Branch:** `feature/mtls`
**Status:** approved

## Problem

The gateway terminates mTLS from clients (`client_ca_path` on `TlsConfig`) but cannot
present a client certificate when connecting to TLS upstreams. Backends that require
mutual TLS are unreachable, and backends on a private PKI can only be reached with
`ssl_verify: false`, which disables verification entirely.

## Scope

- **In:** client cert/key per `upstream` node, optional per-upstream CA bundle,
  applied to HTTPS proxying and `wss` WebSocket relays. Certs load at policy-compile
  time.
- **Out (follow-ups):** callout plugins (forward-auth, OPA, loggers), cert-file
  hot-reload/watching, CRL/OCSP for upstream certs, L4 stream proxy (passthrough
  never terminates TLS).

## Decisions

| Decision | Choice |
|---|---|
| Granularity | Per-`upstream`-node config keys |
| Private CA support | Yes, `ca_cert_path`, replaces native roots for that upstream |
| Connection types | HTTPS proxying + wss relays |
| Cert loading | At plugin construction (policy compile); rotation = touch `gateway.yaml` or restart |
| Pooling | Identity-keyed client cache shared in `src/outbound` (approach A) |

## Config surface

New optional `upstream` node keys, meaningful only with `tls: true`:

```yaml
type: upstream
config:
  tls: true
  client_cert_path: /etc/featherbit/certs/gateway-client.crt
  client_key_path: /etc/featherbit/certs/gateway-client.key
  ca_cert_path: /etc/featherbit/certs/private-ca.crt   # optional
```

Validation rules (all enforced at plugin construction, failing the policy compile):

1. `client_cert_path` and `client_key_path` must appear together.
2. `ca_cert_path` may be used alone (private-CA verification, no client cert).
3. `ca_cert_path` **replaces** the native root store for that upstream.
4. `ca_cert_path` + `ssl_verify: false` is a config error (contradictory intent).
5. `ssl_verify: false` + client cert is allowed: cert presented, verification skipped.
6. Files are read, PEM-parsed, and a rustls `ClientConfig` is dry-built at
   construction, so unreadable files and cert/key mismatches fail early.

As everywhere in the YAML config, values support `${ENV_VAR:-default}` interpolation.

## Architecture

New file `src/outbound/tls.rs`:

- `UpstreamTls::load(cert_path, key_path, ca_path, verify) -> Result<Arc<UpstreamTls>, String>`
  reads and parses the PEM files, dry-builds a `ClientConfig` for validation, and
  computes a stable **content-hash key** (hash of PEM bytes + verify flag). Rotated
  cert contents hash to a new key and therefore a fresh pool.
- Identity-keyed caches, never evicted (bounded by the number of distinct identities;
  documented):
  - `HashMap<u64, PooledClient>` inside `OutboundClient` for buffered HTTP proxying.
  - `HashMap<u64, TlsConnector>` for wss.
  - A process-wide registry `HashMap<u64, Arc<UpstreamTls>>` so non-plugin code
    (the WebSocket relay in `src/server/websocket.rs`) can resolve an identity
    from its key.

Changes to existing code:

- `OutboundRequest` gains `tls: Option<Arc<UpstreamTls>>`. `None` preserves today's
  behavior exactly (verified/insecure client pair chosen by `ssl_verify`). `Some`
  routes through the identity cache; the hyper client is built on first use with
  ALPN h1+h2, matching the default client.
- `client_tls_connector` gains an `Option<Arc<UpstreamTls>>` parameter.
- `UpstreamPlugin::from_config` parses the new keys, calls `UpstreamTls::load`,
  registers the identity in the registry, and stores the `Arc`.
- For WebSocket intent, the upstream node adds `__ws_upstream_tls_key: <hash>` to
  the context message beside the existing `__ws_upstream_tls` /
  `__ws_upstream_ssl_verify`; `websocket.rs` resolves it via the registry.

## Data flow

1. Policy compiles → `UpstreamTls::load` validates files → identity registered.
2. HTTP request → upstream node builds `OutboundRequest { tls: Some(identity), .. }`
   → `OutboundClient::request` fetches/builds the pooled client for that identity →
   TLS handshake presents the client cert → response flows back as today.
3. wss request → node signals `101` + `__ws_upstream_tls_key` → listener calls
   `proxy_upgrade` → connector resolved from registry → relay as today.

## Error handling

- **Compile time:** validation failures return `Err(String)` from `from_config`,
  surfacing through the existing policy-compile failure path (admin API rejects the
  policy with the message).
- **Runtime:** handshake failures (backend rejects our cert, unknown CA, name
  mismatch) surface as `OutboundError::Transport` → routed through the upstream
  node's **error port**, like existing connect failures. wss failures map to the
  existing `WsError::Tls`.

## Testing

- **Unit:** config pairing/contradiction rules; `UpstreamTls::load` happy/sad paths
  using the cert-generation helpers from `src/server/tls.rs` tests; content-hash
  stability (same bytes → same key, different bytes/flags → different key).
- **Integration:** in-process rustls server with `client_auth` required —
  correct cert → 200; missing cert / wrong CA → error port. Private-CA-only case
  (no client cert) verifies against `ca_cert_path`.
- **wss:** connector construction with identity resolved from the registry.

## Documentation

- Upstream plugin reference page (`website/docs/reference/plugins/`).
- `docs/apisix-parity.md`: maps to APISIX `upstream.tls.client_cert`/`client_key`.
- `CLAUDE.md`: mention upstream mTLS (client cert + private CA per upstream node) in
  the transport paragraph.
