//! The connection to a self-hosted Gitea, used as a private upstream. Its
//! access token is read once at startup and only ever sent to the configured
//! Gitea URL: requests here never follow redirects, and git sends it only
//! for URLs under that address (see `formats::git`).

use std::{sync::Mutex, time::Duration};

use serde::Serialize;
use serde_json::Value;

use crate::{config::Config, egress::Egress, store::now_secs};

/// The result of the last connection check, for the System page.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub ok: bool,
    /// Gitea's version, as it reports it.
    pub version: Option<String>,
    /// The account the token belongs to.
    pub user: Option<String>,
    pub error: Option<String>,
    /// Something worth knowing that isn't an error.
    pub note: Option<String>,
    pub checked_at: u64,
    pub offline: bool,
}

/// A failed API call: the HTTP status, when Gitea answered.
#[derive(Debug)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl ApiError {
    fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

pub struct Gitea {
    client: reqwest::Client,
    status: Mutex<Option<Status>>,
    /// Per owner: whether Gitea serves a sparse Cargo index, and when that
    /// was checked.
    cargo_sparse: Mutex<std::collections::HashMap<String, (bool, std::time::Instant)>>,
}

/// How long the answer to "does Gitea serve a sparse Cargo index" is kept.
const CARGO_PROBE_TTL: Duration = Duration::from_secs(10 * 60);

impl Gitea {
    pub fn new(egress: &std::sync::Arc<Egress>) -> anyhow::Result<Self> {
        let client = egress
            .client()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("granary/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            client,
            status: Mutex::default(),
            cargo_sparse: Mutex::default(),
        })
    }

    pub fn status(&self) -> Option<Status> {
        self.status.lock().unwrap().clone()
    }

    /// A GET to Gitea's API (`path` starts with `/api/`), with the token.
    pub async fn api_get(&self, cfg: &Config, path: &str) -> Result<Value, ApiError> {
        let Some(g) = &cfg.gitea else {
            return Err(ApiError::new(None, "GRANARY_GITEA_URL is not set"));
        };
        let url = format!("{}{path}", g.url);
        let resp = self
            .client
            .get(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {}", g.token.expose()),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| ApiError::new(None, crate::error::error_chain(&e)))?;
        let status = resp.status();
        let code = Some(status.as_u16());
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ApiError::new(
                code,
                "Gitea refused the token (HTTP 401); check GRANARY_GITEA_TOKEN_FILE",
            ));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(ApiError::new(
                code,
                format!("the token may not read {path} (HTTP 403)"),
            ));
        }
        if status.is_redirection() {
            return Err(ApiError::new(
                code,
                format!(
                    "Gitea answered with a redirect (HTTP {}), which Granary doesn't follow; check GRANARY_GITEA_URL",
                    status.as_u16()
                ),
            ));
        }
        if !status.is_success() {
            return Err(ApiError::new(
                code,
                format!("{url} returned HTTP {}", status.as_u16()),
            ));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| ApiError::new(code, crate::error::error_chain(&e)))?;
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::new(code, format!("{url} did not return JSON: {e}")))
    }

    /// Whether the connected Gitea serves `owner`'s Cargo registry as a
    /// sparse index (its `config.json` answers); if not, Granary reads the
    /// git index instead. The answer is kept for a few minutes.
    pub async fn cargo_sparse(&self, cfg: &Config, owner: &str) -> Result<bool, String> {
        if let Some((sparse, at)) = self.cargo_sparse.lock().unwrap().get(owner)
            && at.elapsed() < CARGO_PROBE_TTL
        {
            return Ok(*sparse);
        }
        let sparse = match self
            .api_get(cfg, &format!("/api/packages/{owner}/cargo/config.json"))
            .await
        {
            Ok(_) => true,
            Err(e) if e.status == Some(404) => false,
            Err(e) => return Err(format!("Gitea's Cargo registry: {}", e.message)),
        };
        self.cargo_sparse
            .lock()
            .unwrap()
            .insert(owner.to_string(), (sparse, std::time::Instant::now()));
        Ok(sparse)
    }

    /// Check the connection: Gitea's version and the token's owner.
    pub async fn check(&self, cfg: &Config) -> Option<Status> {
        cfg.gitea.as_ref()?;
        let status = if cfg.offline {
            Status {
                error: Some("not contacted: offline mode".into()),
                checked_at: now_secs(),
                offline: true,
                ..Status::default()
            }
        } else {
            let (version, user) = tokio::join!(
                self.api_get(cfg, "/api/v1/version"),
                self.api_get(cfg, "/api/v1/user")
            );
            // A token with only the repository and package scopes may not
            // read its own account; that still counts as connected.
            let user = match user {
                Ok(u) => Ok((u["login"].as_str().map(str::to_string), None)),
                Err(e) if e.status == Some(403) => Ok((
                    None,
                    Some("the token can't read its own account (no read:user scope), so its owner isn't shown".to_string()),
                )),
                Err(e) => Err(e),
            };
            match (version, user) {
                (Ok(v), Ok((user, note))) => Status {
                    ok: true,
                    version: v["version"].as_str().map(str::to_string),
                    user,
                    note,
                    checked_at: now_secs(),
                    ..Status::default()
                },
                (Err(e), _) | (_, Err(e)) => Status {
                    error: Some(e.message),
                    checked_at: now_secs(),
                    ..Status::default()
                },
            }
        };
        if let Some(e) = &status.error
            && !status.offline
        {
            tracing::warn!("Gitea: {e}");
        }
        *self.status.lock().unwrap() = Some(status.clone());
        Some(status)
    }
}
