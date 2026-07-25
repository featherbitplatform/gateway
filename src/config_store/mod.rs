//! Configuration source abstraction.
//!
//! Decides where the gateway's routes/policies/consumers are loaded from and
//! how Admin API mutations are persisted. [`FileConfigStore`] is the default,
//! stateless source (reads `gateway.yaml`, applies edits in memory). The
//! `etcd` feature adds [`EtcdConfigStore`] for shared config delivery across an
//! HA cluster — every instance watches the same etcd prefix and converges on
//! changes. Both sit behind the [`ConfigStore`] trait so `main` and the Admin
//! API are backend-agnostic.

use async_trait::async_trait;
use std::path::PathBuf;

use crate::config::{load_yaml_with_env, GatewayConfig};
use crate::state::SharedState;

pub mod etcd;

/// A source of gateway configuration and the sink for Admin API mutations.
#[async_trait]
pub trait ConfigStore: Send + Sync {
    /// Loads the full gateway config from the source.
    async fn load_all(&self) -> Result<GatewayConfig, String>;

    /// Persists a candidate config (the full desired `GatewayConfig` after an
    /// Admin API mutation) and makes it live.
    ///
    /// Implementations must **reject invalid config** — via
    /// [`SharedState::validate_gateway`] — before persisting, so the Admin API
    /// returns an error synchronously rather than accepting a config that would
    /// fail to compile.
    async fn commit(&self, state: &SharedState, candidate: GatewayConfig) -> Result<(), String>;
}

/// File-backed config store: loads `gateway.yaml` and applies Admin mutations
/// in memory. The on-disk file is **not** rewritten (matching the original
/// behavior — edits are live but not persisted to the file). Stateless,
/// single-node; the default when `config.source` is `file` or unset.
pub struct FileConfigStore {
    path: PathBuf,
}

impl FileConfigStore {
    /// Creates a file store reading from `path` (`gateway.yaml`).
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl ConfigStore for FileConfigStore {
    async fn load_all(&self) -> Result<GatewayConfig, String> {
        load_yaml_with_env(&self.path).map_err(|e| e.to_string())
    }

    async fn commit(&self, state: &SharedState, candidate: GatewayConfig) -> Result<(), String> {
        // apply_gateway validates + compiles before swapping, so an invalid
        // candidate is rejected here with the running config left intact.
        state.apply_gateway(candidate).await
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory config store for tests: `load_all` returns a preset config,
    /// `commit` validates the candidate and records it without touching real
    /// state, so the commit/validation path is testable without a file or etcd.
    pub struct FakeConfigStore {
        pub initial: GatewayConfig,
        pub committed: Mutex<Vec<GatewayConfig>>,
    }

    impl FakeConfigStore {
        pub fn new(initial: GatewayConfig) -> Self {
            Self {
                initial,
                committed: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ConfigStore for FakeConfigStore {
        async fn load_all(&self) -> Result<GatewayConfig, String> {
            Ok(self.initial.clone())
        }

        async fn commit(
            &self,
            state: &SharedState,
            candidate: GatewayConfig,
        ) -> Result<(), String> {
            // Reject invalid config exactly as a real store must.
            state.validate_gateway(&candidate)?;
            self.committed.lock().unwrap().push(candidate);
            Ok(())
        }
    }

    use std::sync::Arc;

    fn empty_state() -> Arc<SharedState> {
        let system = serde_yaml::from_str("{}").unwrap();
        let gateway = serde_yaml::from_str("{}").unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FakeConfigStore::new(serde_yaml::from_str("{}").unwrap())),
            )
            .unwrap(),
        )
    }

    const VALID_GW: &str = r#"
routes:
  - name: r
    match: { path: /api/* }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: client.in }
"#;

    #[tokio::test]
    async fn test_file_commit_applies_valid_config() {
        let state = empty_state();
        // Swap in a real file store so commit exercises apply_gateway.
        let store = FileConfigStore::new("unused.yaml".into());
        let candidate: GatewayConfig = serde_yaml::from_str(VALID_GW).unwrap();

        store.commit(&state, candidate).await.unwrap();
        assert_eq!(state.routes.read().await.len(), 1);
        assert_eq!(state.gateway.read().await.routes[0].name, "r");
    }

    #[tokio::test]
    async fn test_commit_rejects_invalid_config() {
        let state = empty_state();
        // Route references a policy that does not exist.
        let bad: GatewayConfig = serde_yaml::from_str(
            "routes:\n  - name: r\n    match: { path: /x }\n    policy: missing\n",
        )
        .unwrap();

        // Both the file store and the fake store must reject it before persisting.
        assert!(FileConfigStore::new("unused.yaml".into())
            .commit(&state, bad.clone())
            .await
            .is_err());

        let fake = FakeConfigStore::new(serde_yaml::from_str("{}").unwrap());
        assert!(fake.commit(&state, bad).await.is_err());
        assert!(fake.committed.lock().unwrap().is_empty());
        // The running config was never touched.
        assert_eq!(state.routes.read().await.len(), 0);
    }
}
