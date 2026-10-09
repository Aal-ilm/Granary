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
    inventory::classify,
    store::Checksum,
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

async fn redirect_slash(Path(project): Path<String>) -> AppResult<Redirect> {
    let name = normalize(&project);
    if !valid_project(&name) {
        return Err(AppError::not_found());
    }
    Ok(Redirect::permanent(&format!("/pypi/simple/{name}/")))
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
    let expected = if st.store.has_file("pypi", &path).await {
        None
    } else {
        expected_sha256(&st, &path).await
    };
    st.store
        .file("pypi", &path, &url, "application/octet-stream", expected)
        .await
}

/// The sha256 the project's Simple API page lists for this file (or, for a
/// PEP 658 `.metadata` file, for its metadata).
async fn expected_sha256(st: &AppState, path: &str) -> Option<Checksum> {
    let dist = path.strip_suffix(".metadata").unwrap_or(path);
    let (project, _) = classify("pypi", dist)?;
    if !valid_project(&project) {
        return None;
    }
    let index = format!("{}/{project}/", st.cfg.pypi_index);
    let meta = st
        .store
        .meta("pypi", &project, "json", &index, SIMPLE_JSON)
        .await
        .ok()?;
    let want_url = format!("{}/{dist}", st.cfg.pypi_files);
    sha256_from_simple_json(&meta.body, &want_url, path.ends_with(".metadata"))
}

fn sha256_from_simple_json(doc: &[u8], file_url: &str, metadata: bool) -> Option<Checksum> {
    let v: serde_json::Value = serde_json::from_slice(doc).ok()?;
    let f = v.get("files")?.as_array()?.iter().find(|f| {
        f.get("url")
            .and_then(|u| u.as_str())
            .is_some_and(|u| u.split('#').next() == Some(file_url))
    })?;
    let hashes = if metadata {
        // PEP 714 renamed `dist-info-metadata` to `core-metadata`.
        f.get("core-metadata")
            .or_else(|| f.get("dist-info-metadata"))?
    } else {
        f.get("hashes")?
    };
    Checksum::sha256_hex(hashes.get("sha256")?.as_str()?)
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

    #[test]
    fn finds_sha256_in_simple_json() {
        let doc = br#"{"files":[{"filename":"a-1.0-py3-none-any.whl","url":"https://files.pythonhosted.org/packages/aa/bb/cc/a-1.0-py3-none-any.whl","hashes":{"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"},"core-metadata":{"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}]}"#;
        let url = "https://files.pythonhosted.org/packages/aa/bb/cc/a-1.0-py3-none-any.whl";
        assert!(sha256_from_simple_json(doc, url, false).is_some());
        assert!(sha256_from_simple_json(doc, url, true).is_some());
        assert!(sha256_from_simple_json(doc, "https://x/other.whl", false).is_none());
    }
}
