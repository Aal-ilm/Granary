//! Gitea's package registries as a private upstream, against the mock Gitea:
//! routing per format, the dependency confusion rule in both directions,
//! the private Cargo registry, the access policy on cache hits and offline,
//! search and Browse, and the token's reach.

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::Digest;

use super::{
    Harness, Opts, Upstream,
    git::have_git,
    gitea::{MockGitea, TOKEN},
};
use crate::{namespaces::Preset, templates::lines::Eco};

/// Namespaces as the environment would set them.
fn preset(list: &[(Eco, &str)]) -> Preset {
    list.iter()
        .map(|(eco, raw)| {
            (
                eco.as_str().to_string(),
                crate::namespaces::parse_rules(*eco, raw).unwrap(),
            )
        })
        .collect()
}

async fn granary(g: &MockGitea, list: &[(Eco, &str)], opts: Opts) -> Harness {
    Harness::with(Opts {
        gitea: Some(g.gitea()),
        extra_ca: Some(g.ca.clone()),
        namespaces: preset(list),
        ..opts
    })
    .await
}

/// A token with private access, made by the admin.
async fn private_token(h: &Harness) -> String {
    let cookie = h.login().await;
    let (status, _, body) = h
        .send(
            "POST",
            "/api/tokens",
            Some(&cookie),
            Some(json!({"name": "private", "private_access": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A viewer's token, without private access.
async fn viewer_token(h: &Harness) -> String {
    let cookie = h.viewer_login().await;
    let (_, _, body) = h
        .send(
            "POST",
            "/api/tokens",
            Some(&cookie),
            Some(json!({"name": "viewer"})),
        )
        .await;
    serde_json::from_slice::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

fn bearer(token: &str) -> Option<String> {
    Some(format!("Bearer {token}"))
}

fn sri(data: &[u8]) -> String {
    format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(data)))
}

fn sha256(data: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(data))
}

fn sha1(data: &[u8]) -> String {
    use sha1::Digest as _;
    hex::encode(sha1::Sha1::digest(data))
}

/// Gitea builds its URLs from its own ROOT_URL, which needn't be the address
/// Granary uses.
const ROOT_URL: &str = "https://gitea.example.invalid";

/// `@dyn/ui` 1.0.0 in Gitea's npm registry for `acme`.
fn publish_npm(g: &MockGitea, tgz: &[u8]) {
    let doc = json!({
        "name": "@dyn/ui",
        "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": {"name": "@dyn/ui", "version": "1.0.0", "dist": {
            "tarball": format!("{ROOT_URL}/api/packages/acme/npm/@dyn/ui/-/ui-1.0.0.tgz"),
            "integrity": sri(tgz),
        }}},
    });
    let mut p = g.packages.lock().unwrap();
    p.insert(
        "/api/packages/acme/npm/@dyn/ui".into(),
        doc.to_string().into_bytes(),
    );
    p.insert(
        "/api/packages/acme/npm/@dyn/ui/-/ui-1.0.0.tgz".into(),
        tgz.to_vec(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn npm_scopes_come_only_from_gitea() {
    let g = MockGitea::start().await;
    let tgz = b"private tarball".to_vec();
    publish_npm(&g, &tgz);
    let h = granary(&g, &[(Eco::Npm, "@dyn=acme")], Opts::default()).await;
    // A look-alike on the public registry, which must never be used.
    h.mock.set(
        "/npm/@dyn%2fui",
        Upstream::ok(json!({"versions": {"6.6.6": {}}}).to_string()),
    );
    h.mock.set(
        "/npm/@dyn%2fmissing",
        Upstream::ok(json!({"versions": {"6.6.6": {}}}).to_string()),
    );
    h.mock.set(
        "/npm/left-pad",
        Upstream::ok(json!({"versions": {"1.3.0": {}}}).to_string()),
    );
    let token = private_token(&h).await;

    // The packument comes from Gitea, its tarball URL points at Granary
    // whatever Gitea thinks its address is, and the tarball is verified.
    let (status, _, body) = h.request("GET", "/npm/@dyn%2fui", bearer(&token)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let doc: Value = serde_json::from_slice(&body).unwrap();
    let tarball = doc["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(
        tarball.ends_with("/npm/@dyn/ui/-/ui-1.0.0.tgz"),
        "{tarball}"
    );
    assert!(!tarball.contains("gitea.example.invalid"), "{tarball}");
    let (status, _, body) = h
        .request("GET", "/npm/@dyn/ui/-/ui-1.0.0.tgz", bearer(&token))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, tgz);
    assert!(
        h.data
            .join("files/gitea-npm/acme/@dyn/ui/-/ui-1.0.0.tgz")
            .exists()
    );

    // Not on Gitea: a 404, never the public registry's copy.
    let (status, _, _) = h
        .request("GET", "/npm/@dyn%2fmissing", bearer(&token))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.mock.hits("/npm/@dyn%2fui"), 0);
    assert_eq!(h.mock.hits("/npm/@dyn%2fmissing"), 0);

    // A public name never goes to Gitea.
    let (status, _, _) = h.request("GET", "/npm/left-pad", bearer(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        g.package_hits
            .lock()
            .unwrap()
            .iter()
            .all(|p| p.starts_with("/api/packages/acme/npm/@dyn/")),
        "{:?}",
        g.package_hits.lock().unwrap()
    );
    assert!(!h.mock.saw_auth.load(Ordering::SeqCst));

    // Cached now, and still behind the access policy.
    let viewer = viewer_token(&h).await;
    for path in ["/npm/@dyn%2fui", "/npm/@dyn/ui/-/ui-1.0.0.tgz"] {
        let (status, _, _) = h.request("GET", path, bearer(&viewer)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        let (status, _, _) = h.get(path).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }

    // With tarball rewriting off, Gitea's own URLs are kept.
    let plain = granary(
        &g,
        &[(Eco::Npm, "@dyn=acme")],
        Opts {
            npm_rewrite_tarballs: false,
            ..Opts::default()
        },
    )
    .await;
    let token = private_token(&plain).await;
    let (_, _, body) = plain.request("GET", "/npm/@dyn%2fui", bearer(&token)).await;
    let doc: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        doc["versions"]["1.0.0"]["dist"]["tarball"]
            .as_str()
            .unwrap()
            .starts_with(ROOT_URL)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pypi_names_are_normalized_and_come_only_from_gitea() {
    let g = MockGitea::start().await;
    let wheel = b"private wheel".to_vec();
    let file = "dynatheia_foo-1.0-py3-none-any.whl";
    g.packages.lock().unwrap().insert(
        "/api/packages/acme/pypi/simple/dynatheia-foo".into(),
        format!(
            "<!DOCTYPE html><html><body><a href=\"{ROOT_URL}/api/packages/acme/pypi/files/dynatheia-foo/1.0/{file}#sha256={}\">{file}</a></body></html>",
            sha256(&wheel)
        )
        .into_bytes(),
    );
    g.packages.lock().unwrap().insert(
        format!("/api/packages/acme/pypi/files/dynatheia-foo/1.0/{file}"),
        wheel.clone(),
    );
    let h = granary(&g, &[(Eco::Pypi, "dynatheia-*=acme")], Opts::default()).await;
    h.mock.set(
        "/simple/dynatheia-foo/",
        Upstream::ok("<a href=\"x\">evil</a>"),
    );
    h.mock
        .set("/simple/requests/", Upstream::ok("<a>requests</a>"));
    h.mock.set(
        &format!("/files/packages/aa/bb/{file}"),
        Upstream::ok("evil wheel"),
    );
    let token = private_token(&h).await;

    // PEP 503: every spelling is the same private project.
    let mut pages = Vec::new();
    for name in ["dynatheia-foo", "Dynatheia_Foo", "dynatheia.foo"] {
        let (status, _, body) = h
            .request("GET", &format!("/pypi/simple/{name}/"), bearer(&token))
            .await;
        assert_eq!(status, StatusCode::OK, "{name}");
        pages.push(String::from_utf8(body).unwrap());
    }
    assert!(pages.iter().all(|p| p == &pages[0]));
    let link = format!("/pypi/gitea-files/dynatheia-foo/1.0/{file}#sha256=");
    assert!(pages[0].contains(&link), "{}", pages[0]);
    assert!(!pages[0].contains("gitea.example.invalid"));
    assert_eq!(h.mock.hits("/simple/dynatheia-foo/"), 0);

    // The file, checked against the page's sha256.
    let (status, _, body) = h
        .request(
            "GET",
            &format!("/pypi/gitea-files/dynatheia-foo/1.0/{file}"),
            bearer(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, wheel);

    // A private project's file never comes from PyPI's files host, and a
    // missing private project is a 404 without asking PyPI.
    let (status, _, _) = h
        .request(
            "GET",
            &format!("/pypi/files/packages/aa/bb/{file}"),
            bearer(&token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = h
        .request("GET", "/pypi/simple/dynatheia-bar/", bearer(&token))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.mock.hits("/simple/dynatheia-bar/"), 0);
    assert_eq!(h.mock.total(), 0);

    // A public project never goes to Gitea.
    let (status, _, _) = h
        .request("GET", "/pypi/simple/requests/", bearer(&token))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !g.package_hits
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("requests"))
    );

    // Gitea files URLs only exist for private names.
    let (status, _, _) = h
        .request(
            "GET",
            "/pypi/gitea-files/requests/2.0/requests-2.0.tar.gz",
            bearer(&token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maven_group_prefixes_come_only_from_gitea() {
    let g = MockGitea::start().await;
    let pom = b"<project><packaging>jar</packaging></project>".to_vec();
    let jar = b"private jar".to_vec();
    let base = "/api/packages/acme/maven/com/dyn/core/1.0/core-1.0";
    {
        let mut p = g.packages.lock().unwrap();
        p.insert(format!("{base}.pom"), pom.clone());
        p.insert(format!("{base}.pom.sha1"), sha1(&pom).into_bytes());
        p.insert(format!("{base}.jar"), jar.clone());
        p.insert(
            format!("{base}.jar.sha1"),
            sha1(b"something else").into_bytes(),
        );
    }
    let h = granary(&g, &[(Eco::Maven, "com.dyn=acme")], Opts::default()).await;
    h.mock.set(
        "/maven2/com/dyn/core/1.0/core-1.0.pom",
        Upstream::ok("evil"),
    );
    h.mock.set(
        "/maven2/com/dynx/core/1.0/core-1.0.pom",
        Upstream::ok("<project/>"),
    );
    let token = private_token(&h).await;
    let get = |path: &'static str| {
        let (h, token) = (&h, token.clone());
        async move { h.request("GET", path, bearer(&token)).await }
    };

    let (status, _, body) = get("/maven/com/dyn/core/1.0/core-1.0.pom").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, pom);
    // A .sha1 that doesn't match: refused and not cached.
    let (status, _, _) = get("/maven/com/dyn/core/1.0/core-1.0.jar").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        !h.data
            .join("files/gitea-maven/acme/com/dyn/core/1.0/core-1.0.jar")
            .exists()
    );

    // Missing on Gitea: 404, Central is never asked, and the 404 isn't
    // remembered, so publishing it makes it available at once.
    let missing = "/maven/com/dyn/more/2.0/more-2.0.pom";
    for _ in 0..2 {
        assert_eq!(get(missing).await.0, StatusCode::NOT_FOUND);
    }
    let gitea_path = "/api/packages/acme/maven/com/dyn/more/2.0/more-2.0.pom";
    let asked = |g: &MockGitea| {
        g.package_hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == gitea_path)
            .count()
    };
    assert_eq!(asked(&g), 2);
    let more = b"<project/>".to_vec();
    g.packages
        .lock()
        .unwrap()
        .insert(gitea_path.into(), more.clone());
    g.packages
        .lock()
        .unwrap()
        .insert(format!("{gitea_path}.sha1"), sha1(&more).into_bytes());
    assert_eq!(get(missing).await.0, StatusCode::OK);
    assert_eq!(h.mock.hits("/maven2/com/dyn/core/1.0/core-1.0.pom"), 0);
    assert_eq!(h.mock.hits("/maven2/com/dyn/more/2.0/more-2.0.pom"), 0);

    // Whole segments only: com.dynx is public.
    let (status, _, _) = get("/maven/com/dynx/core/1.0/core-1.0.pom").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.mock.hits("/maven2/com/dynx/core/1.0/core-1.0.pom"), 1);
    assert!(
        !g.package_hits
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("dynx"))
    );
}

/// `mycrate` 1.0.0 in Gitea's Cargo registry for `acme`; the index line.
fn publish_crate(g: &MockGitea, krate: &[u8], sparse: bool) -> String {
    let line = json!({"name": "mycrate", "vers": "1.0.0", "deps": [], "cksum": sha256(krate), "features": {}, "yanked": false}).to_string();
    let mut p = g.packages.lock().unwrap();
    if sparse {
        p.insert(
            "/api/packages/acme/cargo/config.json".into(),
            json!({"dl": format!("{ROOT_URL}/api/packages/acme/cargo/api/v1/crates"), "api": ROOT_URL}).to_string().into_bytes(),
        );
        p.insert(
            "/api/packages/acme/cargo/my/cr/mycrate".into(),
            format!("{line}\n").into_bytes(),
        );
    }
    p.insert(
        "/api/packages/acme/cargo/api/v1/crates/mycrate/1.0.0/download".into(),
        krate.to_vec(),
    );
    line
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_private_cargo_registry() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let g = MockGitea::start().await;
    let krate = b"private crate".to_vec();
    publish_crate(&g, &krate, true);
    let h = granary(&g, &[(Eco::Cargo, "acme")], Opts::default()).await;
    let token = private_token(&h).await;

    // Cargo is asked for its token, with Cargo's own challenge.
    let (status, headers, _) = h.get("/cargo-private/config.json").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers["www-authenticate"]
            .to_str()
            .unwrap()
            .starts_with("Cargo")
    );
    let (status, _, body) = h
        .request("GET", "/cargo-private/config.json", Some(token.clone()))
        .await;
    assert_eq!(status, StatusCode::OK);
    let config: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(config["auth-required"], true);
    assert!(
        config["dl"]
            .as_str()
            .unwrap()
            .ends_with("/cargo-private/dl/{crate}/{version}")
    );

    // Index (sparse) and download, checked against the index's cksum.
    let (status, _, body) = h
        .request("GET", "/cargo-private/my/cr/mycrate", Some(token.clone()))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"vers\":\"1.0.0\""));
    let (status, _, body) = h
        .request(
            "GET",
            "/cargo-private/dl/mycrate/1.0.0",
            Some(token.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, krate);
    for bad in [
        "/cargo-private/xx/yy/mycrate",
        "/cargo-private/../config.json",
    ] {
        let (status, _, _) = h.request("GET", bad, Some(token.clone())).await;
        assert_ne!(status, StatusCode::OK, "{bad}");
    }

    // Without private access: nothing, not even the config.
    let viewer = viewer_token(&h).await;
    for path in [
        "/cargo-private/config.json",
        "/cargo-private/my/cr/mycrate",
        "/cargo-private/dl/mycrate/1.0.0",
    ] {
        let (status, _, _) = h.request("GET", path, Some(viewer.clone())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }

    // A Gitea without a sparse index: Granary reads its git index.
    let g2 = MockGitea::start().await;
    let line = publish_crate(&g2, &krate, false);
    g2.create(
        "acme",
        "_cargo-index",
        &[("my/cr/mycrate", format!("{line}\n").as_bytes())],
    );
    let h2 = granary(&g2, &[(Eco::Cargo, "acme")], Opts::default()).await;
    let token2 = private_token(&h2).await;
    let (status, _, body) = h2
        .request("GET", "/cargo-private/my/cr/mycrate", Some(token2.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let (status, _, body) = h2
        .request("GET", "/cargo-private/dl/mycrate/1.0.0", Some(token2))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, krate);

    // No private registry set up: 404.
    let none = granary(&g, &[], Opts::default()).await;
    let (status, _, _) = none
        .request(
            "GET",
            "/cargo-private/config.json",
            Some(none.token.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_serves_cached_private_packages_to_those_who_may_read_them() {
    let g = MockGitea::start().await;
    let tgz = b"private tarball".to_vec();
    publish_npm(&g, &tgz);
    let krate = b"private crate".to_vec();
    publish_crate(&g, &krate, true);
    let list = [(Eco::Npm, "@dyn=acme"), (Eco::Cargo, "acme")];
    let h = granary(&g, &list, Opts::default()).await;
    let token = private_token(&h).await;
    let viewer = viewer_token(&h).await;
    for path in [
        "/npm/@dyn%2fui",
        "/npm/@dyn/ui/-/ui-1.0.0.tgz",
        "/cargo-private/my/cr/mycrate",
        "/cargo-private/dl/mycrate/1.0.0",
    ] {
        let (status, _, _) = h.request("GET", path, bearer(&token)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }

    // The same data, offline: served from the cache, Gitea never asked,
    // and the access policy still applies.
    let offline = granary(
        &g,
        &list,
        Opts {
            offline: true,
            data: Some(h.data.clone()),
            ..Opts::default()
        },
    )
    .await;
    let before = g.package_hits.lock().unwrap().len();
    for path in [
        "/npm/@dyn%2fui",
        "/npm/@dyn/ui/-/ui-1.0.0.tgz",
        "/cargo-private/my/cr/mycrate",
        "/cargo-private/dl/mycrate/1.0.0",
    ] {
        let (status, _, _) = offline.request("GET", path, bearer(&token)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let (status, _, _) = offline.request("GET", path, bearer(&viewer)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        let (status, _, _) = offline.get(path).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
    assert_eq!(g.package_hits.lock().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_and_browse_show_private_packages_to_those_who_may_see_them() {
    let g = MockGitea::start().await;
    let tgz = b"private tarball".to_vec();
    publish_npm(&g, &tgz);
    *g.search.lock().unwrap() = json!([
        {"name": "@dyn/ui", "version": "0.9.0", "type": "npm"},
        {"name": "@dyn/ui", "version": "1.0.0", "type": "npm"},
        {"name": "@other/thing", "version": "1.0.0", "type": "npm"},
    ]);
    let h = granary(&g, &[(Eco::Npm, "@dyn=acme")], Opts::default()).await;
    h.mock.set(
        "/npm/-/v1/search",
        Upstream::ok(
            json!({"objects": [
                {"package": {"name": "@dyn/ui", "version": "6.6.6"}},
                {"package": {"name": "ui-kit", "version": "2.0.0"}},
            ]})
            .to_string(),
        ),
    );
    let admin = h.login().await;
    let token = private_token(&h).await;
    h.request("GET", "/npm/@dyn/ui/-/ui-1.0.0.tgz", bearer(&token))
        .await;

    // Search: the private hit once, from Gitea; the public look-alike gone;
    // Gitea names that don't route to Gitea left out.
    let r = h.api(&admin, "/api/packages/search?eco=npm&q=ui").await;
    let results = r["results"].as_array().unwrap();
    let names: Vec<(&str, bool)> = results
        .iter()
        .map(|x| (x["name"].as_str().unwrap(), x["private"].as_bool().unwrap()))
        .collect();
    assert_eq!(names, vec![("ui-kit", false), ("@dyn/ui", true)], "{r}");
    let private = &results[1];
    assert_eq!(private["latest"], "1.0.0");
    assert_eq!(private["source"], "Gitea");
    assert_eq!(private["cached_versions"], 1);

    // Browse: private, for those who may see it.
    let items = |v: Value| -> Vec<(String, bool)> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap().to_string(),
                    c["private"].as_bool().unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(
        items(h.api(&admin, "/api/components?repo=npm").await),
        vec![("@dyn/ui".to_string(), true)]
    );
    let viewer = h.viewer_login().await;
    assert!(items(h.api(&viewer, "/api/components?repo=npm").await).is_empty());
    let c = h
        .api(
            &admin,
            "/api/component?repo=npm&name=%40dyn%2Fui&private=true",
        )
        .await;
    assert_eq!(c["private"], true);
    let download = c["versions"][0]["files"][0]["download"].as_str().unwrap();
    assert!(
        download.ends_with("/npm/@dyn/ui/-/ui-1.0.0.tgz"),
        "{download}"
    );
    let (status, _, _) = h
        .send(
            "GET",
            "/api/component?repo=npm&name=%40dyn%2Fui&private=true",
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let overview = h.api(&viewer, "/api/overview").await;
    let npm = overview["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo"] == "npm")
        .unwrap()
        .clone();
    assert_eq!(npm["packages"], 0);

    // Deleting it removes the private files and metadata.
    let (status, _, _) = h
        .send(
            "DELETE",
            "/api/component?repo=npm&name=%40dyn%2Fui&private=true",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !h.data
            .join("files/gitea-npm/acme/@dyn/ui/-/ui-1.0.0.tgz")
            .exists()
    );
    assert!(
        !h.data
            .join("meta/gitea-npm/acme/@dyn/ui.abbr.body")
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_stays_with_gitea_across_redirects() {
    let g = MockGitea::start().await;
    let tgz = b"stored elsewhere".to_vec();
    publish_npm(&g, &tgz);
    let h = granary(&g, &[(Eco::Npm, "@dyn=acme")], Opts::default()).await;
    // Gitea with object storage hands out a redirect for the file.
    h.mock.set("/blobs/ui-1.0.0.tgz", Upstream::ok(tgz.clone()));
    g.package_redirects.lock().unwrap().insert(
        "/api/packages/acme/npm/@dyn/ui/-/ui-1.0.0.tgz".into(),
        format!("{}/blobs/ui-1.0.0.tgz", h.upstream_base()),
    );
    let token = private_token(&h).await;
    let (status, _, body) = h
        .request("GET", "/npm/@dyn/ui/-/ui-1.0.0.tgz", bearer(&token))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, tgz);
    assert_eq!(h.mock.hits("/blobs/ui-1.0.0.tgz"), 1);
    assert!(!h.mock.saw_auth.load(Ordering::SeqCst));
    assert!(
        g.seen()
            .iter()
            .all(|a| a == &format!("token {TOKEN}") || a.is_empty())
    );
}

#[tokio::test]
async fn namespaces_are_edited_on_the_system_page_unless_the_environment_sets_them() {
    let g = MockGitea::start().await;
    let h = granary(&g, &[(Eco::Maven, "com.dyn=acme")], Opts::default()).await;
    let admin = h.login().await;
    let put = |format: &'static str, rules: Value| {
        let (h, admin) = (&h, admin.clone());
        async move {
            h.send(
                "PUT",
                "/api/system/gitea/namespaces",
                Some(&admin),
                Some(json!({"format": format, "rules": rules})),
            )
            .await
            .0
        }
    };
    assert_eq!(
        put("npm", json!([{"pattern": "@Dyn", "owner": "acme"}])).await,
        StatusCode::OK
    );
    assert_eq!(
        h.state.namespaces.owner(Eco::Npm, "@dyn/x").as_deref(),
        Some("acme")
    );
    assert_eq!(
        put("npm", json!([{"pattern": "dyn", "owner": "acme"}])).await,
        StatusCode::BAD_REQUEST
    );
    // Locked by GRANARY_GITEA_MAVEN.
    assert_eq!(
        put("maven", json!([{"pattern": "org.x", "owner": "acme"}])).await,
        StatusCode::CONFLICT
    );
    let sys = h.api(&admin, "/api/system").await;
    let lists = sys["gitea"]["namespaces"].as_array().unwrap();
    let maven = lists.iter().find(|n| n["format"] == "maven").unwrap();
    assert_eq!(maven["locked"], true);
    assert_eq!(maven["env"], "GRANARY_GITEA_MAVEN");
    let npm = lists.iter().find(|n| n["format"] == "npm").unwrap();
    assert_eq!(npm["rules"][0]["pattern"], "@dyn");

    // Saved: a restart keeps the page's list.
    let again = granary(
        &g,
        &[(Eco::Maven, "com.dyn=acme")],
        Opts {
            data: Some(h.data.clone()),
            ..Opts::default()
        },
    )
    .await;
    assert_eq!(
        again.state.namespaces.owner(Eco::Npm, "@dyn/x").as_deref(),
        Some("acme")
    );
    // A viewer can't change them.
    let viewer = h.viewer_login().await;
    let (status, _, _) = h
        .send(
            "PUT",
            "/api/system/gitea/namespaces",
            Some(&viewer),
            Some(json!({"format": "npm", "rules": []})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
