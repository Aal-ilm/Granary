//! Private Go modules: the rule (never the public proxy or the checksum
//! database), zips built from git that hash exactly like the go command's,
//! versions and pseudo-versions, Gitea's Go registry, pins, the access
//! policy and offline mode.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::{
    Harness, Opts, Upstream as MockFile,
    git::{TempDir, Upstream, client_git, have_git, ok},
    gitea::{MockGitea, TOKEN},
    gitea_packages::{private_token, viewer_token},
};
use crate::gomod;

const PREFIX: &str = "github.com/acme";

fn have_go() -> bool {
    std::process::Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn bearer(token: &str) -> Option<String> {
    Some(format!("Bearer {token}"))
}

/// Granary with `github.com/acme` private, cloned from `up`. Metadata is
/// never fresh, so every request refreshes the mirror.
async fn granary(up: &Upstream, opts: Opts) -> Harness {
    Harness::with(Opts {
        git_upstream: Some(up.url.clone()),
        go_private: Some(PREFIX.to_string()),
        meta_ttl: Duration::ZERO,
        ..opts
    })
    .await
}

/// The repos every test builds on.
///
/// - `tool`: a root module with a LICENSE, a vendored package, a nested
///   module, a symlink, an export-ignored file and a `sub` module; tagged
///   v1.0.0 and v1.1.0-rc.1, then one more commit.
/// - `new`: the same shapes under `go 1.24`.
/// - `maj`: a /v2 module at the root (major branch layout).
/// - `majdir`: v1 at the root and /v2 in a `v2` directory.
/// - `legacy`: no go.mod, tagged v2.3.0 (+incompatible).
fn fixtures(up: &Upstream) -> PathBuf {
    let shapes = |go: &str, module: &str| -> Vec<(String, Vec<u8>)> {
        vec![
            (
                "go.mod".into(),
                format!("module {module}\n\ngo {go}\n").into_bytes(),
            ),
            ("a.go".into(), b"package tool\n".to_vec()),
            ("LICENSE".into(), b"MIT\n".to_vec()),
            ("vendor/x.example/y/y.go".into(), b"package y\n".to_vec()),
            (
                "vendor/modules.txt".into(),
                b"# x.example/y v1.0.0\n".to_vec(),
            ),
            ("pkg/vendor/vendor.go".into(), b"package vendor\n".to_vec()),
            ("pkg/vendor/foo/foo.go".into(), b"package foo\n".to_vec()),
            (
                "nested/go.mod".into(),
                b"module example.com/nested\n".to_vec(),
            ),
            ("nested/n.go".into(), b"package nested\n".to_vec()),
            (
                ".gitattributes".into(),
                b"secret.txt export-ignore\n".to_vec(),
            ),
            ("secret.txt".into(), b"not for export\n".to_vec()),
            (
                "sub/go.mod".into(),
                format!("module {module}/sub\n\ngo {go}\n").into_bytes(),
            ),
            ("sub/s.go".into(), b"package sub\n".to_vec()),
        ]
    };
    let make = |name: &str, files: Vec<(String, Vec<u8>)>| {
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice()))
            .collect();
        up.create("acme", name, &refs)
    };
    let tool = make("tool", shapes("1.21", "github.com/acme/tool"));
    std::os::unix::fs::symlink("a.go", tool.join("link.go")).unwrap();
    up.commit(&tool, "symlink");
    up.tag(&tool, "v1.0.0");
    up.tag(&tool, "sub/v0.2.0");
    up.change(&tool, &[("b.go", b"package tool\n")], "b");
    up.tag(&tool, "v1.1.0-rc.1");
    up.change(&tool, &[("c.go", b"package tool\n")], "c");

    let new = make("new", shapes("1.24", "github.com/acme/new"));
    up.tag(&new, "v0.1.0");

    let maj = make(
        "maj",
        vec![
            (
                "go.mod".into(),
                b"module github.com/acme/maj/v2\n\ngo 1.21\n".to_vec(),
            ),
            ("m.go".into(), b"package maj\n".to_vec()),
        ],
    );
    up.tag(&maj, "v2.0.0");

    let majdir = make(
        "majdir",
        vec![
            (
                "go.mod".into(),
                b"module github.com/acme/majdir\n\ngo 1.21\n".to_vec(),
            ),
            ("m.go".into(), b"package majdir\n".to_vec()),
            (
                "v2/go.mod".into(),
                b"module github.com/acme/majdir/v2\n\ngo 1.21\n".to_vec(),
            ),
            ("v2/m.go".into(), b"package majdir\n".to_vec()),
        ],
    );
    up.tag(&majdir, "v1.0.0");
    up.tag(&majdir, "v2.1.0");

    let legacy = make(
        "legacy",
        vec![("l.go".into(), b"package legacy\n".to_vec())],
    );
    up.tag(&legacy, "v2.3.0");
    tool
}

