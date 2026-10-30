# Session Timeouts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the five session plugins lua-resty-session's idle, rolling and absolute timeouts with renewal on use, and give `openid-connect` lua-resty-openidc's access-token expiry handling in both storage modes.

**Architecture:** A 25-byte binary envelope (`version || created_at || renewed_at || touched_at || payload`) inside every sealed session blob carries the timestamps the three timers need; `server_session` owns the rules (validity, 75% rolling re-save, 60 s touch, TTL formula) as pure time-injected functions plus async wrappers over the existing cookie/redis backends. A new `Context.deferred_response_headers` field lets a request-phase node attach the renewal `Set-Cookie` to whatever response the graph ends up producing, since `upstream` replaces `ctx.response.headers` wholesale. Plugins only parse the timeouts, call `load` / `renew_if_due`, and push the cookie.

**Tech Stack:** Rust (existing `ring` AES-GCM sealer, `serde_json`, tokio tests, `FakeSessionStore`), TypeScript for the UI form metadata, Markdown docs.

**Spec:** `docs/superpowers/specs/2026-10-30-session-timeouts-design.md`

## Global Constraints

- Defaults: `idling_timeout` 900, `rolling_timeout` 3600, `absolute_timeout` 86400; `0` disables a timer.
- Renewal: full re-save when `now - renewed_at > floor(rolling * 3 / 4)`; otherwise touch when `now - touched_at > 60`. Never on an invalid session.
- TTL on every seal/put/`Max-Age`: `min(rolling, absolute - (now - created_at))` over the non-zero timers; `315_360_000` s (ten years) when both are zero.
- `session.cookie.lifetime` / `session_cookie_lifetime` maps to `absolute_timeout` only when `absolute_timeout` is unset, and logs one `tracing::warn!` at parse.
- Envelope version byte `0x01`; any other first byte is an invalid session (`Ok(None)`), never an error.
- Store failures stay `Err(StoreError)` → `503` `SESSION_STORE_ERROR`; nothing in this plan turns one into a 401 or a redirect.
- Conventional Commits, no `Co-Authored-By`, no session links. Commit after every task.
- Run `cargo fmt` before each commit; the final task runs full `cargo test` and `cargo clippy --all-targets -- -D warnings`.

## Review Focus

1. A rolling re-save in redis mode must keep the same session id and the original `created_at` in `SessionMeta` (else the Sessions panel shows a wrong login time and revocation by id could miss). Pinned in Task 3.
2. An `absolute_timeout` smaller than `rolling_timeout` must cap the TTL at the absolute remainder (a cookie must never outlive the absolute cap). Pinned in Task 2.
3. Two parallel requests arriving just past a threshold both renew; the second cookie must still open (new nonce, same timestamps ± 1 s). Pinned in Task 2 (idempotent `apply`) and Task 3 (two loads of the same blob both succeed).
4. `openid-connect` with `session.refresh: false` must never redirect on token grounds even when a hand-made session carries a stale `expires_at`. Pinned in Task 4.
5. A legacy blob (first byte `{`) from the previous release must yield a clean re-login, not a 503 or a panic. Pinned in Task 2 and Task 4.

---

### Task 1: Deferred response headers on `Context`

**Files:**
- Modify: `src/context/mod.rs` (struct `Context`, `Context::new`, new method)
- Modify: `src/server/listener.rs` (before `build_response` and on the WebSocket 101 branch)
- Modify: `src/plugins/script/lua_runtime.rs:280-292` (carry the field across the Lua round-trip)

**Interfaces:**
- Produces: `pub deferred_response_headers: Vec<(String, String)>` on `Context` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`); `pub fn merge_deferred_response_headers(&mut self)` appending each pair to `self.response.headers` (entry/or_default/push) and leaving the vec empty.

- [ ] **Step 1: Write the failing tests** in `src/context/mod.rs` `mod tests`:

```rust
#[test]
fn test_merge_deferred_headers_appends_after_response_headers_were_replaced() {
    let mut ctx = Context::new(GatewayRequest::default_for_test());
    ctx.deferred_response_headers.push(("set-cookie".into(), "a=1".into()));
    // simulate upstream replacing headers wholesale
    ctx.response.headers = HashMap::from([("content-type".to_string(), vec!["text/plain".to_string()])]);
    ctx.merge_deferred_response_headers();
    assert_eq!(ctx.response.headers["set-cookie"], vec!["a=1".to_string()]);
    assert_eq!(ctx.response.headers["content-type"], vec!["text/plain".to_string()]);
    assert!(ctx.deferred_response_headers.is_empty());
}

