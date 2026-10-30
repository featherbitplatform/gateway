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
pub fn meta_now(ctx: &Context, plugin: &str, subject: &str, ttl: Duration) -> SessionMeta {
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
        expires_at: now.saturating_add(ttl.as_secs()),
    }
}

/// Seals `payload` and returns the session Set-Cookie value. Cookie mode:
/// the sealed blob IS the cookie. Redis mode: the sealed blob goes under a
/// fresh random id; the cookie carries the bare id.
pub async fn establish(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    payload: &[u8],
    ttl: Duration,
    meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
) -> Result<String, StoreError> {
    let sealed = sealer.seal(payload, ttl);
    match backend {
        SessionBackend::Cookie => Ok(build_set_cookie(cookie_name, &sealed, attrs)),
        SessionBackend::Store { store, .. } => {
            let id = SessionId::random();
            store.put(&id, sealed.as_bytes(), ttl, &meta).await?;
            Ok(build_set_cookie(cookie_name, id.as_str(), attrs))
        }
    }
}

/// Opens a session cookie value. `Ok(None)` = treat as unauthenticated
/// (absent/expired/tampered/junk id); `Err` = store outage (503, never 401).
pub async fn load(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    match backend {
        SessionBackend::Cookie => Ok(sealer.open(cookie_value).ok()),
        SessionBackend::Store { store, .. } => {
            let Some(id) = SessionId::parse(cookie_value) else {
                return Ok(None);
            };
            let Some(sealed) = store.get(&id).await? else {
                return Ok(None);
            };
            let sealed = String::from_utf8(sealed).unwrap_or_default();
            Ok(sealer.open(&sealed).ok())
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

/// Rewrites an existing session's payload (token refresh). Redis mode: put
/// under the SAME id (cookie unchanged → returns None). Cookie mode: the
/// caller must send a fresh cookie (returns Some(set_cookie)).
#[allow(clippy::too_many_arguments)] // matches the locked interface in the task-5 brief
pub async fn update(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
    payload: &[u8],
    ttl: Duration,
    meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
) -> Result<Option<String>, StoreError> {
    let sealed = sealer.seal(payload, ttl);
    match backend {
        SessionBackend::Cookie => Ok(Some(build_set_cookie(cookie_name, &sealed, attrs))),
        SessionBackend::Store { store, .. } => {
            let Some(id) = SessionId::parse(cookie_value) else {
                return Ok(None);
            };
            store.put(&id, sealed.as_bytes(), ttl, &meta).await?;
            Ok(None)
        }
    }
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

    #[tokio::test]
    async fn test_cookie_mode_matches_legacy_shape() {
        let sealer = CookieSealer::new("k");
        let set = establish(
            &SessionBackend::Cookie,
            &sealer,
            b"payload",
            Duration::from_secs(60),
            meta(),
            "test_session",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        // Legacy shape: the sealed blob is the cookie value itself.
        let value = set
            .strip_prefix("test_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            load(&SessionBackend::Cookie, &sealer, &value)
                .await
                .unwrap()
                .as_deref(),
            Some(&b"payload"[..])
        );
    }

    #[tokio::test]
    async fn test_store_mode_round_trip_id_cookie_and_revocation() {
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake.clone());
        let sealer = CookieSealer::new("k");
        let set = establish(
            &backend,
            &sealer,
            b"payload",
            Duration::from_secs(60),
            meta(),
            "test_session",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        let value = set
            .strip_prefix("test_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        // The cookie is a bare 32-hex id, far under 4 KB, not the payload.
        assert_eq!(value.len(), 32);
        assert!(crate::sessions::SessionId::parse(&value).is_some());

        assert_eq!(
            load(&backend, &sealer, &value).await.unwrap().as_deref(),
            Some(&b"payload"[..])
        );
        // Junk cookie values are unauthenticated, not errors.
        assert_eq!(load(&backend, &sealer, "not-an-id").await.unwrap(), None);

        // Destroy revokes server-side.
        destroy(&backend, Some(&value), "test_session", "/")
            .await
            .unwrap();
        assert_eq!(load(&backend, &sealer, &value).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_store_outage_is_error_not_unauthenticated() {
        let fake = Arc::new(FakeSessionStore::default());
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let backend = store_backend(fake);
        let sealer = CookieSealer::new("k");
        let id = crate::sessions::SessionId::random();
        assert!(load(&backend, &sealer, id.as_str()).await.is_err());
        assert!(destroy(&backend, Some(id.as_str()), "n", "/")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_update_keeps_id_in_store_mode() {
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake);
        let sealer = CookieSealer::new("k");
        let set = establish(
            &backend,
            &sealer,
            b"v1",
            Duration::from_secs(60),
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        let value = set
            .strip_prefix("n=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let out = update(
            &backend,
            &sealer,
            &value,
            b"v2",
            Duration::from_secs(60),
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "store mode must not reissue the cookie");
        assert_eq!(
            load(&backend, &sealer, &value).await.unwrap().as_deref(),
            Some(&b"v2"[..])
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
