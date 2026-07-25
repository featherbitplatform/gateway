//! Shared upstream load balancer.
//!
//! A [`Balancer`] owns a pool of backend [`Target`]s and picks one per
//! request/connection according to a [`Strategy`] (round-robin,
//! least-connections, or client-IP hash). It is deliberately transport-neutral:
//! the HTTP `upstream` node and the L4 (TCP/UDP) stream proxy both select
//! targets through it, so the load-balancing logic lives in exactly one place.
//!
//! `least_connections` is backed by per-target in-flight counters. Callers bump
//! a counter for the life of a request/connection by holding a guard:
//! [`ConnGuard`] (borrowed — held on the stack for a single request) or
//! [`OwnedConnGuard`] (owns an `Arc<Balancer>` — for a long-lived connection
//! stored outside the borrowing scope, e.g. a UDP session in a map). Both
//! decrement on drop.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::Deserialize;

/// A single backend address (`host:port`) connections can be forwarded to.
#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// Strategy for picking a target from the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Cycles through targets in order (the default).
    #[default]
    RoundRobin,
    /// Picks the target with the fewest in-flight connections.
    LeastConnections,
    /// Hashes the client IP (port stripped) so a client sticks to one target.
    IpHash,
}

impl Strategy {
    /// Accepts spec spelling (`round_robin`, `least_connections`, `ip_hash`)
    /// plus the hyphenated/short variants older UI configs saved
    /// (`round-robin`, `least-conn`).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_lowercase().replace('-', "_").as_str() {
            "round_robin" => Ok(Self::RoundRobin),
            "least_connections" | "least_conn" => Ok(Self::LeastConnections),
            "ip_hash" => Ok(Self::IpHash),
            other => Err(format!(
                "Unknown load_balancing '{}' — supported: round_robin, least_connections, ip_hash",
                other
            )),
        }
    }
}

/// A pool of backend targets plus the state driving target selection.
pub struct Balancer {
    targets: Vec<Target>,
    strategy: Strategy,
    /// Monotonic pick counter driving round-robin selection.
    counter: AtomicUsize,
    /// Per-target in-flight counts (parallel to `targets`), used by
    /// least-connections selection and kept current via the guards.
    in_flight: Vec<AtomicUsize>,
}

impl Balancer {
    /// Builds a balancer over `targets`. Errors if the pool is empty.
    pub fn new(targets: Vec<Target>, strategy: Strategy) -> Result<Self, String> {
        if targets.is_empty() {
            return Err("load balancer requires at least one target".to_string());
        }
        let in_flight = targets.iter().map(|_| AtomicUsize::new(0)).collect();
        Ok(Self {
            targets,
            strategy,
            counter: AtomicUsize::new(0),
            in_flight,
        })
    }

    /// Picks the index of the target to use; `remote_addr` (client `ip:port`)
    /// is only consulted for `ip_hash`.
    pub fn select(&self, remote_addr: &str) -> usize {
        match self.strategy {
            Strategy::RoundRobin => {
                self.counter.fetch_add(1, Ordering::Relaxed) % self.targets.len()
            }
            Strategy::LeastConnections => self
                .in_flight
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.load(Ordering::Relaxed))
                .map(|(i, _)| i)
                .unwrap_or(0),
            Strategy::IpHash => {
                use std::hash::{Hash, Hasher};
                // Hash only the IP so all connections from one client stick to
                // the same target regardless of ephemeral port.
                let ip = remote_addr
                    .rsplit_once(':')
                    .map_or(remote_addr, |(ip, _)| ip);
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                ip.hash(&mut hasher);
                (hasher.finish() as usize) % self.targets.len()
            }
        }
    }

    /// The target at `idx` (from a prior [`select`](Self::select)).
    pub fn target(&self, idx: usize) -> &Target {
        &self.targets[idx]
    }

    /// Number of targets in the pool.
    // Pool accessors: `in_flight_count`/`strategy` are exercised by tests;
    // `len`/`is_empty` round out the API.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// Whether the pool is empty (always false for a constructed `Balancer`).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Current in-flight count for a target (for tests and future metrics).
    #[allow(dead_code)]
    pub fn in_flight_count(&self, idx: usize) -> usize {
        self.in_flight[idx].load(Ordering::Relaxed)
    }

    /// The configured strategy.
    #[allow(dead_code)]
    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Increments the in-flight count for `idx`, returning a borrowed guard that
    /// decrements it on drop. For a request/connection whose lifetime is a
    /// single stack scope (HTTP request, TCP connection).
    pub fn acquire(&self, idx: usize) -> ConnGuard<'_> {
        self.in_flight[idx].fetch_add(1, Ordering::Relaxed);
        ConnGuard(&self.in_flight[idx])
    }

    /// Like [`acquire`](Self::acquire) but owns an `Arc<Balancer>`, so the guard
    /// can be stored past the borrowing scope (e.g. a UDP session in a map).
    pub fn owned_acquire(self: &Arc<Self>, idx: usize) -> OwnedConnGuard {
        self.in_flight[idx].fetch_add(1, Ordering::Relaxed);
        OwnedConnGuard {
            balancer: self.clone(),
            idx,
        }
    }
}

