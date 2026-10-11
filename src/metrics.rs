//! Prometheus metrics at `/metrics`. Scrapers authenticate with a Granary
//! access token (`authorization: Bearer gran_…`), like package managers do.

use std::{fmt::Write, sync::atomic::Ordering};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    AppState,
    admin::{api::usage, disk_info},
    auth::{AuthSession, token_from_headers},
    error::AppError,
    store::now_secs,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/metrics", get(metrics))
}

async fn metrics(State(st): State<AppState>, auth: AuthSession, headers: HeaderMap) -> Response {
    let allowed = auth.user.is_some()
        || match token_from_headers(&headers) {
            // System tokens (the warmer's) may only download packages.
            Some(t) => {
                matches!(st.auth.user_for_token(&t).await, Ok(Some(_)))
                    && matches!(st.auth.is_system_token(&t).await, Ok(false))
            }
            None => false,
        };
    if !allowed {
        let mut resp = AppError::new(
            StatusCode::UNAUTHORIZED,
            "metrics need an access token (authorization: Bearer <token>)",
        )
        .into_response();
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer realm=\"Granary\""),
        );
        return resp;
    }

    let mut out = String::new();
    let mut metric = |name: &str, kind: &str, help: &str, samples: &[(String, f64)]| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (labels, v) in samples {
            let _ = writeln!(out, "{name}{labels} {v}");
        }
    };
    let s = &st.store.stats;
    let n = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed) as f64;

    metric(
        "granary_info",
        "gauge",
        "Granary version.",
        &[(
            format!("{{version=\"{}\"}}", env!("CARGO_PKG_VERSION")),
            1.0,
        )],
    );
    metric(
        "granary_uptime_seconds",
        "gauge",
        "Seconds since Granary started.",
        &[(
            String::new(),
            now_secs().saturating_sub(st.admin.started_at) as f64,
        )],
    );
    metric(
        "granary_file_requests_total",
        "counter",
        "Package file requests by result (hit: served from cache, miss: fetched upstream).",
        &[
            ("{result=\"hit\"}".into(), n(&s.file_hits)),
            ("{result=\"miss\"}".into(), n(&s.file_misses)),
        ],
    );
    metric(
        "granary_metadata_requests_total",
        "counter",
        "Metadata requests by result.",
        &[
            ("{result=\"hit\"}".into(), n(&s.meta_hits)),
            ("{result=\"refresh\"}".into(), n(&s.meta_refreshes)),
            ("{result=\"stale\"}".into(), n(&s.meta_stale_served)),
        ],
    );
    metric(
        "granary_integrity_failures_total",
        "counter",
        "Downloads rejected because they did not match the registry's checksum.",
        &[(String::new(), n(&s.integrity_failures))],
    );

    let inv = st.admin.inventory(&st.store).await;
    let repos = usage(&st, &inv).await;
    let label = |r: &str| format!("{{repo=\"{r}\"}}");
    metric(
        "granary_repository_packages",
        "gauge",
        "Cached packages per repository.",
        &repos
            .iter()
            .map(|r| (label(r.repo), r.packages as f64))
            .collect::<Vec<_>>(),
    );
    metric(
        "granary_repository_files",
        "gauge",
        "Cached files per repository.",
        &repos
            .iter()
            .map(|r| (label(r.repo), r.files as f64))
            .collect::<Vec<_>>(),
    );
    metric(
        "granary_repository_bytes",
        "gauge",
        "Bytes used per repository.",
        &repos
            .iter()
            .flat_map(|r| {
                [
                    (
                        format!("{{repo=\"{}\",kind=\"files\"}}", r.repo),
                        r.size as f64,
                    ),
                    (
                        format!("{{repo=\"{}\",kind=\"metadata\"}}", r.repo),
                        r.meta_size as f64,
                    ),
                ]
            })
            .collect::<Vec<_>>(),
    );

    let dir = st.store.root().to_path_buf();
    if let Ok(Some(d)) = tokio::task::spawn_blocking(move || disk_info(&dir)).await {
        metric(
            "granary_disk_total_bytes",
            "gauge",
            "Size of the data volume.",
            &[(String::new(), d.total as f64)],
        );
        metric(
            "granary_disk_free_bytes",
            "gauge",
            "Free space on the data volume.",
            &[(String::new(), d.free as f64)],
        );
    }

    let health = st.admin.health.lock().unwrap().clone();
    metric(
        "granary_upstream_up",
        "gauge",
        "Whether the last check could reach each upstream registry.",
        &health
            .iter()
            .map(|h| (label(h.repo), if h.ok { 1.0 } else { 0.0 }))
            .collect::<Vec<_>>(),
    );
    metric(
        "granary_upstream_latency_seconds",
        "gauge",
        "Response time of the last upstream check.",
        &health
            .iter()
            .map(|h| (label(h.repo), h.latency_ms as f64 / 1000.0))
            .collect::<Vec<_>>(),
    );

    let tasks = st.admin.tasks.lock().unwrap().clone();
    let task = |t: &str| format!("{{task=\"{t}\"}}");
    metric(
        "granary_task_last_success",
        "gauge",
        "Whether each scheduled task's last run succeeded.",
        &tasks
            .iter()
            .filter_map(|t| t.last_ok.map(|ok| (task(t.id), if ok { 1.0 } else { 0.0 })))
            .collect::<Vec<_>>(),
    );
    metric(
        "granary_task_last_run_timestamp_seconds",
        "gauge",
        "When each scheduled task last ran.",
        &tasks
            .iter()
            .filter_map(|t| t.last_run.map(|at| (task(t.id), at as f64)))
            .collect::<Vec<_>>(),
    );

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}
