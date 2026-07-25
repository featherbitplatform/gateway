//! Admin API and UI server.
//!
//! Runs an axum [`Router`] on a dedicated port, separate from the data plane.
//! Exposes Basic-Auth-protected CRUD endpoints for routes and policies,
//! status/health/metrics endpoints, and serves the embedded React SPA
//! (node-graph editor) as an unauthenticated fallback.

mod auth;
mod consumers;
mod debug;
mod policies;
mod routes;
mod status;
mod ui;

use std::sync::Arc;

use std::time::Duration;

use axum::routing::get;
use axum::Router;
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::AdminConfig;
use crate::server::tls;
use crate::state::SharedState;

/// Binds the admin listener on `admin_config.bind:port` and serves the admin
/// API and UI until the server exits.
///
/// The API routes (`/api/*`, `/healthz`, `/readyz`, `/metrics`) are wrapped in
/// the Basic Auth middleware using credentials from [`AdminConfig`]; any path
/// not matched by the API falls back to the embedded SPA, which is served
/// without auth (the SPA's own API calls carry credentials).
///
/// When `admin_config.tls` is set, the admin listener is TLS-terminated using
/// the same acceptor helper as the data plane; otherwise it serves plain HTTP.
///
/// On shutdown (`shutdown_rx` flips to `true`) the accept loop stops and
/// in-flight requests are drained (up to `drain_timeout`), then this returns.
///
/// Returns an error for a fail-fast startup problem (bind failure, or an
/// unreadable cert/key when TLS is configured). Per-connection errors —
/// including TLS handshake failures — are logged and do not stop the server.
pub async fn start_admin_server(
    admin_config: &AdminConfig,
    state: Arc<SharedState>,
    mut shutdown_rx: watch::Receiver<bool>,
    drain_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        // API routes (with auth)
        .merge(routes::router())
        .merge(policies::router())
        .merge(consumers::router())
        .merge(status::router())
        .merge(debug::router())
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(auth::AuthState {
                username: admin_config.username.clone(),
                password: admin_config.password.clone(),
            }),
            auth::basic_auth_middleware,
        ))
        .with_state(state)
        // UI static files (no auth — the API calls from the UI will authenticate)
        .fallback(get(ui::serve_ui));

    // Fail-fast on a broken TLS setup before binding. Hot-reloadable — a
    // cert-file change swaps in for new admin connections without a restart.
    let tls_config: Option<tls::SharedTlsConfig> = match &admin_config.tls {
        // HTTP/2 is fine for the admin API; the auto builder still serves h1.
        Some(tls_cfg) => {
            let shared = tls::build_reloadable(tls_cfg, true)?;
            tls::spawn_cert_watcher(tls_cfg.clone(), true, shared.clone(), "admin");
            Some(shared)
        }
        None => None,
    };

    let addr = format!("{}:{}", admin_config.bind, admin_config.port);
    let listener = TcpListener::bind(&addr).await?;
    info!(
        "Admin API + UI listening on {} ({})",
        addr,
        if tls_config.is_some() {
            "https"
        } else {
            "http"
        },
    );

    // Manual accept loop (instead of `axum::serve`) so TLS reuses the shared
    // acceptor + connection builder, and so shutdown drains in-flight requests.
    // The axum `Router` is a tower `Service`; `TowerToHyperService` adapts it.
    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _peer) = accepted?;
                let app = app.clone();
                let tls_config = tls_config.clone();
                let watcher = graceful.watcher();

                tokio::spawn(async move {
                    let svc = TowerToHyperService::new(app);
                    match tls_config.as_ref().map(tls::current_acceptor) {
                        Some(acc) => match acc.accept(stream).await {
                            Ok(tls_stream) => {
                                let conn = tls::build_connection(TokioIo::new(tls_stream), svc, true);
                                if let Err(err) = watcher.watch(conn).await {
                                    warn!("Admin connection error: {}", err);
                                }
                            }
                            Err(err) => warn!("Admin TLS handshake failed: {}", err),
                        },
                        None => {
                            let conn = tls::build_connection(TokioIo::new(stream), svc, true);
                            if let Err(err) = watcher.watch(conn).await {
                                warn!("Admin connection error: {}", err);
                            }
                        }
                    }
                });
            }
            _ = shutdown_rx.changed() => break,
        }
    }

    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => info!("Admin API drained"),
        _ = tokio::time::sleep(drain_timeout) => warn!("Admin drain timed out; forcing exit"),
    }
    Ok(())
}
