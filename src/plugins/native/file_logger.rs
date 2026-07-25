//! The `file-logger` node — appends per-request access-log entries as
//! newline-delimited JSON to a local file, mirroring APISIX's `file-logger`
//! plugin.
//!
//! Entries are built with the shared [`build_entry`](crate::plugins::util::log_entry)
//! helper and handed to a [`BatchSink`]; a background task opens the target
//! file in append mode per flush and writes each entry as one JSON line.
//! Writing is fire-and-forget on the request path — [`Plugin::execute`] pushes
//! the entry and returns the context unchanged — so place this node in the
//! response pipeline **after the `upstream` node**.
//!
//! ## Deviations from APISIX
//! - APISIX writes each request immediately; featherbit routes writes through
//!   the shared [`BatchSink`] for consistency with the other loggers. Set
//!   `batch_max_size: 1` to write every entry as it arrives.
//! - The parent directory is **not** created: if it is missing the flush fails
//!   (logged and, per [`BatchConfig`], retried/dropped). The file itself is
//!   created if absent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Serializes each entry as one line of JSON, newline-terminated. Pure and
/// I/O-independent for unit testing.
fn entries_to_lines(entries: &[Value]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&serde_json::to_string(entry).unwrap_or_else(|_| "null".to_string()));
        out.push('\n');
    }
    out
}

/// [`BatchFlusher`] that appends the batch to `path`.
struct FileFlusher {
    path: PathBuf,
}

#[async_trait]
impl BatchFlusher for FileFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let payload = entries_to_lines(entries);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| FlushError {
                message: format!("failed to open log file {}: {e}", self.path.display()),
                first_fail: None,
            })?;
        file.write_all(payload.as_bytes())
            .await
            .map_err(|e| FlushError {
                message: format!("failed to write log file {}: {e}", self.path.display()),
                first_fail: None,
            })
    }
}

/// The `file-logger` plugin node.
pub struct FileLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

impl FileLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    /// - `path` (string, **required**): target file path. Opened in append mode;
    ///   created if absent, but its parent directory must already exist.
    /// - `log_format` (object): custom `name -> "$var"` entry.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`).
    /// - batch keys (optional) — see [`BatchConfig::from_config`]. Use
    ///   `batch_max_size: 1` for immediate per-request writes.
    ///
    /// ```yaml
    /// - id: file-log
    ///   type: file-logger
    ///   config:
    ///     path: /var/log/featherbit/access.log
    ///     batch_max_size: 1
    /// ```
    pub fn from_config(config: &HashMap<String, Value>) -> Result<Self, String> {
        let path = config
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("file-logger: `path` is required")?
            .to_string();

        let log_format = parse_log_format(config)?;
        let include_req_body = config
            .get("include_req_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_resp_body = config
            .get("include_resp_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let batch_cfg = BatchConfig::from_config(config)?;
        let flusher = Arc::new(FileFlusher {
            path: PathBuf::from(&path),
        });
        let sink = BatchSink::spawn(&format!("file-logger:{path}"), batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

#[async_trait]
impl Plugin for FileLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "file-logger"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        let entry = build_entry(
            &ctx,
            self.log_format.as_ref(),
            self.include_req_body,
            self.include_resp_body,
        );
        self.sink.push(entry);
        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_config_requires_path() {
        assert!(FileLoggerPlugin::from_config(&HashMap::new()).is_err());
    }

    #[test]
    fn entries_to_lines_is_newline_delimited() {
        let out = entries_to_lines(&[json!({"a": 1}), json!({"b": 2})]);
        assert_eq!(out, "{\"a\":1}\n{\"b\":2}\n");
    }

    #[tokio::test]
    async fn flusher_appends_to_file() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "featherbit-file-logger-test-{}.log",
            std::process::id()
        ));
        // Clean any stale file from a prior run.
        let _ = tokio::fs::remove_file(&path).await;

        let flusher = FileFlusher { path: path.clone() };
        flusher.flush(&[json!({"n": 1})]).await.unwrap();
        flusher.flush(&[json!({"n": 2})]).await.unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "{\"n\":1}\n{\"n\":2}\n");

        let _ = tokio::fs::remove_file(&path).await;
    }
}
