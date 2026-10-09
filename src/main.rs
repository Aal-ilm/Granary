//! Granary: a small, self-hostable caching proxy for package registries.

mod admin;
mod config;
mod error;
mod formats;
mod inventory;
mod logbuf;
mod store;
mod ui;
mod util;

use std::sync::Arc;

use axum::{Router, routing::get};
use tracing::Level;
use tracing_subscriber::{
    EnvFilter, Layer, filter::Targets, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::{admin::Admin, config::Config, logbuf::LogBuffer, store::Store};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub admin: Arc<Admin>,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .merge(ui::router())
        .merge(admin::api::router())
        .merge(formats::cargo::router())
        .merge(formats::npm::router())
        .merge(formats::pypi::router())
        .with_state(state)
}

/// `granary healthcheck`: exit 0 if the local server answers `/healthz`.
/// Lets the container image run a HEALTHCHECK without shipping curl.
fn healthcheck() -> ! {
    use std::io::{Read, Write};
    let listen = std::env::var("GRANARY_LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let port = listen.rsplit(':').next().unwrap_or("8080");
    let ok = (|| -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{port}"))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200"))
    })()
    .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 })
}

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        healthcheck();
    }
    run()
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    // Logs go to stdout (filtered by RUST_LOG) and, at info level and above,
    // into an in-memory buffer shown on the web UI's Logs page.
    let logs = LogBuffer::default();
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("granary=info"));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env_filter))
        .with(
            logs.clone()
                .with_filter(Targets::new().with_target("granary", Level::INFO)),
        )
        .init();

    let cfg = Config::from_env()?;
    let store = Store::new(cfg.data_dir.clone(), cfg.meta_ttl).await?;
    let admin = Admin::new(&cfg.data_dir, logs).await?;
    tracing::info!(
        data_dir = %store.root().display(),
        meta_ttl_secs = cfg.meta_ttl.as_secs(),
        "starting granary {} on {}",
        env!("CARGO_PKG_VERSION"),
        cfg.listen
    );

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    if cfg.admin_token.is_none() {
        tracing::warn!(
            "GRANARY_ADMIN_TOKEN is not set: anyone who can reach the web UI can delete cached packages"
        );
    }
    let state = AppState {
        cfg: Arc::new(cfg),
        store: Arc::new(store),
        admin: Arc::new(admin),
    };
    admin::spawn_scheduler(state.clone());
    axum::serve(listener, app(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Keep download history from the last few seconds before shutdown.
    if let Err(e) = state.store.flush_access().await {
        tracing::warn!("could not save download history: {e}");
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}
