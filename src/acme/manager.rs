//! The renewal scheduler: one task per managed certificate that decides when
//! to (re)issue, takes the storage lease so only one instance orders, runs
//! `order::issue`, persists, publishes into [`ManagedCerts`], and backs off on
//! failure. Instances that lose the lease poll storage and adopt whatever the
//! leaseholder wrote, refreshing challenge certs meanwhile so the CA may
//! validate through any of them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, Notify};
use tracing::{error, info, warn};

use super::challenge::TlsAlpnSolver;
use super::client::{AcmeClient, AcmeClientFactory};
use super::metrics::AcmeMetrics;
use super::order::{issue, KeyType};
use super::storage::CertStorage;
use super::{
    load_certified_key, now_unix, parse_cert_meta, publish, update, AcmeError, CertId, CertState,
    ManagedCert, ManagedCerts,
};

pub const LEASE_TTL: Duration = Duration::from_secs(300);
/// ARI is re-queried at most this often per certificate.
const ARI_INTERVAL_SECS: i64 = 3_600;

/// Unix time at which the certificate should be renewed: `renew_before` ahead of
/// expiry, or the start of the CA's ARI window if that comes first.
pub fn when_to_renew(not_after: i64, ari: Option<(i64, i64)>, renew_before_secs: i64) -> i64 {
    let by_window = not_after - renew_before_secs;
    match ari {
        Some((start, _)) => by_window.min(start),
        None => by_window,
    }
}

/// `60·2^(failures−1)` seconds, capped at one hour; `0` when there are no failures.
pub fn backoff_secs(failures: u32) -> u64 {
    if failures == 0 {
        return 0;
    }
    60u64
        .saturating_mul(1u64 << (failures - 1).min(10))
        .min(3_600)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenewOutcome {
    Scheduled,
    NotDue,
    InProgress,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ManagedSlot {
    pub id: CertId,
    pub domains: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ManagerConfig {
    pub key_type: KeyType,
    pub renew_before: Duration,
    /// How often an idle slot re-evaluates (ARI changes, peers' writes).
    pub poll_interval: Duration,
    pub lease_ttl: Duration,
    /// How often a non-leaseholder checks storage for a peer's certificate.
    pub peer_poll: Duration,
    /// How often a non-leaseholder refreshes challenge certs from storage.
    pub challenge_refresh: Duration,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            key_type: KeyType::EcdsaP256,
            renew_before: Duration::from_secs(30 * 86_400),
            poll_interval: Duration::from_secs(60),
            lease_ttl: LEASE_TTL,
            peer_poll: Duration::from_secs(60),
            challenge_refresh: Duration::from_secs(2),
        }
    }
}

struct SlotControl {
    waker: Notify,
    force: AtomicBool,
    in_flight: AtomicBool,
}

/// Aborts the wrapped task on drop — including when the future holding it is
/// simply cancelled (e.g. a slot task dropped on shutdown) rather than run to
/// completion, so a lease-keepalive loop can never outlive the order it was
/// keeping alive for. A plain `abort()` call reached only via a specific
/// success/error path does not cover that case.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct Manager {
    cfg: ManagerConfig,
    factory: Arc<dyn AcmeClientFactory>,
    client: Mutex<Option<Arc<dyn AcmeClient>>>,
    storage: Arc<dyn CertStorage>,
    solver: Arc<TlsAlpnSolver>,
    certs: ManagedCerts,
    slots: Vec<ManagedSlot>,
    controls: HashMap<String, SlotControl>,
    metrics: Option<Arc<AcmeMetrics>>,
    owner: String,
}

