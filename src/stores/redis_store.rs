//! Redis/Valkey client for named stores. Fleshed out incrementally:
//! client + ping (Task 3), fixed-window counter (Task 4).

use crate::config::StoreConfig;

/// Placeholder until the `redis` dependency lands (next task): building any
/// store fails loudly rather than pretending to connect.
// removed with the stub in the next task
#[allow(dead_code)]
pub struct RedisStoreClient {
    fingerprint: String,
}

// removed with the stub in the next task
#[allow(dead_code)]
impl RedisStoreClient {
    pub fn build(cfg: &StoreConfig) -> Result<Self, String> {
        Err(format!(
            "store '{}': redis client not implemented yet (plan task 3)",
            cfg.name
        ))
    }

    pub fn fingerprint_of(_cfg: &StoreConfig) -> String {
        String::new()
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
