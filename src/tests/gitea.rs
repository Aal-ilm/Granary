//! Gitea as a private upstream, against a mock Gitea served over HTTPS with
//! a certificate from a test CA: the connection check, the extra CA, the
//! token's reach, private git repos under both access modes, templates made
//! from them, the warmer's access, and the private-address rule.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio_rustls::rustls;

use super::{
    Harness, Opts,
    git::{TempDir, Upstream, client_git, have_git, ok},
    git_templates::{preview, save},
    jobs::{Plan, wait_for, with_fake},
};
use crate::{
    config::{Gitea, Secret},
    policy::Mode,
};

pub(super) const TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

/// A stand-in for Gitea at `https://localhost:{port}`: its version and user
/// API, and `git http-backend` for repos under `root/{owner}/{repo}.git`.
/// Everything needs the token.
#[derive(Clone)]
pub(super) struct MockGitea {
    pub(super) url: String,
    pub(super) host: String,
    /// The test CA's certificate (PEM).
    pub(super) ca: PathBuf,
    root: Arc<TempDir>,
    /// Every Authorization header received, in order ("" when none).
    auth_seen: Arc<Mutex<Vec<String>>>,
    /// Answer API calls with a redirect here.
    redirect: Arc<Mutex<Option<String>>>,
    /// Act like a token without the read:user scope.
    no_user_scope: Arc<std::sync::atomic::AtomicBool>,
    /// Package registry files by path (`/api/packages/...`).
    pub(super) packages: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    /// Package paths answered with a redirect to another URL.
    pub(super) package_redirects: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// Every package registry path requested.
    pub(super) package_hits: Arc<Mutex<Vec<String>>>,
    /// What the package search API returns.
    pub(super) search: Arc<Mutex<Value>>,
}

async fn serve_gitea(
    State(g): State<MockGitea>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    g.auth_seen.lock().unwrap().push(auth.clone());
    let path = uri.path();
    if path.starts_with("/api/packages/") {
        g.package_hits.lock().unwrap().push(path.to_string());
        if auth != format!("token {TOKEN}") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if let Some(to) = g.package_redirects.lock().unwrap().get(path).cloned() {
            return (StatusCode::FOUND, [(header::LOCATION, to)]).into_response();
        }
        return match g.packages.lock().unwrap().get(path).cloned() {
            Some(body) => body.into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if path.starts_with("/api/v1/packages/") {
        if auth != format!("token {TOKEN}") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        return Json(g.search.lock().unwrap().clone()).into_response();
    }
    if path.starts_with("/api/") {
        if let Some(to) = g.redirect.lock().unwrap().clone() {
            return (StatusCode::FOUND, [(header::LOCATION, to)]).into_response();
        }
        if auth != format!("token {TOKEN}") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        return match path {
            "/api/v1/user" if g.no_user_scope.load(std::sync::atomic::Ordering::SeqCst) => {
                StatusCode::FORBIDDEN.into_response()
            }
            "/api/v1/version" => Json(json!({"version": "1.24.6"})).into_response(),
            "/api/v1/user" => Json(json!({"login": "granary-bot", "id": 7})).into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let basic = format!("Basic {}", STANDARD.encode(format!("granary:{TOKEN}")));
    if auth != basic {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"Gitea\"")],
        )
            .into_response();
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    crate::formats::git::cgi(
        cmd,
        &g.root.0,
        &method,
        path,
        uri.query().unwrap_or(""),
        &headers,
        body,
    )
    .await
    .into_response()
}

impl MockGitea {
    pub(super) async fn start() -> Self {
        let root = Arc::new(TempDir::new("gitea"));
        let (ca_pem, tls) = test_tls();
        let ca = root.0.join("test-ca.pem");
        std::fs::write(&ca, ca_pem).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let g = MockGitea {
            url: format!("https://localhost:{port}"),
            host: format!("localhost:{port}"),
            ca,
            root,
            auth_seen: Arc::default(),
            redirect: Arc::default(),
            no_user_scope: Arc::default(),
            packages: Arc::default(),
            package_redirects: Arc::default(),
            package_hits: Arc::default(),
            search: Arc::new(Mutex::new(json!([]))),
        };
        let app = Router::new()
            .fallback(any(serve_gitea))
            .with_state(g.clone());
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
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
        g
    }

    pub(super) fn gitea(&self) -> Gitea {
        Gitea {
            url: self.url.clone(),
            host: self.host.clone(),
            token: Secret::new(TOKEN),
        }
    }

    /// Create `{owner}/{name}.git` with one commit holding `files`.
    pub(super) fn create(&self, owner: &str, name: &str, files: &[(&str, &[u8])]) {
        let bare = self.root.0.join(owner).join(format!("{name}.git"));
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
            &["push", "-q", bare.to_str().unwrap(), "main"],
            &work,
        ));
    }

    pub(super) fn seen(&self) -> Vec<String> {
        self.auth_seen.lock().unwrap().clone()
    }
}

