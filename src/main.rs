//! raycast2api gateway server entry point.
//!
//! Loads configuration, constructs the shared state and HTTP client, and serves
//! the OpenAI-compatible router until an interrupt/terminate signal arrives.

use std::sync::Arc;

use raycast2api::config::Settings;
use raycast2api::raycast::client::RaycastClient;
use raycast2api::server::build_router;
use raycast2api::server::state::{AppDeps, AppState};

#[tokio::main]
async fn main() {
    init_tracing();
    if let Err(message) = run().await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let settings = Settings::from_path(std::path::Path::new("config.json"))
        .map(Arc::new)
        .map_err(|err| {
            format!(
                "configuration error: {err}\nCreate a config.json file with api_key and raycast_signature_secret (see config.example.json)."
            )
        })?;

    let http = raycast2api::proxies::direct_client(settings.upstream_timeout());

    let db = raycast2api::storage::db::Db::open(&settings.accounts_db)
        .map_err(|e| format!("accounts db error: {e}"))?;
    let accounts = Arc::new(raycast2api::account::store::AccountStore::new(db.clone()));

    let statistics = Arc::new(raycast2api::statistics::store::StatisticsStore::new(
        db.clone(),
    ));
    let request_log = Arc::new(raycast2api::log::store::RequestLogStore::new(db.clone()));

    // 启动后每小时清理一次,避免服务空闲时过期日志滞留。
    {
        let request_log = request_log.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            interval.tick().await;
            let mut last_vacuum = std::time::Instant::now();
            loop {
                interval.tick().await;
                let log = request_log.clone();
                let deleted = tokio::task::spawn_blocking(move || log.cleanup_expired())
                    .await
                    .unwrap_or(0);
                if deleted > 0
                    && last_vacuum.elapsed() >= std::time::Duration::from_secs(24 * 60 * 60)
                {
                    let log = request_log.clone();
                    if let Err(error) = tokio::task::spawn_blocking(move || log.vacuum()).await {
                        tracing::warn!(%error, "log maintenance failed");
                    }
                    last_vacuum = std::time::Instant::now();
                }
            }
        });
    }

    let client = RaycastClient::new(http, settings.clone(), statistics.clone());
    let redis = raycast2api::storage::redis_store::connect_from_env()
        .await
        .map_err(|error| format!("Redis configuration error: {error}"))?;
    if settings.dashboard_password.is_some() && redis.is_none() {
        return Err("REDIS_HOST is required when dashboard_password is configured".into());
    }
    let state = AppState::new(AppDeps {
        settings: settings.clone(),
        client: client.clone(),
        accounts: accounts.clone(),
        log: request_log,
        statistics,
        db,
    })
    .map_err(|error| format!("API key initialization error: {error}"))?
    .with_redis(redis);
    let state = Arc::new(state);
    state
        .statistics
        .interrupt_unfinished()
        .expect("failed to recover unfinished statistics");

    // 启动立即刷新一遍账号额度,之后每 6 小时一遍;失败只记日志。
    {
        let accounts = accounts;
        let client = client;
        tokio::spawn(async move {
            loop {
                raycast2api::account::quota_refresh::refresh_all(&accounts, &client).await;
                tokio::time::sleep(raycast2api::account::quota_refresh::REFRESH_INTERVAL).await;
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(settings.bind_addr)
        .await
        .map_err(|err| format!("failed to bind {}: {err}", settings.bind_addr))?;
    tracing::info!(
        "raycast2api gateway listening on http://{}",
        settings.bind_addr
    );
    let mut listeners = vec![listener];

    // Dual-stack: if binding to 0.0.0.0, also bind to [::]; failing that is non-fatal.
    if let Some(dual_addr) = settings.bind_addr_dual {
        match tokio::net::TcpListener::bind(dual_addr).await {
            Ok(listener) => {
                tracing::info!("raycast2api gateway listening on http://{dual_addr}");
                listeners.push(listener);
            }
            Err(err) => tracing::warn!("failed to bind dual-stack {dual_addr}: {err}"),
        }
    }

    let app = build_router(state);
    let mut servers = tokio::task::JoinSet::new();
    for listener in listeners {
        let app = app.clone();
        servers.spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await
        });
    }
    // Any listener failing takes the whole process down; a clean shutdown drains them all.
    while let Some(result) = servers.join_next().await {
        result
            .map_err(|err| format!("server task failed: {err}"))?
            .map_err(|err| format!("server error: {err}"))?;
    }
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::fmt::time::OffsetTime;
    use tracing_subscriber::{EnvFilter, fmt};
    let timer = OffsetTime::new(
        time::UtcOffset::from_hms(8, 0, 0).expect("valid UTC+8 offset"),
        time::format_description::well_known::Rfc3339,
    );
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    fmt().with_timer(timer).with_env_filter(filter).init();
}

/// Resolve once Ctrl-C (or SIGTERM on Unix) is received, enabling graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
