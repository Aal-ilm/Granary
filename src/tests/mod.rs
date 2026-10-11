//! End-to-end tests: the whole app, with a mock upstream registry on
//! localhost and a throwaway data directory.

mod cache_limit;
mod git;
mod git_templates;
mod jobs;
mod maven;
mod npm;
mod offline;
mod search;
mod starter;
mod system_user;
mod templates;

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use tower::ServiceExt;

use crate::{AppState, admin::Admin, auth, config::Config, logbuf::LogBuffer, store::Store};

/// One file the mock upstream serves.
#[derive(Clone)]
pub struct Upstream {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub etag: Option<&'static str>,
}

impl Upstream {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.into(),
            etag: None,
        }
    }
}

/// A request the mock upstream received: path and `If-None-Match`.
type Seen = (String, Option<String>);

/// A registry on localhost that serves canned files and records requests.
#[derive(Clone, Default)]
pub struct Mock {
    pub files: Arc<Mutex<HashMap<String, Upstream>>>,
    pub requests: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    pub fn set(&self, path: &str, file: Upstream) {
        self.files.lock().unwrap().insert(path.to_string(), file);
    }

    pub fn hits(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .count()
    }

    pub fn total(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    async fn serve(State(mock): State<Mock>, uri: Uri, headers: HeaderMap) -> Response {
        let path = uri.path().to_string();
        let inm = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        mock.requests
            .lock()
            .unwrap()
            .push((path.clone(), inm.clone()));
        let Some(f) = mock.files.lock().unwrap().get(&path).cloned() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if f.etag.is_some() && inm.as_deref() == f.etag {
            return StatusCode::NOT_MODIFIED.into_response();
        }
        let mut resp = (f.status, f.body).into_response();
        if let Some(etag) = f.etag {
            resp.headers_mut()
                .insert(header::ETAG, etag.parse().unwrap());
        }
        resp
    }
}

pub struct Harness {
    pub app: Router,
    pub state: AppState,
    /// Granary's own URL when served over TCP (`Opts::serve`).
    pub url: Option<String>,
    /// The mock upstream's base URL, e.g. http://127.0.0.1:1234.
    base: String,
    db: sqlx::SqlitePool,
    pub mock: Mock,
    pub data: PathBuf,
    pub token: String,
}

/// Settings for a test instance.
pub struct Opts {
    pub client_auth: bool,
    pub meta_ttl: Duration,
    pub offline: bool,
    pub cache_limit: Option<u64>,
    pub search: bool,
    /// The warmer's URL (a fake one, or a real warmer.py).
    pub warmer_url: Option<String>,
    /// How the warmer reaches this Granary; set by `Harness::serve` users.
    pub warmer_granary_url: Option<String>,
    pub warm_jobs: usize,
    pub warm_timeout: Duration,
    /// Also serve Granary on a real TCP port, so a warmer can reach it.
    pub serve: bool,
    /// Where git repos are cloned from (a local test upstream).
    pub git_upstream: Option<String>,
    pub git_max_size: u64,
    pub git_timeout: Duration,
    pub npm_rewrite_tarballs: bool,
}

/// The shared secret test warmers expect.
pub const WARMER_SECRET: &str = "test-warmer-secret-0123456789";

impl Default for Opts {
    fn default() -> Self {
        Self {
            client_auth: false,
            meta_ttl: Duration::from_secs(300),
            offline: false,
            cache_limit: None,
            search: true,
            warmer_url: None,
            warmer_granary_url: None,
            warm_jobs: 2,
            warm_timeout: Duration::from_secs(1800),
            serve: false,
            git_upstream: None,
            git_max_size: 2 << 30,
            git_timeout: Duration::from_secs(600),
            npm_rewrite_tarballs: true,
        }
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl Harness {
    /// Start a mock upstream and build Granary against it. Maven's upstream
    /// is `http://<mock>/maven2`.
    pub async fn new(client_auth: bool, meta_ttl: Duration) -> Self {
        Self::with(Opts {
            client_auth,
            meta_ttl,
            ..Opts::default()
        })
        .await
    }

    pub async fn with(opts: Opts) -> Self {
        let Opts {
            client_auth,
            meta_ttl,
            offline,
            cache_limit,
            search,
            warmer_url,
            warmer_granary_url,
            warm_jobs,
            warm_timeout,
            serve,
            git_upstream,
            git_max_size,
            git_timeout,
            npm_rewrite_tarballs,
        } = opts;
        // Bind first so the config can name Granary's own address.
        let own = if serve {
            Some(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        } else {
            None
        };
        let own_url = own
            .as_ref()
            .map(|l| format!("http://{}", l.local_addr().unwrap()));
        let mock = Mock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let upstream = Router::new().fallback(Mock::serve).with_state(mock.clone());
        tokio::spawn(async move { axum::serve(listener, upstream).await });
        let base = format!("http://{addr}");

        let data = std::env::temp_dir().join(format!(
            "granary-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();

        let cfg = Config {
            listen: String::new(),
            data_dir: data.clone(),
            public_url: None,
            meta_ttl,
            cargo_index: format!("{base}/cargo"),
            cargo_dl: format!("{base}/crates/{{crate}}/{{version}}/download"),
            npm_upstream: format!("{base}/npm"),
            pypi_index: format!("{base}/simple"),
            pypi_files: format!("{base}/files"),
            maven_upstream: format!("{base}/maven2"),
            search,
            cargo_api: format!("{base}/crates-api"),
            maven_search: format!("{base}/solr/select"),
            warmer_url,
            warmer_secret: Some(WARMER_SECRET.into()),
            warmer_granary_url: warmer_granary_url
                .or(own_url.clone())
                .unwrap_or_else(|| "http://granary.invalid".into()),
            warm_jobs,
            warm_timeout,
            warm_python: "3.12".into(),
            git_hosts: vec!["github.com".into(), "codeberg.org".into()],
            git_max_size,
            git_timeout,
            git_upstream_override: git_upstream,
            admin_user: "admin".into(),
            admin_password: None,
            client_auth,
            cookie_secure: false,
            session_idle: Duration::from_secs(3600),
            trusted_proxies: Vec::new(),
            audit_days: 365,
            offline,
            npm_rewrite_tarballs,
            cache_limit,
        };
        let store = Store::new(data.clone(), meta_ttl, offline).await.unwrap();
        let admin = Admin::new(&data, LogBuffer::default()).await.unwrap();
        let db = auth::open_db(&data).await.unwrap();
        auth::ensure_admin(&db, "admin", Some("a long test password"))
            .await
            .unwrap();
        crate::templates::sync_builtins(&db).await.unwrap();
        let sessions = auth::SqliteSessionStore::new(db.clone());
        let backend = auth::Backend::new(db).await.unwrap();
        let user = backend.list_users().await.unwrap().remove(0);
        let (_, token) = backend.create_token(user.id, "test", None).await.unwrap();

        let state = AppState {
            cfg: Arc::new(cfg),
            store: Arc::new(store),
            admin: Arc::new(admin),
            auth: backend,
        };
        crate::jobs::spawn_runner(state.clone());
        let app = crate::app(state.clone(), sessions);
        if let Some(listener) = own {
            let app = app.clone();
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
            });
        }
        Self {
            base,
            state: state.clone(),
            url: own_url,
            db: state.auth.db.clone(),
            app,
            mock,
            data,
            token,
        }
    }

    /// Send a request with a raw path (never normalized) and optional
    /// `Authorization` header. Returns the status, headers and body.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        authorization: Option<String>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(a) = authorization {
            req = req.header(header::AUTHORIZATION, a);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, headers, body)
    }

    /// Send a request as the web UI does: with a session cookie, the CSRF
    /// header, and an optional JSON body.
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        json: Option<serde_json::Value>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("x-granary-request", "1");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let body = match json {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, headers, body)
    }

