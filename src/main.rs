//! Command-line entry point: parses `--system-config` / `--gateway-config`,
//! loads both files, and runs the gateway (`featherbit` crate).

/// mimalloc instead of the platform allocator. The published image is a static
/// musl build, and musl's malloc serializes under concurrency: the competitive
/// benchmark measured ~2x throughput at 4 cores (~3x on 64 KiB payloads).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use featherbit::config::{self, ConfigSourceKind, GatewayConfig, SystemConfig};
use featherbit::config_store::{self, ConfigStore, FileConfigStore};
use featherbit::state::SharedState;
use featherbit::{admin, hot_reload, server, stream};
use tracing::{error, info};

/// Command-line arguments: paths to the two YAML configuration files.
#[derive(Parser)]
#[command(name = "featherbit", about = "A lightweight API gateway")]
struct Cli {
    /// Path to system.yaml
    #[arg(long, default_value = "config/system.yaml")]
    system_config: PathBuf,

    /// Path to gateway.yaml
    #[arg(long, default_value = "config/gateway.yaml")]
    gateway_config: PathBuf,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Load system config
    let mut system: SystemConfig = match config::load_yaml_with_env(&cli.system_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load system config: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = system
        .resolve_inherited_tls()
        .and_then(|()| system.validate())
    {
        eprintln!("Invalid system config: {}", e);
        std::process::exit(1);
    }

    // Initialize logging
    init_logging(&system.logging);

    info!("Starting featherbit v{}", env!("CARGO_PKG_VERSION"));

    // Select the config backend and load the initial gateway config.
    let (config_store, gateway, config_path): (
        Arc<dyn ConfigStore>,
        GatewayConfig,
        Option<PathBuf>,
    ) = match system.config.source {
        ConfigSourceKind::File => {
            let store = Arc::new(FileConfigStore::new(cli.gateway_config.clone()));
            let gw = match store.load_all().await {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to load gateway config: {}", e);
                    std::process::exit(1);
                }
            };
            (store, gw, Some(cli.gateway_config.clone()))
        }
        ConfigSourceKind::Etcd => match build_etcd_source(&system, &cli.gateway_config).await {
            Ok(v) => v,
            Err(e) => {
                error!("etcd config source: {}", e);
                std::process::exit(1);
            }
        },
    };

    if let Err(e) = system.validate_against_gateway(&gateway) {
        eprintln!("Invalid config: {}", e);
        std::process::exit(1);
    }

    // Build shared state
    let state = match SharedState::new(system.clone(), gateway, config_path, config_store) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!("Failed to initialize gateway: {}", e);
            std::process::exit(1);
        }
    };

    state
        .resources
        .traffic
        .cache
        .set_capacity(system.cache.max_entries);

    {
        let routes = state.routes.read().await;
        for (route, _) in routes.iter() {
            info!(
                "Route '{}' -> policy '{}' (match: {:?})",
                route.name, route.policy, route.match_rule.path
            );
        }
    }

    // Shutdown coordination: a signal task flips this to `true` on SIGTERM /
    // Ctrl+C; every accept loop watches it, stops accepting, and drains.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received; draining…");
        let _ = shutdown_tx.send(true);
    });

    // Start the config-change watcher appropriate to the source.
    match system.config.source {
        ConfigSourceKind::File => {
            let reload_state = state.clone();
            let watch_path = cli.gateway_config.clone();
            tokio::spawn(async move {
                hot_reload::watch_config(reload_state, watch_path).await;
            });
        }
        ConfigSourceKind::Etcd => {
            spawn_etcd_watch(state.clone(), &system);
        }
    }

    let drain_timeout = std::time::Duration::from_secs(system.timeouts.shutdown_timeout_seconds);

    // Start admin API (if configured), keeping its handle so we can await its
    // drain before exiting.
    let admin_handle = system.admin.as_ref().map(|admin_config| {
        let admin_state = state.clone();
        let admin_cfg = admin_config.clone();
        let admin_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) =
                admin::start_admin_server(&admin_cfg, admin_state, admin_shutdown, drain_timeout)
                    .await
            {
                error!("Admin API error: {}", e);
            }
        })
    });

    // Start L4 (TCP/UDP) stream listeners, if any. Binds fail-fast before the
    // data plane; each listener then runs in its own detached task.
    if !system.stream.is_empty() {
        if let Err(e) =
            stream::start_all(&system.stream, &system.timeouts, shutdown_rx.clone()).await
        {
            error!("Failed to start stream listeners: {}", e);
            std::process::exit(1);
        }
    }

    // Start the data-plane server. It blocks until a shutdown signal, then
    // drains in-flight connections and returns.
    if let Err(e) = server::start_server(&system, state, shutdown_rx).await {
        error!("Server error: {}", e);
        std::process::exit(1);
    }

    // Let the admin API finish draining too before the process exits.
    if let Some(handle) = admin_handle {
        let _ = handle.await;
    }
    info!("Shutdown complete");
}

/// Completes when the process receives a termination signal: Ctrl+C on any
/// platform, or `SIGTERM` on Unix (the signal container orchestrators send).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Builds the etcd config store and the initial gateway config (seeding etcd
/// from the local file when the prefix is empty).
async fn build_etcd_source(
    system: &SystemConfig,
    seed_path: &std::path::Path,
) -> Result<(Arc<dyn ConfigStore>, GatewayConfig, Option<PathBuf>), String> {
    config_store::etcd::build_source(system, seed_path).await
}

/// Spawns the etcd watch task (cluster-wide config convergence).
fn spawn_etcd_watch(state: Arc<SharedState>, system: &SystemConfig) {
    config_store::etcd::spawn_watch(state, system);
}

/// Initializes the global `tracing` subscriber in JSON or plain-text format.
///
/// The `RUST_LOG` environment variable, when set, takes precedence over the
/// level configured in `system.yaml`.
fn init_logging(config: &config::LoggingConfig) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.level));

    match config.format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .init();
        }
        _ => {
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
        }
    }
}
