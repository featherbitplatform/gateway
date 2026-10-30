//! Shared session-backend plumbing for the interactive auth plugins.
//!
//! One config surface (`session.storage: cookie|redis` + `session.store:
//! <name>`, with the flat `session_storage`/`session_store` UI fallbacks)
//! and one establish/load/destroy seam, so the five session plugins differ
//! only in payload shape and flow, not in storage mechanics. Cookie mode is
//! byte-identical to the pre-existing behavior; redis mode puts the SAME
//! sealed bytes server-side and hands the browser a bare 128-bit id.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::context::Context;
use crate::plugins::resources::PluginResources;
use crate::sessions::{SessionId, SessionMeta, SessionStore, StoreError};

use super::cookie_session::{build_set_cookie, delete_cookie, CookieAttrs, CookieSealer};

pub enum SessionBackend {
    Cookie,
    Store {
        store: Arc<dyn SessionStore>,
        store_name: String,
    },
}

impl std::fmt::Debug for SessionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cookie => write!(f, "SessionBackend::Cookie"),
            Self::Store { store_name, .. } => {
                write!(f, "SessionBackend::Store({store_name})")
            }
        }
    }
}

fn nested_or_flat<'a>(
    config: &'a HashMap<String, serde_json::Value>,
    nested: &str,
    flat: &str,
) -> Option<&'a str> {
    config
        .get("session")
        .and_then(|s| s.get(nested))
        .or_else(|| config.get(flat))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Parses `session.storage` / `session.store` and resolves the named store
/// at construction time — a bad reference fails policy compilation.
pub fn parse_backend(
    config: &HashMap<String, serde_json::Value>,
    resources: &Arc<PluginResources>,
    plugin: &str,
) -> Result<SessionBackend, String> {
    match nested_or_flat(config, "storage", "session_storage").unwrap_or("cookie") {
        "cookie" => Ok(SessionBackend::Cookie),
        "redis" => {
            let name = nested_or_flat(config, "store", "session_store").ok_or_else(|| {
                format!(
                    "{plugin}: session.storage 'redis' requires 'session.store' naming a declared stores: entry"
                )
            })?;
            let store = resources.stores.load().session_store(name)?;
            Ok(SessionBackend::Store {
                store,
                store_name: name.to_string(),
            })
        }
        other => Err(format!(
            "{plugin}: unknown session.storage '{other}' — supported: cookie, redis"
        )),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// TTL used when every timer is disabled: ten years, so a sealed blob and a
/// store record still carry a finite expiry.
pub const MAX_TTL_SECS: u64 = 315_360_000;

/// The three session timers, in seconds, with lua-resty-session's semantics
/// and defaults (the library APISIX's session plugins delegate to). `0`
/// disables a timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimeouts {
    /// Session is invalid once this long has passed since the last touch.
    pub idling: u64,
    /// Session is invalid once this long has passed since the last re-save.
    pub rolling: u64,
    /// Session is invalid once this long has passed since login.
    pub absolute: u64,
}

impl Default for SessionTimeouts {
    fn default() -> Self {
        Self {
            idling: 900,
            rolling: 3600,
            absolute: 86400,
        }
    }
}

impl SessionTimeouts {
    /// How long a blob sealed or a record written at `now` may live:
    /// `min(rolling, absolute - elapsed_since_creation)` over the enabled
    /// timers, [`MAX_TTL_SECS`] when both are disabled. Also the cookie
    /// `Max-Age`.
    pub fn ttl(&self, created_at: u64, now: u64) -> Duration {
        let mut ttl = MAX_TTL_SECS;
        if self.rolling > 0 {
            ttl = ttl.min(self.rolling);
        }
        if self.absolute > 0 {
            ttl = ttl.min(self.absolute.saturating_sub(now.saturating_sub(created_at)));
        }
        Duration::from_secs(ttl)
    }
}

fn timeout_field(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
    plugin: &str,
) -> Result<Option<u64>, String> {
    let value = config
        .get("session")
        .and_then(|s| s.get(key))
        .or_else(|| config.get(&format!("session_{key}")));
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{plugin}: session.{key} must be a non-negative integer")),
    }
}

