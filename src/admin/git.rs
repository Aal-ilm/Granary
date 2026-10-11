//! Git repo routes for the web UI. Anyone signed in can list cached repos
//! (private ones only if the access policy lets them read those); adding,
//! refreshing and deleting them needs an admin.

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState,
    auth::{AuthSession, require_admin},
    error::{AppError, AppResult},
    formats::git::{self, RepoId},
    jobs::{GitJob, JobBy, enqueue_git},
    net::ClientIp,
    policy::Principal,
    util::base_url,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/git/repos", get(list).post(add).delete(remove))
        .route("/api/git/repos/refresh", post(refresh))
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

async fn list(State(st): State<AppState>, who: Principal, headers: HeaderMap) -> Json<Value> {
    let root = git::git_root(&st);
    let repos = tokio::task::spawn_blocking(move || git::list(&root))
        .await
        .unwrap_or_default();
    let base = base_url(&st.cfg, &headers);
    let repos: Vec<Value> = repos
        .into_iter()
        .filter(|r| {
            st.policy
                .can_read(&who, git::cached_resource(&st.cfg, r).as_ref())
        })
        .map(|r| {
            let clone_url = format!("{base}/git/{}", r.id.rel());
            let private = git::cached_resource(&st.cfg, &r).is_some();
            let mut v = json!(r);
            v["clone_url"] = json!(clone_url);
            v["private"] = json!(private);
            v
        })
        .collect();
    Json(json!({
        "repos": repos,
        "hosts": st.cfg.git_hosts,
        "gitea_host": st.cfg.gitea.as_ref().map(|g| &g.host),
        "offline": st.cfg.offline,
        "max_size": st.cfg.git_max_size,
    }))
}

#[derive(Deserialize)]
struct AddBody {
    url: String,
}

/// "Add repo": cache it (or refresh it if it's already cached) in a job.
async fn add(
    State(st): State<AppState>,
    auth: AuthSession,
    who: Principal,
    Extension(ip): Extension<ClientIp>,
    Json(body): Json<AddBody>,
) -> AppResult<Response> {
    let user = require_admin(&auth)?;
    let (admin, by) = (user.username.clone(), JobBy::user(user));
    let repo = RepoId::from_url(&st.cfg, &body.url).map_err(AppError::bad_request)?;
    let repo = readable(&st, &who, repo)?;
    if st.cfg.offline {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "offline mode is on, so repos can't be fetched",
        ));
    }
    let target = repo.display();
    let id = enqueue_git(
        &st,
        &GitJob {
            action: "clone".into(),
            repo,
        },
        &by,
    )
    .await
    .map_err(db_error)?;
    audit(&st, &admin, "git.add", &target, ip).await;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

#[derive(Deserialize)]
struct RepoQuery {
    host: String,
    owner: String,
    name: String,
}

/// A repo `who` may read, or "not found" as if it didn't exist.
fn readable(st: &AppState, who: &Principal, repo: RepoId) -> AppResult<RepoId> {
    match repo.resource(&st.cfg) {
        Some(r) if !st.policy.can_read_private(who, &r) => Err(AppError::new(
            StatusCode::NOT_FOUND,
            format!("{} is not cached", repo.display()),
        )),
        _ => Ok(repo),
    }
}

fn cached_repo(st: &AppState, who: &Principal, q: &RepoQuery) -> AppResult<RepoId> {
    let repo = RepoId::new(&st.cfg, &q.host, &q.owner, &q.name).map_err(AppError::bad_request)?;
    let repo = readable(st, who, repo)?;
    if !repo.dir(st).exists() {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            format!("{} is not cached", repo.display()),
        ));
    }
    Ok(repo)
}

async fn refresh(
    State(st): State<AppState>,
    auth: AuthSession,
    who: Principal,
    Extension(ip): Extension<ClientIp>,
    Json(q): Json<RepoQuery>,
) -> AppResult<Response> {
    let user = require_admin(&auth)?;
    let (admin, by) = (user.username.clone(), JobBy::user(user));
    let repo = cached_repo(&st, &who, &q)?;
    if st.cfg.offline {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "offline mode is on, so repos can't be refreshed",
        ));
    }
    let target = repo.display();
    let id = enqueue_git(
        &st,
        &GitJob {
            action: "refresh".into(),
            repo,
        },
        &by,
    )
    .await
    .map_err(db_error)?;
    audit(&st, &admin, "git.refresh", &target, ip).await;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

async fn remove(
    State(st): State<AppState>,
    auth: AuthSession,
    who: Principal,
    Extension(ip): Extension<ClientIp>,
    Query(q): Query<RepoQuery>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.username.clone();
    let repo = cached_repo(&st, &who, &q)?;
    let freed = git::delete(&st, &repo).await?;
    tracing::info!("git: deleted {}", repo.display());
    audit(&st, &admin, "git.delete", &repo.display(), ip).await;
    Ok(Json(json!({ "bytes": freed })))
}