impl Manager {
    pub fn new(
        cfg: ManagerConfig,
        factory: Arc<dyn AcmeClientFactory>,
        storage: Arc<dyn CertStorage>,
        solver: Arc<TlsAlpnSolver>,
        certs: ManagedCerts,
        slots: Vec<ManagedSlot>,
        metrics: Option<Arc<AcmeMetrics>>,
    ) -> Arc<Self> {
        let controls = slots
            .iter()
            .map(|s| {
                (
                    s.id.as_str().to_string(),
                    SlotControl {
                        waker: Notify::new(),
                        force: AtomicBool::new(false),
                        in_flight: AtomicBool::new(false),
                    },
                )
            })
            .collect();
        let owner = format!(
            "{}:{}:{}",
            std::env::var("HOSTNAME").unwrap_or_else(|_| "gateway".into()),
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        Arc::new(Self {
            cfg,
            factory,
            client: Mutex::new(None),
            storage,
            solver,
            certs,
            slots,
            controls,
            metrics,
            owner,
        })
    }

    pub fn slots(&self) -> &[ManagedSlot] {
        &self.slots
    }

    pub fn in_flight(&self, id: &str) -> bool {
        self.controls
            .get(id)
            .is_some_and(|c| c.in_flight.load(Ordering::SeqCst))
    }

    /// Wakes the slot's task. Without `force`, a valid certificate outside its
    /// renewal window is left alone (`NotDue`) — Let's Encrypt's duplicate-cert
    /// limits are the classic footgun.
    pub fn renew_now(&self, id: &str, force: bool) -> RenewOutcome {
        let Some(ctl) = self.controls.get(id) else {
            return RenewOutcome::Unknown;
        };
        if ctl.in_flight.load(Ordering::SeqCst) {
            return RenewOutcome::InProgress;
        }
        let due = self
            .certs
            .load()
            .get(id)
            .map(|c| {
                c.state != CertState::Issued
                    || c.meta.next_renewal_at.is_some_and(|t| t <= now_unix())
            })
            .unwrap_or(true);
        if !due && !force {
            return RenewOutcome::NotDue;
        }
        ctl.force.store(true, Ordering::SeqCst);
        ctl.waker.notify_one();
        RenewOutcome::Scheduled
    }

    /// Spawns the per-slot loops and returns.
    pub async fn run(self: Arc<Self>) {
        for slot in self.slots.clone() {
            let me = self.clone();
            tokio::spawn(async move { me.run_slot(slot).await });
        }
    }

    async fn client(&self) -> Result<Arc<dyn AcmeClient>, AcmeError> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let c = self.factory.connect().await?;
        *guard = Some(c.clone());
        Ok(c)
    }

    fn snapshot(&self, id: &CertId) -> Option<ManagedCert> {
        self.certs.load().get(id.as_str()).cloned()
    }

    fn observe(&self, id: &CertId) {
        if let (Some(m), Some(c)) = (&self.metrics, self.snapshot(id)) {
            let not_after = if c.state == CertState::Placeholder {
                0
            } else {
                c.meta.not_after
            };
            m.observe(id.as_str(), c.state, not_after);
        }
    }

