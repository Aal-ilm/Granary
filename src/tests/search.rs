//! "Get packages": search, exact lookup and single-file caching.

use axum::http::StatusCode;
use serde_json::json;
use sha1::Digest;

use super::{Harness, Opts, Upstream};

fn npm_search_body() -> Vec<u8> {
    json!({"objects": [
        {"package": {"name": "left-pad", "version": "1.3.0", "description": "String left pad"}},
        {"package": {"name": "pad-left", "version": "2.1.0"}}
    ]})
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn searches_each_ecosystem_and_reuses_results() {
    let h = Harness::new(false, std::time::Duration::from_secs(300)).await;
    let cookie = h.login().await;
    h.mock
        .set("/npm/-/v1/search", Upstream::ok(npm_search_body()));
    h.mock.set(
        "/crates-api/crates",
        Upstream::ok(
            json!({"crates": [{"name": "serde", "max_stable_version": "1.0.229", "description": " A serialization framework\n"}]})
                .to_string(),
        ),
    );
    h.mock.set(
        "/solr/select",
        Upstream::ok(
            json!({"response": {"docs": [{"g": "com.google.guava", "a": "guava", "latestVersion": "33.4.0-jre"}]}})
                .to_string(),
        ),
    );

    let d = h.api(&cookie, "/api/packages/search?eco=npm&q=pad").await;
    assert_eq!(d["mode"], "search");
    assert_eq!(d["results"][0]["name"], "left-pad");
    assert_eq!(d["results"][0]["latest"], "1.3.0");
    assert_eq!(d["results"][0]["description"], "String left pad");
    assert_eq!(d["results"][0]["cached_versions"], 0);

    // The same search again within 10 minutes is answered from memory, and
    // isn't held back by the once-a-second limit.
    let again = h.api(&cookie, "/api/packages/search?eco=npm&q=PAD").await;
    assert_eq!(again["results"], d["results"]);
    assert_eq!(h.mock.hits("/npm/-/v1/search"), 1);

    // A different search straight away reaches upstream, so it's limited.
    let (status, _, _) = h
        .send(
            "GET",
            "/api/packages/search?eco=cargo&q=serde",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let d = h
        .api(&cookie, "/api/packages/search?eco=cargo&q=serde")
        .await;
    assert_eq!(d["results"][0]["name"], "serde");
    assert_eq!(d["results"][0]["description"], "A serialization framework");

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let d = h
        .api(&cookie, "/api/packages/search?eco=maven&q=guava")
        .await;
    assert_eq!(d["results"][0]["name"], "com.google.guava:guava");
    assert_eq!(d["results"][0]["latest"], "33.4.0-jre");
}

#[tokio::test]
async fn pypi_is_exact_lookup_only() {
    let h = Harness::new(false, std::time::Duration::from_secs(300)).await;
    let cookie = h.login().await;
    h.mock.set(
        "/simple/requests/",
        Upstream::ok(json!({"versions": ["2.31.0", "2.32.3", "3.0.0b1"], "files": []}).to_string()),
    );
    let d = h
        .api(&cookie, "/api/packages/search?eco=pypi&q=Requests")
        .await;
    assert_eq!(d["mode"], "lookup");
    assert!(d["note"].as_str().unwrap().contains("no search API"));
    assert_eq!(d["results"][0]["name"], "requests");
    assert_eq!(d["results"][0]["latest"], "2.32.3");
    let d = h.api(&cookie, "/api/packages/search?eco=pypi&q=nope").await;
    assert_eq!(d["results"].as_array().unwrap().len(), 0);
}

async fn assert_no_search_hosts(h: &Harness) {
    for host in ["/npm/-/v1/search", "/crates-api/crates", "/solr/select"] {
        assert_eq!(h.mock.hits(host), 0, "{host} must not be contacted");
    }
}

#[tokio::test]
async fn search_off_uses_exact_lookup_only() {
    let h = Harness::with(Opts {
        search: false,
        ..Opts::default()
    })
    .await;
    let cookie = h.login().await;
    h.mock
        .set("/npm/-/v1/search", Upstream::ok(npm_search_body()));
    h.mock.set(
        "/npm/left-pad",
        Upstream::ok(json!({"name": "left-pad", "dist-tags": {"latest": "1.3.0"}, "versions": {"1.3.0": {}}}).to_string()),
    );
    for eco in ["npm", "cargo", "maven"] {
        let d = h
            .api(
                &cookie,
                &format!("/api/packages/search?eco={eco}&q=left-pad"),
            )
            .await;
        assert_eq!(d["mode"], "lookup");
        assert!(d["note"].as_str().unwrap().contains("GRANARY_SEARCH"));
    }
    let d = h
        .api(&cookie, "/api/packages/search?eco=npm&q=left-pad")
        .await;
    assert_eq!(d["results"][0]["latest"], "1.3.0");
    assert_no_search_hosts(&h).await;
}

#[tokio::test]
async fn offline_never_searches_and_only_finds_cached() {
    let h = Harness::with(Opts {
        offline: true,
        ..Opts::default()
    })
    .await;
    let cookie = h.login().await;
    h.mock
        .set("/npm/-/v1/search", Upstream::ok(npm_search_body()));
    let d = h
        .api(&cookie, "/api/packages/search?eco=npm&q=left-pad")
        .await;
    assert_eq!(d["mode"], "lookup");
    assert!(d["note"].as_str().unwrap().contains("Offline"));
    assert_eq!(d["results"].as_array().unwrap().len(), 0);

    // A packument already in the cache can still be looked up.
    let meta = h.data.join("meta/npm");
    std::fs::create_dir_all(&meta).unwrap();
    std::fs::write(
        meta.join("left-pad.abbr.body"),
        json!({"dist-tags": {"latest": "1.3.0"}, "versions": {"1.3.0": {}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        meta.join("left-pad.abbr.json"),
        r#"{"fetched_at":0,"content_type":"application/json","etag":null,"last_modified":null}"#,
    )
    .unwrap();
    let d = h
        .api(&cookie, "/api/packages/search?eco=npm&q=left-pad")
        .await;
    assert_eq!(d["results"][0]["latest"], "1.3.0");
    assert_eq!(h.mock.total(), 0);
}

#[tokio::test]
async fn caches_single_files() {
    let h = Harness::new(false, std::time::Duration::from_secs(300)).await;
    let cookie = h.login().await;

    // npm: the tarball named in the packument, checked against its integrity.
    let tgz = b"pretend tarball".to_vec();
    let integrity = {
        use base64::{Engine, engine::general_purpose::STANDARD};
        format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(&tgz)))
    };
    let base = h.upstream_base();
    h.mock.set(
        "/npm/left-pad",
        Upstream::ok(
            json!({"dist-tags": {"latest": "1.3.0"}, "versions": {"1.3.0": {"dist": {
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
    let (status, _, body) = h
        .send(
            "POST",
            "/api/packages/cache",
            Some(&cookie),
            Some(json!({"eco": "npm", "name": "left-pad", "version": "1.3.0"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(
        h.data
            .join("files/npm/left-pad/-/left-pad-1.3.0.tgz")
            .exists()
    );

    // Maven: the POM, then the jar its packaging names, both verified.
    let rel = "org/example/demo/1.0/demo-1.0";
    let pom = b"<project><packaging>jar</packaging></project>".to_vec();
    let jar = b"PK jar".to_vec();
    for (ext, data) in [("pom", &pom), ("jar", &jar)] {
        h.mock
            .set(&format!("/maven2/{rel}.{ext}"), Upstream::ok(data.clone()));
        h.mock.set(
            &format!("/maven2/{rel}.{ext}.sha1"),
            Upstream::ok(hex::encode(sha1::Sha1::digest(data))),
        );
    }
    let (status, _, body) = h
        .send(
            "POST",
            "/api/packages/cache",
            Some(&cookie),
            Some(json!({"eco": "maven", "name": "org.example:demo", "version": "1.0"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let d: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(d["files"].as_array().unwrap().len(), 2);
    assert_eq!(d["bytes"], (pom.len() + jar.len()) as u64);
    assert!(h.data.join(format!("files/maven/{rel}.jar")).exists());

    // Search results now say left-pad has a cached version.
    h.mock
        .set("/npm/-/v1/search", Upstream::ok(npm_search_body()));
    let d = h.api(&cookie, "/api/packages/search?eco=npm&q=pad").await;
    assert_eq!(d["results"][0]["name"], "left-pad");
    assert_eq!(d["results"][0]["cached_versions"], 1);
    assert_eq!(d["results"][1]["cached_versions"], 0);

    // An unknown version is a clear 404, and nothing is cached for it.
    let (status, _, _) = h
        .send(
            "POST",
            "/api/packages/cache",
            Some(&cookie),
            Some(json!({"eco": "npm", "name": "left-pad", "version": "9.9.9"})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_needs_an_admin() {
    let h = Harness::new(false, std::time::Duration::from_secs(300)).await;
    let (status, _, _) = h
        .send("GET", "/api/packages/search?eco=npm&q=x", None, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let viewer = h.viewer_login().await;
    let (status, _, _) = h
        .send(
            "GET",
            "/api/packages/search?eco=npm&q=x",
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(h.mock.total(), 0);
}
