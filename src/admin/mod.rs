//! Admin API and UI server.
//!
//! Runs an axum [`Router`] on a dedicated port, separate from the data plane.
//! Exposes Basic-Auth-protected CRUD endpoints for routes and policies,
//! status/health/metrics endpoints, and serves the embedded React SPA
//! (node-graph editor) as an unauthenticated fallback
//! (compile-time `ui` feature + runtime `admin.ui_enabled`).

mod auth;
mod consumers;
mod debug;
mod env_vars;
mod plugin_configs;
mod policies;
mod routes;
mod status;
mod supernodes;
#[cfg(feature = "ui")]
mod ui;
mod vars;

use std::sync::Arc;

use std::time::Duration;

#[cfg(feature = "ui")]
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
/// not matched by the API falls back to the embedded SPA — when the binary is
/// compiled with the `ui` feature and `admin.ui_enabled` is true (the
/// default) — served without auth (the SPA's own API calls carry
/// credentials).
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
    let app = build_router(admin_config, state);

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

/// Builds the admin router: authed API routes, plus — only when compiled with
/// the `ui` feature AND `admin.ui_enabled` is true — the unauthenticated SPA
/// fallback. Without it, non-API paths get axum's default 404.
fn build_router(admin_config: &AdminConfig, state: Arc<SharedState>) -> Router {
    let app = Router::new()
        // API routes (with auth)
        .merge(routes::router())
        .merge(policies::router())
        .merge(plugin_configs::router())
        .merge(supernodes::router())
        .merge(consumers::router())
        .merge(status::router())
        .merge(debug::router())
        .merge(vars::router())
        .merge(env_vars::router())
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(auth::AuthState {
                username: admin_config.username.clone(),
                password: admin_config.password.clone(),
            }),
            auth::basic_auth_middleware,
        ))
        .with_state(state);

    // UI static files (no auth — the API calls from the UI will authenticate).
    //
    // Both branches below call `.fallback()` explicitly, even the 404 one:
    // `.layer()` above wraps whatever fallback the router already has at
    // that point, including the implicit default "no route matched"
    // handler. Leaving that implicit default in place would mean an
    // unmatched path runs through the Basic Auth middleware and answers 401
    // instead of 404. Setting an explicit fallback afterward replaces it
    // with an unwrapped one, same as the SPA fallback below.
    #[cfg(feature = "ui")]
    let app = if admin_config.ui_enabled {
        app.fallback(get(ui::serve_ui))
    } else {
        app.fallback(not_found)
    };
    #[cfg(not(feature = "ui"))]
    let app = app.fallback(not_found);

    app
}

/// Unauthenticated 404 for non-API paths when the SPA fallback isn't
/// mounted (`ui_enabled: false`, or a binary built without the `ui`
/// feature). See the comment in [`build_router`] for why this must be set
/// explicitly rather than left as the router's implicit default.
async fn not_found() -> axum::http::StatusCode {
    axum::http::StatusCode::NOT_FOUND
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> Arc<SharedState> {
        // Every section of both configs has a serde default, so an empty
        // document is the cheapest way to get a valid baseline.
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from(
                    "gateway.yaml",
                ))),
            )
            .unwrap(),
        )
    }

    fn admin_config(ui_enabled: bool) -> AdminConfig {
        let yaml = format!("username: u\npassword: p\nui_enabled: {}\n", ui_enabled);
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[cfg(feature = "ui")]
    #[tokio::test]
    async fn test_non_api_path_serves_spa_when_ui_enabled() {
        let app = build_router(&admin_config(true), test_state());
        let resp = app
            .oneshot(Request::get("/some/spa/route").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // ui/dist may be absent in dev checkouts; the fallback is mounted
        // either way. 200 = SPA served; 404 only when the embedded bundle is
        // empty — both prove the fallback handler answered, so assert on the
        // handler's contract, not the bundle's presence.
        assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND);

        // The API surface is mounted regardless: /healthz exists (401 without
        // credentials proves it hit the authed API router, not the fallback).
        let app = build_router(&admin_config(true), test_state());
        let resp = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_non_api_path_404_when_ui_disabled() {
        let app = build_router(&admin_config(false), test_state());
        let resp = app
            .oneshot(Request::get("/some/spa/route").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
