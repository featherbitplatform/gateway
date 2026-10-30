# Session timeouts: idle, rolling and absolute, renewed on use

**Status:** requested in discussion, 2026-10-30
**Branch:** `feature/session-timeouts`

## 1. Intent

An `openid-connect` user who keeps navigating is logged out exactly `session.cookie.lifetime` seconds after login (one hour by default), active or not. Nothing extends a session on use. APISIX does not behave that way: its `openid-connect` plugin hands sessions to lua-resty-session, which keeps a session alive while it is being used, kills it after a period of inactivity, and caps its total life. An operator migrating from APISIX, or anyone comparing the two gateways, expects the same.

The gateway's five interactive session plugins (`openid-connect`, `cas-auth`, `authz-casdoor`, `dingtalk-auth`, `feishu-auth`) share one session layer (`src/plugins/util/server_session.rs`), so the fix belongs there and all five get it.

Verified against the upstream sources on 2026-10-30: APISIX tag `3.17.0` (`apisix/plugins/openid-connect.lua`), lua-resty-openidc 1.9.3, lua-resty-session 4.2.0.

## 2. What APISIX does (the target)

lua-resty-session checks three timers every time it opens a session, and renews on use:

| Timer | Default | Session is invalid when |
|---|---|---|
| `idling_timeout` | 900 s (15 min) | no request since the last touch for longer than this |
| `rolling_timeout` | 3600 s (1 h) | the session has not been re-saved for longer than this |
| `absolute_timeout` | 86400 s (24 h) | this long has passed since login, regardless of activity |

A timer set to `0` is disabled.

Renewal happens inside `session.start()`, which APISIX calls on every request:

- when more than 75% of `rolling_timeout` has elapsed since the last save, the session is re-saved with a new rolling offset (a full re-seal, new cookie);
- otherwise, when more than `touch_threshold` (60 s, fixed) has elapsed since the last touch, only the idling offset is updated (new cookie header);
- both paths emit a `Set-Cookie` on the response.

The cookie's `Max-Age` and the server-side record TTL are `min(rolling_timeout, absolute_timeout - elapsed_since_creation)`, ignoring any timer that is `0`.

All three offsets live inside the cookie and are authenticated, so cookie (stateless) mode enforces them without server state.

APISIX's old `session.cookie.lifetime` is deprecated and maps to `absolute_timeout` when that is unset. The gateway treats the same field as the entire session length. The two readings diverge: an APISIX user with `lifetime: 3600` gets "24 h max, re-login after 15 min idle, re-saved hourly" semantics replaced by "1 h max, 15 min idle"; ours gets "1 h, no renewal, no idle".

Access tokens are handled separately from the session. When the stored access token has expired, lua-resty-openidc refreshes it with the stored refresh token (in cookie mode too, since the refresh token sits in the sealed cookie). If there is no refresh token or the refresh fails, the request is treated as unauthenticated and redirected to the provider, which is a silent re-login when the provider session is still alive. `renew_access_token_on_expiry: false` disables the whole check. `access_token_expires_in` (default 3600) stands in when the token response carries no `expires_in`; `access_token_expires_leeway` (default 0) refreshes that many seconds early.

## 3. Goals

- An active user stays logged in up to `absolute_timeout`; an idle one is dropped after `idling_timeout`; the session is re-sealed at the same thresholds as lua-resty-session. Same defaults, same `0` = disabled.
- Identical semantics in `session.storage: cookie` and `session.storage: redis`.
- All five session plugins, through the shared layer, with no per-plugin reimplementation of the rules.
- `openid-connect` access-token expiry handling matches lua-resty-openidc in both storage modes.
- Existing configs keep working: `session.cookie.lifetime` and the flat `session_cookie_lifetime` are accepted and mapped exactly as APISIX maps them.

## 4. Non-goals

