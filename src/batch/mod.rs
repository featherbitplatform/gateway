//! Batching sink for logger plugins.
//!
//! The featherbit analogue of APISIX's batch processor
//! (`apisix/utils/batch-processor.lua`): log entries produced on the request
//! path are handed to a [`BatchSink`] with a fire-and-forget [`BatchSink::push`]
//! and delivered downstream in batches by a background tokio task. A batch is
//! flushed when it reaches `batch_max_size`, when no new entry has arrived for
//! `inactive_timeout`, or when the oldest buffered entry is `buffer_duration`
//! old — whichever comes first. Failed flushes are retried up to
//! `max_retry_count` times, honouring the flusher's `first_fail` hint so
//! already-delivered entries are not re-sent (mirroring APISIX's
//! `slice_batch`).
//!
//! `push()` never blocks and never awaits: entries go through a bounded mpsc
//! channel and are **dropped with a `tracing::warn!`** when the channel is
//! full (`max_pending_entries`). A rising drop rate is the operator's signal
//! to raise the capacity or fix the downstream.
//!
//! One sink is created per logger node in a compiled policy graph. When the
//! graph is recompiled the old sink is dropped and a new one spawned; dropping
//! the sink closes the channel, the background task drains whatever is still
//! buffered or queued, flushes it, and exits.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{self, Instant};

/// Tuning knobs for a [`BatchSink`], matching the field names (and defaults)
/// of APISIX's batch-processor schema plus featherbit's channel capacity.
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Maximum entries per batch; reaching it triggers an immediate flush.
    /// A value of `1` flushes every entry as it arrives, with no timers.
    /// Default `1000`.
    pub batch_max_size: usize,
    /// Flush the buffer when no new entry has arrived for this long.
    /// Default 5 seconds.
    pub inactive_timeout: Duration,
    /// Flush the buffer when the *oldest* buffered entry is this old, even if
    /// entries keep trickling in fast enough to reset `inactive_timeout`.
    /// Default 60 seconds.
    pub buffer_duration: Duration,
    /// How many times a batch is retried after its first failed flush before
    /// being dropped. Default `0` (no retries).
    pub max_retry_count: u32,
    /// Delay between retry attempts. Default 1 second.
    pub retry_delay: Duration,
    /// Capacity of the channel between [`BatchSink::push`] and the background
    /// task. When full, pushed entries are dropped with a warning.
    /// Default `10_000`.
    pub max_pending_entries: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            batch_max_size: 1000,
            inactive_timeout: Duration::from_secs(5),
            buffer_duration: Duration::from_secs(60),
            max_retry_count: 0,
            retry_delay: Duration::from_secs(1),
            max_pending_entries: 10_000,
        }
    }
}

impl BatchConfig {
    /// Parses the APISIX-shaped keys (`batch_max_size`, `inactive_timeout`,
    /// `buffer_duration`, `max_retry_count`, `retry_delay` — seconds as
    /// integers — and `max_pending_entries`) from a plugin config map, using
    /// the defaults for absent keys.
    ///
    /// Returns an error naming the offending key when a present value is not
    /// a non-negative integer.
    pub fn from_config(config: &HashMap<String, Value>) -> Result<Self, String> {
        let mut cfg = Self::default();
        if let Some(v) = int_key(config, "batch_max_size")? {
            cfg.batch_max_size = v as usize;
        }
        if let Some(v) = int_key(config, "inactive_timeout")? {
            cfg.inactive_timeout = Duration::from_secs(v);
        }
        if let Some(v) = int_key(config, "buffer_duration")? {
            cfg.buffer_duration = Duration::from_secs(v);
        }
        if let Some(v) = int_key(config, "max_retry_count")? {
            cfg.max_retry_count = v as u32;
        }
        if let Some(v) = int_key(config, "retry_delay")? {
            cfg.retry_delay = Duration::from_secs(v);
        }
        if let Some(v) = int_key(config, "max_pending_entries")? {
            cfg.max_pending_entries = v as usize;
        }
        Ok(cfg)
    }
}

