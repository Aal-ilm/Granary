//! Warm jobs: the queue's concurrency limit, cancel, timeout and the cache
//! budget stop, against a fake warmer; then a real warmer.py run.

use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde_json::{Value, json};
use sha1::Digest;

use super::{Harness, Opts, Upstream, WARMER_SECRET};

/// What the fake warmer does for each run.
#[derive(Clone, Default)]
pub(super) struct Plan {
    /// Granary paths to download (with the job's token) before streaming.
    pub fetch: Vec<String>,
    pub lines: usize,
    pub delay: Duration,
    pub lockfiles: HashMap<String, String>,
}

#[derive(Clone, Default)]
pub(super) struct Fake {
    plan: Arc<Mutex<Plan>>,
    pub runs: Arc<Mutex<Vec<Value>>>,
    deletes: Arc<Mutex<Vec<i64>>>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

/// Counts a run as active until its response stream is dropped.
struct Active(Arc<AtomicUsize>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn authorized(h: &HeaderMap) -> bool {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {WARMER_SECRET}"))
}

async fn fake_run(State(f): State<Fake>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap();
    f.runs.lock().unwrap().push(req.clone());
    let now = f.active.fetch_add(1, Ordering::SeqCst) + 1;
    f.max_active.fetch_max(now, Ordering::SeqCst);
    let guard = Active(f.active.clone());
    let plan = f.plan.lock().unwrap().clone();

    let mut out: Vec<String> = Vec::new();
    let client = reqwest::Client::new();
    for path in &plan.fetch {
        let url = format!("{}{path}", req["granary_url"].as_str().unwrap());
        let status = client
            .get(&url)
            .basic_auth("token", req["token"].as_str())
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0);
        out.push(format!("fetched {path}: {status}"));
    }
    let result = format!(
        "##granary {}",
        json!({"ok": true, "lockfiles": plan.lockfiles})
    );
    let stream = futures_util::stream::unfold(
        (0usize, guard, out, plan, result),
        |(i, guard, out, plan, result)| async move {
            let line = if i < out.len() {
                out[i].clone()
            } else if i - out.len() < plan.lines {
                tokio::time::sleep(plan.delay).await;
                format!("working {}", i - out.len())
            } else if i - out.len() == plan.lines {
                result.clone()
            } else {
                return None;
            };
            Some((
                Ok::<_, Infallible>(Bytes::from(format!("{line}\n"))),
                (i + 1, guard, out, plan, result),
            ))
        },
    );
    Response::new(Body::from_stream(stream))
}

async fn fake_delete(State(f): State<Fake>, headers: HeaderMap, Path(id): Path<i64>) -> StatusCode {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED;
    }
    f.deletes.lock().unwrap().push(id);
    StatusCode::OK
}

async fn fake_health(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    json!({"ok": true, "tools": {"npm": "fake"}})
        .to_string()
        .into_response()
}

async fn fake_warmer(plan: Plan) -> (Fake, String) {
    let fake = Fake::default();
    *fake.plan.lock().unwrap() = plan;
    let app = Router::new()
        .route("/run", post(fake_run))
        .route("/run/{id}", delete(fake_delete))
        .route("/health", get(fake_health))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, url)
}

pub(super) async fn with_fake(plan: Plan, opts: Opts) -> (Harness, Fake, String) {
    let (fake, url) = fake_warmer(plan).await;
    let h = Harness::with(Opts {
        warmer_url: Some(url),
        serve: true,
        ..opts
    })
    .await;
    let cookie = h.login().await;
    (h, fake, cookie)
}

