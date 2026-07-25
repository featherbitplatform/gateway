---
title: TLS, HTTP/2 & WebSocket
description: Terminate TLS on the data-plane and admin listeners, serve HTTP/2 to clients and upstreams, and proxy WebSocket connections.
---

# TLS, HTTP/2 & WebSocket

featherbit can terminate TLS on its listeners, speak HTTP/2, and proxy WebSocket connections. TLS and HTTP/2 are configured in `system.yaml`; plain HTTP/1.1 remains the default when you configure neither. WebSocket proxying needs no configuration beyond a normal proxy route.

## TLS termination

Add a `tls` block to the listener to serve HTTPS:

```yaml
listener:
  bind: 0.0.0.0
  port: 8443

tls:
  cert_path: /etc/gateway/tls/cert.pem   # PEM certificate chain
  key_path: /etc/gateway/tls/key.pem     # PEM private key (PKCS#8, PKCS#1, or SEC1)
  min_version: "1.2"                      # "1.2" (default) or "1.3"
```

- TLS uses **rustls** with the **ring** crypto provider.
- The cert/key are loaded **once at startup**. A missing/invalid cert or key, or an unsupported `min_version`, aborts startup with a clear error (fail-fast) rather than failing every handshake at runtime.
- Per-connection TLS handshake failures are logged and dropped — one bad client never affects the accept loop.
- `min_version: "1.2"` allows TLS 1.2 and 1.3; `"1.3"` allows 1.3 only.

Generate a self-signed pair for local testing:

```bash
openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem \
  -days 365 -subj "/CN=localhost"
```

### Multiple certificates by SNI

To front several domains on one listener, add `sni_certs` — each maps an SNI hostname to its own certificate. `cert_path`/`key_path` is the default/fallback for hostnames that match no entry (or connections with no SNI):

```yaml
tls:
  cert_path: /etc/gateway/tls/default.crt   # fallback cert
  key_path: /etc/gateway/tls/default.key
  sni_certs:
    - server_name: api.example.com          # exact
      cert_path: /etc/gateway/tls/api.crt
      key_path: /etc/gateway/tls/api.key
    - server_name: "*.tenant.example.com"    # single-label wildcard
      cert_path: /etc/gateway/tls/tenant.crt
      key_path: /etc/gateway/tls/tenant.key
```

- Matching is case-insensitive; `server_name` is exact or a single-label wildcard (`*.example.com` matches `a.example.com`, not `example.com` or `a.b.example.com`). First match wins, else the default.
- `min_version`, ALPN (HTTP/2), mTLS, and hot-reload are **listener-wide** and apply to every certificate — rotating any `sni_certs` file hot-reloads it just like the default cert.

```bash
openssl s_client -connect localhost:8443 -servername api.example.com </dev/null \
  | openssl x509 -noout -subject      # shows the api.example.com cert
```

### Mutual TLS (client certificates)

Require clients to present a certificate signed by a trusted CA (mTLS) by adding `client_ca_path` to the `tls` block:

```yaml
tls:
  cert_path: /etc/gateway/tls/cert.pem
  key_path: /etc/gateway/tls/key.pem
  client_ca_path: /etc/gateway/tls/client-ca.pem   # CA bundle to verify client certs
  client_cert_required: true                        # default; false = optional
```

- With `client_cert_required: true` (default), a client that presents no cert — or one not signed by `client_ca_path` — is **rejected at the handshake**. Set it to `false` to make the cert optional (anonymous clients allowed; a presented cert is still validated).
- The verified client's identity is exposed to the request pipeline as reserved message keys, so a Lua `script` node, a logger, or a policy can authorize/pin/allowlist clients or record who called:
  - `__client_cert_fingerprint` — SHA-256 fingerprint (lowercase hex), always present for an authenticated client.
  - `__client_cert_subject_cn` — the subject Common Name (if the cert has one).
  - `__client_cert_san_dns` — array of the Subject Alternative Name DNS entries.
  For example, a `script` node can `return ctx.message.__client_cert_subject_cn == "orders-service"` to allow only that service.
- mTLS also applies to the **Admin API** when set under `admin.tls.client_ca_path` (defence-in-depth on top of Basic Auth); identity exposure is data-plane only.

```bash
# Client presenting a cert (accepted); without --cert it is rejected when required.
curl --cert client.pem --key client.key -k https://localhost:8443/
```

Certificate revocation (CRL/OCSP) is not yet implemented.

### Certificate hot-reload