    async fn run_slot(self: Arc<Self>, slot: ManagedSlot) {
        let ctl = &self.controls[slot.id.as_str()];
        let mut failures: u32 = 0;
        let mut ari: Option<(i64, i64)> = None;
        let mut ari_checked_at: i64 = 0;
        self.observe(&slot.id);
        loop {
            let Some(current) = self.snapshot(&slot.id) else {
                return;
            };
            let forced = ctl.force.swap(false, Ordering::SeqCst);
            let now = now_unix();

            // Decide when this cert is due. Order matters: a forced renewal skips
            // the backoff; a failed attempt (placeholder or not) waits it out; a
            // placeholder with no failures yet is due immediately.
            let due_at = if forced {
                now
            } else if failures > 0 {
                current.meta.last_attempt_at.unwrap_or(now) + backoff_secs(failures) as i64
            } else if current.state == CertState::Placeholder {
                now
            } else {
                if now - ari_checked_at >= ARI_INTERVAL_SECS {
                    if let Ok(client) = self.client().await {
                        ari = client
                            .renewal_window(&current.leaf_der)
                            .await
                            .unwrap_or(None);
                    }
                    ari_checked_at = now;
                }
                let t = when_to_renew(
                    current.meta.not_after,
                    ari,
                    self.cfg.renew_before.as_secs() as i64,
                );
                update(&self.certs, &slot.id, |c| c.meta.next_renewal_at = Some(t));
                t
            };

            if now < due_at {
                let wait = Duration::from_secs((due_at - now) as u64).min(self.cfg.poll_interval);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = ctl.waker.notified() => {}
                }
                continue;
            }

            // Due: try to become the leaseholder.
            ctl.in_flight.store(true, Ordering::SeqCst);
            let outcome = self.attempt(&slot).await;
            ctl.in_flight.store(false, Ordering::SeqCst);
            match outcome {
                Ok(true) => {
                    failures = 0;
                    ari_checked_at = 0; // re-query ARI for the new cert
                }
                Ok(false) => {
                    // A peer holds the lease: adopt its result, keep challenges fresh.
                    if self.follow_peer(&slot, ctl).await {
                        // We adopted a peer's fresh certificate: any failure
                        // streak (and cached ARI) we accumulated is about the
                        // *old* certificate and no longer applies. Without
                        // this reset, the next loop pass would take the
                        // `failures > 0` branch off a stale streak and fire a
                        // pointless duplicate order `backoff_secs(failures)`
                        // seconds later.
                        failures = 0;
                        ari = None;
                        ari_checked_at = 0;
                    }
                }
                Err(e) => {
                    failures += 1;
                    error!(
                        "acme: issuance for {} failed (attempt {}): {}",
                        slot.id, failures, e
                    );
                    let now = now_unix();
                    // A placeholder that fails stays `Placeholder` — that is the
                    // sole state `/readyz` keys on (constraints.md), so it must not
                    // flip to `Failed` just because an issuance attempt failed, or
                    // readiness would report ready while still serving a self-signed
                    // cert. A real cert that fails to renew becomes `Failed` but
                    // keeps serving the last-good certificate.
                    update(&self.certs, &slot.id, |c| {
                        if c.state != CertState::Placeholder {
                            c.state = CertState::Failed;
                        }
                        c.meta.last_attempt_at = Some(now);
                        c.meta.last_error = Some(e.to_string());
                        c.meta.next_renewal_at = Some(now + backoff_secs(failures) as i64);
                    });
                    if let Some(m) = &self.metrics {
                        m.attempt(slot.id.as_str(), false, now);
                    }
                    if self
                        .snapshot(&slot.id)
                        .is_some_and(|c| c.state == CertState::Placeholder)
                    {
                        warn!(
                            "acme: {} is still serving a self-signed placeholder",
                            slot.id
                        );
                    }
                }
            }
            self.observe(&slot.id);
        }
    }

    /// One issuance under the lease. `Ok(false)` = a peer holds the lease.
    async fn attempt(&self, slot: &ManagedSlot) -> Result<bool, AcmeError> {
        if !self
            .storage
            .try_acquire_lease(&slot.id, &self.owner, self.cfg.lease_ttl)
            .await?
        {
            return Ok(false);
        }
        update(&self.certs, &slot.id, |c| {
            if c.state == CertState::Issued || c.state == CertState::Failed {
                c.state = CertState::Renewing;
            }
        });
        self.observe(&slot.id);
        let result = async {
            let client = self.client().await?;
            // Keep the lease alive across the order *and* its persistence —
            // an abort-on-drop guard so a cancelled slot future can't leave
            // this loop renewing the lease forever (which would lock every
            // peer out of it). `?` below drops (and so aborts) it on any
            // failure; the explicit `drop` ends it right after `save_cert`.
            let keepalive = AbortOnDrop({
                let storage = self.storage.clone();
                let id = slot.id.clone();
                let owner = self.owner.clone();
                let ttl = self.cfg.lease_ttl;
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(ttl / 4).await;
                        let _ = storage.renew_lease(&id, &owner, ttl).await;
                    }
                })
            });
            let stored = issue(
                client.as_ref(),
                &self.solver,
                &slot.domains,
                self.cfg.key_type,
            )
            .await?;
            self.storage.save_cert(&slot.id, &stored).await?;
            drop(keepalive);
            let (key, leaf) = load_certified_key(&stored.chain_pem, &stored.key_pem)?;
            let meta = parse_cert_meta(&leaf)?;
            let now = now_unix();
            info!(
                "acme: issued certificate for {} (serial {}, expires {})",
                slot.id, meta.serial, meta.not_after
            );
            publish(
                &self.certs,
                &slot.id,
                ManagedCert {
                    key,
                    leaf_der: leaf,
                    state: CertState::Issued,
                    meta: super::CertMeta {
                        last_attempt_at: Some(now),
                        ..meta
                    },
                    domains: slot.domains.clone(),
                },
            );
            if let Some(m) = &self.metrics {
                m.attempt(slot.id.as_str(), true, now);
            }
            Ok::<(), AcmeError>(())
        }
        .await;
        if let Err(e) = self.storage.release_lease(&slot.id, &self.owner).await {
            warn!("acme: releasing lease for {} failed: {}", slot.id, e);
        }
        // On failure, restore the served state (the placeholder/old cert is untouched).
        if result.is_err() {
            update(&self.certs, &slot.id, |c| {
                if c.state == CertState::Renewing {
                    c.state = CertState::Failed;
                }
            });
        }
        result.map(|_| true)
    }

    /// Non-leaseholder path: refresh challenge certs from storage every
    /// `challenge_refresh` and look for the peer's certificate every `peer_poll`,
    /// for at most one lease TTL (then the outer loop re-evaluates). Returns
    /// whether a peer's certificate was adopted, so the caller can reset any
    /// failure/backoff state that no longer applies to it.
    async fn follow_peer(&self, slot: &ManagedSlot, ctl: &SlotControl) -> bool {
        let deadline = tokio::time::Instant::now() + self.cfg.lease_ttl;
        let mut next_adopt = tokio::time::Instant::now();
        while tokio::time::Instant::now() < deadline {
            if let Err(e) = self.solver.refresh_from_storage(&slot.domains).await {
                warn!("acme: challenge refresh for {} failed: {}", slot.id, e);
            }
            if tokio::time::Instant::now() >= next_adopt {
                next_adopt = tokio::time::Instant::now() + self.cfg.peer_poll;
                match self.adopt_from_storage(slot).await {
                    Ok(true) => return true,
                    Ok(false) => {}
                    Err(e) => warn!(
                        "acme: reading peer certificate for {} failed: {}",
                        slot.id, e
                    ),
                }
            }
            if ctl.force.load(Ordering::SeqCst) {
                return false;
            }
            tokio::time::sleep(self.cfg.challenge_refresh).await;
        }
        false
    }

    /// Publishes the stored certificate when it is newer than what we serve.
    async fn adopt_from_storage(&self, slot: &ManagedSlot) -> Result<bool, AcmeError> {
        let Some(stored) = self.storage.load_cert(&slot.id).await? else {
            return Ok(false);
        };
        let (key, leaf) = load_certified_key(&stored.chain_pem, &stored.key_pem)?;
        let meta = parse_cert_meta(&leaf)?;
        let current = self.snapshot(&slot.id);
        let newer = current
            .as_ref()
            .map(|c| c.state == CertState::Placeholder || meta.not_after > c.meta.not_after)
            .unwrap_or(true);
        if !newer || meta.not_after <= now_unix() {
            return Ok(false);
        }
        publish(
            &self.certs,
            &slot.id,
            ManagedCert {
                key,
                leaf_der: leaf,
                state: CertState::Issued,
                meta,
                domains: slot.domains.clone(),
            },
        );
        info!("acme: adopted certificate for {} issued by a peer", slot.id);
        self.observe(&slot.id);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::client::mock::{MockAcmeClient, MockBehavior, MockFactory, MockStep};
    use crate::acme::storage::fs::FsCertStorage;
    use crate::acme::{
        new_managed_certs, placeholder_cert, publish, CertMeta, CertState, ManagedCert,
    };

    #[test]
    fn when_to_renew_takes_the_earlier_of_renew_before_and_ari() {
        let not_after = 1_000_000;
        assert_eq!(when_to_renew(not_after, None, 100), 999_900);
        assert_eq!(
            when_to_renew(not_after, Some((999_000, 999_500)), 100),
            999_000
        );
        assert_eq!(
            when_to_renew(not_after, Some((999_950, 999_990)), 100),
            999_900
        );
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_an_hour() {
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(6), 1_920);
        assert_eq!(backoff_secs(7), 3_600);
        assert_eq!(backoff_secs(40), 3_600);
    }

    struct Harness {
        client: Arc<MockAcmeClient>,
        factory: Arc<MockFactory>,
        storage: Arc<dyn CertStorage>,
        certs: ManagedCerts,
        slot: ManagedSlot,
    }

    fn harness(tag: &str, behavior: MockBehavior) -> Harness {
        let d = std::env::temp_dir().join(format!("fb_acme_mgr_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let storage: Arc<dyn CertStorage> = Arc::new(FsCertStorage::new(d));
        let client = MockAcmeClient::new(behavior);
        let factory = MockFactory::new(client.clone());
        let (id, domains) = CertId::from_domains(&["m.example.com".into()]).unwrap();
        let certs = new_managed_certs();
        let (key, leaf) = placeholder_cert(&domains).unwrap();
        publish(
            &certs,
            &id,
            ManagedCert {
                key,
                leaf_der: leaf,
                state: CertState::Placeholder,
                meta: CertMeta::default(),
                domains: domains.clone(),
            },
        );
        Harness {
            client,
            factory,
            storage,
            certs,
            slot: ManagedSlot { id, domains },
        }
    }

    fn fast_cfg() -> ManagerConfig {
        ManagerConfig {
            poll_interval: Duration::from_millis(50),
            peer_poll: Duration::from_millis(50),
            challenge_refresh: Duration::from_millis(20),
            ..Default::default()
        }
    }

    fn manager(h: &Harness, cfg: ManagerConfig) -> Arc<Manager> {
        Manager::new(
            cfg,
            h.factory.clone(),
            h.storage.clone(),
            TlsAlpnSolver::new(h.storage.clone()),
            h.certs.clone(),
            vec![h.slot.clone()],
            None,
        )
    }

    async fn wait_until(
        certs: &ManagedCerts,
        id: &CertId,
        what: &str,
        pred: impl Fn(&ManagedCert) -> bool,
    ) -> ManagedCert {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(c) = certs.load().get(id.as_str()) {
                if pred(c) {
                    return c.clone();
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_state(certs: &ManagedCerts, id: &CertId, want: CertState) -> ManagedCert {
        wait_until(certs, id, &format!("{want:?}"), |c| c.state == want).await
    }

    /// A failed issuance attempt on a placeholder: still `Placeholder` (that is
    /// the sole state `/readyz` keys on) but with the failure recorded.
    async fn wait_failed_placeholder(certs: &ManagedCerts, id: &CertId) -> ManagedCert {
        wait_until(certs, id, "placeholder with a recorded error", |c| {
            c.state == CertState::Placeholder && c.meta.last_error.is_some()
        })
        .await
    }

    /// `renew_now(id, true)`, retried while the slot is `InProgress` with an
    /// *unrelated* attempt (rather than treating that as a silently dropped
    /// force), bounded by a deadline. A single unretried call would make the
    /// caller's force lost with no signal beyond an eventual, confusing
    /// timeout somewhere else.
    async fn force_renew_scheduled(m: &Manager, id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match m.renew_now(id, true) {
                RenewOutcome::Scheduled => return,
                RenewOutcome::InProgress => {}
                other => panic!(
                    "renew_now({id}, true) returned {other:?}, expected Scheduled or InProgress"
                ),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out getting renew_now to schedule for {id}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits for the mock factory's remaining scripted connect failures to
    /// reach `want` — i.e. for a connect attempt to have actually run (the
    /// counter is decremented from inside `MockFactory::connect`). Used as a
    /// deterministic barrier between two forced retries so the second
    /// `renew_now` call can't coalesce with the first before the scheduler
    /// gives the slot task a chance to run it (the `force` flag is a single
    /// bool, so two signals delivered before either is consumed collapse into
    /// one attempt).
    async fn wait_fail_connects(factory: &MockFactory, want: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while factory
            .fail_connects
            .load(std::sync::atomic::Ordering::SeqCst)
            != want
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for fail_connects to reach {want}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn placeholder_is_issued_on_start_and_persisted() {
        let h = harness("issue", MockBehavior::default());
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        let issued = wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
        assert!(issued.meta.issuer.contains("Mock ACME CA"));
        assert!(issued.meta.next_renewal_at.is_some());
        assert!(h.storage.load_cert(&h.slot.id).await.unwrap().is_some());
        assert_eq!(h.client.orders(), 1);
        // Not due: renew_now without force is refused, with force re-issues.
        assert_eq!(m.renew_now(h.slot.id.as_str(), false), RenewOutcome::NotDue);
        assert_eq!(m.renew_now("nope", true), RenewOutcome::Unknown);
        assert_eq!(
            m.renew_now(h.slot.id.as_str(), true),
            RenewOutcome::Scheduled
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while h.client.orders() < 2 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let renewed = wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
        assert_ne!(renewed.meta.serial, issued.meta.serial);
    }

    #[tokio::test]
    async fn failure_keeps_placeholder_records_error_and_backs_off() {
        let h = harness(
            "fail",
            MockBehavior {
                fail_step: Some(MockStep::Finalize),
                ..Default::default()
            },
        );
        let before = h
            .certs
            .load()
            .get(h.slot.id.as_str())
            .unwrap()
            .leaf_der
            .clone();
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        let failed = wait_failed_placeholder(&h.certs, &h.slot.id).await;
        assert_eq!(
            failed.state,
            CertState::Placeholder,
            "readiness stays keyed on Placeholder, not Failed"
        );
        assert!(failed
            .meta
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("badCSR"));
        assert!(failed.meta.last_attempt_at.is_some());
        assert_eq!(failed.leaf_der, before, "placeholder keeps serving");
        let n = h.client.orders();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            h.client.orders(),
            n,
            "backoff (≥60 s) prevents a hot retry loop"
        );
        // Let the CA recover and force a retry immediately.
        h.client.set_behavior(MockBehavior::default());
        assert_eq!(
            m.renew_now(h.slot.id.as_str(), false),
            RenewOutcome::Scheduled,
            "a placeholder is always due"
        );
        wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
    }

    #[tokio::test]
    async fn unreachable_directory_at_start_is_retried() {
        let h = harness("connect", MockBehavior::default());
        h.factory
            .fail_connects
            .store(2, std::sync::atomic::Ordering::SeqCst);
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        // Two failed connects → still Placeholder (with a recorded error) and
        // backing off; force-renew (retried instead of a single racy call, in
        // case it lands while the previous forced attempt is still
        // in-flight) skips the wait for each of the two remaining connects.
        wait_failed_placeholder(&h.certs, &h.slot.id).await;
        force_renew_scheduled(&m, h.slot.id.as_str()).await;
        // Deterministic barrier: wait for *this* forced attempt's connect()
        // to have actually run (and consumed the last scripted failure)
        // before issuing the second force, so the two forces can't collapse
        // into a single attempt via the shared `force` flag.
        wait_fail_connects(&h.factory, 0).await;
        force_renew_scheduled(&m, h.slot.id.as_str()).await;
        wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
    }

    #[tokio::test]
    async fn peer_holding_the_lease_makes_us_adopt_its_certificate() {
        let h = harness("peer", MockBehavior::default());
        // "Peer" holds the lease and (later) writes the cert.
        assert!(h
            .storage
            .try_acquire_lease(&h.slot.id, "peer", Duration::from_secs(30))
            .await
            .unwrap());
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            h.client.orders(),
            0,
            "must not order while a peer holds the lease"
        );
        let solver = TlsAlpnSolver::new(h.storage.clone());
        let stored =
            crate::acme::order::issue(&h.client, &solver, &h.slot.domains, KeyType::EcdsaP256)
                .await
                .unwrap();
        h.storage.save_cert(&h.slot.id, &stored).await.unwrap();
        let adopted = wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
        assert!(adopted.meta.issuer.contains("Mock ACME CA"));
        assert_eq!(h.client.orders(), 1, "only the peer's order happened");
    }

    /// Regression test for a duplicate-order bug: this node fails a local
    /// issuance first (so its in-loop `failures` counter is nonzero), then a
    /// peer takes the lease and writes a valid certificate that this node
    /// adopts. Adoption must reset the stale failure/backoff state — leaving
    /// it set would (per `backoff_secs`) eventually fire a pointless
    /// duplicate order off the *old* failure streak even though we're now
    /// serving a freshly issued certificate.
    #[tokio::test]
    async fn peer_adoption_resets_backoff_so_no_stale_duplicate_order_follows() {
        let h = harness(
            "peer_backoff",
            MockBehavior {
                fail_step: Some(MockStep::NewOrder),
                ..Default::default()
            },
        );
        let m = manager(&h, fast_cfg());
        m.clone().run().await;
        // Our own first attempt fails and accumulates a failure/backoff.
        wait_failed_placeholder(&h.certs, &h.slot.id).await;
        assert_eq!(h.client.orders(), 1);

        // A peer now takes the lease (ours was released after the failed
        // attempt) while we still have `failures > 0` recorded locally, and
        // force us to notice — we must not order while it holds the lease.
        assert!(h
            .storage
            .try_acquire_lease(&h.slot.id, "peer", Duration::from_secs(30))
            .await
            .unwrap());
        force_renew_scheduled(&m, h.slot.id.as_str()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.client.orders(),
            1,
            "must not order while a peer holds the lease"
        );

        // Let the CA recover and have the "peer" issue and persist a valid
        // certificate while still holding the lease.
        h.client.set_behavior(MockBehavior::default());
        let solver = TlsAlpnSolver::new(h.storage.clone());
        let stored =
            crate::acme::order::issue(&h.client, &solver, &h.slot.domains, KeyType::EcdsaP256)
                .await
                .unwrap();
        h.storage.save_cert(&h.slot.id, &stored).await.unwrap();
        let adopted = wait_state(&h.certs, &h.slot.id, CertState::Issued).await;
        assert!(adopted.meta.issuer.contains("Mock ACME CA"));
        assert_eq!(
            h.client.orders(),
            2,
            "our failed attempt + the peer's order"
        );

        // The real discriminator: `next_renewal_at` is only ever written by
        // the ARI/renew-before branch, which only runs once `failures == 0`
        // (the `failures > 0` branch computes a local `due_at` but never
        // calls `update()`, so the field stays `None` — exactly what
        // `parse_cert_meta`/`adopt_from_storage` leave it at). Seeing it
        // become `Some` after adoption proves the reset actually happened,
        // deterministically and fast, instead of racing (or failing to
        // distinguish within) the real 60 s+ backoff floor.
        let scheduled = wait_until(
            &h.certs,
            &h.slot.id,
            "next_renewal_at recorded after adoption (proves the failure \
             streak was reset, not stuck on the stale-backoff branch)",
            |c| c.state == CertState::Issued && c.meta.next_renewal_at.is_some(),
        )
        .await;
        assert!(scheduled.meta.next_renewal_at.unwrap() > now_unix());

        // Secondary check, kept from the original assertion: no stale-backoff
        // duplicate order follows adoption either.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            h.client.orders(),
            2,
            "adopting a peer's certificate must reset local backoff state, \
             not leave a duplicate order pending"
        );
    }
}
