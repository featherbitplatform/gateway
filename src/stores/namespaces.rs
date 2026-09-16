//! The key namespaces featherbit writes under a store's `key_prefix`.
//!
//! Every key is `{key_prefix}:{namespace}:...`. These must stay disjoint: a
//! policy-written key must never be able to name a key a managed subsystem
//! reads or writes, in either direction. Declaring them in one place -- and
//! testing the real builders against it -- is what keeps that true as
//! subsystems are added.
//!
//! The whole module is gated on `redis-store`: every subsystem it describes is,
//! so in a headless build there are no keys to namespace.

/// Rate-limit counters (`src/stores/counter.rs`).
pub const COUNTERS: &str = "cnt";
/// ACME account, certificate, challenge and lease state (`src/acme/storage/redis.rs`).
pub const ACME: &str = "acme";
/// Server-side sessions (`src/sessions/redis.rs`).
pub const SESSIONS: &str = "sess";

/// Policy-written keys: the `store-get`/`store-set`/`store-incr`/`store-delete` nodes.
pub const POLICY_KV: &str = "kv";

/// Namespaces owned by featherbit itself. A policy can never address these,
/// because every `store-*` key is prefixed with [`POLICY_KV`].
///
/// Test-only by design: nothing in production consults this list. It exists so
/// the disjointness it describes is asserted rather than assumed.
#[cfg(test)]
pub const MANAGED: &[&str] = &[COUNTERS, ACME, SESSIONS];

#[cfg(test)]
mod tests {
    use super::*;

    /// A policy must never be able to address a namespace featherbit owns.
    #[test]
    fn test_policy_namespace_is_not_managed() {
        assert!(!MANAGED.contains(&POLICY_KV));
    }

    #[test]
    fn test_all_namespaces_are_distinct() {
        let all = [COUNTERS, ACME, SESSIONS];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "namespaces must be pairwise distinct");
            }
        }
    }

    /// The drift guard: each subsystem's real key builder must still produce
    /// keys under its declared namespace. A subsystem that changes its prefix,
    /// or a new one that reuses `kv`, fails here.
    #[test]
    fn test_key_builders_stay_inside_their_declared_namespace() {
        let sess = crate::sessions::redis::sess_key("fb", "abc");
        assert!(sess.starts_with(&format!("fb:{}:", SESSIONS)), "{sess}");

        let acme = crate::acme::storage::redis::account_key("fb");
        assert!(acme.starts_with(&format!("fb:{}:", ACME)), "{acme}");

        let cnt = crate::stores::counter::window_key("fb", 7, "u1");
        assert!(cnt.starts_with(&format!("fb:{}:", COUNTERS)), "{cnt}");
    }
}