/// Parses `session.idling_timeout` / `session.rolling_timeout` /
/// `session.absolute_timeout` (nested, or the flat `session_<key>` form the
/// Web UI emits). The deprecated `session.cookie.lifetime` /
/// `session_cookie_lifetime` maps to `absolute_timeout` when that is unset,
/// exactly as APISIX maps it, and logs one warning.
pub fn parse_timeouts(
    config: &HashMap<String, serde_json::Value>,
    plugin: &str,
) -> Result<SessionTimeouts, String> {
    let defaults = SessionTimeouts::default();
    let idling = timeout_field(config, "idling_timeout", plugin)?.unwrap_or(defaults.idling);
    let rolling = timeout_field(config, "rolling_timeout", plugin)?.unwrap_or(defaults.rolling);
    let mut absolute = timeout_field(config, "absolute_timeout", plugin)?;
    let lifetime = config
        .get("session")
        .and_then(|s| s.get("cookie"))
        .and_then(|c| c.get("lifetime"))
        .or_else(|| config.get("session_cookie_lifetime"));
    if let Some(v) = lifetime {
        let secs = v.as_u64().ok_or_else(|| {
            format!("{plugin}: session.cookie.lifetime must be a non-negative integer")
        })?;
        tracing::warn!(
            "{plugin}: session.cookie.lifetime is deprecated; use session.absolute_timeout"
        );
        if absolute.is_none() {
            absolute = Some(secs);
        }
    }
    Ok(SessionTimeouts {
        idling,
        rolling,
        absolute: absolute.unwrap_or(defaults.absolute),
    })
}

/// Format version of the sealed session envelope. Anything else as the
/// first byte of a sealed blob (notably `{` from the pre-envelope JSON
/// payloads) is treated as "not a session".
pub const ENVELOPE_VERSION: u8 = 1;
/// A touch rewrites the session at most this often (seconds).
pub const TOUCH_THRESHOLD_SECS: u64 = 60;
const ENVELOPE_HEADER_LEN: usize = 1 + 8 * 3;

/// Which renewal a valid session is due for on this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renewal {
    None,
    /// Only `touched_at` moves.
    Touch,
    /// `renewed_at` and `touched_at` move: a full re-save.
    Roll,
}

/// What a sealed session blob contains: the three timestamps the timers
/// need (Unix seconds, authenticated along with the payload) plus the
/// plugin's own payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Login time. Never changes.
    pub created_at: u64,
    /// Last full re-save; equals `created_at` at login.
    pub renewed_at: u64,
    /// Last activity touch; equals `renewed_at` after a re-save.
    pub touched_at: u64,
    pub payload: Vec<u8>,
}

impl Envelope {
    pub fn new(payload: Vec<u8>, now: u64) -> Self {
        Self {
            created_at: now,
            renewed_at: now,
            touched_at: now,
            payload,
        }
    }

    /// `version(1) || created_at(8, BE) || renewed_at(8) || touched_at(8) || payload`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENVELOPE_HEADER_LEN + self.payload.len());
        out.push(ENVELOPE_VERSION);
        out.extend_from_slice(&self.created_at.to_be_bytes());
        out.extend_from_slice(&self.renewed_at.to_be_bytes());
        out.extend_from_slice(&self.touched_at.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// `None` for a short blob or any other format version (legacy blobs).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < ENVELOPE_HEADER_LEN || bytes[0] != ENVELOPE_VERSION {
            return None;
        }
        let word = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[1 + 8 * i..1 + 8 * (i + 1)]);
            u64::from_be_bytes(b)
        };
        Some(Self {
            created_at: word(0),
            renewed_at: word(1),
            touched_at: word(2),
            payload: bytes[ENVELOPE_HEADER_LEN..].to_vec(),
        })
    }

    /// All enabled timers still within bounds at `now`.
    pub fn is_valid(&self, t: &SessionTimeouts, now: u64) -> bool {
        let within = |limit: u64, since: u64| limit == 0 || now.saturating_sub(since) <= limit;
        within(t.absolute, self.created_at)
            && within(t.rolling, self.renewed_at)
            && within(t.idling, self.touched_at)
    }

    /// Roll past 75% of the rolling timeout, else touch past
    /// [`TOUCH_THRESHOLD_SECS`] since the last touch, else nothing. Callers
    /// check [`Self::is_valid`] first; an invalid session is never renewed.
    pub fn renewal_due(&self, t: &SessionTimeouts, now: u64) -> Renewal {
        if t.rolling > 0 && now.saturating_sub(self.renewed_at) > t.rolling * 3 / 4 {
            return Renewal::Roll;
        }
        if t.idling > 0 && now.saturating_sub(self.touched_at) > TOUCH_THRESHOLD_SECS {
            return Renewal::Touch;
        }
        Renewal::None
    }

    pub fn apply(&mut self, r: Renewal, now: u64) {
        match r {
            Renewal::None => {}
            Renewal::Touch => self.touched_at = now,
            Renewal::Roll => {
                self.renewed_at = now;
                self.touched_at = now;
            }
        }
    }
}