async fn warm_package(h: &Harness, cookie: &str, name: &str) -> i64 {
    let (status, _, body) = h
        .send(
            "POST",
            "/api/packages/warm",
            Some(cookie),
            Some(json!({"eco": "npm", "name": name, "version": "1.0.0"})),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// Wait until the job reaches one of these states; returns the job.
pub(super) async fn wait_for(h: &Harness, cookie: &str, id: i64, states: &[&str]) -> Value {
    let start = Instant::now();
    loop {
        let d = h.api(cookie, &format!("/api/jobs/{id}")).await;
        if states.contains(&d["job"]["status"].as_str().unwrap()) {
            return d;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "job {id} stuck: {d}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn runs_at_most_the_configured_number_at_once() {
    let plan = Plan {
        lines: 6,
        delay: Duration::from_millis(100),
        ..Plan::default()
    };
    let (h, fake, cookie) = with_fake(
        plan,
        Opts {
            warm_jobs: 2,
            ..Opts::default()
        },
    )
    .await;
    let ids: Vec<i64> = futures_util::future::join_all(
        ["a", "b", "c", "d", "e"].map(|n| warm_package(&h, &cookie, n)),
    )
    .await;
    let mut most_running = 0;
    let start = Instant::now();
    loop {
        let jobs = h.api(&cookie, "/api/jobs").await;
        let list = jobs["jobs"].as_array().unwrap();
        let running = list.iter().filter(|j| j["status"] == "running").count();
        most_running = most_running.max(running);
        if list.iter().all(|j| j["status"] == "done") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "jobs stuck: {jobs}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(most_running, 2, "two jobs should run side by side");
    assert_eq!(fake.max_active.load(Ordering::SeqCst), 2);
    assert_eq!(fake.runs.lock().unwrap().len(), ids.len());

    // The warmer got a pinned one-package manifest and a working token.
    let run = fake
        .runs
        .lock()
        .unwrap()
        .iter()
        .find(|r| {
            r["files"]["package.json"]
                .as_str()
                .unwrap()
                .contains("\"a\": \"1.0.0\"")
        })
        .cloned()
        .expect("a run for package a");
    assert_eq!(run["ecosystem"], "npm");
    assert_eq!(run["platforms"], json!(["linux-x86_64", "macos-arm64"]));
    assert!(run["token"].as_str().unwrap().starts_with("gran_"));
    let d = h.api(&cookie, &format!("/api/jobs/{}", ids[0])).await;
    let log: Vec<&str> = d["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["line"].as_str().unwrap())
        .collect();
    assert!(log.contains(&"working 5"), "{log:?}");
}

#[tokio::test]
async fn cancels_queued_and_running_jobs() {
    let plan = Plan {
        lines: 200,
        delay: Duration::from_millis(50),
        ..Plan::default()
    };
    let (h, fake, cookie) = with_fake(
        plan,
        Opts {
            warm_jobs: 1,
            ..Opts::default()
        },
    )
    .await;
    let first = warm_package(&h, &cookie, "first").await;
    let second = warm_package(&h, &cookie, "second").await;
    wait_for(&h, &cookie, first, &["running"]).await;

    let (status, _, _) = h
        .send(
            "POST",
            &format!("/api/jobs/{second}/cancel"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let d = wait_for(&h, &cookie, second, &["cancelled"]).await;
    assert!(
        d["job"]["message"]
            .as_str()
            .unwrap()
            .contains("before it started")
    );

    let (status, _, _) = h
        .send(
            "POST",
            &format!("/api/jobs/{first}/cancel"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    wait_for(&h, &cookie, first, &["cancelled"]).await;
    assert_eq!(
        *fake.deletes.lock().unwrap(),
        vec![first],
        "the warmer was told to stop"
    );
    assert_eq!(
        fake.runs.lock().unwrap().len(),
        1,
        "the cancelled queued job never ran"
    );

    // Cancelling a finished job is refused.
    let (status, _, _) = h
        .send(
            "POST",
            &format!("/api/jobs/{first}/cancel"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let audit = h.api(&cookie, "/api/audit?action=job").await;
    assert!(
        audit["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "job.cancel")
    );
}

#[tokio::test]
async fn stops_at_the_time_limit() {
    let plan = Plan {
        lines: 100,
        delay: Duration::from_millis(100),
        ..Plan::default()
    };
    let (h, fake, cookie) = with_fake(
        plan,
        Opts {
            warm_timeout: Duration::from_secs(1),
            ..Opts::default()
        },
    )
    .await;
    let id = warm_package(&h, &cookie, "slow").await;
    let d = wait_for(&h, &cookie, id, &["failed", "done"]).await;
    assert_eq!(d["job"]["status"], "failed");
    assert!(
        d["job"]["message"].as_str().unwrap().contains("time limit"),
        "{d}"
    );
    assert_eq!(*fake.deletes.lock().unwrap(), vec![id]);
}

#[tokio::test]
async fn stops_at_the_cache_budget() {
    let jar = "org/example/big/1.0/big-1.0.jar";
    let plan = Plan {
        fetch: vec![format!("/maven/{jar}")],
        lines: 100,
        delay: Duration::from_millis(50),
        ..Plan::default()
    };
    let (h, fake, cookie) = with_fake(
        plan,
        Opts {
            cache_limit: Some(4096),
            ..Opts::default()
        },
    )
    .await;
    let big = vec![7u8; 8192];
    h.mock
        .set(&format!("/maven2/{jar}"), Upstream::ok(big.clone()));
    h.mock.set(
        &format!("/maven2/{jar}.sha1"),
        Upstream::ok(hex::encode(sha1::Sha1::digest(&big))),
    );

    let id = warm_package(&h, &cookie, "big").await;
    let d = wait_for(&h, &cookie, id, &["failed", "done"]).await;
    assert_eq!(d["job"]["status"], "failed");
    assert!(
        d["job"]["message"].as_str().unwrap().contains("budget"),
        "{d}"
    );
    assert_eq!(*fake.deletes.lock().unwrap(), vec![id]);
    // The download that crossed the budget went through Granary.
    assert!(h.data.join("files/maven").join(jar).exists());
    assert_eq!(d["job"]["new_files"], 1);

    // Already over budget: the next job fails straight away.
    let id = warm_package(&h, &cookie, "next").await;
    let d = wait_for(&h, &cookie, id, &["failed", "done"]).await;
    assert!(
        d["job"]["message"]
            .as_str()
            .unwrap()
            .contains("already uses"),
        "{d}"
    );
    assert_eq!(fake.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn template_warm_records_results() {
    let tgz = include_bytes!("fixtures/granary-tiny-1.0.0.npm.tar.gz").to_vec();
    let plan = Plan {
        fetch: vec!["/npm/granary-tiny/-/granary-tiny-1.0.0.tgz".into()],
        lockfiles: HashMap::from([("package-lock.json".into(), "{}".into())]),
        ..Plan::default()
    };
    let (h, _fake, cookie) = with_fake(plan, Opts::default()).await;
    set_tiny_package(&h, &tgz);

    let (_, _, out) = h
        .send(
            "POST",
            "/api/templates",
            Some(&cookie),
            Some(json!({"name": "Tiny", "npm": "\"granary-tiny\": \"^1\""})),
        )
        .await;
    let tid = serde_json::from_slice::<Value>(&out).unwrap()["id"]
        .as_i64()
        .unwrap();
    let (status, _, out) = h
        .send(
            "POST",
            &format!("/api/templates/{tid}/warm"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = serde_json::from_slice::<Value>(&out).unwrap()["id"]
        .as_i64()
        .unwrap();
    let d = wait_for(&h, &cookie, id, &["done", "failed"]).await;
    assert_eq!(d["job"]["status"], "done", "{d}");
    assert_eq!(d["job"]["new_files"], 1);
    assert_eq!(d["job"]["new_packages"], 1);
    assert_eq!(d["job"]["new_bytes"], tgz.len() as u64);
    let t = h.api(&cookie, &format!("/api/templates/{tid}")).await;
    assert!(t["last_warmed_at"].as_i64().is_some());

    // The warmer's token is a system token: listed, downloads only.
    let tokens = h.api(&cookie, "/api/tokens?all=true").await;
    let warmer = tokens["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "warmer")
        .expect("warmer token listed");
    assert_eq!(warmer["system"], true);
}

#[tokio::test]
async fn warming_needs_the_warmer() {
    let h = Harness::new(false, Duration::from_secs(300)).await;
    let cookie = h.login().await;
    let w = h.api(&cookie, "/api/warmer").await;
    assert_eq!(w["configured"], false);
    assert_eq!(
        h.api(&cookie, "/api/packages/status").await["warmer"],
        false
    );
    let (status, _, _) = h
        .send(
            "POST",
            "/api/packages/warm",
            Some(&cookie),
            Some(json!({"eco": "npm", "name": "x", "version": "1.0.0"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Configured but not running: unavailable, and the UI is told so.
    let h = Harness::with(Opts {
        warmer_url: Some("http://127.0.0.1:9".into()),
        ..Opts::default()
    })
    .await;
    let cookie = h.login().await;
    let w = h.api(&cookie, "/api/warmer").await;
    assert_eq!(w["configured"], true);
    assert_eq!(w["available"], false);
    let viewer = h.viewer_login().await;
    let (status, _, _) = h
        .send(
            "POST",
            "/api/packages/warm",
            Some(&viewer),
            Some(json!({"eco": "npm", "name": "x", "version": "1.0.0"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

pub(super) fn set_tiny_package(h: &Harness, tgz: &[u8]) {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let integrity = format!("sha512-{}", STANDARD.encode(sha2::Sha512::digest(tgz)));
    let base = h.upstream_base();
    let packument = json!({
        "name": "granary-tiny",
        "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": {
            "name": "granary-tiny",
            "version": "1.0.0",
            "dist": {
                "tarball": format!("{base}/npm/granary-tiny/-/granary-tiny-1.0.0.tgz"),
                "integrity": integrity,
            },
        }},
    })
    .to_string();
    h.mock.set("/npm/granary-tiny", Upstream::ok(packument));
    h.mock.set(
        "/npm/granary-tiny/-/granary-tiny-1.0.0.tgz",
        Upstream::ok(tgz.to_vec()),
    );
}

/// The real warmer.py running npm against Granary, with Granary as the only
/// registry it knows. Skipped when python3 or npm isn't installed.
#[tokio::test]
async fn real_warmer_caches_a_template_through_granary() {
    let have = |cmd: &str| {
        std::process::Command::new(cmd)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !have("python3") || !have("npm") {
        eprintln!("skipping: python3 and npm are needed for the real warmer test");
        return;
    }
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/warmer/warmer.py");
    let mut child = tokio::process::Command::new("python3")
        .arg(script)
        .env("WARMER_SECRET", WARMER_SECRET)
        .env("WARMER_LISTEN", format!("127.0.0.1:{port}"))
        .env("WARMER_ALLOW_FROM", "127.0.0.1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let h = Harness::with(Opts {
        warmer_url: Some(format!("http://127.0.0.1:{port}")),
        serve: true,
        ..Opts::default()
    })
    .await;
    let cookie = h.login().await;
    let tgz = include_bytes!("fixtures/granary-tiny-1.0.0.npm.tar.gz");
    set_tiny_package(&h, tgz);

    // Wait for the warmer to start (it checks its tools first).
    let start = Instant::now();
    while h.api(&cookie, "/api/warmer").await["available"] != true {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "warmer did not start"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let (_, _, out) = h
        .send(
            "POST",
            "/api/templates",
            Some(&cookie),
            Some(json!({"name": "Tiny", "npm": "\"granary-tiny\": \"^1\"", "platforms": ["linux-x86_64"]})),
        )
        .await;
    let tid = serde_json::from_slice::<Value>(&out).unwrap()["id"]
        .as_i64()
        .unwrap();
    let (status, _, out) = h
        .send(
            "POST",
            &format!("/api/templates/{tid}/warm"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&out)
    );
    let id = serde_json::from_slice::<Value>(&out).unwrap()["id"]
        .as_i64()
        .unwrap();
    let start = Instant::now();
    let d = loop {
        let d = h.api(&cookie, &format!("/api/jobs/{id}")).await;
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
        h.data
            .join("files/npm/granary-tiny/-/granary-tiny-1.0.0.tgz")
            .exists()
    );
    assert_eq!(h.mock.hits("/npm/granary-tiny/-/granary-tiny-1.0.0.tgz"), 1);
    // The token never shows up in the log.
    assert!(log.iter().all(|l| !l.contains("gran_")), "{log:#?}");
    let _ = child.kill().await;
}
