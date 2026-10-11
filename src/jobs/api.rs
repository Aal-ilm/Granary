//! Job routes. Anyone signed in can see jobs and their logs; starting and
//! cancelling them needs an admin.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{JOB_COLUMNS, JobBy, JobRow, NewJob, warmer};
use crate::{
    AppState,
    auth::{AuthSession, require_admin},
    error::{AppError, AppResult},
    net::ClientIp,
    policy::Principal,
    templates::{self, build, lines::Eco},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/jobs", get(list))
        .route("/api/jobs/{id}", get(one))
        .route("/api/jobs/{id}/cancel", post(cancel))
        .route("/api/templates/{id}/warm", post(warm_template))
        .route("/api/packages/warm", post(warm_package))
        .route("/api/warmer", get(warmer_status))
}

fn db_error(e: sqlx::Error) -> AppError {
    AppError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("job database: {e}"),
    )
}

async fn audit(st: &AppState, actor: &str, action: &str, target: &str, ip: ClientIp) {
    crate::audit::record(&st.auth.db, actor, action, target, Some(ip.0), true).await;
}

/// Refuse to queue work the warmer can't do.
async fn need_warmer(st: &AppState) -> AppResult<()> {
    if st.cfg.offline {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "offline mode is on, so nothing new can be fetched",
        ));
    }
    let h = warmer::health(st).await;
    if !h.configured {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "warming needs the warmer service; set GRANARY_WARMER_URL and run the granary-warmer container",
        ));
    }
    if !h.available {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            h.error
                .unwrap_or_else(|| "the warmer is not available".into()),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct ListQuery {
    template: Option<i64>,
}

async fn list(
    State(st): State<AppState>,
    who: Principal,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let sql = format!("SELECT {JOB_COLUMNS} FROM jobs");
    let rows: Vec<JobRow> = match q.template {
        Some(t) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{sql} WHERE template_id = ? ORDER BY id DESC LIMIT 200"
            )))
            .bind(t)
            .fetch_all(&st.auth.db)
            .await
        }
        None => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "{sql} ORDER BY id DESC LIMIT 200"
            )))
            .fetch_all(&st.auth.db)
            .await
        }
    }
    .map_err(db_error)?;
    let rows: Vec<JobRow> = rows
        .into_iter()
        .filter(|j| st.policy.can_read(&who, j.resource().as_ref()))
        .collect();
    Ok(Json(
        json!({ "jobs": rows, "max_running": st.cfg.warm_jobs }),
    ))
}

#[derive(Deserialize)]
struct OneQuery {
    /// Only log lines after this number.
    after: Option<i64>,
}

/// A job `who` may see; one on private content they can't read is "no such
/// job", like one that doesn't exist.
async fn load(st: &AppState, who: &Principal, id: i64) -> AppResult<JobRow> {
    sqlx::query_as::<_, JobRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(&st.auth.db)
    .await
    .map_err(db_error)?
    .filter(|j| st.policy.can_read(who, j.resource().as_ref()))
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "no such job"))
}

async fn one(
    State(st): State<AppState>,
    who: Principal,
    Path(id): Path<i64>,
    Query(q): Query<OneQuery>,
) -> AppResult<Json<Value>> {
    let job = load(&st, &who, id).await?;
    let lines: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT n, at, line FROM job_log WHERE job_id = ? AND n > ? ORDER BY n LIMIT 2000",
    )
    .bind(id)
    .bind(q.after.unwrap_or(0))
    .fetch_all(&st.auth.db)
    .await
    .map_err(db_error)?;
    Ok(Json(json!({
        "job": job,
        "lines": lines.into_iter().map(|(n, at, line)| json!({"n": n, "at": at, "line": line})).collect::<Vec<_>>(),
    })))
}

async fn cancel(
    State(st): State<AppState>,
    auth: AuthSession,
    who: Principal,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.username.clone();
    load(&st, &who, id).await?;
    if !super::cancel(&st, id, &admin).await.map_err(db_error)? {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "that job has already finished",
        ));
    }
    audit(&st, &admin, "job.cancel", &format!("#{id}"), ip).await;
    Ok(Json(json!({ "cancelled": true })))
}

async fn warm_template(
    State(st): State<AppState>,
    auth: AuthSession,
    who: Principal,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let user = require_admin(&auth)?;
    let (admin, by) = (user.username.clone(), JobBy::user(user));
    let t = templates::api::load(&st, &who, id).await?;
    need_warmer(&st).await?;
    let repo = t.source.as_ref().and_then(super::RepoWarm::from_source);
    let new = NewJob {
        kind: "template",
        template_id: Some(id),
        title: &t.input.name,
        spec: &t.input,
        repo,
        private: t.resource(&st.cfg),
    };
    let job = super::enqueue(&st, new, &by).await.map_err(db_error)?;
    audit(
        &st,
        &admin,
        "job.create",
        &format!("#{job} warm {}", t.input.name),
        ip,
    )
    .await;
    Ok((StatusCode::CREATED, Json(json!({ "id": job }))).into_response())
}

#[derive(Deserialize)]
struct PackageBody {
    eco: String,
    name: String,
    version: String,
}

/// Cache one package version with all its dependencies.
async fn warm_package(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Json(body): Json<PackageBody>,
) -> AppResult<Response> {
    let user = require_admin(&auth)?;
    let (admin, by) = (user.username.clone(), JobBy::user(user));
    let eco = Eco::parse(&body.eco).ok_or_else(|| AppError::bad_request("unknown ecosystem"))?;
    let spec = build::single(
        eco,
        body.name.trim(),
        body.version.trim(),
        templates::default_platforms(),
    )
    .map_err(AppError::bad_request)?;
    need_warmer(&st).await?;
    let title = format!(
        "{} {} {}",
        eco.label(),
        body.name.trim(),
        body.version.trim()
    );
    let new = NewJob {
        kind: "package",
        template_id: None,
        title: &title,
        spec: &spec,
        repo: None,
        private: None,
    };
    let job = super::enqueue(&st, new, &by).await.map_err(db_error)?;
    audit(
        &st,
        &admin,
        "job.create",
        &format!("#{job} warm {title}"),
        ip,
    )
    .await;
    Ok((StatusCode::CREATED, Json(json!({ "id": job }))).into_response())
}

async fn warmer_status(State(st): State<AppState>) -> Json<Value> {
    let h = warmer::health(&st).await;
    Json(json!({
        "configured": h.configured,
        "available": h.available && !st.cfg.offline,
        "offline": st.cfg.offline,
        "tools": h.tools,
        "error": h.error,
        "max_running": st.cfg.warm_jobs,
        "timeout_mins": st.cfg.warm_timeout.as_secs() / 60,
    }))
}
