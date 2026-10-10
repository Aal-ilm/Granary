//! Template routes. Anyone signed in can list, view and export templates;
//! creating, editing, cloning and deleting them needs the admin role.

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{PLATFORMS, Template, TemplateInput, default_platforms, lines, manifest};
use crate::{
    AppState,
    auth::{AuthSession, require_admin},
    error::{AppError, AppResult},
    net::ClientIp,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/templates", get(list).post(create))
        .route("/api/templates/check", post(check))
        .route("/api/templates/import", post(import))
        .route("/api/templates/{id}", get(one).put(update).delete(delete))
        .route("/api/templates/{id}/clone", post(clone))
        .route("/api/templates/{id}/export", get(export))
}

fn db_error(e: sqlx::Error) -> AppError {
    AppError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("template database: {e}"),
    )
}

async fn audit(st: &AppState, actor: &str, action: &str, target: &str, ip: ClientIp) {
    crate::audit::record(&st.auth.db, actor, action, target, Some(ip.0), true).await;
}

fn summary(t: &Template) -> Value {
    json!({
        "id": t.id,
        "builtin": t.builtin,
        "name": t.input.name,
        "description": t.input.description,
        "tags": t.input.tags,
        "platforms": t.input.platforms,
        "allow_sdist": t.input.allow_sdist,
        "counts": t.input.counts(),
        "updated_at": t.updated_at,
        "last_warmed_at": t.last_warmed_at,
    })
}

async fn load(st: &AppState, id: i64) -> AppResult<Template> {
    super::get(&st.auth.db, id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "no such template"))
}

async fn list(State(st): State<AppState>) -> AppResult<Json<Value>> {
    let all = super::list(&st.auth.db).await.map_err(db_error)?;
    Ok(Json(json!({
        "templates": all.iter().map(summary).collect::<Vec<_>>(),
        "platforms": PLATFORMS.iter().map(|(id, label)| json!({"id": id, "label": label})).collect::<Vec<_>>(),
        "default_platforms": default_platforms(),
    })))
}

async fn one(State(st): State<AppState>, Path(id): Path<i64>) -> AppResult<Json<Value>> {
    let t = load(&st, id).await?;
    let mut v = summary(&t);
    for eco in lines::Eco::ALL {
        v[eco.as_str()] = json!(t.input.section(eco));
    }
    v["created_at"] = json!(t.created_at);
    Ok(Json(v))
}

/// Validate and tidy a template from the editor, or explain what's wrong.
async fn prepare(
    st: &AppState,
    mut input: TemplateInput,
    id: Option<i64>,
) -> AppResult<TemplateInput> {
    input.normalize();
    if let Some(p) = input.validate() {
        return Err(AppError::bad_request(p.summary()));
    }
    if super::name_taken(&st.auth.db, &input.name, id)
        .await
        .map_err(db_error)?
    {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("a template named {:?} already exists", input.name),
        ));
    }
    Ok(input)
}

#[derive(Deserialize)]
struct CheckQuery {
    /// The template being edited, so its own name doesn't count as taken.
    id: Option<i64>,
}

/// Check a template without saving it: per-field and per-line problems,
/// and how many packages each ecosystem has.
async fn check(
    State(st): State<AppState>,
    auth: AuthSession,
    Query(q): Query<CheckQuery>,
    Json(mut input): Json<TemplateInput>,
) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    input.normalize();
    let mut problems = input.validate().unwrap_or_default();
    if !input.name.is_empty()
        && super::name_taken(&st.auth.db, &input.name, q.id)
            .await
            .map_err(db_error)?
    {
        problems.fields.push((
            "name",
            format!("a template named {:?} already exists", input.name),
        ));
    }
    let ok = problems.fields.is_empty() && problems.lines.is_empty();
    Ok(Json(
        json!({ "ok": ok, "problems": problems.to_json(), "counts": input.counts() }),
    ))
}

async fn create(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Json(input): Json<TemplateInput>,
) -> AppResult<Response> {
    let admin = require_admin(&auth)?.username.clone();
    let input = prepare(&st, input, None).await?;
    let id = super::insert(&st.auth.db, &input).await.map_err(db_error)?;
    audit(&st, &admin, "template.create", &input.name, ip).await;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

async fn update(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
    Json(input): Json<TemplateInput>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.username.clone();
    let existing = load(&st, id).await?;
    if existing.builtin {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "built-in templates are read-only; clone it to make your own version",
        ));
    }
    let input = prepare(&st, input, Some(id)).await?;
    super::update(&st.auth.db, id, &input)
        .await
        .map_err(db_error)?;
    audit(&st, &admin, "template.update", &input.name, ip).await;
    Ok(Json(json!({ "id": id })))
}

async fn delete(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.username.clone();
    let existing = load(&st, id).await?;
    if existing.builtin {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "built-in templates can't be deleted",
        ));
    }
    super::delete(&st.auth.db, id).await.map_err(db_error)?;
    audit(&st, &admin, "template.delete", &existing.input.name, ip).await;
    Ok(Json(json!({ "deleted": true })))
}

async fn clone(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let admin = require_admin(&auth)?.username.clone();
    let source = load(&st, id).await?;
    let mut copy = source.input.clone();
    copy.name = super::copy_name(&st.auth.db, &source.input.name)
        .await
        .map_err(db_error)?;
    let new_id = super::insert(&st.auth.db, &copy).await.map_err(db_error)?;
    audit(
        &st,
        &admin,
        "template.clone",
        &format!("{} -> {}", source.input.name, copy.name),
        ip,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": new_id, "name": copy.name })),
    )
        .into_response())
}

/// A file name for a template: lowercase letters, digits and dashes.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "template".into()
    } else {
        out
    }
}

async fn export(State(st): State<AppState>, Path(id): Path<i64>) -> AppResult<Response> {
    let t = load(&st, id).await?;
    let disposition = format!("attachment; filename=\"{}.toml\"", slug(&t.input.name));
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/toml; charset=utf-8".to_string(),
            ),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        t.input.to_toml(),
    )
        .into_response())
}

#[derive(Deserialize)]
struct ImportBody {
    filename: Option<String>,
    content: String,
}

async fn import(auth: AuthSession, Json(body): Json<ImportBody>) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    let r =
        manifest::import(body.filename.as_deref(), &body.content).map_err(AppError::bad_request)?;
    Ok(Json(json!(r)))
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn slugs() {
        assert_eq!(slug("React + Vite + TS"), "react-vite-ts");
        assert_eq!(slug("Rust embedded (Cortex-M)"), "rust-embedded-cortex-m");
        assert_eq!(slug("!!!"), "template");
    }
}
