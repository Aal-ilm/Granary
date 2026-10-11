//! The git pull-through cache, against a local bare repo served over HTTP
//! as a stand-in for GitHub. Real `git` is used on both sides.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};

use super::{Harness, Opts};

/// A temporary directory removed when dropped.
pub(super) struct TempDir(pub(super) PathBuf);

impl TempDir {
    pub(super) fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "granary-git-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run git as a client: no user or system config, so the developer's own
/// settings (insteadOf rules, credential helpers) can't interfere.
pub(super) fn client_git(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("HOME", cwd)
        .output()
        .unwrap()
}

pub(super) fn ok(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "git failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A stand-in for GitHub: bare repos under `root`, served with
/// `git http-backend` at `{url}/{host}/{owner}/{repo}.git`.
#[derive(Clone)]
pub(super) struct Upstream {
    pub(super) root: Arc<TempDir>,
    pub(super) url: String,
    refs_hits: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    delay: Arc<Mutex<Duration>>,
    /// Set when any request carried an Authorization header.
    pub(super) saw_auth: Arc<AtomicBool>,
}

async fn serve_upstream(
    State(u): State<Upstream>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if uri.path().ends_with("/info/refs") {
        u.refs_hits.fetch_add(1, Ordering::SeqCst);
    }
    if headers.contains_key(axum::http::header::AUTHORIZATION) {
        u.saw_auth.store(true, Ordering::SeqCst);
    }
    if u.fail.load(Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "upstream is down").into_response();
    }
    let delay = *u.delay.lock().unwrap();
    tokio::time::sleep(delay).await;
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    crate::formats::git::cgi(
        cmd,
        &u.root.0,
        &method,
        uri.path(),
        uri.query().unwrap_or(""),
        &headers,
        body,
    )
    .await
    .into_response()
}

impl Upstream {
    pub(super) async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let u = Upstream {
            root: Arc::new(TempDir::new("upstream")),
            url: format!("http://{}", listener.local_addr().unwrap()),
            refs_hits: Arc::default(),
            fail: Arc::default(),
            delay: Arc::default(),
            saw_auth: Arc::default(),
        };
        let app = Router::new()
            .fallback(any(serve_upstream))
            .with_state(u.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        u
    }

    /// Create `github.com/{owner}/{name}.git` with one commit holding `files`.
    pub(super) fn create(&self, owner: &str, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let bare = self
            .root
            .0
            .join("github.com")
            .join(owner)
            .join(format!("{name}.git"));
        std::fs::create_dir_all(&bare).unwrap();
        ok(client_git(
            &["init", "-q", "--bare", "-b", "main", "."],
            &bare,
        ));
        let work = self.root.0.join(format!("work-{owner}-{name}"));
        std::fs::create_dir_all(&work).unwrap();
        ok(client_git(&["init", "-q", "-b", "main", "."], &work));
        for (path, data) in files {
            let file = work.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, data).unwrap();
        }
        ok(client_git(&["add", "."], &work));
        ok(client_git(&["commit", "-q", "-m", "first"], &work));
        ok(client_git(
            &["remote", "add", "origin", bare.to_str().unwrap()],
            &work,
        ));
        ok(client_git(&["push", "-q", "origin", "main"], &work));
        work
    }

    /// Tag the current commit and push the tag.
    pub(super) fn tag(&self, work: &Path, tag: &str) {
        ok(client_git(&["tag", tag], work));
        ok(client_git(&["push", "-q", "origin", tag], work));
    }

    /// Write files and push them as a new commit.
    pub(super) fn change(&self, work: &Path, files: &[(&str, &[u8])], msg: &str) {
        for (path, data) in files {
            let file = work.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, data).unwrap();
        }
        ok(client_git(&["add", "."], work));
        ok(client_git(&["commit", "-q", "-m", msg], work));
        ok(client_git(&["push", "-q", "origin", "main"], work));
    }

    pub(super) fn commit(&self, work: &Path, msg: &str) {
        std::fs::write(work.join(format!("{msg}.txt")), msg).unwrap();
        ok(client_git(&["add", "."], work));
        ok(client_git(&["commit", "-q", "-m", msg], work));
        ok(client_git(&["push", "-q", "origin", "main"], work));
    }

    fn hits(&self) -> usize {
        self.refs_hits.load(Ordering::SeqCst)
    }
}

