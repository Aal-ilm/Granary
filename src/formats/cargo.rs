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
    inventory::meta_key,
    store::Checksum,
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
    // With client authentication on, Cargo sends the registry token on
    // every request (RFC 3139 `auth-required`).
    Json(serde_json::json!({
        "dl": format!("{base}/cargo/dl/{{crate}}/{{version}}"),
        "auth-required": st.cfg.client_auth,
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
    let expected = if st.store.has_file("cargo", &rel).await {
        None
    } else {
        expected_cksum(&st, &lower, &version).await
    };
    st.store
        .file("cargo", &rel, &url, "application/x-tar", expected)
        .await
}

/// The `cksum` the index publishes for this crate version.
async fn expected_cksum(st: &AppState, name: &str, version: &str) -> Option<Checksum> {
    let key = meta_key("cargo", name)?;
    let url = format!("{}/{key}", st.cfg.cargo_index);
    let meta = st
        .store
        .meta("cargo", &key, "raw", &url, "*/*")
        .await
        .ok()?;
    cksum_from_index(&meta.body, version)
}

fn cksum_from_index(index: &[u8], version: &str) -> Option<Checksum> {
    #[derive(serde::Deserialize)]
    struct Line {
        vers: String,
        cksum: String,
    }
    index
        .split(|b| *b == b'\n')
        .filter_map(|l| serde_json::from_slice::<Line>(l).ok())
        .find(|l| l.vers == version)
        .and_then(|l| Checksum::sha256_hex(&l.cksum))
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

    #[test]
    fn finds_cksum_in_index() {
        let index = br#"{"name":"x","vers":"1.0.0","deps":[],"cksum":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","features":{},"yanked":false}
{"name":"x","vers":"1.0.1","deps":[],"cksum":"00","features":{},"yanked":false}"#;
        assert!(cksum_from_index(index, "1.0.0").is_some());
        assert!(cksum_from_index(index, "1.0.1").is_none());
        assert!(cksum_from_index(index, "2.0.0").is_none());
    }
}
