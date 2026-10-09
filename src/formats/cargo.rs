//! Cargo sparse-registry proxy.
//!
//! Client setup (`~/.cargo/config.toml`):
//!
//! ```toml
//! [source.crates-io]
//! replace-with = "granary"
//! [source.granary]
//! registry = "sparse+http://HOST:8080/cargo/"
//! ```
//!
//! Index files are cached as metadata; `.crate` archives are cached forever.
//! Cargo verifies every archive against the checksum in the index itself.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    AppState,
    error::{AppError, AppResult},
    util::base_url,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/cargo/config.json", get(config_json))
        .route("/cargo/dl/{krate}/{version}", get(download))
        .route("/cargo/{*path}", get(index_file))
}

async fn config_json(State(st): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let base = base_url(&st.cfg, &headers);
    Json(serde_json::json!({
        "dl": format!("{base}/cargo/dl/{{crate}}/{{version}}"),
    }))
}

fn valid_crate_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

async fn download(
    State(st): State<AppState>,
    Path((krate, version)): Path<(String, String)>,
) -> AppResult<Response> {
    if !valid_crate_name(&krate) || !valid_version(&version) {
        return Err(AppError::bad_request("invalid crate name or version"));
    }
    // Crate names are case-insensitive, so store under the lowercased name.
    let lower = krate.to_ascii_lowercase();
    let url = st
        .cfg
        .cargo_dl
        .replace("{crate}", &krate)
        .replace("{version}", &version);
    let rel = format!("{lower}/{lower}-{version}.crate");
    st.store
        .file("cargo", &rel, &url, "application/x-tar")
        .await
}

async fn index_file(State(st): State<AppState>, Path(path): Path<String>) -> AppResult<Response> {
    let path = path.to_ascii_lowercase();
    let url = format!("{}/{}", st.cfg.cargo_index, path);
    let meta = st.store.meta("cargo", &path, "raw", &url, "*/*").await?;
    Ok(([(header::CONTENT_TYPE, meta.content_type)], meta.body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert!(valid_crate_name("serde_json"));
        assert!(valid_crate_name("tokio-util"));
        assert!(!valid_crate_name("../x"));
        assert!(!valid_crate_name(""));
        assert!(valid_version("1.0.0-alpha.1+build.5"));
        assert!(!valid_version("1.0/../../x"));
    }
}
