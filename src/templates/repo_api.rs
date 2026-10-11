//! "New from repo" and "Resync from repo".
//!
//! A preview caches the repo through the git cache if needed (as a job the
//! UI follows), pins the ref to a commit, reads the manifests there and
//! shows what the template would hold. Saving reads the same commit again,
//! so what is saved is exactly what was previewed.

use std::collections::BTreeMap;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    RepoSource, TemplateInput, default_platforms,
    repo::{self, MAX_FILES, MAX_LOCKFILE, MAX_MANIFEST},
};
use crate::{
    AppState,
    auth::{AuthSession, require_admin},
    error::{AppError, AppResult},
    formats::git::{self, Refresh, RepoId},
    jobs::{GitJob, enqueue_git},
    net::ClientIp,
    store::now_secs,
};

/// Most bytes of manifests and lockfiles kept with one template.
const MAX_STORED: usize = 32 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/templates/from-repo/preview", post(preview))
        .route("/api/templates/from-repo", post(create))
        .route("/api/templates/{id}/resync", post(resync))
}

fn db_error(e: sqlx::Error) -> AppError {
    AppError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("template database: {e}"),
    )
}

/// A git job already queued or running for this repo, if any.
async fn active_git_job(st: &AppState, repo: &RepoId) -> Option<i64> {
    sqlx::query_as::<_, (i64,)>(
        "SELECT id FROM jobs WHERE kind = 'git' AND title = ? AND status IN ('queued', 'running') ORDER BY id DESC LIMIT 1",
    )
    .bind(format!("Git {}", repo.display()))
    .fetch_optional(&st.auth.db)
    .await
    .ok()
    .flatten()
    .map(|(id,)| id)
}

/// Queue a clone unless the repo is cached or already being cloned.
async fn queue_clone(st: &AppState, repo: RepoId, by: &str) -> AppResult<Option<i64>> {
    if repo.dir(st).exists() {
        return Ok(None);
    }
    if let Some(job) = active_git_job(st, &repo).await {
        return Ok(Some(job));
    }
    let job = enqueue_git(
        st,
        &GitJob {
            action: "clone".into(),
            repo,
        },
        by,
    )
    .await
    .map_err(db_error)?;
    Ok(Some(job))
}

/// Make sure the repo is cached (refreshing it if stale). `Some(job)` means
/// a clone is under way and the caller should wait for that job.
async fn ready(st: &AppState, repo: &RepoId, by: &str) -> AppResult<Option<i64>> {
    if repo.dir(st).exists() {
        if !st.cfg.offline {
            git::ensure(st, repo, Refresh::IfStale, None).await?;
        }
        return Ok(None);
    }
    if st.cfg.offline {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("{} is not cached, and offline mode is on", repo.display()),
        ));
    }
    queue_clone(st, repo.clone(), by).await
}

/// Read the manifests and lockfiles at a commit, workspace members included.
async fn read_repo(
    st: &AppState,
    id: &RepoId,
    commit: &str,
) -> AppResult<BTreeMap<String, String>> {
    let paths = git::list_files(st, id, commit).await?;
    let mut files = BTreeMap::new();
    let mut total = 0usize;
    let mut want = repo::candidates(&paths);
    // Workspace members can name further members only from a root, so one
    // extra round is enough.
    for round in 0..2 {
        for path in want {
            if files.len() >= MAX_FILES {
                break;
            }
            let max = if path.ends_with(".lock") || path.ends_with("-lock.json") {
                MAX_LOCKFILE
            } else {
                MAX_MANIFEST
            };
            if let Some(text) = git::read_file(st, id, commit, &path, max).await? {
                total += text.len();
                if total > MAX_STORED {
                    return Err(AppError::bad_request(
                        "the repo's manifests and lockfiles are too large to keep with a template",
                    ));
                }
                files.insert(path, text);
            }
        }
        if round == 1 {
            break;
        }
        want = repo::workspace_members(&files, &paths);
    }
    Ok(files)
}

fn preview_json(
    st: &AppState,
    repo: &RepoId,
    git_ref: &str,
    commit: &str,
    scan: &repo::Scan,
) -> Value {
    let git_deps: Vec<Value> = scan
        .git_deps
        .iter()
        .map(|d| {
            let cached = RepoId::new(&st.cfg, &d.host, &d.owner, &d.repo)
                .is_ok_and(|id| id.dir(st).exists());
            let mut v = json!(d);
            v["cached"] = json!(cached);
            v
        })
        .collect();
    json!({
        "repo": repo.display(),
        "url": format!("https://{}", repo.display()),
        "ref": git_ref,
        "commit": commit,
        "name": repo.name,
        "description": scan.description,
        "manifests": scan.manifests,
        "lockfiles": scan.lockfiles,
        "counts": scan.counts,
        "skipped": scan.skipped,
        "git_deps": git_deps,
        "lines": {"cargo": scan.cargo, "npm": scan.npm, "pypi": scan.pypi, "maven": scan.maven},
    })
}

#[derive(Deserialize)]
struct PreviewBody {
    url: String,
    #[serde(rename = "ref", default)]
    git_ref: String,
}

