//! The private Cargo registry: Gitea's Cargo registry for one owner, served
//! at `/cargo-private/` as a sparse index of its own.
//!
//! Client setup (`~/.cargo/config.toml`, which `make use-granary` writes):
//!
//! ```toml
//! [registries.granary-private]
//! index = "sparse+http://HOST:8080/cargo-private/"
//! credential-provider = "cargo:token"
//! ```
//!
//! Cargo has no per-name routing, so this is a separate named registry
//! rather than names mixed into crates.io. Everything in it is private: the
//! access policy decides who may read the index and the crates, and Cargo
//! always sends its token (`auth-required`).
//!
//! Granary reads Gitea's sparse index when the connected Gitea serves one,
//! and otherwise Gitea's git index (`{owner}/_cargo-index`) through the git
//! cache. Crates are checked against the index's `cksum` and cached under
//! `gitea-cargo/{owner}/`.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};

use super::{
    cargo::{cksum_from_index, valid_crate_name, valid_version},
    git::{self, Refresh, RepoId},
};
use crate::{
    AppState,
    error::{AppError, AppResult},
    inventory::meta_key,
    policy::{Principal, Resource, refuse},
    util::base_url,
};

/// Largest index file read from the git index.
const MAX_INDEX: u64 = 16 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/cargo-private/config.json", get(config_json))
        .route("/cargo-private/dl/{krate}/{version}", get(download))
        .route("/cargo-private/{*path}", get(index_file))
}

/// The registry's owner, or 404 when there is no private Cargo registry.
fn owner(st: &AppState) -> AppResult<String> {
    st.cfg
        .gitea
        .as_ref()
        .and_then(|_| st.namespaces.cargo_owner())
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                "there is no private Cargo registry (set one up under System, Gitea)",
            )
        })
}

fn gitea_url(st: &AppState) -> &str {
    st.cfg.gitea.as_ref().map_or("", |g| g.url.as_str())
}

async fn config_json(
    State(st): State<AppState>,
    who: Principal,
    uri: Uri,
    headers: HeaderMap,
) -> AppResult<Response> {
    let owner = owner(&st)?;
    let r = Resource::gitea_package("cargo", &owner, "");
    if let Some(answer) = refuse(&st, &who, &r, &headers, uri.path()) {
        return Ok(answer);
    }
    let base = base_url(&st.cfg, &headers);
    Ok(Json(serde_json::json!({
        "dl": format!("{base}/cargo-private/dl/{{crate}}/{{version}}"),
        "auth-required": true,
    }))
    .into_response())
}

async fn index_file(
    State(st): State<AppState>,
    who: Principal,
    uri: Uri,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> AppResult<Response> {
    let owner = owner(&st)?;
    let path = path.to_ascii_lowercase();
    let krate = path.rsplit('/').next().unwrap_or("");
    // Only real index paths: the crate's place in the index layout.
    if !valid_crate_name(krate) || meta_key("cargo", krate).as_deref() != Some(path.as_str()) {
        return Err(AppError::not_found());
    }
    let r = Resource::gitea_package("cargo", &owner, krate);
    if let Some(answer) = refuse(&st, &who, &r, &headers, uri.path()) {
        return Ok(answer);
    }
    let body = index_entry(&st, &owner, &path).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain")], body).into_response())
}

/// One index file, from Gitea's sparse index if it has one, else from its
/// git index.
pub async fn index_entry(st: &AppState, owner: &str, path: &str) -> AppResult<Bytes> {
    let sparse = if st.store.offline() {
        // Offline: whichever copy is cached.
        st.store
            .has_meta("gitea-cargo", &format!("{owner}/index/{path}"), "raw")
            .await
    } else {
        st.gitea
            .cargo_sparse(&st.cfg, owner)
            .await
            .map_err(AppError::bad_gateway)?
    };
    if sparse {
        let url = format!("{}/api/packages/{owner}/cargo/{path}", gitea_url(st));
        let meta = st
            .store
            .meta(
                "gitea-cargo",
                &format!("{owner}/index/{path}"),
                "raw",
                &url,
                "*/*",
            )
            .await?;
        return Ok(meta.body);
    }
    let host = st.cfg.gitea.as_ref().map(|g| g.host.as_str()).unwrap_or("");
    let id = RepoId::new(&st.cfg, host, owner, "_cargo-index").map_err(AppError::bad_gateway)?;
    git::ensure(st, &id, Refresh::IfStale, None).await?;
    let commit = git::resolve_ref(st, &id, None).await?;
    git::read_file(st, &id, &commit, path, MAX_INDEX)
        .await?
        .map(Bytes::from)
        .ok_or_else(AppError::not_found)
}

async fn download(
    State(st): State<AppState>,
    who: Principal,
    uri: Uri,
    headers: HeaderMap,
    Path((krate, version)): Path<(String, String)>,
) -> AppResult<Response> {
    let owner = owner(&st)?;
    if !valid_crate_name(&krate) || !valid_version(&version) {
        return Err(AppError::bad_request("invalid crate name or version"));
    }
    let lower = krate.to_ascii_lowercase();
    let r = Resource::gitea_package("cargo", &owner, &lower);
    if let Some(answer) = refuse(&st, &who, &r, &headers, uri.path()) {
        return Ok(answer);
    }
    fetch_crate(&st, &owner, &lower, &version).await
}

/// Serve one private `.crate`, downloading it from Gitea and checking it
/// against the index on first use.
pub async fn fetch_crate(
    st: &AppState,
    owner: &str,
    krate: &str,
    version: &str,
) -> AppResult<Response> {
    let rel = format!("{owner}/{krate}/{krate}-{version}.crate");
    let url = format!(
        "{}/api/packages/{owner}/cargo/api/v1/crates/{krate}/{version}/download",
        gitea_url(st)
    );
    let expected = if st.store.has_file("gitea-cargo", &rel).await {
        None
    } else {
        let path = meta_key("cargo", krate).ok_or_else(AppError::not_found)?;
        let index = index_entry(st, owner, &path).await?;
        Some(cksum_from_index(&index, version).ok_or_else(AppError::not_found)?)
    };
    st.store
        .file("gitea-cargo", &rel, &url, "application/x-tar", expected)
        .await
}