/// Reads `key` from the config map as a non-negative integer. `None` (and
/// JSON `null`) mean "absent, use the default"; any other non-integer value
/// is an error.
fn int_key(config: &HashMap<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match config.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            format!("batch config key `{key}` must be a non-negative integer, got {v}")
        }),
    }
}

/// Error returned by [`BatchFlusher::flush`] when a batch (or part of one)
/// could not be delivered.
#[derive(Debug)]
pub struct FlushError {
    /// Human-readable description of the failure, included in retry/drop logs.
    pub message: String,
    /// 0-based index of the first entry that failed; entries before it are
    /// treated as delivered and only the tail is retried. `None` means the
    /// whole batch failed and is retried in full.
    pub first_fail: Option<usize>,
}

/// Delivers a batch of log entries downstream (HTTP endpoint, file, syslog,
/// ...). Implemented by each logger plugin; the batching, timing, and retry
/// logic all live in [`BatchSink`].
#[async_trait::async_trait]
pub trait BatchFlusher: Send + Sync + 'static {
    /// Attempts to deliver `entries`. On partial success, return a
    /// [`FlushError`] with `first_fail` set so the sink retries only the
    /// undelivered tail.
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError>;
}

/// Handle to a spawned batching task. Cheap to clone; the background task
/// flushes any remaining entries and exits when the last handle is dropped.
#[derive(Clone)]
pub struct BatchSink {
    tx: mpsc::Sender<Value>,
    name: String,
}

impl BatchSink {
    /// Spawns the background flush task and returns the sink handle.
    ///
    /// `name` identifies the sink (typically the logger node id) in warning
    /// and error logs.
    pub fn spawn(name: &str, cfg: BatchConfig, flusher: Arc<dyn BatchFlusher>) -> Self {
        let name = name.to_string();
        let (tx, rx) = mpsc::channel(cfg.max_pending_entries.max(1));
        tokio::spawn(run_loop(name.clone(), cfg, flusher, rx));
        Self { tx, name }
    }

    /// Non-blocking enqueue of one log entry.
    ///
    /// Never blocks or awaits: when the channel is full (the background task
    /// is not keeping up, e.g. a slow or retrying downstream) the entry is
    /// **dropped** and a `tracing::warn!` is emitted. The rate of such drops
    /// is the operator's signal to raise `max_pending_entries`.
    pub fn push(&self, entry: Value) {
        match self.tx.try_send(entry) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(sink = %self.name, "batch sink queue full, dropping log entry");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(sink = %self.name, "batch sink task gone, dropping log entry");
            }
        }
    }
}

/// The background flush loop: buffers entries from `rx` and flushes on size,
/// inactivity, age, or channel close (drain-on-drop).
async fn run_loop(
    name: String,
    cfg: BatchConfig,
    flusher: Arc<dyn BatchFlusher>,
    mut rx: mpsc::Receiver<Value>,
) {
    let batch_max = cfg.batch_max_size.max(1);
    let mut buffer: Vec<Value> = Vec::new();
    // Timestamps are only meaningful while `buffer` is non-empty.
    let mut first_entry_at = Instant::now();
    let mut last_entry_at = first_entry_at;

    loop {
        if buffer.is_empty() {
            // Nothing buffered: no deadline to watch, wait solely on recv().
            match rx.recv().await {
                Some(entry) => {
                    first_entry_at = Instant::now();
                    last_entry_at = first_entry_at;
                    buffer.push(entry);
                }
                None => break, // sink dropped, nothing buffered: done
            }
        } else {
            // Flush when the oldest entry turns buffer_duration old or the
            // stream has been quiet for inactive_timeout, whichever is first.
            let deadline = std::cmp::min(
                first_entry_at + cfg.buffer_duration,
                last_entry_at + cfg.inactive_timeout,
            );
            tokio::select! {
                maybe_entry = rx.recv() => match maybe_entry {
                    Some(entry) => {
                        last_entry_at = Instant::now();
                        buffer.push(entry);
                    }
                    None => {
                        // Sink dropped: drain what we have, then exit.
                        flush_with_retry(&name, &cfg, &flusher, &mut buffer).await;
                        break;
                    }
                },
                _ = time::sleep_until(deadline) => {
                    flush_with_retry(&name, &cfg, &flusher, &mut buffer).await;
                }
            }
        }

        // Size trigger; with batch_max_size == 1 this flushes every entry as
        // it arrives and the timer arm above is never reached.
        if buffer.len() >= batch_max {
            flush_with_retry(&name, &cfg, &flusher, &mut buffer).await;
        }
    }

    tracing::debug!(sink = %name, "batch sink channel closed, flush task exiting");
}

