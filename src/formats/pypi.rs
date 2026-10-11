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
//!
//! Names listed as private namespaces (compared after PEP 503 normalization,
//! see `namespaces`) come from Gitea's PyPI registry instead, and only from
//! there: their pages and files are never fetched from PyPI. Their files are
//! served at `/pypi/gitea-files/{project}/{version}/{file}` and cached under
//! `gitea-pypi/{owner}/`.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, Uri, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};

use crate::{
    AppState,
    error::{AppError, AppResult},
    inventory::classify,
    policy::{Principal, Resource, refuse},
    store::{Checksum, Meta},
    templates::lines::Eco,
    util::base_url,
};

pub const SIMPLE_JSON: &str = "application/vnd.pypi.simple.v1+json";
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
        .route("/pypi/gitea-files/{*path}", get(gitea_file))
}

/// The Gitea owner of a (normalized) project name, when it is private.
pub fn private_owner(st: &AppState, project: &str) -> Option<String> {
    st.cfg.gitea.as_ref()?;
    st.namespaces.owner(Eco::Pypi, project)
}

/// Gitea's PyPI registry for `owner`, without a trailing slash.
fn gitea_base(st: &AppState, owner: &str) -> String {
    let url = st.cfg.gitea.as_ref().map_or("", |g| g.url.as_str());
    format!("{url}/api/packages/{owner}/pypi")
}

/// A project's page, from Gitea for a private name, else from the index.
/// Gitea serves HTML only, so private pages are always HTML.
pub async fn project_meta(st: &AppState, name: &str, json: bool) -> AppResult<Meta> {
    match private_owner(st, name) {
        Some(owner) => {
            let url = format!("{}/simple/{name}", gitea_base(st, &owner));
            st.store
                .meta(
                    "gitea-pypi",
                    &format!("{owner}/{name}"),
                    "html",
                    &url,
                    SIMPLE_HTML,
                )
                .await
        }
        None => {
            let url = format!("{}/{name}/", st.cfg.pypi_index);
            let (variant, accept) = if json {
                ("json", SIMPLE_JSON)
            } else {
                ("html", SIMPLE_HTML)
            };
            st.store.meta("pypi", name, variant, &url, accept).await
        }
    }
}

/// Every file a project page links to: (URL without its fragment, file
/// name, sha256 when given). Reads the JSON and the HTML forms.
pub fn page_files(body: &[u8]) -> Vec<(String, String, Option<String>)> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        return v["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| {
                Some((
                    f["url"].as_str()?.split('#').next()?.to_string(),
                    f["filename"].as_str()?.to_string(),
                    f["hashes"]["sha256"].as_str().map(String::from),
                ))
            })
            .collect();
    }
    let text = String::from_utf8_lossy(body);
    text.split("href=\"")
        .skip(1)
        .filter_map(|rest| {
            let href = rest.split('"').next()?.replace("&amp;", "&");
            let (url, fragment) = href.split_once('#').unwrap_or((&href, ""));
            let file = url.rsplit('/').next()?.to_string();
            let sha = fragment
                .strip_prefix("sha256=")
                .map(|h| h.split('&').next().unwrap_or(h).to_string());
            (!file.is_empty()).then(|| (url.to_string(), file, sha))
        })
        .collect()
}

/// Point a Gitea project page's file links at Granary's `/pypi/gitea-files/`.
/// Gitea builds them from its own idea of its address (`ROOT_URL`), so
/// everything up to the registry path is replaced.
fn rewrite_gitea_links(page: &str, owner: &str, ours: &str) -> String {
    let marker = format!("/api/packages/{owner}/pypi/files/");
    let mut out = String::with_capacity(page.len());
    let mut rest = page;
    while let Some(i) = rest.find("href=\"") {
        let (head, tail) = rest.split_at(i + 6);
        out.push_str(head);
        let end = tail.find('"').unwrap_or(tail.len());
        let href = &tail[..end];
        match href.split_once(&marker) {
            Some((_, path)) => out.push_str(&format!("{ours}{path}")),
            None => out.push_str(href),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
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

pub fn valid_project(name: &str) -> bool {
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
    who: Principal,
    uri: Uri,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> AppResult<Response> {
    let name = normalize(&project);
    if !valid_project(&name) {
        return Err(AppError::not_found());
    }
    let Some(owner) = private_owner(&st, &name) else {
        let url = format!("{}/{name}/", st.cfg.pypi_index);
        return serve_page(&st, &headers, &name, url).await;
    };
    let r = Resource::gitea_package("pypi", &owner, &name);
    if let Some(answer) = refuse(&st, &who, &r, &headers, uri.path()) {
        return Ok(answer);
    }
    let meta = project_meta(&st, &name, false).await?;
    let ours = format!("{}/pypi/gitea-files/", base_url(&st.cfg, &headers));
    let body = rewrite_gitea_links(&String::from_utf8_lossy(&meta.body), &owner, &ours);
    Ok(([(header::CONTENT_TYPE, meta.content_type)], body).into_response())
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
    // A file of a private project never comes from PyPI.
    let dist = path.strip_suffix(".metadata").unwrap_or(&path);
    if let Some((project, _)) = classify("pypi", dist)
        && private_owner(&st, &project).is_some()
    {
        return Err(AppError::not_found());
    }
    fetch_file(&st, &path).await
}

async fn gitea_file(
    State(st): State<AppState>,
    who: Principal,
    uri: Uri,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> AppResult<Response> {
    let mut parts = path.split('/');
    let (Some(project), Some(_version), Some(_file), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AppError::not_found());
    };
    let name = normalize(project);
    let Some(owner) = private_owner(&st, &name) else {
        return Err(AppError::not_found());
    };
    let r = Resource::gitea_package("pypi", &owner, &name);
    if let Some(answer) = refuse(&st, &who, &r, &headers, uri.path()) {
        return Ok(answer);
    }
    fetch_gitea_file(&st, &owner, &path).await
}

/// Serve one file of a private project (`{project}/{version}/{file}`),
/// downloading it from Gitea and checking its sha256 on first use.
pub async fn fetch_gitea_file(st: &AppState, owner: &str, path: &str) -> AppResult<Response> {
    let rel = format!("{owner}/{path}");
    let url = format!("{}/files/{path}", gitea_base(st, owner));
    let expected = if st.store.has_file("gitea-pypi", &rel).await {
        None
    } else {
        let project = normalize(path.split('/').next().unwrap_or(""));
        let file = path.rsplit('/').next().unwrap_or("");
        project_meta(st, &project, false)
            .await
            .ok()
            .and_then(|m| {
                page_files(&m.body)
                    .into_iter()
                    .find(|(_, f, _)| f == file)
                    .and_then(|(_, _, sha)| sha)
            })
            .and_then(|h| Checksum::sha256_hex(&h))
    };
    st.store
        .file(
            "gitea-pypi",
            &rel,
            &url,
            "application/octet-stream",
            expected,
        )
        .await
}

/// Serve one distribution file (path under the files host), downloading and
/// verifying it on first use.
pub async fn fetch_file(st: &AppState, path: &str) -> AppResult<Response> {
    let url = format!("{}/{path}", st.cfg.pypi_files);
    let expected = if st.store.has_file("pypi", path).await {
        None
    } else {
        expected_sha256(st, path).await
    };
    st.store
        .file("pypi", path, &url, "application/octet-stream", expected)
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