Certificates are **hot-reloaded** — no configuration or restart needed. Both the data-plane and Admin listeners watch their cert/key files (and their parent directory, so Kubernetes secret symlink swaps and cert-manager/Let's Encrypt renewals are picked up). When the files change, the new certificate is served on **new** connections within ~1 s; in-flight connections are unaffected. A bad or half-written cert during rotation is logged and the current certificate is kept — TLS is never dropped mid-rotation.

See [`examples/system-tls.yaml`](https://github.com/) for a complete example.

## HTTP/2

```yaml
http2:
  enabled: true   # default
```

When enabled, the listener serves HTTP/2 **alongside** HTTP/1.1, negotiated per connection:

- **Over TLS** — ALPN advertises `h2` then `http/1.1`; a client that supports HTTP/2 gets it, others fall back to HTTP/1.1.
- **Over plaintext** — HTTP/2 cleartext (h2c) via prior knowledge is accepted, as is HTTP/1.1. The connection's first bytes are sniffed to pick the protocol.

Set `http2.enabled: false` to serve HTTP/1.1 only (ALPN then advertises `http/1.1` alone).

### HTTP/2 to upstreams

The shared outbound client (used by the `upstream` node and callout plugins) advertises `h2` and `http/1.1` via ALPN. TLS upstreams that support HTTP/2 are called over h2; plain-`http://` upstreams stay HTTP/1.1. No configuration is required.

## Admin API over TLS

The Admin API/UI listener can be TLS-terminated too, reusing the same `TlsConfig`:

```yaml
admin:
  bind: 0.0.0.0
  port: 9090
  username: ${ADMIN_USER}
  password: ${ADMIN_PASSWORD}
  tls:
    cert_path: /etc/gateway/tls/cert.pem
    key_path: /etc/gateway/tls/key.pem
```

## Verifying

```bash
# HTTP/2 over TLS (expect: ALPN server accepted h2, HTTP/2 200)
curl -vk --http2 https://localhost:8443/

# Force HTTP/1.1 (falls back cleanly)
curl -vk --http1.1 https://localhost:8443/

# Inspect the negotiated ALPN + protocol version
openssl s_client -connect localhost:8443 -alpn h2,http/1.1 </dev/null

# min_version enforcement: a TLS 1.1 handshake against min_version "1.2" is rejected
openssl s_client -connect localhost:8443 -tls1_1 </dev/null

# h2c prior-knowledge over plaintext (when tls is not set)
curl -v --http2-prior-knowledge http://localhost:8080/
```

## WebSocket proxying

featherbit proxies WebSocket connections transparently — no special configuration is needed beyond a normal proxy route (`listener → upstream → client`). When a client sends a WebSocket upgrade (`Connection: Upgrade`, `Upgrade: websocket`):

1. The **policy graph still runs**, so access-phase plugins apply — authentication, CORS, rate limiting, and `proxy-rewrite` path munging all take effect on the handshake request. A plugin that rejects the request (e.g. a 401 from an auth node) cleanly prevents the upgrade.
2. The `upstream` node resolves the backend target (using the same load-balancing strategy as HTTP) and signals a `101`.
3. The gateway opens the WebSocket handshake to the upstream, echoes the upstream's `Sec-WebSocket-Accept` back to the client, and then **relays bytes bidirectionally** between client and upstream until either side closes.

```yaml
# gateway.yaml — a WebSocket route is just a proxy route
routes:
  - name: chat
    match: { path: /ws }
    policy: ws-proxy
policies:
  - name: ws-proxy
    nodes:
      - { id: listener, type: listener }
      - { id: backend, type: upstream, config: { targets: [ { host: chat-backend, port: 8080 } ] } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: backend.in }
      - { from: backend.success, to: client.in }
```

**Client-facing `wss://`** works automatically — TLS is terminated at the listener (see above), and the WebSocket is proxied over the decrypted connection.

### TLS to the upstream (`wss://`)

To reach a **TLS** WebSocket backend, set `tls: true` on the `upstream` node (add `ssl_verify: false` to accept a self-signed backend cert):

```yaml
- { id: backend, type: upstream, config: { targets: [ { host: chat-backend, port: 8443 } ], tls: true } }
```

The same `tls` / `ssl_verify` options apply to the buffered HTTP path too (`https://` upstream). See the [`upstream` node reference](../reference/plugins/upstream.md).

Notes and limits:

- The relay is a **transparent byte pump** — the gateway does not parse WebSocket frames, so per-message body-transform/logging plugins do not apply after the upgrade (only access-phase plugins, on the handshake, do).
- The upstream leg is plaintext `ws://` by default, or `wss://` when the `upstream` node sets `tls`. Cert verification uses the system's native root store unless `ssl_verify: false`.
- If the upstream handshake fails (unreachable, TLS error, or it rejects the upgrade), the client receives a `502` and the handshake does not complete.

### HTTP/2 WebSockets (RFC 8441)

Clients on an HTTP/2 connection can open WebSockets via the **extended CONNECT** mechanism (RFC 8441) — no configuration needed. The gateway advertises `SETTINGS_ENABLE_CONNECT_PROTOCOL`, accepts a `CONNECT` request carrying `:protocol = websocket`, and bridges it to the (HTTP/1.1) upstream. This works over both `wss://` (h2 negotiated by ALPN over TLS) and cleartext h2c.

- The **upstream leg stays HTTP/1.1** — the gateway synthesizes the `Sec-WebSocket-Key`/`-Version` the h2 client doesn't send. Upstream-side RFC 8441 (h2 WebSocket to the backend) is not implemented.
- An h2 WebSocket request arrives with method `CONNECT` (not `GET`), so a route `match` that constrains `methods` to `GET` will not match it; match on **path** instead. The authority is carried in `:authority` rather than a `Host` header, so host-based route matching may not apply.

Verify with [`websocat`](https://github.com/vi/websocat):

```bash
websocat ws://127.0.0.1:8080/ws          # against a ws:// backend
websocat wss://127.0.0.1:8443/ws -k       # against a TLS listener (self-signed)
```

## Not covered (yet)

- mTLS / client-certificate authentication (the server requests no client cert).
- SNI-based multi-certificate selection (a single cert per listener).
- Certificate hot-reload (a cert change needs a restart).
- OCSP stapling / GM (SM2) — the parked `ocsp-stapling` and `gm` plugins.
- `wss://` to the upstream, HTTP/2 WebSockets (RFC 8441), and L4 (TCP/UDP) stream proxying — separate roadmap items.