/// Decrements a target's in-flight counter when a request/connection completes.
pub struct ConnGuard<'a>(&'a AtomicUsize);

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Owned counterpart to [`ConnGuard`] for connections tracked outside the
/// borrowing scope; decrements the target's in-flight counter on drop.
pub struct OwnedConnGuard {
    balancer: Arc<Balancer>,
    idx: usize,
}

impl Drop for OwnedConnGuard {
    fn drop(&mut self) {
        self.balancer.in_flight[self.idx].fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(n: usize) -> Vec<Target> {
        (0..n)
            .map(|i| Target {
                host: format!("backend-{}", i),
                port: 3000,
            })
            .collect()
    }

    #[test]
    fn test_strategy_parse_aliases() {
        assert_eq!(
            Strategy::parse("round_robin").unwrap(),
            Strategy::RoundRobin
        );
        assert_eq!(
            Strategy::parse("round-robin").unwrap(),
            Strategy::RoundRobin
        );
        assert_eq!(
            Strategy::parse("least_connections").unwrap(),
            Strategy::LeastConnections
        );
        assert_eq!(
            Strategy::parse("least-conn").unwrap(),
            Strategy::LeastConnections
        );
        assert_eq!(Strategy::parse("IP_HASH").unwrap(), Strategy::IpHash);
        assert!(Strategy::parse("random").is_err());
    }

    #[test]
    fn test_new_rejects_empty_pool() {
        assert!(Balancer::new(vec![], Strategy::RoundRobin).is_err());
    }

    #[test]
    fn test_round_robin_cycles() {
        let b = Balancer::new(targets(3), Strategy::RoundRobin).unwrap();
        let picks: Vec<usize> = (0..6).map(|_| b.select("1.2.3.4:555")).collect();
        assert_eq!(picks, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn test_ip_hash_is_sticky_per_ip() {
        let b = Balancer::new(targets(3), Strategy::IpHash).unwrap();
        let a = b.select("10.0.0.1:1111");
        // same IP, different ephemeral port -> same target
        assert_eq!(a, b.select("10.0.0.1:2222"));
        assert_eq!(a, b.select("10.0.0.1:3333"));
    }

    #[test]
    fn test_least_connections_picks_idle_target_via_guards() {
        let b = Balancer::new(targets(3), Strategy::LeastConnections).unwrap();
        // Load targets 0 and 2 heavily, leave 1 idle.
        let _g0a = b.acquire(0);
        let _g0b = b.acquire(0);
        let _g2 = b.acquire(2);
        assert_eq!(b.select("1.2.3.4:555"), 1);
        assert_eq!(b.in_flight_count(0), 2);
        assert_eq!(b.in_flight_count(2), 1);
    }

    #[test]
    fn test_guard_decrements_on_drop() {
        let b = Balancer::new(targets(2), Strategy::LeastConnections).unwrap();
        {
            let _g = b.acquire(0);
            assert_eq!(b.in_flight_count(0), 1);
        }
        assert_eq!(b.in_flight_count(0), 0);
    }

    #[test]
    fn test_owned_guard_decrements_on_drop() {
        let b = Arc::new(Balancer::new(targets(2), Strategy::LeastConnections).unwrap());
        let g = b.owned_acquire(1);
        assert_eq!(b.in_flight_count(1), 1);
        drop(g);
        assert_eq!(b.in_flight_count(1), 0);
    }
}
