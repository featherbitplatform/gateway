//! Test-only helper that captures `tracing` output emitted on the current
//! thread, so tests can assert on the operational log lines a code path
//! produces (e.g. "this rejection is logged at warn").

use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing::subscriber::DefaultGuard;

/// Buffer shared between the subscriber's writer and the test.
#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    /// Everything logged so far, as one string.
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs a thread-local subscriber recording `WARN`-and-above lines
/// (plain text, no ANSI) until the returned guard drops. Works inside
/// `#[tokio::test]` (current-thread runtime) as long as the code under test
/// does not hop threads.
pub fn capture_warnings() -> (DefaultGuard, LogBuffer) {
    let buf = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    (tracing::subscriber::set_default(subscriber), buf)
}