/// A test CA (its PEM) and a server config with a certificate it signed
/// for localhost.
fn test_tls() -> (String, Arc<rustls::ServerConfig>) {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose, SanType,
    };
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "Granary test CA");
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    leaf.subject_alt_names
        .push(SanType::IpAddress([127, 0, 0, 1].into()));
    leaf.distinguished_name
        .push(DnType::CommonName, "localhost");
    let leaf_cert = leaf.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        )
        .unwrap();
    (ca_cert.pem(), Arc::new(config))
}

/// Granary with the mock Gitea, its CA trusted, and `up` as GitHub.
async fn granary(g: &MockGitea, up: &Upstream, opts: Opts) -> Harness {
    Harness::with(Opts {
        serve: true,
        git_upstream: Some(up.url.clone()),
        gitea: Some(g.gitea()),
        extra_ca: Some(g.ca.clone()),
        ..opts
    })
    .await
}

fn refs(host: &str, repo: &str) -> String {
    format!("/git/{host}/{repo}.git/info/refs?service=git-upload-pack")
}

fn basic(token: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("x:{token}")))
}

async fn new_token(h: &Harness, cookie: &str, private_access: bool) -> (StatusCode, Value) {
    let (status, _, body) = h
        .send(
            "POST",
            "/api/tokens",
            Some(cookie),
            Some(json!({"name": "ci", "private_access": private_access})),
        )
        .await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Clone through Granary with a token, on a blocking thread.
async fn clone_with(h: &Harness, path: &str, token: &str) -> (TempDir, std::process::Output) {
    let dir = TempDir::new("gitea-client");
    let base = h
        .url
        .as_ref()
        .unwrap()
        .replace("http://", &format!("http://x:{token}@"));
    let url = format!("{base}{path}");
    let cwd = dir.0.clone();
    let out =
        tokio::task::spawn_blocking(move || client_git(&["clone", "-q", &url, "checkout"], &cwd))
            .await
            .unwrap();
    (dir, out)
}

fn repo_names(v: &Value) -> Vec<String> {
    v["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}/{}",
                r["owner"].as_str().unwrap(),
                r["name"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connects_with_the_extra_ca_and_never_shows_the_token() {
    let g = MockGitea::start().await;
    let up = Upstream::start().await;
    let h = granary(&g, &up, Opts::default()).await;
    let cookie = h.login().await;

    let (status, _, body) = h
        .send("POST", "/api/system/gitea/check", Some(&cookie), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let s: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(s["status"]["ok"], true, "{s}");
    assert_eq!(s["status"]["version"], "1.24.6");
    assert_eq!(s["status"]["user"], "granary-bot");
    assert!(g.seen().iter().all(|a| a == &format!("token {TOKEN}")));

    // The System page shows the connection, never the token; nor do the logs.
    let (_, _, sys) = h.send("GET", "/api/system", Some(&cookie), None).await;
    let sys = String::from_utf8_lossy(&sys);
    assert!(sys.contains(&g.url) && sys.contains("granary-bot"), "{sys}");
    let (_, _, logs) = h.send("GET", "/api/logs", Some(&cookie), None).await;
    let (_, _, me) = h.send("GET", "/api/auth/me", Some(&cookie), None).await;
    for text in [
        sys.to_string(),
        String::from_utf8_lossy(&logs).to_string(),
        String::from_utf8_lossy(&me).to_string(),
    ] {
        assert!(!text.contains(TOKEN), "the token leaked: {text}");
    }

    // git's CA bundle: the system's roots, then the extra CA, so public
    // hosts keep working.
    let bundle = std::fs::read_to_string(h.data.join("state/ca-bundle.pem")).unwrap();
    let ca = std::fs::read_to_string(&g.ca).unwrap();
    assert!(bundle.trim_end().ends_with(ca.trim_end()));
    let system = rustls_native_certs::load_native_certs().certs.len();
    assert_eq!(bundle.matches("BEGIN CERTIFICATE").count(), system + 1);

    // A token without the read:user scope still connects; its owner just
    // isn't known.
    g.no_user_scope
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cookie_again = h.login().await;
    let (_, _, body) = h
        .send("POST", "/api/system/gitea/check", Some(&cookie_again), None)
        .await;
    let s: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(s["status"]["ok"], true, "{s}");
    assert!(s["status"]["user"].is_null());
    assert!(s["status"]["note"].as_str().unwrap().contains("read:user"));

    // Without the extra CA, the internal certificate isn't trusted.
    let plain = Harness::with(Opts {
        gitea: Some(g.gitea()),
        ..Opts::default()
    })
    .await;
    let cookie = plain.login().await;
    let (_, _, body) = plain
        .send("POST", "/api/system/gitea/check", Some(&cookie), None)
        .await;
    let s: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(s["status"]["ok"], false);
    let err = s["status"]["error"].as_str().unwrap().to_ascii_lowercase();
    assert!(
        err.contains("certificate") || err.contains("issuer"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_never_follows_a_redirect() {
    let g = MockGitea::start().await;
    let up = Upstream::start().await;
    let h = granary(&g, &up, Opts::default()).await;
    let cookie = h.login().await;
    *g.redirect.lock().unwrap() = Some(format!("{}/elsewhere", up.url));
    let (_, _, body) = h
        .send("POST", "/api/system/gitea/check", Some(&cookie), None)
        .await;
    let s: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(s["status"]["ok"], false);
    assert!(
        s["status"]["error"].as_str().unwrap().contains("redirect"),
        "{s}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!up.saw_auth.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_repos_need_private_access() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let g = MockGitea::start().await;
    let up = Upstream::start().await;
    g.create("acme", "secret", &[("README.md", b"private")]);
    up.create("acme", "demo", &[("README.md", b"public")]);
    // Downloads open to anyone, so anonymous access is tested too.
    let h = granary(&g, &up, Opts::default()).await;
    let host = g.host.clone();
    let admin = h.login().await;

    // Anonymous: asked to sign in, whether or not the repo exists, and
    // Gitea is never contacted.
    for repo in ["acme/secret", "acme/nope"] {
        let (status, headers, _) = h.get(&refs(&host, repo)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{repo}");
        assert_eq!(headers["www-authenticate"], "Basic realm=\"granary\"");
    }
    assert!(g.seen().is_empty());

    // The admin's existing token has no private access: not found.
    let (status, _, _) = h
        .request("GET", &refs(&host, "acme/secret"), Some(basic(&h.token)))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(g.seen().is_empty());

    // A token with private access clones it, through Granary, which sends
    // Gitea its own token.
    let (status, t) = new_token(&h, &admin, true).await;
    assert_eq!(status, StatusCode::OK, "{t}");
    let token = t["token"].as_str().unwrap().to_string();
    let (dir, out) = clone_with(&h, &format!("/git/{host}/acme/secret.git"), &token).await;
    ok(out);
    assert_eq!(
        std::fs::read(dir.0.join("checkout/README.md")).unwrap(),
        b"private"
    );
    let gitea_basic = format!("Basic {}", STANDARD.encode(format!("granary:{TOKEN}")));
    let seen = g.seen();
    assert!(
        !seen.is_empty() && seen.iter().all(|a| a == &gitea_basic),
        "{seen:?}"
    );

    // A public repo: the Gitea token is never sent there.
    let (_, out) = clone_with(&h, "/git/github.com/acme/demo.git", &token).await;
    ok(out);
    assert!(!up.saw_auth.load(std::sync::atomic::Ordering::SeqCst));

    // Lists and counts: the admin sees both, a viewer only the public one.
    let viewer = h.viewer_login().await;
    let all = h.api(&admin, "/api/git/repos").await;
    assert_eq!(repo_names(&all), vec!["acme/demo", "acme/secret"]);
    assert_eq!(all["repos"][1]["private"], true);
    assert_eq!(
        repo_names(&h.api(&viewer, "/api/git/repos").await),
        vec!["acme/demo"]
    );
    let git_count = |o: &Value| {
        o["repos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["repo"] == "git")
            .unwrap()["packages"]
            .clone()
    };
    assert_eq!(git_count(&h.api(&admin, "/api/overview").await), 2);
    assert_eq!(git_count(&h.api(&viewer, "/api/overview").await), 1);

    // A viewer can't hand on access they don't have.
    let (status, _) = new_token(&h, &viewer, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Given private access, the viewer sees it.
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE username = 'viewer'")
        .fetch_one(&h.state.auth.db)
        .await
        .unwrap();
    let (status, _, _) = h
        .send(
            "PATCH",
            &format!("/api/users/{id}"),
            Some(&admin),
            Some(json!({"private_access": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(repo_names(&h.api(&viewer, "/api/git/repos").await).len(), 2);

    // A git job on a private repo: hidden from those who can't read it,
    // and the token never shows up in its log.
    g.create("acme", "secret2", &[("a.txt", b"a")]);
    let (status, _, body) = h
        .send(
            "POST",
            "/api/git/repos",
            Some(&admin),
            Some(json!({"url": format!("https://{host}/acme/secret2")})),
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
    let d = wait_for(&h, &admin, job, &["done", "failed"]).await;
    assert_eq!(d["job"]["status"], "done", "{d}");
    assert_eq!(d["job"]["private"], true);
    assert!(!d.to_string().contains(TOKEN));
    assert!(
        !d.to_string()
            .contains(&STANDARD.encode(format!("granary:{TOKEN}")))
    );
    let (status, _, _) = h
        .send(
            "PATCH",
            &format!("/api/users/{id}"),
            Some(&admin),
            Some(json!({"private_access": false})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .send("GET", &format!("/api/jobs/{job}"), Some(&viewer), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let jobs = h.api(&viewer, "/api/jobs").await;
    assert!(
        jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["id"] != job)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modes_switch_without_a_migration() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let g = MockGitea::start().await;
    let up = Upstream::start().await;
    g.create("acme", "secret", &[("README.md", b"private")]);
    let h = granary(&g, &up, Opts::default()).await;
    let host = g.host.clone();
    let admin = h.login().await;
    let viewer = h.viewer_login().await;
    let (_, t) = new_token(&h, &viewer, false).await;
    let viewer_token = t["token"].as_str().unwrap().to_string();
    let warmer_token = h.state.auth.warmer_token().await.unwrap();
    let status_for = |token: String| {
        let h = &h;
        let path = refs(&host, "acme/secret");
        async move { h.request("GET", &path, Some(basic(&token))).await.0 }
    };

    // Scoped (the default): the viewer's token can't read it.
    assert_eq!(
        status_for(viewer_token.clone()).await,
        StatusCode::NOT_FOUND
    );
    let set = |mode: &'static str| {
        let (h, admin) = (&h, admin.clone());
        async move {
            h.send(
                "PUT",
                "/api/system/private-access",
                Some(&admin),
                Some(json!({ "mode": mode })),
            )
            .await
            .0
        }
    };

    // Signed-in: any valid token, but never the warmer's standing one.
    assert_eq!(set("signed-in").await, StatusCode::OK);
    assert_eq!(status_for(viewer_token.clone()).await, StatusCode::OK);
    assert_eq!(
        status_for(warmer_token.clone()).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        repo_names(&h.api(&viewer, "/api/git/repos").await),
        vec!["acme/secret"]
    );
    let me = h.api(&viewer, "/api/auth/me").await;
    assert_eq!(me["private"]["mode"], "signed-in");

    // And back: nothing else changed.
    assert_eq!(set("scoped").await, StatusCode::OK);
    assert_eq!(status_for(viewer_token).await, StatusCode::NOT_FOUND);
    assert!(repo_names(&h.api(&viewer, "/api/git/repos").await).is_empty());
    assert_eq!(set("everyone").await, StatusCode::BAD_REQUEST);

    // Set in the environment: locked.
    let locked = Harness::with(Opts {
        gitea: Some(g.gitea()),
        private_access: Some(Mode::SignedIn),
        ..Opts::default()
    })
    .await;
    let cookie = locked.login().await;
    let (status, _, _) = locked
        .send(
            "PUT",
            "/api/system/private-access",
            Some(&cookie),
            Some(json!({"mode": "scoped"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let sys = locked.api(&cookie, "/api/system").await;
    assert_eq!(sys["private_access"]["locked"], true);
    assert_eq!(sys["private_access"]["mode"], "signed-in");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_warmer_reads_private_content_only_for_its_starter() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let g = MockGitea::start().await;
    let up = Upstream::start().await;
    g.create("acme", "secret", &[("README.md", b"private")]);
    let path = refs(&g.host, "acme/secret");
    let (h, fake, admin) = with_fake(
        Plan {
            fetch: vec![path.clone()],
            ..Plan::default()
        },
        Opts {
            git_upstream: Some(up.url.clone()),
            gitea: Some(g.gitea()),
            extra_ca: Some(g.ca.clone()),
            ..Opts::default()
        },
    )
    .await;

    // A job an admin started: the warmer reads the private repo.
    let (status, _, body) = h
        .send(
            "POST",
            "/api/packages/warm",
            Some(&admin),
            Some(json!({"eco": "npm", "name": "left-pad", "version": "1.3.0"})),
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
    let d = wait_for(&h, &admin, job, &["done", "failed"]).await;
    let lines = d["lines"].to_string();
    assert!(lines.contains(&format!("fetched {path}: 200")), "{lines}");

    // Its token stopped working when the job ended.
    let used = fake.runs.lock().unwrap()[0]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _, _) = h.request("GET", &path, Some(basic(&used))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A job acting for someone without private access reads nothing private,
    // and neither does the standing warmer token.
    h.viewer_login().await;
    let (viewer,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE username = 'viewer'")
        .fetch_one(&h.state.auth.db)
        .await
        .unwrap();
    let for_viewer = h
        .state
        .auth
        .job_token(9999, viewer, Duration::from_secs(60))
        .await
        .unwrap();
    let (status, _, _) = h.request("GET", &path, Some(basic(&for_viewer))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let standing = h.state.auth.warmer_token().await.unwrap();
    let (status, _, _) = h.request("GET", &path, Some(basic(&standing))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Job tokens download only, like the warmer's.
    let (status, _, _) = h
        .request("GET", "/metrics", Some(format!("Bearer {for_viewer}")))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Schedules record who set them; scheduled warms act for that person.
    let t = h.api(&admin, "/api/templates").await["templates"][0]["id"]
        .as_i64()
        .unwrap();
    let (status, _, _) = h
        .send(
            "PUT",
            &format!("/api/templates/{t}/schedule"),
            Some(&admin),
            Some(json!({"schedule": "daily"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        crate::templates::schedule_by(&h.state.auth.db, t)
            .await
            .unwrap(),
        Some(1)
    );
    crate::jobs::queue_scheduled(&h.state).await;
    let (acting_for,): (Option<i64>,) =
        sqlx::query_as("SELECT acting_for FROM jobs WHERE template_id = ? ORDER BY id DESC")
            .bind(t)
            .fetch_one(&h.state.auth.db)
            .await
            .unwrap();
    assert_eq!(acting_for, Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn templates_from_private_repos_follow_the_policy() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let g = MockGitea::start().await;
    let up = Upstream::start().await;
    g.create(
        "acme",
        "app",
        &[(
            "package.json",
            br#"{"name":"app","dependencies":{"left-pad":"^1.3.0"}}"#,
        )],
    );
    let h = granary(&g, &up, Opts::default()).await;
    let admin = h.login().await;
    let viewer = h.viewer_login().await;

    let p = preview(&h, &admin, &format!("https://{}/acme/app", g.host), "").await;
    let (id, _) = save(&h, &admin, &p, "Private app").await;
    let t = h.api(&admin, &format!("/api/templates/{id}")).await;
    assert_eq!(t["private"], true);

    // Hidden from the viewer everywhere, as if it didn't exist.
    let names = |v: &Value| -> Vec<String> {
        v["templates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(names(&h.api(&admin, "/api/templates").await).contains(&"Private app".to_string()));
    assert!(!names(&h.api(&viewer, "/api/templates").await).contains(&"Private app".to_string()));
    for path in [
        format!("/api/templates/{id}"),
        format!("/api/templates/{id}/starter"),
        format!("/api/templates/{id}/export"),
    ] {
        let (status, _, _) = h.send("GET", &path, Some(&viewer), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _, _) = h
        .send(
            "GET",
            &format!("/api/templates/{id}/starter"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // A copy is just as private.
    let (status, _, body) = h
        .send(
            "POST",
            &format!("/api/templates/{id}/clone"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let copy = serde_json::from_slice::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        h.api(&admin, &format!("/api/templates/{copy}")).await["private"],
        true
    );
    let (status, _, _) = h
        .send(
            "GET",
            &format!("/api/templates/{copy}"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_private_address_rule() {
    if !have_git() {
        return eprintln!("skipping: git is not installed");
    }
    let g = MockGitea::start().await;
    let up = Upstream::start().await;
    up.create("acme", "demo", &[("README.md", b"public")]);
    g.create("acme", "secret", &[("README.md", b"private")]);
    // Nothing allowed past the rule but the Gitea host itself.
    let h = granary(
        &g,
        &up,
        Opts {
            private_hosts: Vec::new(),
            ..Opts::default()
        },
    )
    .await;

    // Upstreams on a private address are refused, for packages and git.
    let (status, _, body) = h.get("/npm/left-pad").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("GRANARY_PRIVATE_HOSTS"));
    let (status, _, body) = h.get(&refs("github.com", "acme/demo")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("GRANARY_PRIVATE_HOSTS"));
    assert!(!up.saw_auth.load(std::sync::atomic::Ordering::SeqCst));

    // The Gitea host is the exception.
    let admin = h.login().await;
    let (_, t) = new_token(&h, &admin, true).await;
    let (status, _, _) = h
        .request(
            "GET",
            &refs(&g.host, "acme/secret"),
            Some(basic(t["token"].as_str().unwrap())),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}