pub(super) fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

pub(super) async fn granary(up: &Upstream, opts: Opts) -> Harness {
    Harness::with(Opts {
        serve: true,
        git_upstream: Some(up.url.clone()),
        ..opts
    })
    .await
}

/// Clone through Granary on a blocking thread (git is a separate process).
async fn clone(h: &Harness, repo: &str, extra: &[&str]) -> (TempDir, std::process::Output) {
    let dir = TempDir::new("client");
    let url = format!("{}/git/github.com/{repo}", h.url.as_ref().unwrap());
    let mut args: Vec<String> = vec!["clone".into(), "-q".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    args.push(url);
    args.push("checkout".into());
    let cwd = dir.0.clone();
    let out = tokio::task::spawn_blocking(move || {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        client_git(&refs, &cwd)
    })
    .await
    .unwrap();
    (dir, out)
}

fn commits(dir: &TempDir) -> usize {
    ok(client_git(
        &["rev-list", "--count", "HEAD"],
        &dir.0.join("checkout"),
    ))
    .trim()
    .parse()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clones_shallow_clones_and_refuses_push() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    let work = up.create("acme", "demo", &[("README.md", b"hello")]);
    up.commit(&work, "second");
    let h = granary(&up, Opts::default()).await;

    let (dir, out) = clone(&h, "acme/demo.git", &[]).await;
    ok(out);
    assert_eq!(
        std::fs::read(dir.0.join("checkout/README.md")).unwrap(),
        b"hello"
    );
    assert_eq!(commits(&dir), 2);

    // Shallow, and without the .git suffix, from the cache.
    let (dir, out) = clone(&h, "acme/demo", &["--depth", "1"]).await;
    ok(out);
    assert_eq!(commits(&dir), 1);
    assert!(h.data.join("git/github.com/acme/demo.git/HEAD").exists());

    // Partial clone (blob filter) works too.
    let (_dir, out) = clone(&h, "acme/demo.git", &["--filter=blob:none"]).await;
    ok(out);

    // Push is refused.
    let checkout = dir.0.join("checkout");
    std::fs::write(checkout.join("x.txt"), "x").unwrap();
    ok(client_git(&["add", "."], &checkout));
    ok(client_git(&["commit", "-q", "-m", "x"], &checkout));
    let out = client_git(&["push", "origin", "HEAD"], &checkout);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("403"));
    let (status, _, _) = h
        .get("/git/github.com/acme/demo.git/info/refs?service=git-receive-pack")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = h
        .request(
            "POST",
            "/git/github.com/acme/demo.git/git-receive-pack",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn needs_a_token_like_maven() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    up.create("acme", "private-ish", &[("a", b"a")]);
    let h = granary(
        &up,
        Opts {
            client_auth: true,
            ..Opts::default()
        },
    )
    .await;
    let path = "/git/github.com/acme/private-ish.git/info/refs?service=git-upload-pack";
    let (status, headers, _) = h.get(path).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["www-authenticate"], "Basic realm=\"granary\"");
    assert_eq!(up.hits(), 0);

    // git answers the challenge with the URL's credentials.
    let dir = TempDir::new("auth");
    let base = h
        .url
        .as_ref()
        .unwrap()
        .replace("http://", &format!("http://x:{}@", h.token));
    let url = format!("{base}/git/github.com/acme/private-ish.git");
    ok(client_git(&["clone", "-q", &url, "c"], &dir.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pulls_through_refreshes_and_serves_stale() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    let work = up.create("acme", "demo", &[("a", b"a")]);
    // TTL zero: every fetch tries to refresh first.
    let h = granary(
        &up,
        Opts {
            meta_ttl: Duration::ZERO,
            ..Opts::default()
        },
    )
    .await;
    let cookie = h.login().await;

    let (dir, out) = clone(&h, "acme/demo.git", &[]).await;
    ok(out);
    assert_eq!(commits(&dir), 1);
    let after_first = up.hits();
    assert!(after_first >= 1);

    up.commit(&work, "second");
    let (dir, out) = clone(&h, "acme/demo.git", &[]).await;
    ok(out);
    assert_eq!(commits(&dir), 2, "refreshed from upstream");
    assert!(up.hits() > after_first);

    // Upstream down: the cached copy is served, and the failure recorded.
    up.fail.store(true, Ordering::SeqCst);
    up.commit(&work, "third");
    let (dir, out) = clone(&h, "acme/demo.git", &[]).await;
    ok(out);
    assert_eq!(commits(&dir), 2, "stale copy served");
    let repos = h.api(&cookie, "/api/git/repos").await;
    assert!(repos["repos"][0]["last_error"].is_string(), "{repos}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_repos_are_not_refetched_and_misses_coalesce() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    up.create("acme", "demo", &[("a", b"a")]);
    *up.delay.lock().unwrap() = Duration::from_millis(400);
    let h = granary(&up, Opts::default()).await;

    // Two clients at once for an uncached repo: one upstream clone.
    let (a, b) = tokio::join!(clone(&h, "acme/demo.git", &[]), clone(&h, "acme/demo", &[]));
    ok(a.1);
    ok(b.1);
    assert_eq!(up.hits(), 1, "one upstream clone for two clients");

    // Within the TTL, more clones don't touch upstream.
    ok(clone(&h, "acme/demo.git", &[]).await.1);
    assert_eq!(up.hits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refuses_other_hosts_bad_names_and_odd_paths() {
    let up = Upstream::start().await;
    let h = granary(&up, Opts::default()).await;
    for (path, want) in [
        (
            "/git/evil.example/a/b.git/info/refs?service=git-upload-pack",
            StatusCode::NOT_FOUND,
        ),
        (
            "/git/127.0.0.1/a/b.git/info/refs?service=git-upload-pack",
            StatusCode::NOT_FOUND,
        ),
        (
            "/git/localhost/a/b.git/info/refs?service=git-upload-pack",
            StatusCode::NOT_FOUND,
        ),
        (
            "/git/github.com/-a/b.git/info/refs?service=git-upload-pack",
            StatusCode::NOT_FOUND,
        ),
        (
            "/git/github.com/a/..b.git/info/refs?service=git-upload-pack",
            StatusCode::NOT_FOUND,
        ),
        (
            "/git/github.com/a/../b.git/info/refs?service=git-upload-pack",
            StatusCode::NOT_FOUND,
        ),
        (
            "/git/github.com/a/%2e%2e/info/refs?service=git-upload-pack",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/git/github.com/a/b.git/objects/info/packs",
            StatusCode::NOT_FOUND,
        ),
        ("/git/github.com/a/b.git/info/refs", StatusCode::FORBIDDEN),
    ] {
        let (status, _, _) = h.get(path).await;
        assert_eq!(status, want, "{path}");
    }
    assert_eq!(up.hits(), 0);
    let (_, _, body) = h.get("/git/hosts").await;
    assert_eq!(
        String::from_utf8(body).unwrap(),
        "github.com\ncodeberg.org\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stops_clones_that_are_too_big_or_too_slow() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    // Random bytes don't compress, so the clone really is this big.
    let big: Vec<u8> = (0..12_500u32)
        .flat_map(|i| {
            use sha2::Digest;
            sha2::Sha256::digest(i.to_le_bytes()).to_vec()
        })
        .collect();
    up.create("acme", "big", &[("blob.bin", &big)]);
    up.create("acme", "slow", &[("a", b"a")]);

    let h = granary(
        &up,
        Opts {
            git_max_size: 64 * 1024,
            ..Opts::default()
        },
    )
    .await;
    let (status, _, body) = h
        .get("/git/github.com/acme/big.git/info/refs?service=git-upload-pack")
        .await;
    assert_eq!(
        status,
        StatusCode::INSUFFICIENT_STORAGE,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(String::from_utf8_lossy(&body).contains("GRANARY_GIT_MAX_SIZE"));
    assert!(!h.data.join("git/github.com/acme/big.git").exists());
    let leftovers = std::fs::read_dir(h.data.join("tmp")).unwrap().count();
    assert_eq!(leftovers, 0, "the partial clone was deleted");

    let h = granary(
        &up,
        Opts {
            git_timeout: Duration::from_secs(1),
            ..Opts::default()
        },
    )
    .await;
    *up.delay.lock().unwrap() = Duration::from_secs(4);
    let (status, _, body) = h
        .get("/git/github.com/acme/slow.git/info/refs?service=git-upload-pack")
        .await;
    assert_eq!(
        status,
        StatusCode::GATEWAY_TIMEOUT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(!h.data.join("git/github.com/acme/slow.git").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_serves_only_cached_repos() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    up.create("acme", "demo", &[("a", b"a")]);
    up.create("acme", "other", &[("b", b"b")]);
    let h = granary(
        &up,
        Opts {
            offline: true,
            meta_ttl: Duration::ZERO,
            ..Opts::default()
        },
    )
    .await;
    // Put a mirror in the cache by hand, as if cached while online.
    let cached = h.data.join("git/github.com/acme");
    std::fs::create_dir_all(&cached).unwrap();
    let src = up.root.0.join("github.com/acme/demo.git");
    ok(client_git(
        &["clone", "-q", "--mirror", src.to_str().unwrap(), "demo.git"],
        &cached,
    ));

    let (dir, out) = clone(&h, "acme/demo.git", &[]).await;
    ok(out);
    assert_eq!(commits(&dir), 1);
    let (status, _, body) = h
        .get("/git/github.com/acme/other.git/info/refs?service=git-upload-pack")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("offline mode"));
    assert_eq!(
        up.hits(),
        0,
        "offline never contacts upstream, even when stale"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn add_refresh_and_delete_from_the_ui() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let up = Upstream::start().await;
    let work = up.create("acme", "demo", &[("a", b"a")]);
    let h = granary(&up, Opts::default()).await;
    let cookie = h.login().await;

    let (status, _, body) = h
        .send(
            "POST",
            "/api/git/repos",
            Some(&cookie),
            Some(json!({"url": "https://github.com/acme/demo"})),
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
    let d = super::jobs::wait_for(&h, &cookie, job, &["done", "failed"]).await;
    assert_eq!(d["job"]["status"], "done", "{d}");
    assert_eq!(d["job"]["new_packages"], 1);
    let log: Vec<&str> = d["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["line"].as_str().unwrap())
        .collect();
    assert!(log.iter().any(|l| l.contains("Submodules")), "{log:?}");

    let repos = h.api(&cookie, "/api/git/repos").await;
    let r = &repos["repos"][0];
    assert_eq!(r["host"], "github.com");
    assert_eq!(r["name"], "demo");
    assert_eq!(r["default_branch"], "main");
    assert!(r["size"].as_u64().unwrap() > 0);
    assert!(
        r["clone_url"]
            .as_str()
            .unwrap()
            .ends_with("/git/github.com/acme/demo.git")
    );

    // Counted on the Overview.
    let o = h.api(&cookie, "/api/overview").await;
    let git = o["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo"] == "git")
        .unwrap()
        .clone();
    assert_eq!(git["packages"], 1);

    up.commit(&work, "second");
    let target = json!({"host": "github.com", "owner": "acme", "name": "demo"});
    let (status, _, body) = h
        .send(
            "POST",
            "/api/git/repos/refresh",
            Some(&cookie),
            Some(target),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let job = serde_json::from_slice::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    let d = super::jobs::wait_for(&h, &cookie, job, &["done", "failed"]).await;
    assert_eq!(d["job"]["status"], "done", "{d}");

    // Viewers can look but not change anything; bad URLs are refused.
    let viewer = h.viewer_login().await;
    h.api(&viewer, "/api/git/repos").await;
    let (status, _, _) = h
        .send(
            "POST",
            "/api/git/repos",
            Some(&viewer),
            Some(json!({"url": "https://github.com/acme/demo"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for bad in [
        "https://evil.example/a/b",
        "http://github.com/a/b",
        "https://github.com/a/b/tree/main",
    ] {
        let (status, _, _) = h
            .send(
                "POST",
                "/api/git/repos",
                Some(&cookie),
                Some(json!({"url": bad})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }

    let (status, _, _) = h
        .send(
            "DELETE",
            "/api/git/repos?host=github.com&owner=acme&name=demo",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!h.data.join("git/github.com/acme").exists());
    let audit = h.api(&cookie, "/api/audit?action=git").await;
    let actions: Vec<&str> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    for a in ["git.add", "git.refresh", "git.delete"] {
        assert!(actions.contains(&a), "{a}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn old_jobs_tables_gain_git_jobs_and_keep_their_logs() {
    let dir = TempDir::new("migrate");
    let state = dir.0.join("state");
    std::fs::create_dir_all(&state).unwrap();
    // A database as the previous version left it.
    {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(state.join("granary.db"))
                    .create_if_missing(true)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        let old = crate::jobs::SCHEMA.replace(", 'git'", "");
        sqlx::query(sqlx::AssertSqlSafe(old))
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(crate::jobs::LOG_SCHEMA)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO jobs (id, kind, title, spec, status, created_by, created_at) VALUES (7, 'template', 'Old', '{}', 'done', 'admin', 1)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO job_log (job_id, n, at, line) VALUES (7, 1, 1, 'kept')")
            .execute(&db)
            .await
            .unwrap();
        db.close().await;
    }
    let db = crate::auth::open_db(&dir.0).await.unwrap();
    let (line,): (String,) = sqlx::query_as("SELECT line FROM job_log WHERE job_id = 7")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(line, "kept");
    sqlx::query("INSERT INTO jobs (kind, title, spec, status, created_by, created_at) VALUES ('git', 'Git x', '{}', 'queued', 'admin', 2)")
        .execute(&db)
        .await
        .unwrap();
    // Logs still cascade with their job.
    sqlx::query("DELETE FROM jobs WHERE id = 7")
        .execute(&db)
        .await
        .unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM job_log")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// `make use-granary TOOLS=git`: the gitconfig holding the token is private,
/// ~/.gitconfig only includes it, and plain GitHub URLs go through Granary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_setup_keeps_the_token_out_of_gitconfig() {
    let have = |cmd: &str| {
        Command::new(cmd)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !have("git") || !have("bash") || !have("curl") || !have("base64") {
        return eprintln!("skipping: bash, curl, base64 and git are needed");
    }
    let up = Upstream::start().await;
    up.create("acme", "demo", &[("README.md", b"through granary")]);
    let h = granary(
        &up,
        Opts {
            client_auth: true,
            ..Opts::default()
        },
    )
    .await;
    let home = TempDir::new("home");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/granary-client.sh");
    let run = |cmd: &str| {
        let out = Command::new("bash")
            .arg(script)
            .arg(cmd)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", &home.0)
            .env("TOOLS", "git")
            .env("GRANARY_URL", h.url.as_ref().unwrap())
            .env("GRANARY_TOKEN", &h.token)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // A worker thread may block here: the test runtime has several.
    run("use-granary");
    let home_path = home.0.clone();

    let cfg = home_path.join(".config/granary-client/gitconfig");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let global = std::fs::read_to_string(home_path.join(".gitconfig")).unwrap();
    assert!(global.contains(cfg.to_str().unwrap()), "{global}");
    use base64::{Engine, engine::general_purpose::STANDARD};
    let basic = STANDARD.encode(format!("token:{}", h.token));
    for secret in [h.token.as_str(), basic.as_str()] {
        assert!(
            !global.contains(secret),
            "the token must not be in ~/.gitconfig"
        );
    }
    assert!(std::fs::read_to_string(&cfg).unwrap().contains(&basic));

    // A plain GitHub URL now clones through Granary, with the token.
    let out = Command::new("git")
        .args(["clone", "-q", "https://github.com/acme/demo", "c"])
        .current_dir(&home_path)
        .env("HOME", &home_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    ok(out);
    assert_eq!(
        std::fs::read(home_path.join("c/README.md")).unwrap(),
        b"through granary"
    );
    assert_eq!(up.hits(), 1);
}