/// Run `go` so that it downloads `github.com/acme/*` straight from `up`,
/// with no proxy and no checksum database: the reference to compare with.
fn go_direct(up: &Upstream, home: &Path, args: &[&str]) -> String {
    let mut gitconfig = String::new();
    for repo in ["tool", "new", "maj", "majdir", "legacy"] {
        gitconfig.push_str(&format!(
            "[url \"{}/github.com/acme/{repo}.git\"]\n\tinsteadOf = https://github.com/acme/{repo}\n",
            up.url
        ));
    }
    std::fs::write(home.join("gitconfig"), gitconfig).unwrap();
    let out = std::process::Command::new("go")
        .args(args)
        .current_dir(home)
        .env("GOENV", "off")
        .env("GOPROXY", "direct")
        .env("GOPRIVATE", PREFIX)
        .env("GOFLAGS", "-modcacherw")
        .env("GOMODCACHE", home.join("modcache"))
        .env("GOPATH", home.join("gopath"))
        .env("GOCACHE", home.join("gocache"))
        .env("GOTOOLCHAIN", "local")
        .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "go {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

async fn get(h: &Harness, token: &str, path: &str) -> (StatusCode, Vec<u8>) {
    let (status, _, body) = h.request("GET", path, bearer(token)).await;
    (status, body)
}

fn h1_of_zip(dir: &TempDir, module: &str, version: &str, zip: &[u8]) -> String {
    let path = dir.0.join("granary.zip");
    std::fs::write(&path, zip).unwrap();
    gomod::zip_hash(&path, module, version).unwrap()
}

/// Granary's zips and go.mod files hash exactly like the ones the go
/// command makes from the same repositories. Skipped without git or go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zips_and_versions_match_the_go_command() {
    if !have_git() || !have_go() {
        return eprintln!("skipping: git and go are needed");
    }
    let up = Upstream::start().await;
    fixtures(&up);
    let h = granary(&up, Opts::default()).await;
    let token = private_token(&h).await;
    let home = TempDir::new("go-direct");

    let cases = [
        ("github.com/acme/tool", "v1.0.0"),
        ("github.com/acme/tool", "v1.1.0-rc.1"),
        ("github.com/acme/tool/sub", "v0.2.0"),
        ("github.com/acme/new", "v0.1.0"),
        ("github.com/acme/maj/v2", "v2.0.0"),
        ("github.com/acme/majdir", "v1.0.0"),
        ("github.com/acme/majdir/v2", "v2.1.0"),
        ("github.com/acme/legacy", "v2.3.0+incompatible"),
    ];
    for (module, version) in cases {
        let want: Value = {
            let (up, home) = (up.clone(), home.0.clone());
            let arg = format!("{module}@{version}");
            let out = tokio::task::spawn_blocking(move || {
                go_direct(&up, &home, &["mod", "download", "-json", &arg])
            })
            .await
            .unwrap();
            serde_json::from_str(&out).unwrap()
        };
        let base = format!(
            "/go/{}/@v/{}",
            gomod::escape(module),
            gomod::escape(version)
        );
        let (status, zip) = get(&h, &token, &format!("{base}.zip")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{module}@{version}: {}",
            String::from_utf8_lossy(&zip)
        );
        let listing = |data: &[u8]| -> Vec<String> {
            let mut z = zip::ZipArchive::new(std::io::Cursor::new(data.to_vec())).unwrap();
            (0..z.len())
                .map(|i| {
                    let f = z.by_index(i).unwrap();
                    format!("{} ({})", f.name().unwrap(), f.size())
                })
                .collect()
        };
        assert_eq!(
            h1_of_zip(&home, module, version, &zip),
            want["Sum"].as_str().unwrap(),
            "zip of {module}@{version}\n  Granary: {:?}\n  go:      {:?}",
            listing(&zip),
            listing(&std::fs::read(want["Zip"].as_str().unwrap()).unwrap())
        );
        let (status, go_mod) = get(&h, &token, &format!("{base}.mod")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            gomod::go_mod_hash(&go_mod),
            want["GoModSum"].as_str().unwrap(),
            "go.mod of {module}@{version}"
        );
        let (status, info) = get(&h, &token, &format!("{base}.info")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_slice::<Value>(&info).unwrap()["Version"],
            version
        );
    }

    // What's in a zip: no vendored packages, nested modules or symlinks;
    // export-ignore has no effect (as with go); the root LICENSE in a
    // subdirectory module.
    let names = |zip: &[u8]| -> Vec<String> {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(zip.to_vec())).unwrap();
        let mut out: Vec<String> = (0..z.len())
            .map(|i| z.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        out.sort();
        out
    };
    let (_, zip) = get(&h, &token, "/go/github.com/acme/tool/@v/v1.0.0.zip").await;
    let p = "github.com/acme/tool@v1.0.0/";
    assert_eq!(
        names(&zip),
        [
            ".gitattributes",
            "LICENSE",
            "a.go",
            "go.mod",
            "secret.txt",
            "symlink.txt",
            "vendor/modules.txt"
        ]
        .map(|f| format!("{p}{f}"))
    );
    let (_, zip) = get(&h, &token, "/go/github.com/acme/tool/sub/@v/v0.2.0.zip").await;
    let p = "github.com/acme/tool/sub@v0.2.0/";
    assert_eq!(
        names(&zip),
        ["LICENSE", "go.mod", "s.go"].map(|f| format!("{p}{f}"))
    );
    // go 1.24 drops vendor/modules.txt and keeps pkg/vendor/vendor.go.
    let (_, zip) = get(&h, &token, "/go/github.com/acme/new/@v/v0.1.0.zip").await;
    let p = "github.com/acme/new@v0.1.0/";
    assert_eq!(
        names(&zip),
        [
            ".gitattributes",
            "LICENSE",
            "a.go",
            "go.mod",
            "pkg/vendor/vendor.go",
            "secret.txt"
        ]
        .map(|f| format!("{p}{f}"))
    );

    // Version lists and queries agree with `go list`.
    for module in [
        "github.com/acme/tool",
        "github.com/acme/tool/sub",
        "github.com/acme/maj/v2",
        "github.com/acme/majdir",
        "github.com/acme/majdir/v2",
        "github.com/acme/legacy",
    ] {
        let want = {
            let (up, home, m) = (up.clone(), home.0.clone(), module.to_string());
            tokio::task::spawn_blocking(move || {
                go_direct(&up, &home, &["list", "-m", "-versions", &m])
            })
            .await
            .unwrap()
        };
        let want: Vec<&str> = want.split_whitespace().skip(1).collect();
        let (status, list) = get(&h, &token, &format!("/go/{module}/@v/list")).await;
        assert_eq!(status, StatusCode::OK, "{module}");
        let list = String::from_utf8(list).unwrap();
        assert_eq!(
            list.lines().collect::<Vec<_>>(),
            want,
            "versions of {module}"
        );
    }
    for (module, query) in [
        ("github.com/acme/tool", "main"),
        ("github.com/acme/tool/sub", "main"),
        ("github.com/acme/maj/v2", "main"),
        ("github.com/acme/legacy", "main"),
    ] {
        let want: Value = {
            let (up, home) = (up.clone(), home.0.clone());
            let arg = format!("{module}@{query}");
            let out = tokio::task::spawn_blocking(move || {
                go_direct(&up, &home, &["list", "-m", "-json", &arg])
            })
            .await
            .unwrap();
            serde_json::from_str(&out).unwrap()
        };
        let (status, info) = get(&h, &token, &format!("/go/{module}/@v/{query}.info")).await;
        assert_eq!(status, StatusCode::OK, "{module}@{query}");
        let info: Value = serde_json::from_slice(&info).unwrap();
        assert_eq!(info["Version"], want["Version"], "{module}@{query}");
        assert_eq!(info["Time"], want["Time"], "{module}@{query}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn versions_latest_and_pseudo_versions() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    let tool = fixtures(&up);
    let h = granary(&up, Opts::default()).await;
    let token = private_token(&h).await;
    let json = |b: Vec<u8>| serde_json::from_slice::<Value>(&b).unwrap();

    let (_, list) = get(&h, &token, "/go/github.com/acme/tool/@v/list").await;
    assert_eq!(String::from_utf8(list).unwrap(), "v1.0.0\nv1.1.0-rc.1\n");
    // @latest: the highest release, not the pre-release after it.
    let (_, latest) = get(&h, &token, "/go/github.com/acme/tool/@latest").await;
    assert_eq!(json(latest)["Version"], "v1.0.0");
    // No tags: a pseudo-version of the default branch's head.
    up.create(
        "acme",
        "untagged",
        &[("go.mod", b"module github.com/acme/untagged\n")],
    );
    let (_, list) = get(&h, &token, "/go/github.com/acme/untagged/@v/list").await;
    assert!(list.is_empty());
    let (_, latest) = get(&h, &token, "/go/github.com/acme/untagged/@latest").await;
    let v = json(latest)["Version"].as_str().unwrap().to_string();
    assert!(
        v.starts_with("v0.0.0-") && v.len() == "v0.0.0-20060102150405-abcdefabcdef".len(),
        "{v}"
    );

    // The head of main: a pseudo-version after v1.1.0-rc.1.
    let (_, info) = get(&h, &token, "/go/github.com/acme/tool/@v/main.info").await;
    let pseudo = json(info)["Version"].as_str().unwrap().to_string();
    assert!(pseudo.starts_with("v1.1.0-rc.1.0."), "{pseudo}");
    let head = ok(client_git(&["rev-parse", "HEAD"], &tool))
        .trim()
        .to_string();
    assert!(pseudo.ends_with(&head[..12]), "{pseudo}");
    // Asked for by that pseudo-version, by commit, and as a zip.
    for path in [
        format!("/go/github.com/acme/tool/@v/{pseudo}.info"),
        format!("/go/github.com/acme/tool/@v/{}.info", &head[..12]),
        format!("/go/github.com/acme/tool/@v/{pseudo}.zip"),
    ] {
        assert_eq!(get(&h, &token, &path).await.0, StatusCode::OK, "{path}");
    }
    // A pseudo-version must match its commit: the timestamp, and a base
    // that is a tag before it.
    let (stem, rev) = pseudo.rsplit_once('-').unwrap();
    let wrong_time = format!("{}20000101000000-{rev}", &stem[..stem.len() - 14]);
    let wrong_base = format!("v1.9.1-0.{}-{rev}", &stem[stem.len() - 14..]);
    for bad in [
        wrong_time,
        wrong_base,
        "v1.0.5".to_string(),
        "v3.0.0".to_string(),
    ] {
        let (status, body) = get(
            &h,
            &token,
            &format!("/go/github.com/acme/tool/@v/{bad}.info"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{bad}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    // A module that isn't where its path says.
    let (status, _) = get(&h, &token, "/go/github.com/acme/tool/nope/@v/list").await;
    assert_eq!(status, StatusCode::OK); // no tags for it
    let (status, _) = get(&h, &token, "/go/github.com/acme/tool/nope/@v/main.info").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&h, &token, "/go/github.com/acme/norepo/@v/list").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_modules_stay_private() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    fixtures(&up);
    let h = granary(&up, Opts::default()).await;
    let token = private_token(&h).await;
    let viewer = viewer_token(&h).await;
    // Look-alikes on the public proxy, which must never be asked.
    h.mock.set(
        "/goproxy/github.com/acme/tool/@v/list",
        MockFile::ok("v9.9.9\n"),
    );
    h.mock.set(
        "/goproxy/github.com/acme/ghost/@v/list",
        MockFile::ok("v9.9.9\n"),
    );
    h.mock.set(
        "/goproxy/github.com/other/pub/@v/list",
        MockFile::ok("v1.0.0\n"),
    );

    let zip = "/go/github.com/acme/tool/@v/v1.0.0.zip";
    for path in [
        "/go/github.com/acme/tool/@v/list",
        "/go/github.com/acme/tool/@latest",
        "/go/github.com/acme/tool/@v/v1.0.0.info",
        "/go/github.com/acme/tool/@v/v1.0.0.mod",
        zip,
    ] {
        // Anonymous: asked for a token, whether or not it exists. Without
        // private access: not found. Both before and after it is cached.
        for _ in 0..2 {
            let (status, headers, _) = h.get(path).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
            assert!(headers.contains_key("www-authenticate"));
            assert_eq!(
                get(&h, &viewer, path).await.0,
                StatusCode::NOT_FOUND,
                "{path}"
            );
            assert_eq!(get(&h, &token, path).await.0, StatusCode::OK, "{path}");
        }
    }
    assert_eq!(
        h.get("/go/github.com/acme/ghost/@v/list").await.0,
        StatusCode::UNAUTHORIZED
    );
    // Not in its source: a 404, never the public proxy's copy.
    assert_eq!(
        get(&h, &token, "/go/github.com/acme/ghost/@v/list").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&h, &token, "/go/github.com/acme/tool/@v/v7.0.0.zip")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // The checksum database is never asked about a private module.
    let (status, body) = get(
        &h,
        &token,
        "/go/sumdb/sum.golang.org/lookup/github.com/acme/tool@v1.0.0",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("GONOSUMDB"));
    assert_eq!(h.mock.total(), 0, "{:?}", h.mock.requests.lock().unwrap());

    // A public path never reaches a private source: it goes to the proxy.
    let (status, body) = get(&h, &token, "/go/github.com/other/pub/@v/list").await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, b"v1.0.0\n".as_slice())
    );
    assert!(!h.data.join("git/github.com/other").exists());

    // Cached apart from public modules, and listed as private.
    assert!(
        h.data
            .join("files/go-private/git/github.com/acme/tool/@v/v1.0.0")
            .exists()
    );
    assert!(!h.data.join("files/go/github.com/acme").exists());
    let admin = h.login().await;
    let c = h.api(&admin, "/api/components?repo=go").await;
    assert_eq!(c["items"][0]["name"], "github.com/acme/tool");
    assert_eq!(c["items"][0]["private"], true);
    let viewer_cookie = h.sign_in("viewer", "a long viewer password").await;
    assert!(
        h.api(&viewer_cookie, "/api/components?repo=go").await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // The prefix list, for GONOSUMDB: only to those with private access.
    let (_, list) = get(&h, &token, "/go/private-prefixes").await;
    assert_eq!(list, b"github.com/acme\n");
    let (status, list) = get(&h, &viewer, "/go/private-prefixes").await;
    assert_eq!((status, list.len()), (StatusCode::OK, 0));

    // Offline, on the same data: what is cached is served, and versions
    // not built yet are built from the cached mirror. The policy holds.
    let offline = granary(
        &up,
        Opts {
            offline: true,
            data: Some(h.data.clone()),
            ..Opts::default()
        },
    )
    .await;
    for path in [
        zip,
        "/go/github.com/acme/tool/@v/v1.1.0-rc.1.zip",
        "/go/github.com/acme/tool/@v/list",
    ] {
        assert_eq!(
            get(&offline, &token, path).await.0,
            StatusCode::OK,
            "{path}"
        );
        assert_eq!(
            get(&offline, &viewer, path).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
        assert_eq!(
            offline.get(path).await.0,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    assert_eq!(
        get(&offline, &token, "/go/github.com/acme/new/@v/list")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

/// The audit log's integrity entries.
async fn integrity_entries(h: &Harness) -> Vec<String> {
    sqlx::query_as::<_, (String,)>(
        "SELECT target FROM audit WHERE action = 'integrity.go_module' ORDER BY id",
    )
    .fetch_all(&h.state.auth.db)
    .await
    .unwrap()
    .into_iter()
    .map(|(t,)| t)
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moved_tag_is_refused() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    let tool = fixtures(&up);
    let h = granary(&up, Opts::default()).await;
    let token = private_token(&h).await;
    let admin = h.login().await;
    let zip_path = "/go/github.com/acme/tool/@v/v1.0.0.zip";
    let mod_path = "/go/github.com/acme/tool/@v/v1.0.0.mod";
    let (_, original) = get(&h, &token, zip_path).await;
    let (_, original_mod) = get(&h, &token, mod_path).await;

    // Upstream moves v1.0.0 to another commit, with a changed go.mod.
    up.change(
        &tool,
        &[
            ("go.mod", b"module github.com/acme/tool\n\ngo 1.22\n"),
            ("evil.go", b"package tool\n"),
        ],
        "moved",
    );
    ok(client_git(&["tag", "-f", "v1.0.0"], &tool));
    ok(client_git(&["push", "-q", "-f", "origin", "v1.0.0"], &tool));

    // What is cached keeps being served.
    assert_eq!(get(&h, &token, zip_path).await.1, original);
    assert_eq!(get(&h, &token, mod_path).await.1, original_mod);
    assert!(integrity_entries(&h).await.is_empty());

    // Once the cached copy is gone, a rebuild gives a different hash: it
    // is refused, counted, and written to the audit log with both hashes.
    let (status, _, _) = h
        .send(
            "DELETE",
            "/api/component?repo=go&name=github.com%2Facme%2Ftool&private=true",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let failures = || {
        h.state
            .store
            .stats
            .integrity_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before = failures();
    let (status, body) = get(&h, &token, zip_path).await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(String::from_utf8_lossy(&body).contains("go-unpin"));
    assert_eq!(get(&h, &token, mod_path).await.0, StatusCode::BAD_GATEWAY);
    assert_eq!(failures(), before + 2);
    assert!(
        !h.data
            .join("files/go-private/git/github.com/acme/tool/@v/v1.0.0")
            .exists()
    );
    let log = integrity_entries(&h).await;
    assert_eq!(log.len(), 2, "{log:?}");
    let dir = TempDir::new("pin");
    let pinned = h1_of_zip(&dir, "github.com/acme/tool", "v1.0.0", &original);
    assert!(
        log[0].starts_with("github.com/acme/tool@v1.0.0 zip: pinned "),
        "{}",
        log[0]
    );
    assert!(log[0].contains(&pinned), "{}", log[0]);
    assert_eq!(log[0].matches("h1:").count(), 2, "{}", log[0]);
    assert!(log[1].contains("go.mod") && log[1].contains(&gomod::go_mod_hash(&original_mod)));

    // Other versions are unaffected, and unpinning accepts the new one.
    assert_eq!(
        get(&h, &token, "/go/github.com/acme/tool/@v/v1.1.0-rc.1.zip")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        crate::goprivate::unpin(&h.state.auth.db, "github.com/acme/tool", "v1.0.0")
            .await
            .unwrap(),
        2
    );
    let (status, moved) = get(&h, &token, zip_path).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(moved, original);
}

/// A module in Gitea's Go registry for `acme`.
fn publish_go(g: &MockGitea, module: &str, version: &str, body: &str) -> Vec<u8> {
    let zip = {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (name, data) in [
            ("go.mod", format!("module {module}\n")),
            ("lib.go", body.to_string()),
        ] {
            z.start_file(format!("{module}@{version}/{name}"), opts)
                .unwrap();
            z.write_all(data.as_bytes()).unwrap();
        }
        z.finish().unwrap();
        buf.into_inner()
    };
    let base = format!("/api/packages/acme/go/{module}/@v");
    let mut p = g.packages.lock().unwrap();
    p.insert(format!("{base}/list"), format!("{version}\n").into_bytes());
    p.insert(format!("{base}/{version}.zip"), zip.clone());
    p.insert(
        format!("{base}/{version}.mod"),
        format!("module {module}\n").into_bytes(),
    );
    p.insert(
        format!("{base}/{version}.info"),
        format!("{{\"Version\":\"{version}\"}}").into_bytes(),
    );
    zip
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn giteas_go_registry_is_a_source_and_its_versions_are_pinned() {
    let g = MockGitea::start().await;
    let module = "corp.example/lib";
    let zip = publish_go(&g, module, "v1.0.0", "package lib\n");
    let h = Harness::with(Opts {
        gitea: Some(g.gitea()),
        extra_ca: Some(g.ca.clone()),
        go_private: Some("corp.example=gitea:acme".into()),
        meta_ttl: Duration::ZERO,
        ..Opts::default()
    })
    .await;
    h.mock.set(
        "/goproxy/corp.example/lib/@v/list",
        MockFile::ok("v9.9.9\n"),
    );
    let token = private_token(&h).await;
    let admin = h.login().await;
    let base = "/go/corp.example/lib/@v";

    let (status, list) = get(&h, &token, &format!("{base}/list")).await;
    assert_eq!(
        (status, list.as_slice()),
        (StatusCode::OK, b"v1.0.0\n".as_slice())
    );
    assert_eq!(
        get(&h, &token, &format!("{base}/v1.0.0.info")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get(&h, &token, &format!("{base}/v1.0.0.mod")).await.0,
        StatusCode::OK
    );
    let (status, body) = get(&h, &token, &format!("{base}/v1.0.0.zip")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(body, zip);
    assert!(
        h.data
            .join("files/go-private/gitea-acme/corp.example/lib/@v/v1.0.0")
            .exists()
    );
    // Gitea's token went to Gitea only; the public proxy was never asked.
    assert!(g.seen().iter().all(|a| a == &format!("token {TOKEN}")));
    assert_eq!(h.mock.total(), 0);
    assert!(!h.mock.saw_auth.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        h.get(&format!("{base}/list")).await.0,
        StatusCode::UNAUTHORIZED
    );

    // Gitea starts serving different bytes for v1.0.0. The cached copy is
    // still served; once it is gone, the new one is refused and logged.
    publish_go(&g, module, "v1.0.0", "package lib // changed\n");
    assert_eq!(get(&h, &token, &format!("{base}/v1.0.0.zip")).await.1, zip);
    let (status, _, _) = h
        .send(
            "DELETE",
            "/api/component?repo=go&name=corp.example%2Flib&private=true",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = get(&h, &token, &format!("{base}/v1.0.0.zip")).await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        !h.data
            .join("files/go-private/gitea-acme/corp.example/lib/@v/v1.0.0")
            .exists()
    );
    let log = integrity_entries(&h).await;
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(
        log[0].starts_with("corp.example/lib@v1.0.0 zip: pinned h1:"),
        "{}",
        log[0]
    );
    assert!(log[0].contains("gitea:acme now gives h1:"), "{}", log[0]);
    // The go.mod didn't change, so it still matches its pin.
    assert_eq!(
        get(&h, &token, &format!("{base}/v1.0.0.mod")).await.0,
        StatusCode::OK
    );
    // A new version is fine.
    publish_go(&g, module, "v1.1.0", "package lib // next\n");
    assert_eq!(
        get(&h, &token, &format!("{base}/v1.1.0.zip")).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn prefixes_are_edited_on_the_system_page_unless_the_environment_sets_them() {
    let h = Harness::with(Opts::default()).await;
    let admin = h.login().await;
    let put = |rules: Value| {
        let (h, admin) = (&h, admin.clone());
        async move {
            h.send(
                "PUT",
                "/api/system/go-private",
                Some(&admin),
                Some(json!({ "rules": rules })),
            )
            .await
            .0
        }
    };
    assert_eq!(
        put(json!([
            {"pattern": "gitea.example.lan/", "source": ""},
            {"pattern": "go.example.com", "source": "git:gitea.example.lan/acme"},
        ]))
        .await,
        StatusCode::OK
    );
    let sys = h.api(&admin, "/api/system").await;
    assert_eq!(
        sys["go_private"]["rules"][0],
        json!({"pattern": "gitea.example.lan", "source": "git"})
    );
    assert_eq!(sys["go_private"]["locked"], false);
    assert!(h.state.goprivate.route("go.example.com/tool").is_some());
    assert!(h.state.goprivate.route("golang.org/x/text").is_none());
    for bad in [
        json!([{"pattern": "a b", "source": ""}]),
        json!([{"pattern": "x.example", "source": "svn"}]),
        json!([{"pattern": "x.example", "source": ""}, {"pattern": "x.example", "source": ""}]),
    ] {
        assert_eq!(put(bad).await, StatusCode::BAD_REQUEST);
    }
    // Saved across a restart.
    let again = Harness::with(Opts {
        data: Some(h.data.clone()),
        ..Opts::default()
    })
    .await;
    assert_eq!(
        again.state.goprivate.patterns(),
        vec!["gitea.example.lan", "go.example.com"]
    );

    // Set by the environment: locked.
    let locked = Harness::with(Opts {
        go_private: Some("corp.example".into()),
        ..Opts::default()
    })
    .await;
    let cookie = locked.login().await;
    let (status, _, _) = locked
        .send(
            "PUT",
            "/api/system/go-private",
            Some(&cookie),
            Some(json!({"rules": []})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        locked.api(&cookie, "/api/system").await["go_private"]["locked"],
        true
    );
}

/// A real `go build` of a private module through Granary over HTTPS, with
/// the token in netrc, `GONOSUMDB` set and `GOPRIVATE` unset: the client
/// setup the README gives. Skipped without git or go, and on macOS, where
/// `go` only trusts the system's certificate store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_go_builds_a_private_module_over_https_with_a_netrc_token() {
    if !have_git() || !have_go() || cfg!(target_os = "macos") {
        return eprintln!("skipping: needs git and go, on a system where go honors SSL_CERT_FILE");
    }
    let up = Upstream::start().await;
    fixtures(&up);
    let h = granary(
        &up,
        Opts {
            client_auth: true,
            ..Opts::default()
        },
    )
    .await;
    let token = private_token(&h).await;

    // Granary behind TLS, with a certificate from a test CA.
    let (ca_pem, tls) = super::gitea::test_tls();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let app = h.app.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                continue;
            };
            let (acceptor, app) = (acceptor.clone(), app.clone());
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let svc = hyper_util::service::TowerToHyperService::new(app);
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                .await;
            });
        }
    });

    let dir = TempDir::new("go-https");
    std::fs::write(dir.0.join("ca.pem"), ca_pem).unwrap();
    std::fs::write(
        dir.0.join("go.mod"),
        "module example.com/app\n\ngo 1.21\n\nrequire github.com/acme/tool v1.0.0\n",
    )
    .unwrap();
    std::fs::write(
        dir.0.join("main.go"),
        "package main\n\nimport _ \"github.com/acme/tool\"\n\nfunc main() {}\n",
    )
    .unwrap();
    let build = |netrc: String| {
        let cwd = dir.0.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::write(cwd.join("netrc"), netrc).unwrap();
            let _ = std::fs::remove_dir_all(cwd.join("modcache"));
            std::process::Command::new("go")
                .args(["build", "-mod=mod", "-o", "app", "."])
                .current_dir(&cwd)
                .env("GOENV", "off")
                .env("GOPROXY", format!("https://localhost:{port}/go"))
                .env("GONOPROXY", "")
                .env("GONOSUMDB", PREFIX)
                .env_remove("GOPRIVATE")
                .env("NETRC", cwd.join("netrc"))
                .env("SSL_CERT_FILE", cwd.join("ca.pem"))
                .env("GOFLAGS", "-modcacherw")
                .env("GOMODCACHE", cwd.join("modcache"))
                .env("GOPATH", cwd.join("gopath"))
                .env("GOCACHE", cwd.join("gocache"))
                .env("GOTOOLCHAIN", "local")
                .env("HOME", &cwd)
                .output()
                .unwrap()
        })
    };
    // Without a token, Go is turned away.
    let out = build(String::new()).await.unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("401"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // With the token in netrc, the private module builds. Go 1.24 and
    // later match the host with its port; earlier versions, without.
    let out = build(format!(
        "machine localhost:{port} login token password {token}\nmachine localhost login token password {token}\n"
    ))
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let sum = std::fs::read_to_string(dir.0.join("go.sum")).unwrap();
    assert!(sum.contains("github.com/acme/tool v1.0.0 h1:"), "{sum}");
    assert_eq!(
        h.mock.total(),
        0,
        "nothing private reached the public proxy"
    );
}

/// Get packages on a private path: someone who may not read it finds
/// nothing, and nothing is fetched from git, Gitea or the public proxy;
/// with access it is found and cached from its own source.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_packages_keeps_private_modules_private() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    fixtures(&up);
    let h = granary(&up, Opts::default()).await;
    h.mock.set(
        "/goproxy/github.com/acme/tool/@v/list",
        MockFile::ok("v9.9.9\n"),
    );
    let st = &h.state;
    let viewer = crate::policy::Principal::User {
        user: crate::auth::User::for_tests(crate::auth::Role::Viewer, false),
        token: None,
    };
    for who in [&crate::policy::Principal::Anonymous, &viewer] {
        assert!(
            crate::search::lookup_go(st, who, "github.com/acme/tool")
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(!h.data.join("git/github.com/acme").exists(), "no clone");
    assert_eq!(h.mock.total(), 0);

    let admin = h.login().await;
    let d = h
        .api(
            &admin,
            "/api/packages/search?eco=go&q=github.com%2Facme%2Ftool",
        )
        .await;
    assert_eq!(d["results"][0]["name"], "github.com/acme/tool");
    assert_eq!(d["results"][0]["private"], true);
    assert_eq!(d["results"][0]["latest"], "v1.0.0");
    let (status, _, body) = h
        .send(
            "POST",
            "/api/packages/cache",
            Some(&admin),
            Some(json!({"eco": "go", "name": "github.com/acme/tool", "version": "v1.0.0"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(
        h.data
            .join("files/go-private/git/github.com/acme/tool/@v/v1.0.0")
            .exists()
    );
    // The public proxy's look-alike was never asked.
    assert_eq!(h.mock.total(), 0);

    // Cache on a private path someone may not read answers exactly like
    // an unknown public path, so the prefixes stay hidden.
    let answer = |module: &'static str| {
        let viewer = viewer.clone();
        async move {
            let e = crate::search::cache_one(
                st,
                &viewer,
                crate::templates::lines::Eco::Go,
                module,
                "v1.0.0",
                None,
            )
            .await
            .unwrap_err();
            (e.status, e.message.replace(module, "MODULE"))
        }
    };
    let private = answer("github.com/acme/tool").await;
    let public = answer("github.com/other/nope").await;
    assert_eq!(private, public);
    assert_eq!(
        private,
        (StatusCode::NOT_FOUND, "MODULE has no version v1.0.0".into())
    );
}