/// Builds the meta envelope from the context's `__route`/`__policy` vars.
/// `created_at`/`expires_at` are filled in by whichever write persists it.
pub fn meta_now(ctx: &Context, plugin: &str, subject: &str) -> SessionMeta {
    let var = |k: &str| {
        ctx.message
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let now = now_unix();
    SessionMeta {
        id: String::new(),
        subject: subject.to_string(),
        plugin: plugin.to_string(),
        policy: var("__policy"),
        route: var("__route"),
        created_at: now,
        expires_at: now,
    }
}

/// Seals `envelope` at `now` with the timers' TTL and persists it: cookie
/// mode returns the sealed blob as the cookie; redis mode `put`s it under
/// `id` and returns the bare id as the cookie. Every `Set-Cookie` built
/// here carries `Max-Age = ttl`, whatever `attrs.max_age` says.
#[allow(clippy::too_many_arguments)]
async fn persist(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    id: Option<SessionId>,
    envelope: &Envelope,
    timeouts: &SessionTimeouts,
    mut meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
    now: u64,
) -> Result<String, StoreError> {
    let ttl = timeouts.ttl(envelope.created_at, now);
    let sealed = sealer.seal(&envelope.encode(), ttl);
    let attrs = CookieAttrs {
        path: attrs.path,
        max_age: Some(ttl.as_secs()),
        http_only: attrs.http_only,
        secure: attrs.secure,
        same_site: attrs.same_site,
    };
    match backend {
        SessionBackend::Cookie => Ok(build_set_cookie(cookie_name, &sealed, &attrs)),
        SessionBackend::Store { store, .. } => {
            let id = id.unwrap_or_else(SessionId::random);
            meta.created_at = envelope.created_at;
            meta.expires_at = now.saturating_add(ttl.as_secs());
            store.put(&id, sealed.as_bytes(), ttl, &meta).await?;
            Ok(build_set_cookie(cookie_name, id.as_str(), &attrs))
        }
    }
}

/// Seals `payload` in a fresh envelope and returns the session Set-Cookie
/// value. Cookie mode: the sealed blob IS the cookie. Redis mode: the sealed
/// blob goes under a fresh random id; the cookie carries the bare id.
pub async fn establish(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    payload: &[u8],
    timeouts: &SessionTimeouts,
    meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
) -> Result<String, StoreError> {
    let now = now_unix();
    let envelope = Envelope::new(payload.to_vec(), now);
    persist(
        backend,
        sealer,
        None,
        &envelope,
        timeouts,
        meta,
        cookie_name,
        attrs,
        now,
    )
    .await
}

/// A session that passed every timer, plus the renewal it is due for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub envelope: Envelope,
    pub renewal: Renewal,
}

/// Reads and opens the sealed blob behind a cookie value without applying
/// the timers. `Ok(None)` = absent/unsealable/legacy; `Err` = store outage.
async fn fetch_envelope(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
) -> Result<Option<Envelope>, StoreError> {
    let sealed = match backend {
        SessionBackend::Cookie => cookie_value.to_string(),
        SessionBackend::Store { store, .. } => {
            let Some(id) = SessionId::parse(cookie_value) else {
                return Ok(None);
            };
            let Some(sealed) = store.get(&id).await? else {
                return Ok(None);
            };
            String::from_utf8(sealed).unwrap_or_default()
        }
    };
    Ok(sealer.open(&sealed).ok().and_then(|b| Envelope::decode(&b)))
}

/// Opens a session cookie value and applies the timers. `Ok(None)` = treat
/// as unauthenticated (absent/expired/tampered/junk id/legacy blob/timer
/// exceeded); `Err` = store outage (503, never 401). The caller decodes
/// `envelope.payload`, and — once it knows the subject — completes any
/// due renewal with [`renew_if_due`].
pub async fn load(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
    timeouts: &SessionTimeouts,
) -> Result<Option<Opened>, StoreError> {
    let Some(envelope) = fetch_envelope(backend, sealer, cookie_value).await? else {
        return Ok(None);
    };
    let now = now_unix();
    if !envelope.is_valid(timeouts, now) {
        return Ok(None);
    }
    let renewal = envelope.renewal_due(timeouts, now);
    Ok(Some(Opened { envelope, renewal }))
}

