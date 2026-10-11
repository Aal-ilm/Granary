//! "New from repo" against a fixture repo served by a local upstream: a
//! Cargo workspace, an npm workspace with a lockfile and a requirements.txt.

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::{
    Harness, Opts,
    git::{Upstream, granary, have_git},
    jobs::{Plan, set_tiny_package, wait_for, with_fake},
};

const ROOT_CARGO: &str = r#"[workspace]
members = ["crates/*"]
resolver = "2"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
"#;
const CORE: &str = r#"[package]
name = "mono-core"
version = "0.1.0"

[dependencies]
serde = { workspace = true }
anyhow = "1"
itoa = { git = "https://github.com/dtolnay/itoa" }
"#;
const CLI: &str = r#"[package]
name = "mono-cli"
version = "0.1.0"

[dependencies]
mono-core = { path = "../core" }
clap = { version = "4", features = ["derive"] }
"#;
const WEB: &str = r#"{"name":"web","private":true,"workspaces":["packages/*"],"devDependencies":{"typescript":"^5"}}"#;
const UI: &str =
    r#"{"name":"@mono/ui","dependencies":{"react":"^19","@mono/utils":"workspace:*"}}"#;
const UTILS: &str = r#"{"name":"@mono/utils","dependencies":{"lodash":"^4"}}"#;

fn fixture(up: &Upstream) -> std::path::PathBuf {
    up.create(
        "acme",
        "mono",
        &[
            ("Cargo.toml", ROOT_CARGO.as_bytes()),
            ("Cargo.lock", b"version = 4\n"),
            ("crates/core/Cargo.toml", CORE.as_bytes()),
            ("crates/cli/Cargo.toml", CLI.as_bytes()),
            ("web/package.json", WEB.as_bytes()),
            ("web/package-lock.json", b"{\"lockfileVersion\":3}\n"),
            ("web/packages/ui/package.json", UI.as_bytes()),
            ("web/packages/utils/package.json", UTILS.as_bytes()),
            (
                "web/node_modules/left-pad/package.json",
                br#"{"dependencies":{"should-not":"1"}}"#,
            ),
            ("requirements.txt", b"fastapi>=0.115\n"),
            ("README.md", b"mono"),
        ],
    )
}

/// Preview a repo, following the clone job the first time.
async fn preview(h: &Harness, cookie: &str, url: &str, git_ref: &str) -> Value {
    for _ in 0..2 {
        let (status, _, body) = h
            .send(
                "POST",
                "/api/templates/from-repo/preview",
                Some(cookie),
                Some(json!({"url": url, "ref": git_ref})),
            )
            .await;
        let d: Value =
            serde_json::from_slice(&body).unwrap_or(json!({"raw": String::from_utf8_lossy(&body)}));
        match status {
            StatusCode::OK => return d,
            StatusCode::ACCEPTED => {
                assert_eq!(d["cloning"], true);
                let job = d["job"].as_i64().unwrap();
                let j = wait_for(h, cookie, job, &["done", "failed"]).await;
                assert_eq!(j["job"]["status"], "done", "{j}");
            }
            other => panic!("preview: {other} {d}"),
        }
    }
    panic!("the repo was never cached");
}

