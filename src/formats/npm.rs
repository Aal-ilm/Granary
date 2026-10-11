//! npm registry proxy.
//!
//! Client setup (`~/.npmrc` or a project `.npmrc`):
//!
//! ```text
//! registry=http://HOST:8080/npm/
//! ```
//!
//! Packuments (package metadata) are cached with a TTL and their tarball URLs
//! are rewritten to point back at this server. Tarballs are cached forever;
//! npm checks each one against the `integrity` hash in the packument.
//! A few registry API endpoints under `/-/` (audit, search, ping) are passed
//! through uncached. Everything else there (login, tokens, publishing,
//! ownership…) is refused, so Granary can't be used to relay credentials or
//! account changes to the public registry.

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};

use crate::{
    AppState,
    error::{AppError, AppResult},
    store::Checksum,
    util::base_url,
};

pub const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";

pub fn router() -> Router<AppState> {
    Router::new().route("/npm/{*rest}", any(handle))
}

async fn handle(
    State(st): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Path(rest): Path<String>,
    body: Bytes,
) -> AppResult<Response> {
    if rest.starts_with("-/") {
        if !passthrough_allowed(&method, &rest) {
            return Err(AppError::new(
                StatusCode::NOT_FOUND,
                "Granary only forwards npm audit, search and ping requests",
            ));
        }
        if st.store.offline() {
            return Err(AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Granary is in offline mode (GRANARY_OFFLINE), so npm audit, search and ping are unavailable",
            ));
        }
        return passthrough(&st, method, &headers, &rest, uri.query(), body).await;
    }
    if method != Method::GET && method != Method::HEAD {
        return Err(AppError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "this is a read-only proxy; publishing is not supported yet",
        ));
    }
    if let Some((pkg, file)) = rest.split_once("/-/") {
        tarball(&st, pkg, file).await
    } else {
        packument(&st, &headers, &rest).await
    }
}

pub fn valid_package_name(name: &str) -> bool {
    let ok_chars = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    };
    if name.len() > 214 {
        return false;
    }
    match name.strip_prefix('@') {
        Some(scoped) => {
            matches!(scoped.split_once('/'), Some((scope, pkg)) if ok_chars(scope) && ok_chars(pkg))
        }
        None => ok_chars(name) && !name.starts_with('.'),
    }
}

async fn packument(st: &AppState, headers: &HeaderMap, name: &str) -> AppResult<Response> {
    if !valid_package_name(name) {
        return Err(AppError::not_found());
    }
    let wants_abbreviated = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains(ABBREVIATED));
    let (variant, accept) = if wants_abbreviated {
        ("abbr", ABBREVIATED)
    } else {
        ("full", "application/json")
    };

    // Scoped names are requested upstream as `@scope%2fname`.
    let url = format!("{}/{}", st.cfg.npm_upstream, name.replace('/', "%2f"));
    let meta = st.store.meta("npm", name, variant, &url, accept).await?;

    // Left alone, the tarball URLs still reach Granary: npm maps
    // registry.npmjs.org onto the configured registry.
    if !st.cfg.npm_rewrite_tarballs {
        return Ok(([(header::CONTENT_TYPE, meta.content_type)], meta.body).into_response());
    }
    let body = rewrite_tarball_urls(
        &String::from_utf8_lossy(&meta.body),
        &st.cfg.npm_upstream,
        &format!("{}/npm", base_url(&st.cfg, headers)),
    );
    Ok(([(header::CONTENT_TYPE, meta.content_type)], body).into_response())
}

/// Point every `"tarball"` URL that references the upstream registry at us.
/// Only the value of `tarball` keys is touched, so README text is untouched.
pub fn rewrite_tarball_urls(doc: &str, upstream: &str, ours: &str) -> String {
    let mut out = doc.to_string();
    for sep in [":", ": "] {
        let from = format!("\"tarball\"{sep}\"{upstream}/");
        let to = format!("\"tarball\"{sep}\"{ours}/");
        out = out.replace(&from, &to);
    }
    out
}

/// Serve one tarball (`<pkg>/-/<file>`), downloading and verifying it on first use.
pub async fn tarball(st: &AppState, pkg: &str, file: &str) -> AppResult<Response> {
    let valid_file = file.ends_with(".tgz") && !file.contains('/') && !file.starts_with('.');
    if !valid_package_name(pkg) || !valid_file {
        return Err(AppError::not_found());
    }
    let url = format!("{}/{pkg}/-/{file}", st.cfg.npm_upstream);
    let rel = format!("{pkg}/-/{file}");
    let expected = if st.store.has_file("npm", &rel).await {
        None
    } else {
        expected_integrity(st, pkg, file).await
    };
    st.store
        .file("npm", &rel, &url, "application/octet-stream", expected)
        .await
}