- `refresh_session_interval` (APISIX's periodic `prompt=none` re-authorization to pick up fresh claims). Separate feature.
- `remember` cookies (lua-resty-session's second, long-lived cookie). APISIX does not expose them.
- Making `touch_threshold` or the 75% renewal point configurable. APISIX's schema does not expose them either.
- Cross-instance coordination of cookie-mode refreshes. Cookie mode has no coordination point; two tabs may both refresh. lua-resty-openidc has the same race. Redis mode keeps the existing lock.
- Migrating sessions sealed by the current format. One re-login at upgrade is acceptable (see 6.3).

## 5. Anti-goals

- A session that an attacker can extend by editing the cookie. Every timestamp the rules depend on is inside the AES-GCM sealed payload.
- Cookie churn: a `Set-Cookie` on every response. Renewal only fires past the thresholds above, so a burst of parallel asset requests emits at most one new cookie per threshold crossing and all of them carry a valid session either way.
- Silently losing a renewal cookie because the `upstream` node replaced the response headers (see 6.4).

## 6. Design

### 6.1 Configuration (all five plugins)

Under `session:` (nested) with flat `session_<name>` fallbacks for the Web UI form, exactly like the existing `session.storage` / `session_storage`:

| Option | Type | Default | Meaning |
|---|---|---|---|
| `session.idling_timeout` | integer seconds | `900` | `0` disables |
| `session.rolling_timeout` | integer seconds | `3600` | `0` disables |
| `session.absolute_timeout` | integer seconds | `86400` | `0` disables |
| `session.cookie.lifetime` / `session_cookie_lifetime` | integer seconds | none | **Deprecated.** Mapped to `absolute_timeout` when `absolute_timeout` is unset. A config that sets it logs one warning at compile time. |

The three timers parse into one shared `SessionTimeouts { idling, rolling, absolute }` value in `server_session`, with a `parse_timeouts(config, plugin)` next to the existing `parse_backend`. `dingtalk-auth` and `feishu-auth`, whose old `lifetime` default was 86400, change nothing by default; the other three lose the implicit one-hour cap and gain the 24 h cap, matching an APISIX node with the same config.

`openid-connect` additionally gains (APISIX names, APISIX defaults):

| Option | Type | Default |
|---|---|---|
| `renew_access_token_on_expiry` | boolean | `true` |
| `access_token_expires_in` | integer seconds | `3600` |
| `access_token_expires_leeway` | integer seconds | `0` |

`session.refresh` (ours, default `true`) stays as the switch that decides whether refresh material (`refresh_token`, `expires_at`) is captured at login and used. It now applies to both storage modes.

### 6.2 The session envelope (`server_session`)

Every payload the plugins hand to `establish`, and every payload `load` returns, is wrapped in a binary envelope inside the sealed blob:

```
version(1 byte = 0x01) || created_at(8, BE) || renewed_at(8, BE) || touched_at(8, BE) || payload
```

- `created_at`: login time. Never changes.
- `renewed_at`: last full re-save. Equals `created_at` at login.
- `touched_at`: last activity touch. Equals `renewed_at` after a re-save.

The timestamps are Unix seconds and sit inside the AES-GCM plaintext, so they are authenticated.

**Validity on open**, with `now` and the configured timeouts (a `0` timer skips its check):

- `now - created_at > absolute` → invalid
- `now - renewed_at > rolling` → invalid
- `now - touched_at > idling` → invalid

Invalid means `Ok(None)` from `load`: unauthenticated, never an error.

**Renewal on open** (only when the session is valid):

- if `rolling > 0` and `now - renewed_at > floor(rolling * 0.75)`: `renewed_at = touched_at = now`, re-save
- else if `idling > 0` and `now - touched_at > 60`: `touched_at = now`, re-save

A re-save seals the envelope again (new nonce) with TTL `ttl(now) = min(rolling, absolute - (now - created_at))`, ignoring `0` timers and capped at ten years when both are `0`. In cookie mode the re-sealed blob is the new cookie value; in redis mode it is `put` under the same id, with the store TTL and `SessionMeta.expires_at` set from the same `ttl(now)` and `SessionMeta.created_at` kept at the envelope's `created_at`. Either way the browser gets a `Set-Cookie` with `Max-Age = ttl(now)` (redis mode: the same bare id; the header refreshes the browser-side expiry).

The sealer's own embedded expiry (`CookieSealer::seal(payload, ttl)`) is set to the same `ttl(now)`, so a blob also hard-expires at the point where the rules would reject it anyway. The sealer is not changed.

`establish` seals a fresh envelope with all three timestamps at `now` and the same TTL formula. `destroy` is unchanged.

Public API change, in two steps because a redis re-save needs a full `SessionMeta` (subject, plugin, policy, route) and the subject only exists inside the plugin-specific payload: `load` returns an `Opened { envelope, renewal }` (the decoded envelope plus which renewal, if any, is due) and takes only the `SessionTimeouts`; the plugin decodes the payload, builds the meta with `meta_now(ctx, plugin, subject)`, and calls `renew_if_due(...)`, which applies the renewal, re-saves, and returns the `Set-Cookie` to attach (or `None` when nothing was due). `update` (used by the `openid-connect` token refresh) takes the loaded envelope's `created_at`, sets `renewed_at = touched_at = now`, and uses `ttl(now)` instead of the full lifetime; that removes the documented over-approximation where a refresh granted a whole new lifetime.

**Redis mode re-save and the refresh lock.** A touch or roll rewrites the whole record. Done naively it races the lock-coordinated token refresh: request A reads old tokens, request B refreshes under the lock, A's re-save lands last and the rotated refresh token is lost. So in redis mode `renew_if_due` takes the same per-session store lock (`try_lock`, 10 s TTL), re-reads the record, applies the renewal to the fresh envelope, puts, and unlocks; when the lock is held it skips the renewal for this request (the holder's write carries fresh timestamps anyway). Cookie mode has no coordination point; its two-tab race is accepted (section 4).

Time is a parameter (`now: u64`) on the pure envelope functions so the rules are unit-testable without sleeping; the async wrappers pass the wall clock.

### 6.3 Legacy blobs

A blob sealed by the current code starts with the payload's first byte (always `{`, 0x7B, since every plugin stores JSON). The envelope's first byte is `0x01`. `load` treats anything that is not version `0x01` as invalid, so sessions from before the upgrade cost one re-login. Noted in the docs and the changelog.

### 6.4 Deferred response headers

A renewal, a cookie-mode token refresh, and (already today, silently broken) the `csrf` node's cookie all need to set a header on the *final* response from a request-phase node. The `upstream` node replaces `ctx.response.headers` wholesale with the upstream's headers, so anything placed there before proxying is lost.

`Context` gains a field:

```rust
/// Headers appended to the final client response after the graph finishes,
/// whatever node produced the response. For request-phase nodes that must
/// set a response header (session renewal cookies) without being overwritten
/// by `upstream`'s header replacement.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub deferred_response_headers: Vec<(String, String)>,
```

The listener appends them to `result_ctx.response.headers` just before `build_response`, on every path that builds a response from the context (buffered, streamed, and the WebSocket 101 path). The Lua marshalling does not expose the field and carries it across a script round-trip unchanged. `multi_auth`'s response restore does not touch it.

The session plugins push their renewal `Set-Cookie` there on the success path. Redirect responses (login, callback, logout) keep setting `set-cookie` directly on the response they build, as today.

Out of scope but recorded: `csrf` should move its success-path cookie to the same field in a follow-up fix.

### 6.5 `openid-connect` access-token handling

At the callback, when `session.refresh` is on (both storage modes), capture `refresh_token` and `expires_at = now + (expires_in or access_token_expires_in)`. When `session.refresh` is off, capture neither, as today.

On a request with a valid session, after the envelope rules and before attaching identity, when `renew_access_token_on_expiry` is on and the session carries an `access_token`:

- if `now + access_token_expires_leeway < expires_at`: nothing to do;
- else if a `refresh_token` is stored: refresh. Redis mode keeps the existing lock-coordinated path. Cookie mode calls the token endpoint directly, rewrites the session through `update`, and pushes the resulting `Set-Cookie` to `deferred_response_headers`. A refresh failure at the IdP falls back to a fresh login (`begin_auth`), as the redis path already does; a store failure is still a 503;
- else (no refresh token): fall back to a fresh login.

When `renew_access_token_on_expiry` is off, the request proceeds with whatever token is stored, which is exactly today's cookie-mode behavior.

A session with no `expires_at` (which can only happen with `session.refresh: false`) is never refreshed and never expires on token grounds, as today.

**Cookie size guard.** Refresh tokens from common providers run 1 to 2 KB, the sealer does no chunking (lua-resty-session does, which is how APISIX survives this), and a browser silently drops a cookie over 4 KB, which would turn into a login loop. So in cookie mode, when the sealed session cookie value would exceed 4000 bytes and a `refresh_token` is present, the plugin drops the refresh token (keeping `expires_at`, so the session falls back to re-login at token expiry exactly as before this change), re-seals, and logs a `warn!` with the sizes. The same guard runs on the refresh rewrite. Documented on the plugin page, with `session.storage: redis` as the fix for providers with large tokens.

### 6.6 Surfaces to update

- `src/plugins/util/server_session.rs`: envelope, timeouts, renewal, new `load` shape, `parse_timeouts`.
- `src/context/mod.rs`, `src/server/listener.rs`, `src/plugins/script/lua_runtime.rs`: deferred headers.
- The five plugins: parse timeouts, pass them, push the renewal cookie on success. `openid-connect` also: the three new options, cookie-mode refresh, `update` through the envelope.
- `ui/src/pluginConfig.ts`: three timer fields on each of the five forms; the lifetime field relabelled as deprecated with a hint. `openid-connect` gets the three token options.
- `website/docs/reference/plugins/{openid-connect,cas-auth,authz-casdoor,dingtalk-auth,feishu-auth}.md`: the timers, the deprecation, a short "Session lifetime" subsection on the `openid-connect` page explaining the three timers and the renewal rule; the token-refresh section rewritten for both modes.
- `docs/apisix-parity.md`: `openid-connect` row notes.
- `website/docs/reference/roadmap.md` (the honest-state ledger; there is no changelog): the server-side-sessions row records the breaking change, the one re-login at upgrade, and the default lifetime change for `openid-connect`, `cas-auth`, `authz-casdoor`.
- `src/debug/trace.rs`: the deferred headers are redacted at capture like `set-cookie` on the response.

### 6.7 Tests

- `server_session` unit tests, time-injected: envelope round trip; legacy blob rejected; each timer rejects when exceeded and is skipped at `0`; rolling renewal fires past 75% and not before; idling touch fires past 60 s and not before; a renewal never fires on an invalid session; `ttl()` formula including the both-zero cap; redis mode re-save keeps the id, resets store TTL, keeps `created_at`.
- `openid-connect`: callback captures refresh material in cookie mode; cookie-mode refresh success rewrites the session and emits a deferred `Set-Cookie`; refresh failure falls back to login; no refresh token falls back to login; `renew_access_token_on_expiry: false` passes the stale token through; leeway honoured; redis path unchanged (existing tests pass with the new `update` TTL).
- Each of the other four plugins: one test that a session past the touch threshold yields a deferred `Set-Cookie` on the success path, and one that config parsing maps `lifetime` to `absolute_timeout`.
- Listener: deferred headers survive the `upstream` header replacement on the buffered and streamed paths.
- The `admin::policies` catalog/docs tests keep passing (no new node types).

## 7. Calls made without asking

- All five plugins, not only `openid-connect`, because the shared layer is where the rules live and APISIX applies lua-resty-session to all of its session plugins.
- Binary envelope rather than a JSON wrapper: cheap to detect legacy blobs, no cookie size growth beyond 25 bytes.
- Legacy sessions are invalidated at upgrade rather than migrated.
- `access_token_expires_leeway` defaults to `0` as in APISIX, replacing the previous hard-coded 30 s.
- The `csrf` cookie bug is noted, not fixed here.
- Redis-mode renewals take the refresh lock and skip when it is held, rather than adding a `touch` method to the `SessionStore` trait.
- Cookie-mode refresh-token capture stays on by default (APISIX parity) with the 4000-byte drop guard instead of chunked cookies.
- The breaking-change notice goes in the roadmap ledger, since the repo keeps no changelog.