/// Completes the renewal [`load`] found due, if any, and returns the
/// `Set-Cookie` to attach to the response. Cookie mode re-seals the
/// envelope with fresh timestamps. Redis mode takes the per-session lock
/// shared with the `openid-connect` token refresh so a touch can never
/// overwrite a concurrent refresh: when the lock is held the renewal is
/// skipped for this request (the holder's write carries fresh timestamps);
/// otherwise the record is re-read under the lock, renewed, `put` under the
/// same id, and the browser gets the same id with a fresh `Max-Age`.
#[allow(clippy::too_many_arguments)]
pub async fn renew_if_due(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
    opened: &Opened,
    timeouts: &SessionTimeouts,
    meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
) -> Result<Option<String>, StoreError> {
    if opened.renewal == Renewal::None {
        return Ok(None);
    }
    let now = now_unix();
    match backend {
        SessionBackend::Cookie => {
            let mut envelope = opened.envelope.clone();
            envelope.apply(opened.renewal, now);
            persist(
                backend,
                sealer,
                None,
                &envelope,
                timeouts,
                meta,
                cookie_name,
                attrs,
                now,
            )
            .await
            .map(Some)
        }
        SessionBackend::Store { store, .. } => {
            let Some(id) = SessionId::parse(cookie_value) else {
                return Ok(None);
            };
            if !store.try_lock(&id, Duration::from_secs(10)).await? {
                return Ok(None);
            }
            let result = async {
                let Some(mut envelope) = fetch_envelope(backend, sealer, cookie_value).await?
                else {
                    return Ok(None);
                };
                let renewal = envelope.renewal_due(timeouts, now);
                if renewal == Renewal::None {
                    return Ok(None);
                }
                envelope.apply(renewal, now);
                persist(
                    backend,
                    sealer,
                    Some(id.clone()),
                    &envelope,
                    timeouts,
                    meta,
                    cookie_name,
                    attrs,
                    now,
                )
                .await
                .map(Some)
            }
            .await;
            // Best effort: the lock self-heals via its TTL, and a failed
            // unlock must not turn a persisted renewal into a 503.
            if let Err(e) = store.unlock(&id).await {
                tracing::warn!("session renewal unlock failed (self-heals via TTL): {e}");
            }
            result
        }
    }
}

/// Revokes the session and returns the delete-cookie header value. A store
/// delete failure is an Err — server-side revocation is the entire point of
/// redis mode, so a logout that silently leaves the session live must fail
/// loudly (503) instead.
pub async fn destroy(
    backend: &SessionBackend,
    cookie_value: Option<&str>,
    cookie_name: &str,
    path: &str,
) -> Result<String, StoreError> {
    if let (SessionBackend::Store { store, .. }, Some(value)) = (backend, cookie_value) {
        if let Some(id) = SessionId::parse(value) {
            store.delete(&id).await?;
        }
    }
    Ok(delete_cookie(cookie_name, path))
}

/// Rewrites an existing session's payload (token refresh) as a full re-save
/// that keeps the login time `created_at`, so a refresh never extends the
/// absolute timeout. Redis mode puts under the SAME id. Either way the
/// returned `Set-Cookie` must reach the browser (cookie mode: the new blob;
/// redis mode: the same id with a fresh `Max-Age`).
#[allow(clippy::too_many_arguments)]
pub async fn update(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
    created_at: u64,
    payload: &[u8],
    timeouts: &SessionTimeouts,
    meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
) -> Result<String, StoreError> {
    let now = now_unix();
    let envelope = Envelope {
        created_at,
        renewed_at: now,
        touched_at: now,
        payload: payload.to_vec(),
    };
    let id = match backend {
        SessionBackend::Cookie => None,
        // A junk id cannot have loaded a session; persisting under a fresh
        // id would orphan the browser's cookie, so this is a plain outage.
        SessionBackend::Store { .. } => Some(
            SessionId::parse(cookie_value)
                .ok_or_else(|| StoreError("session update: malformed session id".to_string()))?,
        ),
    };
    persist(
        backend,
        sealer,
        id,
        &envelope,
        timeouts,
        meta,
        cookie_name,
        attrs,
        now,
    )
    .await
}

/// Seals an envelope with explicit timestamps, for tests that need a
/// session in a chosen state without sleeping.
#[cfg(test)]
pub fn seal_envelope_for_test(
    sealer: &CookieSealer,
    payload: &[u8],
    created_at: u64,
    renewed_at: u64,
    touched_at: u64,
    ttl: Duration,
) -> String {
    let envelope = Envelope {
        created_at,
        renewed_at,
        touched_at,
        payload: payload.to_vec(),
    };
    sealer.seal(&envelope.encode(), ttl)
}