#[test]
fn test_deferred_headers_absent_from_serialized_context_when_empty() {
    let ctx = Context::new(GatewayRequest::default_for_test());
    let json = serde_json::to_value(&ctx).unwrap();
    assert!(json.get("deferred_response_headers").is_none());
}
```
(Use whatever request constructor the existing context tests already use instead of `default_for_test` if one exists.)

- [ ] **Step 2: Run** `cargo test context::tests::test_merge_deferred` — expect compile failure (no field).
- [ ] **Step 3: Add the field and method** to `Context`; initialise in `Context::new`; fix every non-test `Context { .. }` literal the compiler reports (`src/vars/template.rs`, `src/vars/mod.rs` if non-test).
- [ ] **Step 4: Listener.** In `handle_request` (`src/server/listener.rs`), call `result_ctx.merge_deferred_response_headers()` right after the graph returns (before the WebSocket branch, so both the 101 path and `build_response` see them). On the 101 path, the response comes from `websocket::proxy_upgrade` and not from `result_ctx.response.headers`; append each pair of a clone taken before the merge with `resp.headers_mut().append(name, value)` when both parse (`HeaderName::from_bytes`, `HeaderValue::from_str`), skipping invalid ones.
- [ ] **Step 5: Lua.** In the function at `lua_runtime.rs:280`, clone `ctx.deferred_response_headers` before the match and assign it onto `new_ctx` in the `Ok` arm. Add test `test_lua_round_trip_keeps_deferred_headers` next to the existing marshalling tests: push a pair, run a no-op script, assert it survives.
- [ ] **Step 6: Debug traces.** `src/debug/trace.rs` redacts `set-cookie` on the response at capture; apply the same redaction to `deferred_response_headers` entries whose name is in the redacted set (case-insensitive). Add test `test_snapshot_redacts_deferred_set_cookie` beside the existing redaction test.
- [ ] **Step 7: Run** `cargo test context:: lua_round_trip debug::trace` — expect PASS.
- [ ] **Step 8: Commit** `feat(context): deferred response headers applied after the graph finishes`

---

### Task 2: Session timeouts, envelope and renewal rules (pure)

**Files:**
- Modify: `src/plugins/util/server_session.rs` (new types and functions; tests in its `mod tests`)

**Interfaces:**
- Produces:
  - `pub struct SessionTimeouts { pub idling: u64, pub rolling: u64, pub absolute: u64 }` with `impl Default` (900/3600/86400), `pub const MAX_TTL_SECS: u64 = 315_360_000`, `pub fn ttl(&self, created_at: u64, now: u64) -> Duration`.
  - `pub fn parse_timeouts(config: &HashMap<String, serde_json::Value>, plugin: &str) -> Result<SessionTimeouts, String>` reading `session.idling_timeout|session_idling_timeout`, `session.rolling_timeout|…`, `session.absolute_timeout|…`, and the deprecated `session.cookie.lifetime|session_cookie_lifetime` → `absolute` when unset (with `tracing::warn!("{plugin}: session.cookie.lifetime is deprecated; use session.absolute_timeout")`). A non-u64 value is `Err("{plugin}: session.<key> must be a non-negative integer")`.
  - `pub const ENVELOPE_VERSION: u8 = 1; pub const TOUCH_THRESHOLD_SECS: u64 = 60;`
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Envelope { pub created_at: u64, pub renewed_at: u64, pub touched_at: u64, pub payload: Vec<u8> }` with `pub fn new(payload: Vec<u8>, now: u64) -> Self`, `pub fn encode(&self) -> Vec<u8>`, `pub fn decode(bytes: &[u8]) -> Option<Self>`, `pub fn is_valid(&self, t: &SessionTimeouts, now: u64) -> bool`, `pub fn renewal_due(&self, t: &SessionTimeouts, now: u64) -> Renewal`, `pub fn apply(&mut self, r: Renewal, now: u64)`.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Renewal { None, Touch, Roll }`.

- [ ] **Step 1: Write the failing tests** (all with explicit `now` values, no sleeping):

```rust
#[test] fn test_timeouts_default_matches_lua_resty_session() { assert_eq!(SessionTimeouts::default(), SessionTimeouts { idling: 900, rolling: 3600, absolute: 86400 }); }
#[test] fn test_parse_timeouts_nested_flat_and_lifetime_mapping() {
    // nested wins; flat fallback; lifetime → absolute only when absolute unset
    let c = cfg(json!({"session": {"idling_timeout": 10, "cookie": {"lifetime": 500}}, "session_rolling_timeout": 20}));
    assert_eq!(parse_timeouts(&c, "p").unwrap(), SessionTimeouts { idling: 10, rolling: 20, absolute: 500 });
    let c = cfg(json!({"session": {"absolute_timeout": 7, "cookie": {"lifetime": 500}}}));
    assert_eq!(parse_timeouts(&c, "p").unwrap().absolute, 7);
    let c = cfg(json!({"session_cookie_lifetime": 42}));
    assert_eq!(parse_timeouts(&c, "p").unwrap().absolute, 42);
    assert!(parse_timeouts(&cfg(json!({"session": {"rolling_timeout": -1}})), "p").is_err());
}
#[test] fn test_ttl_is_min_of_rolling_and_absolute_remainder_ignoring_zero() {
    let t = SessionTimeouts { idling: 0, rolling: 3600, absolute: 5000 };
    assert_eq!(t.ttl(1000, 1000).as_secs(), 3600);
    assert_eq!(t.ttl(1000, 3000).as_secs(), 3000);          // absolute remainder wins (Review Focus 2)
    assert_eq!(SessionTimeouts { idling: 0, rolling: 0, absolute: 100 }.ttl(0, 60).as_secs(), 40);
    assert_eq!(SessionTimeouts { idling: 0, rolling: 0, absolute: 0 }.ttl(0, 0).as_secs(), MAX_TTL_SECS);
    assert_eq!(SessionTimeouts { idling: 0, rolling: 10, absolute: 100 }.ttl(0, 100).as_secs(), 0);
}
#[test] fn test_envelope_round_trip_and_legacy_rejected() {
    let e = Envelope::new(b"{\"a\":1}".to_vec(), 1234);
    let bytes = e.encode();
    assert_eq!(bytes[0], ENVELOPE_VERSION); assert_eq!(bytes.len(), 25 + 7);
    assert_eq!(Envelope::decode(&bytes).unwrap(), e);
    assert!(Envelope::decode(b"{\"claims\":{}}").is_none());   // legacy blob (Review Focus 5)
    assert!(Envelope::decode(&bytes[..20]).is_none());
}
#[test] fn test_validity_each_timer_and_zero_disables() {
    let t = SessionTimeouts::default();
    let e = Envelope { created_at: 0, renewed_at: 0, touched_at: 0, payload: vec![] };
    assert!(e.is_valid(&t, 900)); assert!(!e.is_valid(&t, 901));                       // idling
    let e2 = Envelope { touched_at: 3500, ..e.clone() };
    assert!(e2.is_valid(&t, 3600)); assert!(!e2.is_valid(&t, 3601));                   // rolling
    let e3 = Envelope { renewed_at: 86000, touched_at: 86000, ..e.clone() };
    assert!(e3.is_valid(&t, 86400)); assert!(!e3.is_valid(&t, 86401));                 // absolute
    let off = SessionTimeouts { idling: 0, rolling: 0, absolute: 0 };
    assert!(e.is_valid(&off, u64::MAX / 2));
}
#[test] fn test_renewal_thresholds() {
    let t = SessionTimeouts::default();
    let e = Envelope { created_at: 0, renewed_at: 0, touched_at: 0, payload: vec![] };
    assert_eq!(e.renewal_due(&t, 60), Renewal::None);
    assert_eq!(e.renewal_due(&t, 61), Renewal::Touch);
    assert_eq!(e.renewal_due(&t, 2700), Renewal::Touch);     // == 75% is not yet a roll
    assert_eq!(e.renewal_due(&t, 2701), Renewal::Roll);
    let no_roll = SessionTimeouts { rolling: 0, ..t };
    assert_eq!(e.renewal_due(&no_roll, 5000), Renewal::Touch);
    let none = SessionTimeouts { idling: 0, rolling: 0, absolute: 0 };
    assert_eq!(e.renewal_due(&none, 5000), Renewal::None);
    let mut r = e.clone(); r.apply(Renewal::Roll, 3000);
    assert_eq!((r.created_at, r.renewed_at, r.touched_at), (0, 3000, 3000));
    let mut tch = e.clone(); tch.apply(Renewal::Touch, 100);
    assert_eq!((tch.created_at, tch.renewed_at, tch.touched_at), (0, 0, 100));
    let mut again = r.clone(); again.apply(Renewal::Roll, 3000); assert_eq!(again, r);   // idempotent (Review Focus 3)
}
```

- [ ] **Step 2: Run** `cargo test server_session::tests::test_timeouts` — expect compile failure.
- [ ] **Step 3: Implement** the types and functions listed in Interfaces. `encode` = `[1u8] ++ created_at.to_be_bytes() ++ renewed_at ++ touched_at ++ payload`; `decode` requires `len >= 25` and `bytes[0] == 1`. `renewal_due` checks `Roll` first (`rolling > 0 && now.saturating_sub(renewed_at) > rolling * 3 / 4`), then `Touch` (`idling > 0 && now.saturating_sub(touched_at) > TOUCH_THRESHOLD_SECS`). `ttl` uses `absolute.saturating_sub(now.saturating_sub(created_at))`.
- [ ] **Step 4: Run** the six tests — expect PASS.
- [ ] **Step 5: Commit** `feat(sessions): timeout model and sealed session envelope`

---

### Task 3: Async session API over the envelope

**Files:**
- Modify: `src/plugins/util/server_session.rs` (`meta_now`, `establish`, `load`, `update`, new `renew_if_due`, test helper)

**Interfaces:**
- Consumes: Task 2 types; existing `CookieSealer`, `CookieAttrs`, `SessionStore`.
- Produces (every `Set-Cookie` built here carries `Max-Age = ttl`, overriding `attrs.max_age`):
  - `pub fn meta_now(ctx: &Context, plugin: &str, subject: &str) -> SessionMeta` (no `ttl` param; `created_at = expires_at = now`; the callee below fills `expires_at`).
  - `pub async fn establish(backend, sealer, payload: &[u8], timeouts: &SessionTimeouts, meta: SessionMeta, cookie_name: &str, attrs: &CookieAttrs<'_>) -> Result<String, StoreError>`.
  - `pub struct Opened { pub envelope: Envelope, pub renewal: Renewal }`
  - `pub async fn load(backend, sealer, cookie_value: &str, timeouts: &SessionTimeouts) -> Result<Option<Opened>, StoreError>` — `Ok(None)` for absent/unsealable/legacy/invalid-by-timers.
  - `pub async fn renew_if_due(backend, sealer, cookie_value: &str, opened: &Opened, timeouts, meta: SessionMeta, cookie_name, attrs) -> Result<Option<String>, StoreError>` — `None` when `opened.renewal == Renewal::None`; otherwise applies the renewal at `now_unix()`, re-seals with `timeouts.ttl(created_at, now)`, cookie mode returns the new cookie; redis mode: `try_lock(id, 10 s)`; if not acquired return `Ok(None)` (skip this request); else `get` the record again, decode its envelope (if it vanished or no longer decodes, unlock and return `Ok(None)`), recompute `renewal_due` on that fresh envelope, apply, `put` under the same id (`meta.created_at = envelope.created_at`, `meta.expires_at = now + ttl`), unlock (best effort, `warn!` on failure), and return a `Set-Cookie` with the same id and the new `Max-Age`.
  - `pub async fn update(backend, sealer, cookie_value: &str, created_at: u64, payload: &[u8], timeouts, meta, cookie_name, attrs) -> Result<String, StoreError>` — new envelope `{created_at, renewed_at: now, touched_at: now}`, same TTL rule, always returns the `Set-Cookie`.
  - `#[cfg(test)] pub fn seal_envelope_for_test(sealer: &CookieSealer, payload: &[u8], created_at: u64, renewed_at: u64, touched_at: u64, ttl: Duration) -> String` and `#[cfg(test)] pub fn seal_fresh_for_test(sealer, payload) -> String` (all three timestamps = now, ttl 600 s).

- [ ] **Step 1: Write the failing tests** in `server_session::tests` (cookie mode with `SessionBackend::Cookie`, redis mode with `FakeSessionStore`):

```rust
#[tokio::test] async fn test_establish_then_load_fresh_session_no_renewal() { /* establish; read cookie value out of the Set-Cookie; load → Some(Opened{renewal: None}); payload equal; Set-Cookie contains "Max-Age=3600" with default timeouts */ }
#[tokio::test] async fn test_load_rejects_legacy_blob() { /* sealer.seal(b"{\"x\":1}", 60) → load → Ok(None) */ }
#[tokio::test] async fn test_load_rejects_idle_session_and_renews_touched_one() { /* seal_envelope_for_test with touched_at = now-901 → Ok(None); touched_at = now-61 → Some(Opened{renewal: Touch}); renew_if_due → Some(cookie); loading that cookie → renewal None and touched_at within 1 s of now */ }
#[tokio::test] async fn test_roll_keeps_created_at_in_cookie_and_redis() { /* renewed_at = now-2701: renewal Roll; cookie mode: new blob decodes with created_at unchanged, renewed_at ≈ now; redis mode: same id in the Set-Cookie, fake store record rewritten, meta.created_at == original created_at, meta.expires_at ≈ now+ttl (Review Focus 1) */ }
#[tokio::test] async fn test_two_loads_of_same_due_blob_both_renew_successfully() { /* same Opened renewed twice → two different cookie strings, both load to Some (Review Focus 3) */ }
#[tokio::test] async fn test_update_keeps_created_at_and_caps_ttl_by_absolute() { /* timeouts {rolling 3600, absolute 5000}, created_at = now-3000 → update → Set-Cookie Max-Age=2000, decoded envelope created_at unchanged */ }
#[tokio::test] async fn test_store_failure_on_renew_is_err() { /* fake.fail = true → renew_if_due Err */ }
#[tokio::test] async fn test_redis_renew_skips_when_lock_held_and_rereads_under_lock() { /* fake.try_lock(id) taken by the test → renew_if_due Ok(None) and record unchanged; unlock; put a NEWER payload under the same id behind the Opened's back → renew_if_due writes timestamps onto the newer payload, not the stale one; lock released afterwards */ }
```

- [ ] **Step 2: Run** `cargo test server_session::tests` — expect compile failures.
- [ ] **Step 3: Implement** per Interfaces. Factor one private `fn seal_and_set_cookie(backend, sealer, cookie_value: Option<&str>, envelope: &Envelope, timeouts, meta, cookie_name, attrs, now) -> Result<String, StoreError>` used by `establish` (fresh random id), `renew_if_due` and `update` (existing id parsed from `cookie_value`). Existing callers in the five plugins will not compile yet; that is expected until Tasks 4–6 — to keep the build green per task, leave the old `load`/`update`/`establish` signatures as `#[deprecated]` thin wrappers only if the plugins cannot be updated in this task; otherwise update the call sites minimally (compile-only, no behaviour change: pass `&SessionTimeouts { idling: 0, rolling: 0, absolute: lifetime }` and ignore `renewal`) so the crate builds and all existing tests pass.
- [ ] **Step 4: Run** `cargo test server_session` then full `cargo test` — expect PASS (existing OIDC/CAS/etc. tests that seal `SessionData` directly must be switched to `seal_fresh_for_test`).
- [ ] **Step 5: Commit** `feat(sessions): establish, load, renew and update through the timed envelope`

---

### Task 4: `openid-connect`: timeouts, both-mode refresh, APISIX token options

**Files:**
- Modify: `src/plugins/native/openid_connect.rs` (`Interactive`, `build_interactive`, `execute_interactive`, `refresh_if_needed`, `do_refresh`, `handle_callback`, module docs, tests)

**Interfaces:**
- Consumes: Task 1 field; Task 3 API.
- Produces: config keys `renew_access_token_on_expiry` (bool, default `true`), `access_token_expires_in` (u64, default `3600`), `access_token_expires_leeway` (u64, default `0`); `Interactive { timeouts: SessionTimeouts, renew_on_expiry: bool, access_token_expires_in: u64, access_token_expires_leeway: u64, .. }` replacing `session_lifetime`.

- [ ] **Step 1: Write the failing tests** (reuse `interactive_explicit_cfg`, `spawn_json_server`, `req_ctx`, `resources_with_fake_store`):

```rust
#[test] fn test_interactive_timeouts_defaults_and_lifetime_mapping() { /* default → timeouts == SessionTimeouts::default(); session.cookie.lifetime 900 → absolute 900; flat session_cookie_lifetime 1200 → absolute 1200; the three new token options parse with defaults true/3600/0 */ }
#[tokio::test] async fn test_cookie_mode_touch_emits_deferred_set_cookie() { /* seal_envelope_for_test(SessionData{claims sub u1}, touched_at now-61) as oidc_session → execute → success, message.user_id == "u1", deferred_response_headers has one ("set-cookie", v) with v starting "oidc_session=" and containing "Max-Age=" */ }
#[tokio::test] async fn test_cookie_mode_fresh_session_emits_no_cookie() { /* seal_fresh_for_test → success, deferred empty */ }
#[tokio::test] async fn test_cookie_mode_idle_session_redirects_to_login() { /* touched_at now-901 → port Some("redirect") to the authorization endpoint */ }
#[tokio::test] async fn test_legacy_cookie_blob_redirects_to_login() { /* sealer.seal(SessionData json, 60) → redirect, not error (Review Focus 5) */ }
#[tokio::test] async fn test_callback_captures_refresh_material_in_cookie_mode() { /* token server returns id_token-less? no: reuse the existing callback-success fixture if one exists; otherwise assert via do_refresh path below — minimal: a token response with refresh_token+expires_in in cookie mode produces a session whose decoded payload has refresh_token and expires_at ≈ now+expires_in; with "expires_in" absent, expires_at ≈ now+3600 */ }
#[tokio::test] async fn test_cookie_mode_refresh_success_rewrites_cookie() { /* spawn_json_server 200 {"access_token":"new-at","refresh_token":"new-rt","expires_in":3600}; session with access_token "old", refresh_token "rt", expires_at 1 → success; one deferred set-cookie; decoding its envelope payload gives access_token "new-at" and created_at preserved */ }
#[tokio::test] async fn test_cookie_mode_refresh_failure_falls_back_to_login() { /* token_endpoint 127.0.0.1:1 → redirect */ }
#[tokio::test] async fn test_cookie_mode_expired_without_refresh_token_falls_back_to_login() { /* access_token "old", expires_at 1, no refresh_token → redirect */ }
#[tokio::test] async fn test_renew_on_expiry_false_passes_stale_token_through() { /* same session, renew_access_token_on_expiry false → success, x-access-token "old" */ }
#[tokio::test] async fn test_session_refresh_false_never_redirects_on_token_grounds() { /* session.refresh false, stale expires_at → success (Review Focus 4) */ }
#[tokio::test] async fn test_leeway_triggers_early_refresh() { /* leeway 120, expires_at now+60, refresh server → success with "new-at" */ }
#[test] fn test_cookie_size_guard_drops_refresh_token_over_4000_bytes() { /* pure helper `fit_cookie(session: SessionData, seal: impl Fn(&[u8]) -> String) -> (SessionData, String)`: a 3500-byte refresh_token → returned data has refresh_token None, expires_at kept, sealed value <= 4000; a 100-byte one is kept untouched */ }
```
Keep the existing redis tests; switch their hand-sealed blobs to `seal_envelope_for_test`/`seal_fresh_for_test`.

- [ ] **Step 2: Run** `cargo test openid_connect::tests::test_cookie_mode` — expect failures.
- [ ] **Step 3: Implement.** `build_interactive`: `timeouts = server_session::parse_timeouts(config, "openid-connect")?`, read the three options (nested-free, top-level keys). `read_session` returns `Option<(Opened, SessionData)>`. `execute_interactive`: after a valid session, run the token check, then (if no refresh happened) `renew_if_due`; push `("set-cookie", v)` onto `ctx.deferred_response_headers`. Token check (`refresh_if_needed`): `Continue` when `!flow.refresh_enabled || !flow.renew_on_expiry || session.access_token.is_none() || session.expires_at.is_none() || now + leeway < expires_at`; else no refresh token → `Break(begin_auth)`; else cookie mode → `do_refresh` directly, redis mode → existing lock path. `do_refresh(ctx, flow, raw_cookie, created_at, refresh_token, old) -> Result<(SessionData, String), RefreshFailure>` calls `server_session::update`. Callback: capture `refresh_token` and `expires_at = now + expires_in.unwrap_or(access_token_expires_in)` whenever `flow.refresh_enabled` and an `access_token` is present, in both modes. Cookie-mode size guard (`COOKIE_VALUE_LIMIT: usize = 4000`): in cookie mode only, both at the callback and in `do_refresh`, seal via a `fit_cookie`-style step that drops `refresh_token` and re-seals when the sealed value exceeds the limit, logging `warn!("openid-connect: sealed session cookie is {n} bytes; dropping refresh_token to fit (use session.storage: redis for large tokens)")`. Since `establish`/`update` seal internally, implement the guard as: serialize → `sealer.seal` dry run for the length check (the sealed length depends only on payload length) → if over, drop the token and serialize again → then call `establish`/`update` with the final payload. Replace the module-doc paragraphs about "redis mode only" refresh and the "no refresh in cookie mode" deviation with the new behaviour.
- [ ] **Step 4: Run** `cargo test openid_connect` — expect PASS.
- [ ] **Step 5: Commit** `feat(openid-connect): session timeouts and token refresh in both storage modes`

---

### Task 5: `cas-auth` and `authz-casdoor`

**Files:**
- Modify: `src/plugins/native/cas_auth.rs` (`cookie_lifetime` → `timeouts`, `read_session`, success path, tests)
- Modify: `src/plugins/native/authz_casdoor.rs` (same)

**Interfaces:**
- Consumes: Task 3 API, Task 1 field.
- Produces: `timeouts: SessionTimeouts` field on each plugin struct; `session_cookie_u64` deleted from both files (replaced by `parse_timeouts`).

- [ ] **Step 1: Write the failing tests** in each file:

```rust
#[test] fn test_session_timeouts_parse_and_lifetime_maps_to_absolute() { /* lifetime 60 → timeouts.absolute 60, idling/rolling defaults; existing `cookie_lifetime` assertions updated */ }
#[tokio::test] async fn test_touched_session_emits_deferred_set_cookie_on_success() { /* seal_envelope_for_test(payload, touched_at now-61) → success + one deferred set-cookie with the plugin's cookie name */ }
#[tokio::test] async fn test_idle_session_starts_login() { /* touched_at now-901 → the plugin's redirect/login outcome */ }
```

- [ ] **Step 2: Run** them — expect compile failures.
- [ ] **Step 3: Implement**: parse in `from_config`; `read_session` returns the decoded value plus the `Opened`; on the success path call `renew_if_due` with `meta_now(ctx, "<plugin>", &subject)` and push the cookie; `establish` call sites pass `&self.timeouts`; `session_attrs`/`cookie_attrs` set `max_age: None` (server_session fills it). Docs comments on the structs updated.
- [ ] **Step 4: Run** `cargo test cas_auth authz_casdoor` — expect PASS.
- [ ] **Step 5: Commit** `feat(cas-auth,authz-casdoor): session timeouts with renewal on use`

---

### Task 6: `dingtalk-auth` and `feishu-auth`

**Files:**
- Modify: `src/plugins/native/dingtalk_auth.rs`, `src/plugins/native/feishu_auth.rs` (session struct `cookie_lifetime` → `timeouts`, `read_session`, `execute_session`, tests)

**Interfaces:** as Task 5. Note both default `lifetime` to 86400 today; with the mapping, an unset lifetime now yields `SessionTimeouts::default()` (absolute 86400, unchanged).

- [ ] **Step 1: Write the failing tests** — same three as Task 5 per file (the "login" outcome here is the `redirect` port to the provider, or `denied` where the plugin denies without a code; use whatever the existing no-session test asserts).
- [ ] **Step 2: Run** — expect failures.
- [ ] **Step 3: Implement** as in Task 5; `read_session`'s `(Option<T>, Option<String>)` return grows to carry the `Opened` so `execute_session` can renew after attaching identity.
- [ ] **Step 4: Run** `cargo test dingtalk feishu` — expect PASS.
- [ ] **Step 5: Commit** `feat(dingtalk-auth,feishu-auth): session timeouts with renewal on use`

---

### Task 7: UI form fields and documentation

**Files:**
- Modify: `ui/src/pluginConfig.ts` (five forms)
- Modify: `website/docs/reference/plugins/openid-connect.md`, `cas-auth.md`, `authz-casdoor.md`, `dingtalk-auth.md`, `feishu-auth.md`
- Modify: `CLAUDE.md` (the "Server-side sessions" bullet: refresh is no longer redis-only; sessions renew on use)
- Modify: `website/docs/reference/roadmap.md` (server-side-sessions row: the breaking change, one re-login at upgrade, default lifetime change for `openid-connect`, `cas-auth`, `authz-casdoor`, the cookie-size guard)

- [ ] **Step 1: UI.** In each of the five forms, directly after `session_cookie_path`, add:
```ts
{ key: 'session_idling_timeout', label: 'Idle timeout (s)', type: 'number', default: 900, hint: 'session ends after this much inactivity; 0 disables' },
{ key: 'session_rolling_timeout', label: 'Rolling timeout (s)', type: 'number', default: 3600, hint: 'session is re-sealed while in use; 0 disables' },
{ key: 'session_absolute_timeout', label: 'Absolute timeout (s)', type: 'number', default: 86400, hint: 'hard cap since login; 0 disables' },
```
and change the `session_cookie_lifetime` entry's label to `'Session lifetime (s, deprecated)'` with hint `'deprecated: maps to absolute timeout when that is unset'` and no `default`. On `openid-connect` add after `session_refresh`:
```ts
{ key: 'renew_access_token_on_expiry', label: 'Renew access token on expiry', type: 'switch', default: true, hint: 'expired token: refresh with the stored refresh token, else re-login' },
{ key: 'access_token_expires_in', label: 'Access token lifetime (s)', type: 'number', default: 3600, hint: 'used when the token response has no expires_in' },
{ key: 'access_token_expires_leeway', label: 'Refresh leeway (s)', type: 'number', default: 0 },
```
- [ ] **Step 2: Docs.** On every page: replace the `session.cookie.lifetime` row with rows for `session.idling_timeout` (900), `session.rolling_timeout` (3600), `session.absolute_timeout` (86400) and a **Deprecated** row for `session.cookie.lifetime` ("mapped to `session.absolute_timeout` when that is unset"). Add a `#### Session lifetime` subsection to `openid-connect.md` (the three timers table, the 75%/60 s renewal rule, `Max-Age`/store TTL formula, that both storage modes enforce them, that sessions from before this version require one re-login) and link to it from the other four pages' session sections. Rewrite `openid-connect.md`'s "Token refresh" subsection for both modes, add the three new option rows, note the cookie-size cost of a refresh token in cookie mode and `session.refresh: false` as the opt-out, and update the Limitations note (cookie mode now refreshes; still no revocation). Replace the two sibling-page sentences claiming `upstream` would drop a success-path `Set-Cookie` with a sentence that the renewal cookie is deferred to the final response.
- [ ] **Step 3: Verify** `cd ui && npm ci && npx tsc --noEmit` passes, and `cargo test admin::policies` (docs/sidebar/icon consistency tests) passes.
- [ ] **Step 4: Commit** `docs(sessions): document idle, rolling and absolute timeouts; ui form fields`

---

### Task 8: Final verification

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] Full `cargo test` (expect every suite green; count ≥ 1477 + new tests)
- [ ] `graphify update .` from the repo root
- [ ] Commit any fmt/clippy fallout as `chore: fmt and clippy`