async fn preview(
    State(st): State<AppState>,
    auth: AuthSession,
    Json(body): Json<PreviewBody>,
) -> AppResult<Response> {
    let admin = require_admin(&auth)?.username.clone();
    let repo = RepoId::from_url(&st.cfg, &body.url).map_err(AppError::bad_request)?;
    if let Some(job) = ready(&st, &repo, &admin).await? {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "cloning": true, "job": job })),
        )
            .into_response());
    }
    let git_ref = body.git_ref.trim().to_string();
    let commit = git::resolve_ref(&st, &repo, Some(&git_ref)).await?;
    let files = read_repo(&st, &repo, &commit).await?;
    let scan = repo::scan(&files, &st.cfg.git_hosts);
    Ok(Json(preview_json(&st, &repo, &git_ref, &commit, &scan)).into_response())
}

/// The repo at a previewed commit, read again.
async fn read_at(
    st: &AppState,
    url: &str,
    commit: &str,
) -> AppResult<(RepoId, BTreeMap<String, String>, repo::Scan)> {
    let repo = RepoId::from_url(&st.cfg, url).map_err(AppError::bad_request)?;
    let pinned = commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit());
    if !pinned || !repo.dir(st).exists() || !git::has_commit(st, &repo, commit).await {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "that commit isn't in the cached repo; preview again",
        ));
    }
    let files = read_repo(st, &repo, commit).await?;
    let scan = repo::scan(&files, &st.cfg.git_hosts);
    Ok((repo, files, scan))
}

/// Cache the allowed git dependencies that aren't cached yet.
async fn queue_git_deps(st: &AppState, scan: &repo::Scan, by: &str) -> usize {
    let mut queued = 0;
    if st.cfg.offline {
        return 0;
    }
    for d in scan.git_deps.iter().filter(|d| d.allowed) {
        if let Ok(id) = RepoId::new(&st.cfg, &d.host, &d.owner, &d.repo)
            && let Ok(Some(_)) = queue_clone(st, id, by).await
        {
            queued += 1;
        }
    }
    queued
}

#[derive(Deserialize)]
struct CreateBody {
    url: String,
    #[serde(rename = "ref", default)]
    git_ref: String,
    commit: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_platforms")]
    platforms: Vec<String>,
    #[serde(default)]
    allow_sdist: bool,
}

async fn create(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Json(body): Json<CreateBody>,
) -> AppResult<Response> {
    let admin = require_admin(&auth)?.username.clone();
    let (repo, files, scan) = read_at(&st, &body.url, &body.commit).await?;
    let mut input = TemplateInput {
        name: body.name,
        description: body.description,
        tags: body.tags,
        platforms: body.platforms,
        allow_sdist: body.allow_sdist,
        cargo: scan.cargo.clone(),
        npm: scan.npm.clone(),
        pypi: scan.pypi.clone(),
        maven: scan.maven.clone(),
    };
    input.normalize();
    if let Some(p) = input.validate() {
        return Err(AppError::bad_request(p.summary()));
    }
    if super::name_taken(&st.auth.db, &input.name, None)
        .await
        .map_err(db_error)?
    {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("a template named {:?} already exists", input.name),
        ));
    }
    let id = super::insert(&st.auth.db, &input).await.map_err(db_error)?;
    let source = RepoSource {
        url: format!("https://{}", repo.display()),
        git_ref: body.git_ref.trim().to_string(),
        commit: body.commit.clone(),
        synced_at: now_secs() as i64,
        files,
    };
    super::set_source(&st.auth.db, id, &source)
        .await
        .map_err(db_error)?;
    let git_jobs = queue_git_deps(&st, &scan, &admin).await;
    crate::audit::record(
        &st.auth.db,
        &admin,
        "template.create",
        &format!(
            "{} (from {} at {})",
            input.name,
            source.url,
            &source.commit[..12]
        ),
        Some(ip.0),
        true,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "git_jobs": git_jobs })),
    )
        .into_response())
}

#[derive(Deserialize)]
struct ResyncBody {
    commit: String,
}

/// Replace a repo template's package lines with what the previewed commit
/// lists. Its name, tags, platforms and schedule are kept.
async fn resync(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
    Json(body): Json<ResyncBody>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.username.clone();
    let t = super::get(&st.auth.db, id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "no such template"))?;
    let Some(old) = t.source.clone() else {
        return Err(AppError::bad_request(
            "this template wasn't made from a repo",
        ));
    };
    let (_, files, scan) = read_at(&st, &old.url, &body.commit).await?;
    let mut input = t.input.clone();
    input.cargo = scan.cargo.clone();
    input.npm = scan.npm.clone();
    input.pypi = scan.pypi.clone();
    input.maven = scan.maven.clone();
    input.normalize();
    if let Some(p) = input.validate() {
        return Err(AppError::bad_request(p.summary()));
    }
    let source = RepoSource {
        commit: body.commit.clone(),
        synced_at: now_secs() as i64,
        files,
        ..old
    };
    if !super::resync(&st.auth.db, id, &input, &source)
        .await
        .map_err(db_error)?
    {
        return Err(AppError::bad_request("this template can't be resynced"));
    }
    let git_jobs = queue_git_deps(&st, &scan, &admin).await;
    crate::audit::record(
        &st.auth.db,
        &admin,
        "template.resync",
        &format!("{} (to {})", input.name, &source.commit[..12]),
        Some(ip.0),
        true,
    )
    .await;
    Ok(Json(
        json!({ "id": id, "counts": input.counts(), "git_jobs": git_jobs }),
    ))
}
