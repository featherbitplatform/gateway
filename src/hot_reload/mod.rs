//! Hot-reload of the gateway configuration: watches `gateway.yaml` for
//! changes (via the `notify` crate) and triggers a `SharedState` reload so
//! route and policy edits apply without restarting the process.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::state::SharedState;

/// Watches config files for changes and triggers hot-reload.
///
/// Spawns a dedicated OS thread running a `notify` watcher on the config
/// file's parent directory (recursively), forwarding modify/create events
/// over a channel to this async loop. Events are debounced: after the first
/// event the loop waits 500ms and drains any further events, so a burst of
/// filesystem notifications (as editors typically produce) results in a
/// single `SharedState::reload_from_disk` call. Reload failures are logged
/// and leave the previously loaded configuration in place. Runs until the
/// event channel closes; intended to be spawned as a long-lived task.
pub async fn watch_config(state: Arc<SharedState>, config_path: PathBuf) {
    let (tx, mut rx) = mpsc::channel::<()>(1);

    let path = config_path.clone();
    std::thread::spawn(move || {
        let rt_tx = tx.clone();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            let _ = rt_tx.blocking_send(());
                        }
                        _ => {}
                    }
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    error!("Failed to create config file watcher: {e}; hot-reload disabled");
                    return;
                }
            };

        // Watch the parent directory of the config file. The gateway boots
        // without a gateway.yaml, so the directory may not exist yet: that
        // is not fatal — hot-reload is simply unavailable until a restart
        // (the Admin API keeps working, and `POST /api/config/reload` still
        // picks the file up once it is created).
        let watch_dir = path.parent().unwrap_or(&path);
        if let Err(e) = watcher.watch(watch_dir, RecursiveMode::Recursive) {
            warn!(
                "Cannot watch {} for config changes: {e}; hot-reload disabled until restart",
                watch_dir.display()
            );
            return;
        }

        info!("File watcher started on {:?}", watch_dir);

        // Keep the watcher alive
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    });

    // Debounce: wait for changes, batch them, then reload
    loop {
        if rx.recv().await.is_none() {
            break;
        }

        // Debounce: drain any additional events within 500ms
        tokio::time::sleep(Duration::from_millis(500)).await;
        while rx.try_recv().is_ok() {}

        info!("Config change detected, reloading...");
        match state.reload_from_disk().await {
            Ok(_) => info!("Config reloaded successfully"),
            Err(e) => error!("Config reload failed: {}", e),
        }
    }
}
