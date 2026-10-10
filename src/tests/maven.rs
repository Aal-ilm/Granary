//! The Maven proxy against a mock upstream.

use std::time::Duration;

use axum::http::{StatusCode, header};
use sha1::{Digest, Sha1};

use super::{Harness, Upstream};

const DIR: &str = "org/example/demo/1.2.0";
const JAR: &str = "org/example/demo/1.2.0/demo-1.2.0.jar";
const METADATA: &str = "org/example/demo/maven-metadata.xml";

fn sha1_hex(data: &[u8]) -> String {
    hex::encode(Sha1::digest(data))
}

fn up(rel: &str) -> String {
    format!("/maven2/{rel}")
}

fn ours(rel: &str) -> String {
    format!("/maven/{rel}")
}

async fn open() -> Harness {
    Harness::new(false, Duration::from_secs(300)).await
}

#[tokio::test]
async fn caches_verified_jar_and_serves_hits_locally() {
    let h = open().await;
    let jar = b"PK\x03\x04 pretend jar".to_vec();
    h.mock.set(&up(JAR), Upstream::ok(jar.clone()));
    // Central's .sha1 files sometimes carry the file name after the digest.
    h.mock.set(
        &up(&format!("{JAR}.sha1")),
        Upstream::ok(format!("{}  demo-1.2.0.jar\n", sha1_hex(&jar))),
    );

    let (status, headers, body) = h.get(&ours(JAR)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, jar);
    assert_eq!(headers[header::CONTENT_TYPE], "application/java-archive");

    // Second request is a cache hit: upstream isn't asked again.
    let (status, _, body) = h.get(&ours(JAR)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, jar);
    assert_eq!(h.mock.hits(&up(JAR)), 1);

    // The .sha1 fetched for verification is cached next to the jar.
    let (status, _, body) = h.get(&ours(&format!("{JAR}.sha1"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .starts_with(&sha1_hex(&jar))
    );
    assert_eq!(h.mock.hits(&up(&format!("{JAR}.sha1"))), 1);
    assert!(
        h.data
            .join("files/maven")
            .join(format!("{JAR}.sha1"))
            .exists()
    );

    // HEAD works too, from the cache.
    let (status, _, _) = h.request("HEAD", &ours(JAR), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.mock.hits(&up(JAR)), 1);
}

#[tokio::test]
async fn revalidates_metadata_and_serves_stale_on_error() {
    // A zero TTL makes every request revalidate with upstream.
    let h = Harness::new(false, Duration::ZERO).await;
    let xml = b"<metadata><versioning><latest>1.2.0</latest></versioning></metadata>".to_vec();
    h.mock.set(
        &up(METADATA),
        Upstream {
            status: StatusCode::OK,
            body: xml.clone(),
            etag: Some("\"v1\""),
        },
    );

    let (status, headers, body) = h.get(&ours(METADATA)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, xml);
    assert_eq!(headers[header::CONTENT_TYPE], "application/xml");

    // Revalidation sends the ETag; upstream's 304 keeps the cached body.
    let (status, _, body) = h.get(&ours(METADATA)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, xml);
    let reqs = h.mock.requests.lock().unwrap().clone();
    let meta_reqs: Vec<_> = reqs.iter().filter(|(p, _)| *p == up(METADATA)).collect();
    assert_eq!(meta_reqs.len(), 2);
    assert_eq!(meta_reqs[0].1, None);
    assert_eq!(meta_reqs[1].1.as_deref(), Some("\"v1\""));

    // Upstream failing: the last good copy is served (stale-if-error).
    h.mock.set(
        &up(METADATA),
        Upstream {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: b"down".to_vec(),
            etag: None,
        },
    );
    let (status, _, body) = h.get(&ours(METADATA)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, xml);
}

#[tokio::test]
async fn checksum_mismatch_is_502_and_not_cached() {
    let h = open().await;
    let pom = b"<project>tampered</project>".to_vec();
    let rel = format!("{DIR}/demo-1.2.0.pom");
    h.mock.set(&up(&rel), Upstream::ok(pom));
    h.mock.set(
        &up(&format!("{rel}.sha1")),
        Upstream::ok(sha1_hex(b"<project>original</project>")),
    );

    let (status, _, _) = h.get(&ours(&rel)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!h.data.join("files/maven").join(&rel).exists());
    assert!(
        !h.data
            .join("files/maven")
            .join(format!("{rel}.sha1"))
            .exists()
    );
    // No partial download is left behind.
    assert_eq!(std::fs::read_dir(h.data.join("tmp")).unwrap().count(), 0);

    // Not cached: the next request goes upstream again and fails again.
    let (status, _, _) = h.get(&ours(&rel)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(h.mock.hits(&up(&rel)), 2);
}

#[tokio::test]
async fn snapshots_are_404_without_asking_upstream() {
    let h = open().await;
    for rel in [
        "org/example/demo/1.3.0-SNAPSHOT/demo-1.3.0-SNAPSHOT.jar",
        "org/example/demo/1.3.0-SNAPSHOT/maven-metadata.xml",
    ] {
        let (status, _, _) = h.get(&ours(rel)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{rel}");
    }
    assert_eq!(h.mock.total(), 0);
}

#[tokio::test]
async fn upstream_404_is_remembered() {
    let h = open().await;
    let rel = format!("{DIR}/demo-1.2.0-javadoc.jar");
    for _ in 0..3 {
        let (status, _, _) = h.get(&ours(&rel)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // One probe for the .sha1, one for the jar; the rest came from the
    // negative cache.
    assert_eq!(h.mock.hits(&up(&format!("{rel}.sha1"))), 1);
    assert_eq!(h.mock.hits(&up(&rel)), 1);
}

#[tokio::test]
async fn auth_challenge_and_basic_token() {
    let h = Harness::new(true, Duration::from_secs(300)).await;
    let module = format!("{DIR}/demo-1.2.0.module");
    h.mock.set(&up(&module), Upstream::ok(b"{}".to_vec()));

    let (status, headers, _) = h.get(&ours(&module)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"granary\"");
    assert_eq!(h.mock.total(), 0);

    let wrong = h.basic("token", "gran_wrong");
    let (status, _, _) = h.request("GET", &ours(&module), Some(wrong)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The username is ignored; the password is the token.
    let right = h.basic("anything", &h.token);
    let (status, _, body) = h.request("GET", &ours(&module), Some(right)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"{}");
}

#[tokio::test]
async fn rejects_traversal_and_odd_paths() {
    let h = open().await;
    for path in [
        "/maven/org/example/../../../etc/passwd",
        "/maven/org/example/demo/1.2.0/..",
        "/maven/org//example/demo/1.2.0/demo-1.2.0.jar",
        "/maven/org/example%2fdemo/1.2.0/demo-1.2.0.jar",
        "/maven/org/example%2Fdemo/1.2.0/demo-1.2.0.jar",
        "/maven/org/example%5cdemo/1.2.0/demo-1.2.0.jar",
        "/maven/org/%2e%2e/%2e%2e/etc/passwd",
    ] {
        let (status, _, _) = h.get(path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    assert_eq!(h.mock.total(), 0);
}

#[tokio::test]
async fn only_get_and_head() {
    let h = open().await;
    for method in ["PUT", "POST", "DELETE"] {
        let (status, headers, _) = h.request(method, &ours(JAR), None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(headers[header::ALLOW], "GET, HEAD");
    }
    assert_eq!(h.mock.total(), 0);
}
