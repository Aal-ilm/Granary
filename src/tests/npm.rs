//! npm packuments, with and without tarball URL rewriting.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::Digest;

use super::{Harness, Opts, Upstream};

/// The tarball URL the packument hands out. Also checks that the tarball is
/// served from the path npm requests for it.
async fn packument_tarball(rewrite: bool) -> (Harness, String) {
    let h = Harness::with(Opts {
        npm_rewrite_tarballs: rewrite,
        ..Opts::default()
    })
    .await;
    let tgz = b"pretend tarball".to_vec();
    let integrity = {
        use base64::{Engine, engine::general_purpose::STANDARD};
        format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(&tgz)))
    };
    let base = h.upstream_base().to_string();
    h.mock.set(
        "/npm/left-pad",
        Upstream::ok(
            json!({"name": "left-pad", "dist-tags": {"latest": "1.3.0"}, "versions": {"1.3.0": {"dist": {
                "tarball": format!("{base}/npm/left-pad/-/left-pad-1.3.0.tgz"),
                "integrity": integrity,
            }}}})
            .to_string(),
        ),
    );
    h.mock.set(
        "/npm/left-pad/-/left-pad-1.3.0.tgz",
        Upstream::ok(tgz.clone()),
    );

    let (status, _, body) = h.get("/npm/left-pad").await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_slice(&body).unwrap();
    let url = doc["versions"]["1.3.0"]["dist"]["tarball"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(url.ends_with("/npm/left-pad/-/left-pad-1.3.0.tgz"), "{url}");

    // Either way, the path npm requests from the registry serves the file.
    let (status, _, body) = h.get("/npm/left-pad/-/left-pad-1.3.0.tgz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, tgz);
    (h, url)
}

#[tokio::test]
async fn tarball_urls_point_at_granary_by_default() {
    let (h, url) = packument_tarball(true).await;
    assert!(!url.starts_with(h.upstream_base()), "{url}");
}

#[tokio::test]
async fn tarball_urls_can_stay_as_the_registry_lists_them() {
    let (h, url) = packument_tarball(false).await;
    assert_eq!(
        url,
        format!("{}/npm/left-pad/-/left-pad-1.3.0.tgz", h.upstream_base())
    );
}