/// Flushes `buffer`, retrying per `cfg` on failure. Honours `first_fail` by
/// dropping the delivered head and retrying only the tail. Entries arriving
/// during a retry sleep simply queue in the channel and form the next batch.
/// When retries are exhausted, the remaining entries are dropped with a
/// `tracing::error!`.
async fn flush_with_retry(
    name: &str,
    cfg: &BatchConfig,
    flusher: &Arc<dyn BatchFlusher>,
    buffer: &mut Vec<Value>,
) {
    if buffer.is_empty() {
        return;
    }
    let mut entries = std::mem::take(buffer);
    let mut failures: u32 = 0;

    loop {
        let err = match flusher.flush(&entries).await {
            Ok(()) => {
                tracing::debug!(sink = %name, count = entries.len(), "batch flushed");
                return;
            }
            Err(err) => err,
        };

        if let Some(first_fail) = err.first_fail {
            // Entries before first_fail were delivered; retry only the tail.
            entries.drain(..first_fail.min(entries.len()));
        }
        if entries.is_empty() {
            // The flusher errored but claimed every entry was delivered;
            // nothing left to retry.
            tracing::warn!(sink = %name, error = %err.message,
                "flush reported an error but no entries remain to retry");
            return;
        }

        failures += 1;
        if failures > cfg.max_retry_count {
            tracing::error!(sink = %name, dropped = entries.len(), error = %err.message,
                "batch sink exceeded max_retry_count, dropping entries");
            return;
        }
        tracing::warn!(sink = %name, remaining = entries.len(), attempt = failures,
            error = %err.message, "batch flush failed, retrying");
        time::sleep(cfg.retry_delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Records every flush call; pops a scripted [`FlushError`] per call
    /// until the script is exhausted, then succeeds.
    struct RecordingFlusher {
        calls: Mutex<Vec<Vec<Value>>>,
        failures: Mutex<VecDeque<FlushError>>,
    }

    impl RecordingFlusher {
        fn new() -> Arc<Self> {
            Self::failing_with(Vec::new())
        }

        fn failing_with(failures: Vec<FlushError>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                failures: Mutex::new(failures.into()),
            })
        }

        fn calls(&self) -> Vec<Vec<Value>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl BatchFlusher for RecordingFlusher {
        async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
            self.calls.lock().unwrap().push(entries.to_vec());
            match self.failures.lock().unwrap().pop_front() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }
    }

    /// Polls `cond` for up to ~1s of (possibly auto-advanced) time.
    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..100 {
            if cond() {
                return;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not met within timeout");
    }

    /// Long timers so only the trigger under test can fire.
    fn quiet_cfg() -> BatchConfig {
        BatchConfig {
            inactive_timeout: Duration::from_secs(3600),
            buffer_duration: Duration::from_secs(3600),
            ..BatchConfig::default()
        }
    }

    #[tokio::test]
    async fn size_triggered_flush() {
        let flusher = RecordingFlusher::new();
        let cfg = BatchConfig {
            batch_max_size: 3,
            ..quiet_cfg()
        };
        let sink = BatchSink::spawn("size", cfg, flusher.clone());

        sink.push(json!({"n": 1}));
        sink.push(json!({"n": 2}));
        sink.push(json!({"n": 3}));

        wait_for(|| flusher.calls().len() == 1).await;
        let calls = flusher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 3);
        assert_eq!(calls[0][2], json!({"n": 3}));
    }

    #[tokio::test]
    async fn inactive_timeout_flush() {
        tokio::time::pause();

        let flusher = RecordingFlusher::new();
        let cfg = BatchConfig {
            batch_max_size: 100,
            inactive_timeout: Duration::from_secs(5),
            buffer_duration: Duration::from_secs(60),
            ..BatchConfig::default()
        };
        let sink = BatchSink::spawn("inactive", cfg, flusher.clone());

        sink.push(json!("entry"));
        // Let the background task receive the entry and arm its deadline
        // (yield_now keeps the clock still, unlike sleep under paused time).
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(
            flusher.calls().is_empty(),
            "must not flush before the timeout"
        );

        tokio::time::advance(Duration::from_secs(6)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        let calls = flusher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], vec![json!("entry")]);
    }

    #[tokio::test]
    async fn batch_max_size_one_flushes_immediately() {
        let flusher = RecordingFlusher::new();
        let cfg = BatchConfig {
            batch_max_size: 1,
            ..quiet_cfg()
        };
        let sink = BatchSink::spawn("one", cfg, flusher.clone());

        sink.push(json!(1));
        sink.push(json!(2));

        wait_for(|| flusher.calls().len() == 2).await;
        let calls = flusher.calls();
        assert_eq!(calls, vec![vec![json!(1)], vec![json!(2)]]);
    }

    #[tokio::test]
    async fn first_fail_retries_only_the_tail() {
        let flusher = RecordingFlusher::failing_with(vec![FlushError {
            message: "partial delivery".into(),
            first_fail: Some(1),
        }]);
        let cfg = BatchConfig {
            batch_max_size: 3,
            max_retry_count: 2,
            retry_delay: Duration::from_millis(10),
            ..quiet_cfg()
        };
        let sink = BatchSink::spawn("retry", cfg, flusher.clone());

        sink.push(json!("a"));
        sink.push(json!("b"));
        sink.push(json!("c"));

        wait_for(|| flusher.calls().len() == 2).await;
        let calls = flusher.calls();
        assert_eq!(calls[0], vec![json!("a"), json!("b"), json!("c")]);
        // "a" (index 0) was delivered; only the tail is retried.
        assert_eq!(calls[1], vec![json!("b"), json!("c")]);
    }

    #[tokio::test]
    async fn drain_on_drop() {
        let flusher = RecordingFlusher::new();
        let sink = BatchSink::spawn("drain", quiet_cfg(), flusher.clone());

        sink.push(json!(1));
        sink.push(json!(2));
        drop(sink);

        wait_for(|| flusher.calls().len() == 1).await;
        let calls = flusher.calls();
        assert_eq!(calls[0], vec![json!(1), json!(2)]);
    }

    #[tokio::test]
    async fn from_config_defaults_and_errors() {
        // All keys absent: defaults.
        let cfg = BatchConfig::from_config(&HashMap::new()).unwrap();
        assert_eq!(cfg.batch_max_size, 1000);
        assert_eq!(cfg.inactive_timeout, Duration::from_secs(5));
        assert_eq!(cfg.buffer_duration, Duration::from_secs(60));
        assert_eq!(cfg.max_retry_count, 0);
        assert_eq!(cfg.retry_delay, Duration::from_secs(1));
        assert_eq!(cfg.max_pending_entries, 10_000);

        // Present keys override defaults.
        let map: HashMap<String, Value> = [
            ("batch_max_size".to_string(), json!(10)),
            ("inactive_timeout".to_string(), json!(2)),
            ("buffer_duration".to_string(), json!(30)),
            ("max_retry_count".to_string(), json!(3)),
            ("retry_delay".to_string(), json!(7)),
            ("max_pending_entries".to_string(), json!(500)),
        ]
        .into();
        let cfg = BatchConfig::from_config(&map).unwrap();
        assert_eq!(cfg.batch_max_size, 10);
        assert_eq!(cfg.inactive_timeout, Duration::from_secs(2));
        assert_eq!(cfg.buffer_duration, Duration::from_secs(30));
        assert_eq!(cfg.max_retry_count, 3);
        assert_eq!(cfg.retry_delay, Duration::from_secs(7));
        assert_eq!(cfg.max_pending_entries, 500);

        // Non-integer value is an error naming the key.
        let map: HashMap<String, Value> = [("batch_max_size".to_string(), json!("many"))].into();
        let err = BatchConfig::from_config(&map).unwrap_err();
        assert!(
            err.contains("batch_max_size"),
            "error should name the key: {err}"
        );
    }
}
