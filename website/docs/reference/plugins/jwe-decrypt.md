---
title: jwe-decrypt
description: Decrypts a JWE (dir + A256GCM) token from a request header and forwards the plaintext to another header.
---

<span className="plugin-chip" style={{'--chip-color': '#8b5cf6'}}>jwe-decrypt</span>

Reads a [JWE](https://datatracker.ietf.org/doc/html/rfc7516)-encrypted token from a request header, decrypts it with a symmetric key, and forwards the decrypted plaintext into another request header before the request is proxied upstream. Place it early in the request pipeline, before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `header` | string | `Authorization` | Header carrying the encrypted token; matched case-insensitively. An optional `Bearer ` prefix is stripped. |
| `forward_header` | string | `Authorization` | Header the decrypted plaintext is written to before proxying. |
| `strict` | boolean | `true` | When `true`, a request with no token is rejected. When `false`, a request with no token passes through unchanged. |
| `key` | string | — | Inline symmetric key, base64-encoded, that must decode to exactly 32 bytes (AES-256). Used for every request. |
| `use_consumers` | boolean | `false` | Resolve the key per request from the gateway `consumers:` section. The token's `kid` (in its protected header) selects the consumer whose `jwe-decrypt` credential is `{key: <kid>, secret: <32-byte key>, is_base64_encoded: <bool>}`. |
| `alg` | string | `dir` | Key-management algorithm. Only `dir` is supported; any other value is a **config error**. |
| `enc` | string | `A256GCM` | Content-encryption algorithm. Only `A256GCM` is supported; any other value is a **config error**. |

At least one of `key` / `use_consumers` must be provided.

```yaml
- id: jwe
  type: jwe-decrypt
  config:
    header: Authorization
    forward_header: Authorization
    strict: true
    use_consumers: true
```

With a consumer store:

```yaml
consumers:
  - name: alice
    credentials:
      jwe-decrypt:
        key: alice-kid          # matched against the token's `kid`
        secret: "0123456789abcdef0123456789abcdef"  # 32 bytes
        is_base64_encoded: false
```

## Behavior

The token is read from `header` (stripping a `Bearer ` prefix), parsed as a 5-part JWE compact serialization (`protected.encrypted_key.iv.ciphertext.tag`), and its protected header is checked for `alg: dir` / `enc: A256GCM`. The 32-byte AES key comes from the inline `key` or, with `use_consumers`, from the consumer selected by the token's `kid`. The ciphertext is decrypted with AES-256-GCM (the protected header being the AEAD additional-authenticated-data, per RFC 7516).

On success the decrypted plaintext replaces the value of `forward_header` and the request continues through the **success** port. On any failure the request is rejected and routed through the **error** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`
- Error code appended to `context.errors`: `JWE_INVALID`

Failure covers a missing token (when `strict`), a malformed compact serialization, an unsupported `alg`/`enc` in the token, an unknown or missing `kid`, and any failed AEAD authentication (tampered ciphertext, wrong key).

## Limitations

The plugin implements the `dir` (direct) key-management algorithm with `A256GCM` content encryption, and nothing else:

- **Only `dir` + `A256GCM` is supported.** There is no RSA-OAEP, ECDH-ES, key-wrap, or alternative content cipher. A node whose config sets `alg`/`enc` to anything else **fails at config load**; a *token* that declares another algorithm is rejected at request time with `JWE_INVALID`.
- The `encrypted_key` segment of the compact serialization must be empty (as it always is for `dir`). A non-empty segment is rejected.
- The consumer `secret` must be a 32-byte AES-256 key — either 32 raw bytes (`is_base64_encoded: false`) or base64url that decodes to 32 bytes (`is_base64_encoded: true`).
- **All rejections return `401`.** A single `401` with the `JWE_INVALID` code is used for every rejection path, consistent with featherbit's other auth plugins.