/// Seals `payload` as a session established right now (ten-minute seal).
#[cfg(test)]
pub fn seal_fresh_for_test(sealer: &CookieSealer, payload: &[u8]) -> String {
    let now = now_unix();
    seal_envelope_for_test(sealer, payload, now, now, now, Duration::from_secs(600))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::FakeSessionStore;

    fn store_backend(fake: Arc<FakeSessionStore>) -> SessionBackend {
        SessionBackend::Store {
            store: fake,
            store_name: "s1".to_string(),
        }
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            id: String::new(),
            subject: "alice".to_string(),
            plugin: "test".to_string(),
            policy: String::new(),
            route: String::new(),
            created_at: 0,
            expires_at: 0,
        }
    }

    fn cookie_value(set_cookie: &str, name: &str) -> String {
        set_cookie
            .strip_prefix(&format!("{name}="))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    fn max_age(set_cookie: &str) -> u64 {
        set_cookie
            .split("; ")
            .find_map(|a| a.strip_prefix("Max-Age="))
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn establish_default(backend: &SessionBackend, sealer: &CookieSealer) -> String {
        establish(
            backend,
            sealer,
            b"payload",
            &SessionTimeouts::default(),
            meta(),
            "test_session",
            &CookieAttrs::default(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_establish_then_load_fresh_session_no_renewal() {
        let sealer = CookieSealer::new("k");
        let set = establish_default(&SessionBackend::Cookie, &sealer).await;
        assert_eq!(max_age(&set), 3600, "Max-Age is the rolling timeout: {set}");
        let value = cookie_value(&set, "test_session");
        let opened = load(
            &SessionBackend::Cookie,
            &sealer,
            &value,
            &SessionTimeouts::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(opened.envelope.payload, b"payload");
        assert_eq!(opened.renewal, Renewal::None);
        assert!(now_unix() - opened.envelope.created_at <= 1);
    }

    #[tokio::test]
    async fn test_store_mode_round_trip_id_cookie_and_revocation() {
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake.clone());
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts::default();
        let set = establish_default(&backend, &sealer).await;
        let value = cookie_value(&set, "test_session");
        // The cookie is a bare 32-hex id, far under 4 KB, not the payload.
        assert_eq!(value.len(), 32);
        assert!(crate::sessions::SessionId::parse(&value).is_some());

        let opened = load(&backend, &sealer, &value, &t).await.unwrap().unwrap();
        assert_eq!(opened.envelope.payload, b"payload");
        // Junk cookie values are unauthenticated, not errors.
        assert!(load(&backend, &sealer, "not-an-id", &t)
            .await
            .unwrap()
            .is_none());

        destroy(&backend, Some(&value), "test_session", "/")
            .await
            .unwrap();
        assert!(load(&backend, &sealer, &value, &t).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_store_outage_is_error_not_unauthenticated() {
        let fake = Arc::new(FakeSessionStore::default());
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let backend = store_backend(fake);
        let sealer = CookieSealer::new("k");
        let id = crate::sessions::SessionId::random();
        assert!(
            load(&backend, &sealer, id.as_str(), &SessionTimeouts::default())
                .await
                .is_err()
        );
        assert!(destroy(&backend, Some(id.as_str()), "n", "/")
            .await
            .is_err());
    }

    /// A blob sealed by the pre-envelope code (the JSON payload directly) is
    /// not a session: one re-login at upgrade, never a 503.
    #[tokio::test]
    async fn test_load_rejects_legacy_blob() {
        let sealer = CookieSealer::new("k");
        let legacy = sealer.seal(b"{\"x\":1}", Duration::from_secs(60));
        assert!(load(
            &SessionBackend::Cookie,
            &sealer,
            &legacy,
            &SessionTimeouts::default()
        )
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn test_load_rejects_idle_session_and_renews_touched_one() {
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts::default();
        let now = now_unix();
        let idle = seal_envelope_for_test(
            &sealer,
            b"p",
            now - 1000,
            now - 1000,
            now - 901,
            Duration::from_secs(600),
        );
        assert!(load(&SessionBackend::Cookie, &sealer, &idle, &t)
            .await
            .unwrap()
            .is_none());

        let touched = seal_envelope_for_test(
            &sealer,
            b"p",
            now - 1000,
            now - 1000,
            now - 61,
            Duration::from_secs(600),
        );
        let opened = load(&SessionBackend::Cookie, &sealer, &touched, &t)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.renewal, Renewal::Touch);
        let set = renew_if_due(
            &SessionBackend::Cookie,
            &sealer,
            &touched,
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap()
        .expect("a touch re-issues the cookie");
        let again = load(
            &SessionBackend::Cookie,
            &sealer,
            &cookie_value(&set, "n"),
            &t,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(again.renewal, Renewal::None);
        assert!(now_unix() - again.envelope.touched_at <= 1);
        assert_eq!(
            again.envelope.renewed_at,
            now - 1000,
            "a touch does not roll"
        );
        assert_eq!(again.envelope.payload, b"p");
    }

    #[tokio::test]
    async fn test_roll_keeps_created_at_in_cookie_and_redis() {
        use crate::sessions::SessionStore as _;
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts::default();
        let now = now_unix();
        let created = now - 5000;

        // Cookie mode.
        let due = seal_envelope_for_test(
            &sealer,
            b"p",
            created,
            now - 2701,
            now - 10,
            Duration::from_secs(600),
        );
        let opened = load(&SessionBackend::Cookie, &sealer, &due, &t)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.renewal, Renewal::Roll);
        let set = renew_if_due(
            &SessionBackend::Cookie,
            &sealer,
            &due,
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap()
        .unwrap();
        let rolled = load(
            &SessionBackend::Cookie,
            &sealer,
            &cookie_value(&set, "n"),
            &t,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(rolled.envelope.created_at, created);
        assert!(now_unix() - rolled.envelope.renewed_at <= 1);

        // Redis mode: same id, record rewritten, meta keeps the login time.
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake.clone());
        let id = crate::sessions::SessionId::random();
        fake.put(&id, due.as_bytes(), Duration::from_secs(600), &meta())
            .await
            .unwrap();
        let opened = load(&backend, &sealer, id.as_str(), &t)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.renewal, Renewal::Roll);
        let set = renew_if_due(
            &backend,
            &sealer,
            id.as_str(),
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            cookie_value(&set, "n"),
            id.as_str(),
            "redis mode keeps the id"
        );
        assert_eq!(max_age(&set), t.ttl(created, now_unix()).as_secs());
        let rolled = load(&backend, &sealer, id.as_str(), &t)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rolled.envelope.created_at, created);
        assert!(now_unix() - rolled.envelope.renewed_at <= 1);
        let listed = fake
            .list(&crate::sessions::SessionFilter::default())
            .await
            .unwrap();
        let m = listed
            .sessions
            .iter()
            .find(|m| m.id == id.as_str())
            .unwrap();
        assert_eq!(m.created_at, created);
        assert_eq!(m.subject, "alice");
        assert!(m.expires_at >= now_unix() + t.ttl(created, now_unix()).as_secs() - 1);
        // The refresh lock is free again.
        assert!(fake.try_lock(&id, Duration::from_secs(1)).await.unwrap());
    }

    #[tokio::test]
    async fn test_two_loads_of_same_due_blob_both_renew_successfully() {
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts::default();
        let now = now_unix();
        let due = seal_envelope_for_test(
            &sealer,
            b"p",
            now - 100,
            now - 100,
            now - 61,
            Duration::from_secs(600),
        );
        let opened = load(&SessionBackend::Cookie, &sealer, &due, &t)
            .await
            .unwrap()
            .unwrap();
        let a = renew_if_due(
            &SessionBackend::Cookie,
            &sealer,
            &due,
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap()
        .unwrap();
        let b = renew_if_due(
            &SessionBackend::Cookie,
            &sealer,
            &due,
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_ne!(a, b, "fresh nonce per seal");
        for set in [a, b] {
            assert!(load(
                &SessionBackend::Cookie,
                &sealer,
                &cookie_value(&set, "n"),
                &t
            )
            .await
            .unwrap()
            .is_some());
        }
    }

    #[tokio::test]
    async fn test_update_keeps_created_at_and_caps_ttl_by_absolute() {
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts {
            idling: 900,
            rolling: 3600,
            absolute: 5000,
        };
        let now = now_unix();
        let created = now - 3000;
        let set = update(
            &SessionBackend::Cookie,
            &sealer,
            "ignored-in-cookie-mode",
            created,
            b"v2",
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        assert_eq!(max_age(&set), 2000);
        let opened = load(
            &SessionBackend::Cookie,
            &sealer,
            &cookie_value(&set, "n"),
            &t,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(opened.envelope.created_at, created);
        assert_eq!(opened.envelope.payload, b"v2");
        assert!(now_unix() - opened.envelope.renewed_at <= 1);
    }

    #[tokio::test]
    async fn test_update_keeps_id_in_store_mode() {
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake);
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts::default();
        let set = establish(
            &backend,
            &sealer,
            b"v1",
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        let value = cookie_value(&set, "n");
        let set2 = update(
            &backend,
            &sealer,
            &value,
            now_unix(),
            b"v2",
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        assert_eq!(cookie_value(&set2, "n"), value, "store mode keeps the id");
        assert_eq!(
            load(&backend, &sealer, &value, &t)
                .await
                .unwrap()
                .unwrap()
                .envelope
                .payload,
            b"v2"
        );
    }

    #[tokio::test]
    async fn test_store_failure_on_renew_is_err() {
        use crate::sessions::SessionStore as _;
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake.clone());
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts::default();
        let now = now_unix();
        let due = seal_envelope_for_test(
            &sealer,
            b"p",
            now - 100,
            now - 100,
            now - 61,
            Duration::from_secs(600),
        );
        let id = crate::sessions::SessionId::random();
        fake.put(&id, due.as_bytes(), Duration::from_secs(600), &meta())
            .await
            .unwrap();
        let opened = load(&backend, &sealer, id.as_str(), &t)
            .await
            .unwrap()
            .unwrap();
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(renew_if_due(
            &backend,
            &sealer,
            id.as_str(),
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default()
        )
        .await
        .is_err());
    }

    /// Redis-mode renewal must not clobber a concurrent token refresh: it
    /// skips when the per-session lock is held, and otherwise re-reads the
    /// record under the lock before writing new timestamps onto it.
    #[tokio::test]
    async fn test_redis_renew_skips_when_lock_held_and_rereads_under_lock() {
        use crate::sessions::SessionStore as _;
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake.clone());
        let sealer = CookieSealer::new("k");
        let t = SessionTimeouts::default();
        let now = now_unix();
        let due = seal_envelope_for_test(
            &sealer,
            b"old-tokens",
            now - 100,
            now - 100,
            now - 61,
            Duration::from_secs(600),
        );
        let id = crate::sessions::SessionId::random();
        fake.put(&id, due.as_bytes(), Duration::from_secs(600), &meta())
            .await
            .unwrap();
        let opened = load(&backend, &sealer, id.as_str(), &t)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opened.renewal, Renewal::Touch);

        // Lock held by a refresher: skip, record untouched.
        assert!(fake.try_lock(&id, Duration::from_secs(10)).await.unwrap());
        let out = renew_if_due(
            &backend,
            &sealer,
            id.as_str(),
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        assert!(out.is_none());
        assert_eq!(fake.get(&id).await.unwrap().unwrap(), due.as_bytes());
        fake.unlock(&id).await.unwrap();

        // A refresh landed between our load and our renewal: the re-read
        // under the lock sees the newer payload and keeps it.
        let newer = seal_envelope_for_test(
            &sealer,
            b"new-tokens",
            now - 100,
            now - 100,
            now - 61,
            Duration::from_secs(600),
        );
        fake.put(&id, newer.as_bytes(), Duration::from_secs(600), &meta())
            .await
            .unwrap();
        let out = renew_if_due(
            &backend,
            &sealer,
            id.as_str(),
            &opened,
            &t,
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        assert!(out.is_some());
        let after = load(&backend, &sealer, id.as_str(), &t)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.envelope.payload, b"new-tokens");
        assert!(now_unix() - after.envelope.touched_at <= 1);
        assert!(
            fake.try_lock(&id, Duration::from_secs(1)).await.unwrap(),
            "lock released"
        );
    }

    #[test]
    fn test_parse_backend_errors() {
        use crate::plugins::resources::PluginResources;
        let resources = PluginResources::empty();
        let cfg = |json: serde_json::Value| -> HashMap<String, serde_json::Value> {
            serde_json::from_value(json).unwrap()
        };
        assert!(matches!(
            parse_backend(&cfg(serde_json::json!({})), &resources, "p").unwrap(),
            SessionBackend::Cookie
        ));
        let err = parse_backend(
            &cfg(serde_json::json!({"session": {"storage": "redis"}})),
            &resources,
            "p",
        )
        .unwrap_err();
        assert!(err.contains("requires 'session.store'"), "{err}");
        let err = parse_backend(
            &cfg(serde_json::json!({"session": {"storage": "memcached"}})),
            &resources,
            "p",
        )
        .unwrap_err();
        assert!(err.contains("unknown session.storage"), "{err}");
        let err = parse_backend(
            &cfg(serde_json::json!({"session_storage": "redis", "session_store": "nope"})),
            &resources,
            "p",
        )
        .unwrap_err();
        assert!(err.contains("'nope'"), "{err}");
    }

    // ---- timeouts, envelope and renewal rules (pure, time-injected) ----

    fn cfg(v: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn test_timeouts_default_matches_lua_resty_session() {
        assert_eq!(
            SessionTimeouts::default(),
            SessionTimeouts {
                idling: 900,
                rolling: 3600,
                absolute: 86400
            }
        );
    }

    #[test]
    fn test_parse_timeouts_nested_flat_and_lifetime_mapping() {
        let c = cfg(serde_json::json!({
            "session": {"idling_timeout": 10, "cookie": {"lifetime": 500}},
            "session_rolling_timeout": 20
        }));
        assert_eq!(
            parse_timeouts(&c, "p").unwrap(),
            SessionTimeouts {
                idling: 10,
                rolling: 20,
                absolute: 500
            }
        );
        // An explicit absolute_timeout wins over the deprecated lifetime.
        let c = cfg(serde_json::json!({
            "session": {"absolute_timeout": 7, "cookie": {"lifetime": 500}}
        }));
        assert_eq!(parse_timeouts(&c, "p").unwrap().absolute, 7);
        // The flat UI form of the deprecated key maps too.
        let c = cfg(serde_json::json!({"session_cookie_lifetime": 42}));
        assert_eq!(parse_timeouts(&c, "p").unwrap().absolute, 42);
        // Nothing set: lua-resty-session defaults.
        assert_eq!(
            parse_timeouts(&HashMap::new(), "p").unwrap(),
            SessionTimeouts::default()
        );
        let err = parse_timeouts(
            &cfg(serde_json::json!({"session": {"rolling_timeout": -1}})),
            "p",
        )
        .unwrap_err();
        assert!(err.contains("rolling_timeout"), "{err}");
    }

    #[test]
    fn test_ttl_is_min_of_rolling_and_absolute_remainder_ignoring_zero() {
        let t = SessionTimeouts {
            idling: 0,
            rolling: 3600,
            absolute: 5000,
        };
        assert_eq!(t.ttl(1000, 1000).as_secs(), 3600);
        // Absolute remainder caps the rolling window.
        assert_eq!(t.ttl(1000, 3000).as_secs(), 3000);
        let abs_only = SessionTimeouts {
            idling: 0,
            rolling: 0,
            absolute: 100,
        };
        assert_eq!(abs_only.ttl(0, 60).as_secs(), 40);
        let none = SessionTimeouts {
            idling: 0,
            rolling: 0,
            absolute: 0,
        };
        assert_eq!(none.ttl(0, 0).as_secs(), MAX_TTL_SECS);
        let past = SessionTimeouts {
            idling: 0,
            rolling: 10,
            absolute: 100,
        };
        assert_eq!(past.ttl(0, 100).as_secs(), 0);
    }

    #[test]
    fn test_envelope_round_trip_and_legacy_rejected() {
        let e = Envelope::new(b"{\"a\":1}".to_vec(), 1234);
        assert_eq!(
            (e.created_at, e.renewed_at, e.touched_at),
            (1234, 1234, 1234)
        );
        let bytes = e.encode();
        assert_eq!(bytes[0], ENVELOPE_VERSION);
        assert_eq!(bytes.len(), 25 + 7);
        assert_eq!(Envelope::decode(&bytes).unwrap(), e);
        // A pre-envelope blob (JSON payload sealed directly) is not a session.
        assert!(Envelope::decode(b"{\"claims\":{}}").is_none());
        assert!(Envelope::decode(&bytes[..20]).is_none());
    }

    #[test]
    fn test_validity_each_timer_and_zero_disables() {
        let t = SessionTimeouts::default();
        let e = Envelope {
            created_at: 0,
            renewed_at: 0,
            touched_at: 0,
            payload: vec![],
        };
        // idling: 900 s since the last touch
        assert!(e.is_valid(&t, 900));
        assert!(!e.is_valid(&t, 901));
        // rolling: 3600 s since the last re-save, touched recently
        let e2 = Envelope {
            touched_at: 3500,
            ..e.clone()
        };
        assert!(e2.is_valid(&t, 3600));
        assert!(!e2.is_valid(&t, 3601));
        // absolute: 86400 s since login, renewed and touched recently
        let e3 = Envelope {
            renewed_at: 86000,
            touched_at: 86000,
            ..e.clone()
        };
        assert!(e3.is_valid(&t, 86400));
        assert!(!e3.is_valid(&t, 86401));
        let off = SessionTimeouts {
            idling: 0,
            rolling: 0,
            absolute: 0,
        };
        assert!(e.is_valid(&off, u64::MAX / 2));
    }

    #[test]
    fn test_renewal_thresholds() {
        let t = SessionTimeouts::default();
        let e = Envelope {
            created_at: 0,
            renewed_at: 0,
            touched_at: 0,
            payload: vec![],
        };
        assert_eq!(e.renewal_due(&t, 60), Renewal::None);
        assert_eq!(e.renewal_due(&t, 61), Renewal::Touch);
        // Exactly 75% of the rolling timeout is not yet a roll.
        assert_eq!(e.renewal_due(&t, 2700), Renewal::Touch);
        assert_eq!(e.renewal_due(&t, 2701), Renewal::Roll);
        let no_roll = SessionTimeouts { rolling: 0, ..t };
        assert_eq!(e.renewal_due(&no_roll, 5000), Renewal::Touch);
        let none = SessionTimeouts {
            idling: 0,
            rolling: 0,
            absolute: 0,
        };
        assert_eq!(e.renewal_due(&none, 5000), Renewal::None);

        let mut r = e.clone();
        r.apply(Renewal::Roll, 3000);
        assert_eq!((r.created_at, r.renewed_at, r.touched_at), (0, 3000, 3000));
        let mut tch = e.clone();
        tch.apply(Renewal::Touch, 100);
        assert_eq!(
            (tch.created_at, tch.renewed_at, tch.touched_at),
            (0, 0, 100)
        );
        // Applying the same renewal twice at the same instant is idempotent.
        let mut again = r.clone();
        again.apply(Renewal::Roll, 3000);
        assert_eq!(again, r);
    }
}