async fn save(h: &Harness, cookie: &str, p: &Value, name: &str) -> (i64, Value) {
    let (status, _, body) = h
        .send(
            "POST",
            "/api/templates/from-repo",
            Some(cookie),
            Some(json!({"url": p["url"], "ref": p["ref"], "commit": p["commit"], "name": name, "tags": ["mono"]})),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let d: Value = serde_json::from_slice(&body).unwrap();
    (d["id"].as_i64().unwrap(), d)
}

fn paths(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().or(m.as_str()).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn previews_and_saves_a_workspace_repo() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    fixture(&up);
    let h = granary(&up, Opts::default()).await;
    let cookie = h.login().await;

    let p = preview(&h, &cookie, "https://github.com/acme/mono", "").await;
    assert_eq!(p["repo"], "github.com/acme/mono");
    assert_eq!(p["commit"].as_str().unwrap().len(), 40);
    assert_eq!(
        paths(&p["manifests"]),
        vec![
            "Cargo.toml",
            "crates/cli/Cargo.toml",
            "crates/core/Cargo.toml",
            "requirements.txt",
            "web/package.json",
            "web/packages/ui/package.json",
            "web/packages/utils/package.json",
        ],
        "workspace members are found three levels down; node_modules is ignored"
    );
    assert_eq!(
        paths(&p["lockfiles"]),
        vec!["Cargo.lock", "web/package-lock.json"]
    );
    assert_eq!(
        p["counts"],
        json!({"cargo": 3, "npm": 3, "pypi": 1, "maven": 0})
    );
    let cargo = p["lines"]["cargo"].as_str().unwrap();
    assert!(
        cargo.contains(r#"serde = { version = "1", features = ["derive"] }"#),
        "{cargo}"
    );
    assert!(!cargo.contains("mono-core"));
    let dep = &p["git_deps"][0];
    assert_eq!(dep["url"], "https://github.com/dtolnay/itoa");
    assert_eq!(dep["allowed"], true);
    assert_eq!(dep["cached"], false);
    let skipped: Vec<&str> = p["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["item"].as_str().unwrap())
        .collect();
    assert!(skipped.contains(&"itoa"), "{skipped:?}");

    let (id, saved) = save(&h, &cookie, &p, "Mono").await;
    assert_eq!(
        saved["git_jobs"], 1,
        "the git dependency is queued for caching"
    );
    let t = h.api(&cookie, &format!("/api/templates/{id}")).await;
    assert_eq!(t["builtin"], false);
    assert_eq!(t["tags"], json!(["mono"]));
    assert_eq!(
        t["counts"],
        json!({"cargo": 3, "npm": 3, "pypi": 1, "maven": 0})
    );
    assert_eq!(t["source"]["url"], "https://github.com/acme/mono");
    assert_eq!(t["source"]["commit"], p["commit"]);
    assert_eq!(
        t["source"]["lockfiles"],
        json!(["Cargo.lock", "web/package-lock.json"])
    );
    assert!(
        t["cargo"]
            .as_str()
            .unwrap()
            .contains("# crates/core/Cargo.toml")
    );

    // Same name again is refused; a commit Granary doesn't have is refused.
    let (status, _, _) = h
        .send(
            "POST",
            "/api/templates/from-repo",
            Some(&cookie),
            Some(json!({"url": p["url"], "commit": p["commit"], "name": "Mono"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) = h
        .send(
            "POST",
            "/api/templates/from-repo",
            Some(&cookie),
            Some(json!({"url": p["url"], "commit": "0".repeat(40), "name": "Other"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Admins only.
    let viewer = h.viewer_login().await;
    let (status, _, _) = h
        .send(
            "POST",
            "/api/templates/from-repo/preview",
            Some(&viewer),
            Some(json!({"url": "https://github.com/acme/mono"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resyncs_and_pins_refs() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    let work = fixture(&up);
    up.tag(&work, "v1");
    // TTL zero, so a preview fetches new commits.
    let h = granary(
        &up,
        Opts {
            meta_ttl: Duration::ZERO,
            ..Opts::default()
        },
    )
    .await;
    let cookie = h.login().await;

    let first = preview(&h, &cookie, "https://github.com/acme/mono", "").await;
    let (id, _) = save(&h, &cookie, &first, "Mono").await;

    // Upstream moves on: a new Python dependency, and react goes away.
    up.change(
        &work,
        &[
            ("requirements.txt", b"fastapi>=0.115\nhttpx\n"),
            (
                "web/packages/ui/package.json",
                br#"{"name":"@mono/ui","dependencies":{"@mono/utils":"workspace:*"}}"#,
            ),
        ],
        "deps",
    );
    let next = preview(&h, &cookie, "https://github.com/acme/mono", "").await;
    assert_ne!(next["commit"], first["commit"]);
    assert_eq!(next["counts"]["pypi"], 2);
    assert_eq!(next["counts"]["npm"], 2);

    let (status, _, body) = h
        .send(
            "POST",
            &format!("/api/templates/{id}/resync"),
            Some(&cookie),
            Some(json!({"commit": next["commit"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let t = h.api(&cookie, &format!("/api/templates/{id}")).await;
    assert_eq!(t["name"], "Mono", "name and tags are kept");
    assert_eq!(t["tags"], json!(["mono"]));
    assert_eq!(t["counts"]["pypi"], 2);
    assert!(
        !t["npm"].as_str().unwrap().contains("react"),
        "lines are replaced, not merged"
    );
    assert_eq!(t["source"]["commit"], next["commit"]);

    // A tag pins the original commit.
    let tagged = preview(&h, &cookie, "https://github.com/acme/mono", "v1").await;
    assert_eq!(tagged["commit"], first["commit"]);
    assert_eq!(tagged["counts"]["pypi"], 1);

    for (r, want) in [
        ("a..b", StatusCode::BAD_REQUEST),
        ("no-such-branch", StatusCode::NOT_FOUND),
    ] {
        let (status, _, _) = h
            .send(
                "POST",
                "/api/templates/from-repo/preview",
                Some(&cookie),
                Some(json!({"url": "https://github.com/acme/mono", "ref": r})),
            )
            .await;
        assert_eq!(status, want, "{r}");
    }
    let audit = h.api(&cookie, "/api/audit?action=template").await;
    assert!(
        audit["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "template.resync")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warming_uses_the_repo_lockfiles() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    fixture(&up);
    let (h, fake, cookie) = with_fake(
        Plan::default(),
        Opts {
            git_upstream: Some(up.url.clone()),
            ..Opts::default()
        },
    )
    .await;
    let p = preview(&h, &cookie, "https://github.com/acme/mono", "").await;
    let (id, _) = save(&h, &cookie, &p, "Mono").await;

    let (status, _, body) = h
        .send(
            "POST",
            &format!("/api/templates/{id}/warm"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let job = serde_json::from_slice::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    let d = wait_for(&h, &cookie, job, &["done", "failed"]).await;
    assert_eq!(d["job"]["status"], "done", "{d}");

    let runs = fake.runs.lock().unwrap().clone();
    let by = |eco: &str| {
        runs.iter()
            .filter(|r| r["ecosystem"] == eco)
            .cloned()
            .collect::<Vec<_>>()
    };
    let cargo = by("cargo");
    assert_eq!(cargo.len(), 1, "one locked Cargo run, no generated one");
    let repo = &cargo[0]["repo"];
    assert!(
        repo["url"]
            .as_str()
            .unwrap()
            .ends_with("/git/github.com/acme/mono.git"),
        "{repo}"
    );
    assert!(
        repo["url"]
            .as_str()
            .unwrap()
            .starts_with(h.url.as_ref().unwrap()),
        "through Granary, never upstream"
    );
    assert_eq!(repo["commit"], p["commit"]);
    assert_eq!(repo["dir"], "");
    assert_eq!(by("npm")[0]["repo"]["dir"], "web");
    let pypi = by("pypi");
    assert!(pypi[0]["repo"].is_null());
    assert_eq!(pypi[0]["files"]["requirements.txt"], "fastapi>=0.115\n");
    assert_eq!(cargo[0]["git_hosts"], json!(["github.com", "codeberg.org"]));
    let log: Vec<&str> = d["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["line"].as_str().unwrap())
        .collect();
    assert!(
        log.contains(
            &"--- npm from the repo's package-lock.json in web (linux-x86_64, macos-arm64)"
        ),
        "{log:?}"
    );
}

/// The real warmer.py runs `npm ci` in a checkout made through Granary's
/// git cache. Skipped when git, python3 or npm is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_warmer_runs_npm_ci_in_the_checkout() {
    let have = |cmd: &str| {
        std::process::Command::new(cmd)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !have("git") || !have("python3") || !have("npm") {
        return eprintln!("skipping: git, python3 and npm are needed");
    }
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::Digest;
    let tgz = include_bytes!("fixtures/granary-tiny-1.0.0.npm.tar.gz");
    let integrity = format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(tgz)));
    let lock = json!({
        "name": "app",
        "lockfileVersion": 3,
        "requires": true,
        "packages": {
            "": {"name": "app", "dependencies": {"granary-tiny": "1.0.0"}},
            "node_modules/granary-tiny": {
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/granary-tiny/-/granary-tiny-1.0.0.tgz",
                "integrity": integrity,
                "license": "MIT",
            },
        },
    })
    .to_string();
    let up = Upstream::start().await;
    up.create(
        "acme",
        "app",
        &[
            (
                "package.json",
                br#"{"name":"app","private":true,"dependencies":{"granary-tiny":"1.0.0"}}"#,
            ),
            ("package-lock.json", lock.as_bytes()),
            // A repo's own registry setting must not take effect.
            (".npmrc", b"registry=http://evil.invalid/\n"),
        ],
    );

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = tokio::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/warmer/warmer.py"))
        .env("WARMER_SECRET", super::WARMER_SECRET)
        .env("WARMER_LISTEN", format!("127.0.0.1:{port}"))
        .env("WARMER_ALLOW_FROM", "127.0.0.1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let h = Harness::with(Opts {
        warmer_url: Some(format!("http://127.0.0.1:{port}")),
        git_upstream: Some(up.url.clone()),
        serve: true,
        ..Opts::default()
    })
    .await;
    let cookie = h.login().await;
    set_tiny_package(&h, tgz);
    let start = std::time::Instant::now();
    while h.api(&cookie, "/api/warmer").await["available"] != true {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "warmer did not start"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let p = preview(&h, &cookie, "https://github.com/acme/app", "").await;
    let (status, _, body) = h
        .send(
            "POST",
            "/api/templates/from-repo",
            Some(&cookie),
            Some(json!({"url": p["url"], "commit": p["commit"], "name": "App", "platforms": ["linux-x86_64"]})),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let id = serde_json::from_slice::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    let (_, _, body) = h
        .send(
            "POST",
            &format!("/api/templates/{id}/warm"),
            Some(&cookie),
            None,
        )
        .await;
    let job = serde_json::from_slice::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    let start = std::time::Instant::now();
    let d = loop {
        let d = h.api(&cookie, &format!("/api/jobs/{job}")).await;
        if ["done", "failed"].contains(&d["job"]["status"].as_str().unwrap()) {
            break d;
        }
        assert!(start.elapsed() < Duration::from_secs(120), "warm job stuck");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let log: Vec<&str> = d["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["line"].as_str().unwrap())
        .collect();
    assert_eq!(d["job"]["status"], "done", "{log:#?}");
    assert!(
        log.iter().any(|l| l.contains("npm ci --ignore-scripts")),
        "{log:#?}"
    );
    assert!(
        log.iter().any(|l| l.contains("Removed 1 tool config")),
        "{log:#?}"
    );
    assert!(
        h.data
            .join("files/npm/granary-tiny/-/granary-tiny-1.0.0.tgz")
            .exists()
    );
    assert_eq!(h.mock.hits("/npm/granary-tiny/-/granary-tiny-1.0.0.tgz"), 1);
    assert!(
        log.iter().all(|l| !l.contains("gran_")),
        "no token in the log"
    );
    let _ = child.kill().await;
}
