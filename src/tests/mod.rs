//! End-to-end tests: the whole app, with a mock upstream registry on
//! localhost and a throwaway data directory.

mod maven;

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
    pub mock: Mock,
    pub data: PathBuf,
    pub token: String,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

impl Harness {
    /// Start a mock upstream and build Granary against it. Maven's upstream
    /// is `http://<mock>/maven2`.
    pub async fn new(client_auth: bool, meta_ttl: Duration) -> Self {
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
            admin_user: "admin".into(),
            admin_password: None,
            client_auth,
            cookie_secure: false,
            session_idle: Duration::from_secs(3600),
            trusted_proxies: Vec::new(),
            audit_days: 365,
        };
        let store = Store::new(data.clone(), meta_ttl).await.unwrap();
        let admin = Admin::new(&data, LogBuffer::default()).await.unwrap();
        let db = auth::open_db(&data).await.unwrap();
        auth::ensure_admin(&db, "admin", Some("a long test password"))
            .await
            .unwrap();
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
        Self {
            app: crate::app(state, sessions),
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