    /// Sign in as the admin and return the session cookie.
    pub async fn login(&self) -> String {
        let (status, headers, body) = self
            .send(
                "POST",
                "/api/auth/login",
                None,
                Some(serde_json::json!({"username": "admin", "password": "a long test password"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let set = headers[header::SET_COOKIE].to_str().unwrap();
        set.split(';').next().unwrap().to_string()
    }

    pub fn upstream_base(&self) -> &str {
        &self.base
    }

    /// Create a viewer account and sign in as it; returns the session cookie.
    pub async fn viewer_login(&self) -> String {
        let hash = auth::hash_password("a long viewer password".into())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at) VALUES ('viewer', ?, 'viewer', 0)",
        )
        .bind(hash)
        .execute(&self.db)
        .await
        .unwrap();
        let (status, headers, _) = self
            .send(
                "POST",
                "/api/auth/login",
                None,
                Some(
                    serde_json::json!({"username": "viewer", "password": "a long viewer password"}),
                ),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    /// GET a JSON API route as the signed-in admin.
    pub async fn api(&self, cookie: &str, path: &str) -> serde_json::Value {
        let (status, _, body) = self.send("GET", path, Some(cookie), None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice(&body).unwrap()
    }

    pub async fn get(&self, path: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        self.request("GET", path, None).await
    }

    /// HTTP Basic credentials the way Maven sends them: any username, the
    /// token as the password.
    pub fn basic(&self, user: &str, password: &str) -> String {
        use base64::{Engine, engine::general_purpose::STANDARD};
        format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data);
    }
}
