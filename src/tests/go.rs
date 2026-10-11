//! The Go module proxy against a mock proxy, checksum database and
//! vulnerability database. The checksum database is a real transparency
//! log signed with a test key (see `sumdb::testlog`), so verification runs
//! exactly as it does against sum.golang.org.

use std::{io::Write, sync::atomic::Ordering};

use axum::http::StatusCode;
use serde_json::Value;

use super::{Harness, Opts, Upstream};
use crate::{
    gomod,
    sumdb::{Tile, testlog::Log},
};

/// A module zip for `module@version` with these files.
fn module_zip(module: &str, version: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (name, body) in files {
            zip.start_file(format!("{module}@{version}/{name}"), opts)
                .unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    buf.into_inner()
}

fn zip_sum(module: &str, version: &str, zip: &[u8]) -> String {
    let dir = super::git::TempDir::new("gozip");
    let path = dir.0.join("m.zip");
    std::fs::write(&path, zip).unwrap();
    gomod::zip_hash(&path, module, version).unwrap()
}

/// A mock Go world: proxy files, a signed log, the tiles it needs.
struct World {
    h: Harness,
    log: Log,
}

impl World {
    async fn new(opts: Opts) -> Self {
        let log = Log::new("test.example");
        let h = Harness::with(Opts {
            go_sumdb_key: Some(log.vkey.clone()),
            ..opts
        })
        .await;
        Self { h, log }
    }

    /// Publish a version on the mock proxy; `record` adds it to the log.
    fn publish(
        &mut self,
        module: &str,
        version: &str,
        files: &[(&str, &str)],
        record: bool,
    ) -> Vec<u8> {
        let em = gomod::escape(module);
        let ev = gomod::escape(version);
        let zip = module_zip(module, version, files);
        let go_mod = files
            .iter()
            .find(|(n, _)| *n == "go.mod")
            .map_or(format!("module {module}\n"), |(_, b)| b.to_string());
        let m = &self.h.mock;
        m.set(
            &format!("/goproxy/{em}/@v/{ev}.zip"),
            Upstream::ok(zip.clone()),
        );
        m.set(
            &format!("/goproxy/{em}/@v/{ev}.mod"),
            Upstream::ok(go_mod.clone()),
        );
        m.set(
            &format!("/goproxy/{em}/@v/{ev}.info"),
            Upstream::ok(format!(
                "{{\"Version\":\"{version}\",\"Time\":\"2026-01-02T03:04:05Z\"}}"
            )),
        );
        m.set(
            &format!("/goproxy/{em}/@v/list"),
            Upstream::ok(format!("{version}\n")),
        );
        m.set(
            &format!("/goproxy/{em}/@latest"),
            Upstream::ok(format!("{{\"Version\":\"{version}\"}}")),
        );
        if record {
            let id = self.log.add(&format!(
                "{module} {version} {}\n{module} {version}/go.mod {}\n",
                zip_sum(module, version, &zip),
                gomod::go_mod_hash(go_mod.as_bytes())
            ));
            self.serve_log();
            m.set(
                &format!("/sumdb/lookup/{em}@{ev}"),
                Upstream::ok(self.log.lookup(id)),
            );
        }
        zip
    }

    /// Serve the log's latest note and every tile of its current size.
    fn serve_log(&self) {
        let m = &self.h.mock;
        m.set("/sumdb/latest", Upstream::ok(self.log.signed_tree()));
        let n = self.log.records.len() as i64;
        // Small logs: one partial tile per level that has hashes.
        let mut width = n;
        let mut level = 0;
        while width > 0 {
            let t = Tile {
                h: 8,
                l: level,
                n: 0,
                w: width.min(256),
            };
            m.set(
                &format!("/sumdb/{}", t.path()),
                Upstream::ok(self.log.tile(&t)),
            );
            width >>= 8;
            level += 1;
        }
    }
}

const HELLO: &str = "example.com/Hello";
const HELLO_FILES: &[(&str, &str)] = &[
    ("go.mod", "module example.com/Hello\n\ngo 1.22\n"),
    (
        "hello.go",
        "package hello\n\nfunc Hello() string { return \"hi\" }\n",
    ),
];

#[tokio::test]
async fn serves_and_verifies_modules() {
    let mut w = World::new(Opts::default()).await;
    let zip = w.publish(HELLO, "v1.0.0", HELLO_FILES, true);
    let h = &w.h;
    let base = "/go/example.com/!hello/@v";

    let (status, _, body) = h.get(&format!("{base}/list")).await;
    assert_eq!(
        (status, body.as_slice()),
        (StatusCode::OK, b"v1.0.0\n".as_slice())
    );
    let (status, _, body) = h.get("/go/example.com/!hello/@latest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["Version"],
        "v1.0.0"
    );
    let (status, _, _) = h.get(&format!("{base}/v1.0.0.info")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, body) = h.get(&format!("{base}/v1.0.0.mod")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    assert_eq!(body, HELLO_FILES[0].1.as_bytes());
    let (status, headers, body) = h.get(&format!("{base}/v1.0.0.zip")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers["content-type"], "application/zip");
    assert_eq!(body, zip);

    // Cached, case-encoded, with the record and its tile; served again
    // without asking upstream.
    let files = h.data.join("files");
    assert!(files.join("go/example.com/!hello/@v/v1.0.0").exists());
    assert!(files.join("go/example.com/!hello/@v/v1.0.0.mod").exists());
    assert!(
        files
            .join("go-sumdb/test.example/lookup/example.com/!hello@v1.0.0")
            .exists()
    );
    assert!(
        files
            .join("go-sumdb/test.example/tile/8/0/000.p/1")
            .exists()
    );
    let asked = h.mock.total();
    for p in ["v1.0.0.zip", "v1.0.0.mod", "v1.0.0.info"] {
        assert_eq!(h.get(&format!("{base}/{p}")).await.0, StatusCode::OK);
    }
    assert_eq!(h.mock.total(), asked);

    // Browse lists it by its real name.
    let cookie = h.login().await;
    let c = h.api(&cookie, "/api/components?repo=go").await;
    assert_eq!(c["items"][0]["name"], HELLO);
    assert_eq!(c["items"][0]["versions"], 1);
    let detail = h
        .api(&cookie, "/api/component?repo=go&name=example.com%2FHello")
        .await;
    assert!(
        detail["versions"][0]["files"][0]["download"]
            .as_str()
            .unwrap()
            .ends_with("/go/example.com/!hello/@v/v1.0.0.zip")
    );
}

#[tokio::test]
async fn refuses_bad_paths_and_maps_gone_to_not_found() {
    let mut w = World::new(Opts::default()).await;
    w.publish(HELLO, "v1.0.0", HELLO_FILES, true);
    let h = &w.h;
    for bad in [
        "/go/example.com/Hello/@v/list",   // uppercase must be encoded
        "/go/example.com/!!hello/@v/list", // bad encoding
        "/go/example.com/!1hello/@v/list",
        "/go/golang/@v/list",                   // no dot in the first element
        "/go/example.com/m/v1/@v/list",         // /v1 isn't a suffix
        "/go/example.com/!hello/@v/v2.0.0.zip", // wrong major version
        "/go/example.com/!hello/@v/v1.0.0.tar",
        "/go/example.com/!hello/@v/../list",
        "/go/example.com/!hello",
    ] {
        let (status, headers, _) = h.get(bad).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
        assert!(
            headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain"),
            "{bad}"
        );
    }
    // 410 Gone upstream is a 404 here, so `go` falls back as it would.
    h.mock.set(
        "/goproxy/example.com/gone/@v/list",
        Upstream {
            status: StatusCode::GONE,
            body: b"gone".to_vec(),
            etag: None,
        },
    );
    assert_eq!(
        h.get("/go/example.com/gone/@v/list").await.0,
        StatusCode::NOT_FOUND
    );
    // Queries (branches) are passed on and cached as metadata.
    h.mock.set(
        "/goproxy/example.com/!hello/@v/main.info",
        Upstream::ok(r#"{"Version":"v1.0.1-0.20260102030405-abcdefabcdef"}"#),
    );
    assert_eq!(
        h.get("/go/example.com/!hello/@v/main.info").await.0,
        StatusCode::OK
    );
    let (status, _, _) = h
        .request("PUT", "/go/example.com/!hello/@v/list", None)
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn refuses_what_the_checksum_database_does_not_vouch_for() {
    let mut w = World::new(Opts::default()).await;
    w.publish(HELLO, "v1.0.0", HELLO_FILES, true);
    // Upstream serves different bytes than the log records.
    w.h.mock.set(
        "/goproxy/example.com/!hello/@v/v1.0.0.zip",
        Upstream::ok(module_zip(
            HELLO,
            "v1.0.0",
            &[("hello.go", "package evil\n")],
        )),
    );
    w.h.mock.set(
        "/goproxy/example.com/!hello/@v/v1.0.0.mod",
        Upstream::ok("module example.com/Hello\n\nrequire evil.example v1.0.0\n"),
    );
    // And a version the log has never heard of.
    w.publish(
        "example.com/norecord",
        "v1.0.0",
        &[("a.go", "package a\n")],
        false,
    );
    let h = &w.h;
    let failures = || {
        h.state
            .store
            .stats
            .integrity_failures
            .load(Ordering::Relaxed)
    };
    let before = failures();
    for p in [
        "/go/example.com/!hello/@v/v1.0.0.zip",
        "/go/example.com/!hello/@v/v1.0.0.mod",
        "/go/example.com/norecord/@v/v1.0.0.zip",
        "/go/example.com/norecord/@v/v1.0.0.mod",
    ] {
        let (status, _, body) = h.get(p).await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "{p}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(failures(), before + 4);
    assert!(
        !h.data
            .join("files/go/example.com/!hello/@v/v1.0.0")
            .exists()
    );
    assert!(
        !h.data
            .join("files/go/example.com/norecord/@v/v1.0.0")
            .exists()
    );
    let (_, _, body) = h.get("/go/example.com/norecord/@v/v1.0.0.zip").await;
    assert!(String::from_utf8_lossy(&body).contains("GRANARY_GO_ALLOW_UNVERIFIED"));

    // A forged record with the genuine signed tree is refused too.
    let mut forged = String::from_utf8(w.log.lookup(0)).unwrap();
    forged = forged.replace("h1:", "h1:X");
    h.mock.set(
        "/sumdb/lookup/example.com/!hello@v1.0.1",
        Upstream::ok(forged),
    );
    w.publish(HELLO, "v1.0.1", HELLO_FILES, false);
    let h = &w.h;
    let (status, _, body) = h.get("/go/example.com/!hello/@v/v1.0.1.zip").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("could not be verified"));
    let (status, _, _) = h
        .get("/go/sumdb/test.example/lookup/example.com/!hello@v1.0.1")
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    // With GRANARY_GO_ALLOW_UNVERIFIED on, a module without a record is
    // served (still checked to be a well-formed zip of that module).
    let mut open = World::new(Opts {
        go_allow_unverified: true,
        ..Opts::default()
    })
    .await;
    open.publish(
        "example.com/norecord",
        "v1.0.0",
        &[("a.go", "package a\n")],
        false,
    );
    let (status, _, _) = open.h.get("/go/example.com/norecord/@v/v1.0.0.zip").await;
    assert_eq!(status, StatusCode::OK);
    // A changed one is still refused: there is a record to compare with.
    open.publish(HELLO, "v1.0.0", HELLO_FILES, true);
    open.h.mock.set(
        "/goproxy/example.com/!hello/@v/v1.0.0.zip",
        Upstream::ok(module_zip(
            HELLO,
            "v1.0.0",
            &[("hello.go", "package evil\n")],
        )),
    );
    let (status, _, _) = open.h.get("/go/example.com/!hello/@v/v1.0.0.zip").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn serves_the_checksum_database() {
    let mut w = World::new(Opts::default()).await;
    w.publish(HELLO, "v1.0.0", HELLO_FILES, true);
    w.publish(
        "example.com/two",
        "v1.2.3",
        &[("a.go", "package a\n")],
        true,
    );
    let h = &w.h;
    let (status, _, body) = h.get("/go/sumdb/test.example/supported").await;
    assert_eq!((status, body.len()), (StatusCode::OK, 0));
    // Any other database: 404, so `go` handles it as it would otherwise.
    assert_eq!(
        h.get("/go/sumdb/sum.golang.org/supported").await.0,
        StatusCode::NOT_FOUND
    );
    let (status, _, body) = h.get("/go/sumdb/test.example/latest").await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).starts_with("go.sum database tree\n2\n"));
    let (status, _, body) = h
        .get("/go/sumdb/test.example/lookup/example.com/two@v1.2.3")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, w.log.lookup(1));
    let (status, _, body) = h.get("/go/sumdb/test.example/tile/8/0/000.p/2").await;
    assert_eq!((status, body.len()), (StatusCode::OK, 64));
    for bad in [
        "/go/sumdb/test.example/tile/8/0/1",
        "/go/sumdb/test.example/lookup/example.com/two",
        "/go/sumdb/test.example/other",
    ] {
        assert_eq!(h.get(bad).await.0, StatusCode::NOT_FOUND, "{bad}");
    }
    assert_eq!(
        h.get("/go/sumdb/test.example/lookup/example.com/nothere@v1.0.0")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn mirrors_the_vulnerability_database_and_works_offline() {
    let mut w = World::new(Opts::default()).await;
    w.publish(HELLO, "v1.0.0", HELLO_FILES, true);
    let h = &w.h;
    h.mock.set(
        "/vuln/index/db.json",
        Upstream::ok(r#"{"modified":"2026-01-01T00:00:00Z"}"#),
    );
    h.mock.set(
        "/vuln/ID/GO-2099-0001.json",
        Upstream::ok(r#"{"id":"GO-2099-0001"}"#),
    );
    assert_eq!(h.get("/go/vuln/index/db.json").await.0, StatusCode::OK);
    assert_eq!(
        h.get("/go/vuln/ID/GO-2099-0001.json").await.0,
        StatusCode::OK
    );
    for bad in [
        "/go/vuln/index/other.json",
        "/go/vuln/ID/x/y.json",
        "/go/vuln/vulndb.zip",
    ] {
        assert_eq!(h.get(bad).await.0, StatusCode::NOT_FOUND, "{bad}");
    }

    // The daily task stores the whole database, plain and gzipped.
    let mut zip = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut zip);
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (name, body) in [
            ("index/db.json", r#"{"modified":"2026-02-02T00:00:00Z"}"#),
            ("index/modules.json", "[]"),
            ("index/vulns.json", "[]"),
            ("ID/GO-2099-0002.json", r#"{"id":"GO-2099-0002"}"#),
        ] {
            z.start_file(name, opts).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }
    h.mock
        .set("/vuln/vulndb.zip", Upstream::ok(zip.into_inner()));
    let cookie = h.login().await;
    let (status, _, _) = h
        .send(
            "PUT",
            "/api/tasks/go-vulndb/enabled",
            Some(&cookie),
            Some(serde_json::json!({"enabled": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(crate::admin::run_task(&h.state, "go-vulndb").await);
    let tasks = h.api(&cookie, "/api/tasks").await;
    let t = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "go-vulndb")
        .unwrap()
        .clone();
    assert_eq!(t["last_ok"], true, "{t}");
    assert_eq!(t["enabled"], true);

    // Cache the module, then go offline on the same data.
    for p in ["v1.0.0.zip", "v1.0.0.mod", "v1.0.0.info", "list"] {
        assert_eq!(
            h.get(&format!("/go/example.com/!hello/@v/{p}")).await.0,
            StatusCode::OK
        );
    }
    h.get("/go/sumdb/test.example/latest").await;
    let offline = Harness::with(Opts {
        offline: true,
        data: Some(h.data.clone()),
        go_sumdb_key: Some(w.log.vkey.clone()),
        ..Opts::default()
    })
    .await;
    let before = offline.mock.total();
    for p in [
        "/go/example.com/!hello/@v/v1.0.0.zip",
        "/go/example.com/!hello/@v/v1.0.0.mod",
        "/go/example.com/!hello/@v/v1.0.0.info",
        "/go/example.com/!hello/@v/list",
        "/go/sumdb/test.example/latest",
        "/go/sumdb/test.example/lookup/example.com/!hello@v1.0.0",
        "/go/sumdb/test.example/tile/8/0/000.p/1",
        "/go/vuln/ID/GO-2099-0002.json",
        "/go/vuln/ID/GO-2099-0002.json.gz",
        "/go/vuln/index/modules.json.gz",
    ] {
        let (status, _, body) = offline.get(p).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{p}: {}",
            String::from_utf8_lossy(&body)
        );
        if p.ends_with(".gz") {
            let mut out = String::new();
            std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&body[..]), &mut out)
                .unwrap();
            assert!(out.starts_with('{') || out.starts_with('['), "{p}: {out}");
        }
    }
    assert_eq!(offline.mock.total(), before);
    let (status, _, body) = offline.get("/go/example.com/other/@v/v1.0.0.zip").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("offline"));
}

/// A real `go` against Granary, with the real proxy.golang.org,
/// sum.golang.org and vuln.go.dev behind it. `go` may only reach the
/// checksum database through Granary: HTTPS_PROXY points nowhere. Skipped
/// without `go` or network access.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_go_builds_through_granary() {
    let have_go = std::process::Command::new("go")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success());
    let online = reqwest::get("https://proxy.golang.org/rsc.io/quote/@v/list")
        .await
        .is_ok_and(|r| r.status().is_success());
    if !have_go || !online {
        return eprintln!("skipping: go and network access are needed");
    }
    let h = Harness::with(Opts {
        serve: true,
        go_real: true,
        ..Opts::default()
    })
    .await;
    let dir = super::git::TempDir::new("go-client");
    std::fs::write(
        dir.0.join("go.mod"),
        "module example.com/app\n\ngo 1.21\n\nrequire rsc.io/quote v1.5.2\n",
    )
    .unwrap();
    std::fs::write(
        dir.0.join("main.go"),
        "package main\n\nimport (\n\t\"fmt\"\n\n\t\"rsc.io/quote\"\n)\n\nfunc main() { fmt.Println(quote.Hello()) }\n",
    )
    .unwrap();
    let url = format!("{}/go", h.url.as_ref().unwrap());
    let cwd = dir.0.clone();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("go")
            .args(["build", "-mod=mod", "-o", "app", "."])
            .current_dir(&cwd)
            .env("GOENV", "off")
            .env("GOPROXY", &url)
            .env("GONOPROXY", "")
            .env_remove("GOPRIVATE")
            .env_remove("GONOSUMDB")
            .env_remove("GOSUMDB")
            .env("GOFLAGS", "-modcacherw")
            .env("GOMODCACHE", cwd.join("modcache"))
            .env("GOPATH", cwd.join("gopath"))
            .env("GOCACHE", cwd.join("gocache"))
            .env("GOTOOLCHAIN", "local")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let sum = std::fs::read_to_string(dir.0.join("go.sum")).unwrap();
    assert!(
        sum.contains("rsc.io/quote v1.5.2 h1:w5fcysjrx7yqtD/aO+QwRjYZOKnaM9Uh2b40tElTs3Y="),
        "{sum}"
    );
    // Every module came through Granary, verified, and `go` checked them
    // against the checksum database through it too.
    let files = h.data.join("files");
    assert!(files.join("go/rsc.io/quote/@v/v1.5.2").exists());
    assert!(
        files
            .join("go-sumdb/sum.golang.org/lookup/rsc.io/quote@v1.5.2")
            .exists()
    );
    assert!(
        dir.0
            .join("modcache/cache/download/sumdb/sum.golang.org")
            .exists()
    );
    let vuln = h.get("/go/vuln/index/db.json.gz").await;
    assert_eq!(vuln.0, StatusCode::OK);
}