/// The `dist.integrity` the packument lists for this tarball.
async fn expected_integrity(st: &AppState, pkg: &str, file: &str) -> Option<Checksum> {
    let url = format!("{}/{}", st.cfg.npm_upstream, pkg.replace('/', "%2f"));
    let meta = st
        .store
        .meta("npm", pkg, "abbr", &url, ABBREVIATED)
        .await
        .ok()?;
    let suffix = format!("/{pkg}/-/{file}");
    tokio::task::spawn_blocking(move || integrity_from_packument(&meta.body, &suffix))
        .await
        .ok()
        .flatten()
}

fn integrity_from_packument(doc: &[u8], tarball_suffix: &str) -> Option<Checksum> {
    let v: serde_json::Value = serde_json::from_slice(doc).ok()?;
    v.get("versions")?.as_object()?.values().find_map(|ver| {
        let dist = ver.get("dist")?;
        let tarball = dist.get("tarball")?.as_str()?;
        if !tarball.ends_with(tarball_suffix) {
            return None;
        }
        Checksum::from_sri(dist.get("integrity")?.as_str()?)
    })
}

/// The registry API calls npm makes during installs that Granary forwards.
fn passthrough_allowed(method: &Method, rest: &str) -> bool {
    match *method {
        Method::POST => matches!(
            rest,
            "-/npm/v1/security/advisories/bulk"
                | "-/npm/v1/security/audits/quick"
                | "-/npm/v1/security/audits"
        ),
        Method::GET | Method::HEAD => matches!(rest, "-/v1/search" | "-/ping"),
        _ => false,
    }
}

const MAX_PASSTHROUGH_BYTES: usize = 32 * 1024 * 1024;

/// Forward an allowed registry API call without caching. The client's
/// credentials are never forwarded.
async fn passthrough(
    st: &AppState,
    method: Method,
    headers: &HeaderMap,
    rest: &str,
    query: Option<&str>,
    body: Bytes,
) -> AppResult<Response> {
    let mut url = format!("{}/{rest}", st.cfg.npm_upstream);
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }
    st.store.check_url(&url)?;
    let mut req = st
        .store
        .passthrough_client()
        .request(method, &url)
        .body(body);
    for name in [
        header::CONTENT_TYPE,
        header::ACCEPT,
        header::CONTENT_ENCODING,
    ] {
        if let Some(v) = headers.get(&name) {
            req = req.header(name, v);
        }
    }
    let resp = req.send().await?;
    let status = resp.status();
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_PASSTHROUGH_BYTES as u64)
    {
        return Err(AppError::bad_gateway("upstream response too large"));
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_PASSTHROUGH_BYTES {
        return Err(AppError::bad_gateway("upstream response too large"));
    }
    let mut out = Response::builder().status(status);
    if let Some(ct) = content_type {
        out = out.header(header::CONTENT_TYPE, ct);
    }
    Ok(out.body(Body::from(bytes)).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_tarball_fields() {
        let doc = r#"{"readme":"see https://registry.npmjs.org/x","versions":{"1.0.0":{"dist":{"tarball":"https://registry.npmjs.org/x/-/x-1.0.0.tgz"}}}}"#;
        let out = rewrite_tarball_urls(doc, "https://registry.npmjs.org", "http://lan:8080/npm");
        assert!(out.contains(r#""tarball":"http://lan:8080/npm/x/-/x-1.0.0.tgz""#));
        assert!(out.contains("see https://registry.npmjs.org/x"));
    }

    #[test]
    fn package_names() {
        assert!(valid_package_name("react"));
        assert!(valid_package_name("@types/node"));
        assert!(!valid_package_name("@types/../x"));
        assert!(!valid_package_name("../etc"));
        assert!(!valid_package_name(".hidden"));
    }

    #[test]
    fn passthrough_allowlist() {
        assert!(passthrough_allowed(
            &Method::POST,
            "-/npm/v1/security/advisories/bulk"
        ));
        assert!(passthrough_allowed(&Method::GET, "-/v1/search"));
        assert!(passthrough_allowed(&Method::GET, "-/ping"));
        assert!(!passthrough_allowed(
            &Method::PUT,
            "-/user/org.couchdb.user:me"
        ));
        assert!(!passthrough_allowed(&Method::POST, "-/npm/v1/tokens"));
        assert!(!passthrough_allowed(&Method::GET, "-/whoami"));
        assert!(!passthrough_allowed(&Method::DELETE, "-/v1/search"));
    }
}
