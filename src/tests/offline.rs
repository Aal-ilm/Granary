//! GRANARY_OFFLINE: cached data is served, nothing else is fetched, and no
//! upstream is ever contacted.

use axum::http::StatusCode;

use super::{Harness, Opts, Upstream};

async fn offline() -> Harness {
    Harness::with(Opts {
        offline: true,
        ..Opts::default()
    })
    .await
}

#[tokio::test]
async fn uncached_requests_never_reach_upstream() {
    let h = offline().await;
    // Upstream has everything; offline mode must still not ask for any of it.
    let paths = [
        ("/cargo/3/s/syn", "/cargo/3/s/syn"),
        ("/cargo/dl/syn/2.0.0", "/crates/syn/2.0.0/download"),
        ("/npm/left-pad", "/npm/left-pad"),
        (
            "/npm/left-pad/-/left-pad-1.3.0.tgz",
            "/npm/left-pad/-/left-pad-1.3.0.tgz",
        ),
        ("/pypi/simple/requests/", "/simple/requests/"),
        (
            "/pypi/files/packages/aa/bb/requests-2.0.tar.gz",
            "/files/packages/aa/bb/requests-2.0.tar.gz",
        ),
        (
            "/maven/org/example/demo/1.0/demo-1.0.jar",
            "/maven2/org/example/demo/1.0/demo-1.0.jar",
        ),
        (
            "/maven/org/example/demo/maven-metadata.xml",
            "/maven2/org/example/demo/maven-metadata.xml",
        ),
    ];
    for (_, upstream) in paths {
        h.mock.set(upstream, Upstream::ok(b"x".to_vec()));
    }
    for (ours, _) in paths {
        let (status, _, body) = h.get(ours).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{ours}");
        assert!(
            String::from_utf8_lossy(&body).contains("offline mode"),
            "{ours} should say why"
        );
    }
    // The message names the file that was asked for, not a helper lookup.
    let (_, _, body) = h.get("/maven/org/example/demo/1.0/demo-1.0.jar").await;
    let msg = String::from_utf8_lossy(&body);
    assert!(
        msg.starts_with("org/example/demo/1.0/demo-1.0.jar is not cached"),
        "{msg}"
    );

    // npm's uncached pass-through calls are refused too.
    let (status, _, _) = h.get("/npm/-/ping").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    assert_eq!(h.mock.total(), 0);
}

#[tokio::test]
async fn cached_files_and_expired_metadata_are_served() {
    let h = offline().await;
    let jar = "org/example/demo/1.0/demo-1.0.jar";
    let file = h.data.join("files/maven").join(jar);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"cached jar").unwrap();

    // Metadata fetched long ago: online it would be revalidated, offline it
    // is served as it is.
    let meta = h.data.join("meta/maven/org/example/demo");
    std::fs::create_dir_all(&meta).unwrap();
    std::fs::write(meta.join("maven-metadata.xml.raw.body"), b"<metadata/>").unwrap();
    std::fs::write(
        meta.join("maven-metadata.xml.raw.json"),
        br#"{"fetched_at":0,"content_type":"application/xml","etag":null,"last_modified":null}"#,
    )
    .unwrap();

    let (status, _, body) = h.get(&format!("/maven/{jar}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"cached jar");
    let (status, _, body) = h.get("/maven/org/example/demo/maven-metadata.xml").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"<metadata/>");
    assert_eq!(h.mock.total(), 0);
}
