//! PyPI proxy implementing the Simple Repository API (PEP 503 / PEP 691).
//!
//! Client setup (`pip.conf`, or `uv` / `poetry` equivalents):
//!
//! ```ini
//! [global]
//! index-url = http://HOST:8080/pypi/simple/
//! ```
//!
//! Project pages are cached with a TTL and their file links are rewritten to
//! point back at this server. Distribution files (and their `.metadata`
//! companions) are cached forever; pip verifies the `sha256` in each link.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};

use crate::{
    AppState,
    error::{AppError, AppResult},
    util::base_url,
};

const SIMPLE_JSON: &str = "application/vnd.pypi.simple.v1+json";
const SIMPLE_HTML: &str = "text/html";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/pypi/simple",
            get(|| async { Redirect::permanent("/pypi/simple/") }),
        )
        .route("/pypi/simple/", get(root_index))
        .route("/pypi/simple/{project}", get(redirect_slash))
        .route("/pypi/simple/{project}/", get(project_page))
        .route("/pypi/files/{*path}", get(file))
}

/// PEP 503 name normalization.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !last_sep {
                out.push('-');
            }
            last_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        }
    }
    out
}

fn valid_project(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn variant(headers: &HeaderMap) -> (&'static str, &'static str) {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains(SIMPLE_JSON) {
        ("json", SIMPLE_JSON)
    } else {
        ("html", SIMPLE_HTML)
    }
}

async fn redirect_slash(Path(project): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/pypi/simple/{}/", normalize(&project)))
}

async fn root_index(State(st): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    serve_page(&st, &headers, "_index", format!("{}/", st.cfg.pypi_index)).await
}

async fn project_page(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> AppResult<Response> {
    let name = normalize(&project);
    if !valid_project(&name) {
        return Err(AppError::not_found());
    }
    let url = format!("{}/{name}/", st.cfg.pypi_index);
    serve_page(&st, &headers, &name, url).await
}

async fn serve_page(
    st: &AppState,
    headers: &HeaderMap,
    key: &str,
    url: String,
) -> AppResult<Response> {
    let (variant, accept) = variant(headers);
    let meta = st.store.meta("pypi", key, variant, &url, accept).await?;
    let ours = format!("{}/pypi/files/", base_url(&st.cfg, headers));
    let body =
        String::from_utf8_lossy(&meta.body).replace(&format!("{}/", st.cfg.pypi_files), &ours);
    Ok(([(header::CONTENT_TYPE, meta.content_type)], body).into_response())
}

async fn file(State(st): State<AppState>, Path(path): Path<String>) -> AppResult<Response> {
    let url = format!("{}/{path}", st.cfg.pypi_files);
    st.store
        .file("pypi", &path, &url, "application/octet-stream")
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pep503_normalization() {
        assert_eq!(normalize("Django"), "django");
        assert_eq!(normalize("zope.interface"), "zope-interface");
        assert_eq!(normalize("Foo__Bar-.baz"), "foo-bar-baz");
    }
}
