//! raycast2api gateway server entry point.
//!
//! Loads configuration, constructs the shared state and HTTP client, and serves
//! the OpenAI-compatible router until an interrupt/terminate signal arrives.

use std::sync::Arc;

use raycast2api::config::Settings;
use raycast2api::raycast::client::RaycastClient;
use raycast2api::server::build_router;
use raycast2api::server::state::AppState;

#[tokio::main]
async fn main() {
    init_tracing();

    let settings = match Settings::from_config() {
        Ok(settings) => Arc::new(settings),
        Err(err) => {
            eprintln!("configuration error: {err}");
            eprintln!(
                "Create a config.json file or set RAYCAST_DEVICE_ID environment variables (see .env.example)."
            );
            std::process::exit(1);
        }
    };

    let http = reqwest::Client::builder()
        .build()
        .expect("failed to build HTTP client");

    let db = raycast2api::db::Db::open(&settings.accounts_db).unwrap_or_else(|e| {
        eprintln!("accounts db error: {e}");
        std::process::exit(1);
    });
    let accounts = Arc::new(raycast2api::account::store::AccountStore::new(db));
    if let Err(err) = raycast2api::account::import::import_file(&accounts, "account.json") {
        eprintln!("account import error: {err}");
        std::process::exit(1);
    }

    let client = RaycastClient::new(http, settings.clone(), accounts.clone());
    let state = Arc::new(AppState::new(settings.clone(), client.clone(), accounts));

    let listener = match tokio::net::TcpListener::bind(settings.bind_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind {}: {err}", settings.bind_addr);
            std::process::exit(1);
        }
    };

    tracing::info!(
        "raycast2api gateway listening on http://{}",
        settings.bind_addr
    );

    if settings.api_key.is_none() {
        tracing::warn!("API_KEY is not set — the gateway will accept unauthenticated requests");
    }

    let app = build_router(state);

    // Dual-stack: if binding to 0.0.0.0, also bind to [::]
    if let Some(dual_addr) = settings.bind_addr_dual {
        match tokio::net::TcpListener::bind(dual_addr).await {
            Ok(dual_listener) => {
                tracing::info!("raycast2api gateway listening on http://{}", dual_addr);

                // Serve both listeners concurrently
                let app_clone = app.clone();
                let primary_handle = tokio::spawn(async move {
                    if let Err(err) = axum::serve(listener, app)
                        .with_graceful_shutdown(shutdown_signal())
                        .await
                    {
                        tracing::error!("primary server error: {err}");
                    }
                });

                let dual_handle = tokio::spawn(async move {
                    if let Err(err) = axum::serve(dual_listener, app_clone)
                        .with_graceful_shutdown(shutdown_signal())
                        .await
                    {
                        tracing::error!("dual-stack server error: {err}");
                    }
                });

                // Wait for either to fail
                tokio::select! {
                    _ = primary_handle => {},
                    _ = dual_handle => {},
                }
            }
            Err(err) => {
                tracing::warn!("failed to bind dual-stack {}: {err}", dual_addr);
                // Fall back to primary listener only
                if let Err(err) = axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown_signal())
                    .await
                {
                    tracing::error!("server error: {err}");
                    std::process::exit(1);
                }
            }
        }
    } else {
        // Single-stack mode
        if let Err(err) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            tracing::error!("server error: {err}");
            std::process::exit(1);
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    fmt().with_env_filter(filter).init();
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
