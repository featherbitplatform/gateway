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
}
