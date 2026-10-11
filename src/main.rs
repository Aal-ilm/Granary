//! Granary: a small, self-hostable caching proxy for package registries.

mod admin;
mod audit;
mod auth;
mod config;
mod error;
mod formats;
mod inventory;
mod jobs;
mod logbuf;
mod metrics;
mod net;
mod search;
mod store;
mod templates;
#[cfg(test)]
mod tests;
mod ui;
mod util;

use std::{net::SocketAddr, sync::Arc};

use axum::{Router, middleware, routing::get};
use axum_login::AuthManagerLayerBuilder;
use tower_sessions::{Expiry, SessionManagerLayer, cookie::SameSite};
use tracing::Level;
use tracing_subscriber::{
    EnvFilter, Layer, filter::Targets, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::{
    admin::Admin,
    auth::{Backend, SqliteSessionStore},
    config::Config,
    logbuf::LogBuffer,
    store::Store,
};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Arc<Store>,
    pub admin: Arc<Admin>,
    pub auth: Backend,
}

pub fn app(state: AppState, sessions: SqliteSessionStore) -> Router {
    // Package downloads: access token or signed-in session (if required).
    let packages = Router::new()
        .merge(formats::cargo::router())
        .merge(formats::npm::router())
        .merge(formats::pypi::router())
        .merge(formats::maven::router())
        .merge(formats::git::router())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::client_guard,
        ));

    // JSON API for the web UI: signed-in session for everything but sign-in.
    let api = Router::new()
        .merge(admin::api::router())
        .merge(admin::git::router())
        .merge(auth::api::router())
        .merge(audit::router())
        .merge(search::router())
        .merge(templates::api::router())
        .merge(jobs::api::router())
        .route_layer(middleware::from_fn(auth::api_guard));

    let idle = state.cfg.session_idle.as_secs() as i64;
    let session_layer = SessionManagerLayer::new(sessions)
        .with_name("granary_session")
        .with_http_only(true)
        .with_same_site(SameSite::Strict)
        .with_secure(state.cfg.cookie_secure)
        .with_expiry(Expiry::OnInactivity(time::Duration::seconds(idle)));
    let auth_layer = AuthManagerLayerBuilder::new(state.auth.clone(), session_layer).build();

    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .merge(ui::router())
        .merge(api)
        .merge(packages)
        .merge(metrics::router())
        .layer(auth_layer)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            net::client_ip,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            net::security_headers,
        ))
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
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("healthcheck") => healthcheck(),
        Some("reset-password") => {
            let Some(user) = args.get(2) else {
                anyhow::bail!("usage: granary reset-password <username> [--disable-2fa]");
            };
            let disable_2fa = args.iter().any(|a| a == "--disable-2fa");
            let cfg = Config::from_env()?;
            tokio::runtime::Runtime::new()?.block_on(auth::reset_password_cli(
                &cfg.data_dir,
                user,
                disable_2fa,
            ))
        }
        Some(other) => anyhow::bail!(
            "unknown command {other:?}. Commands: healthcheck, reset-password <username> [--disable-2fa]"
        ),
        None => run(),
    }
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
    let store = Store::new(cfg.data_dir.clone(), cfg.meta_ttl, cfg.offline).await?;
    let admin = Admin::new(&cfg.data_dir, logs).await?;

    let db = auth::open_db(&cfg.data_dir).await?;
    if let Some(password) =
        auth::ensure_admin(&db, &cfg.admin_user, cfg.admin_password.as_deref()).await?
    {
        // Printed straight to the container log, never to the in-app log view.
        println!();
        println!("  Granary created an admin account.");
        println!("    Username: {}", cfg.admin_user);
        println!("    Password: {password}");
        println!("  Sign in and you'll be asked to choose a new password.");
        println!();
    }
    templates::sync_builtins(&db).await?;
    let sessions = SqliteSessionStore::new(db.clone());
    // Sessions from versions that stored raw session IDs can't be looked up
    // any more; remove them (those people sign in once more).
    if let Ok(n) = sessions.remove_unhashed().await
        && n > 0
    {
        tracing::info!("removed {n} sessions saved in the old format");
    }
    let backend = Backend::new(db).await?;
    tracing::info!(
        data_dir = %store.root().display(),
        meta_ttl_secs = cfg.meta_ttl.as_secs(),
        "starting granary {} on {}",
        env!("CARGO_PKG_VERSION"),
        cfg.listen
    );

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    if !cfg.trusted_proxies.is_empty() {
        tracing::info!(
            "trusting X-Forwarded-* headers from {} proxy range(s)",
            cfg.trusted_proxies.len()
        );
    }
    if cfg.warmer_url.is_some() && cfg.warmer_secret.is_none() {
        tracing::warn!(
            "GRANARY_WARMER_URL is set but GRANARY_WARMER_SECRET is not; the warmer will refuse Granary's jobs"
        );
    }
    if cfg.offline {
        tracing::warn!(
            "GRANARY_OFFLINE is on: Granary serves only what is cached and never contacts an upstream"
        );
    }
    if !cfg.client_auth {
        tracing::warn!(
            "GRANARY_CLIENT_AUTH is off: anyone who can reach Granary can download packages through it"
        );
    }
    let state = AppState {
        cfg: Arc::new(cfg),
        store: Arc::new(store),
        admin: Arc::new(admin),
        auth: backend,
    };
    admin::spawn_scheduler(state.clone(), sessions.clone());
    jobs::spawn_runner(state.clone());
    let service = app(state.clone(), sessions).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, service)
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
