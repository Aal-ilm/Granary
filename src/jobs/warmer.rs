//! Talking to the warmer service.
//!
//! `POST /run` takes one ecosystem's manifests and streams the tools'
//! output back as plain text lines. The last line is
//! `##granary {"ok": ..., "error": ..., "lockfiles": {...}}`.
//! `DELETE /run/{job}` stops a job's tools. `GET /health` lists the tools.
//! Every request carries the shared secret as a bearer token.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{BUDGET_CHECK, Budget, JobLog};
use crate::{AppState, templates::lines::Eco};

/// Marks the result line at the end of a run.
const RESULT_PREFIX: &str = "##granary ";

#[derive(Serialize)]
pub struct RunRequest {
    pub job: i64,
    pub ecosystem: Eco,
    pub files: HashMap<String, String>,
    pub platforms: Vec<String>,
    pub allow_sdist: bool,
    pub python_version: String,
    /// How the warmer reaches Granary.
    pub granary_url: String,
    /// The warmer's access token for downloads.
    pub token: String,
    /// For a repo template's lockfile install: the checkout to run in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoCheckout>,
    /// Git hosts whose clones go through Granary (git dependencies).
    pub git_hosts: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepoCheckout {
    /// Granary's /git/ URL for the repo.
    pub url: String,
    pub commit: String,
    /// The lockfile's folder within the repo ("" for the root).
    pub dir: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RunResult {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// Lockfiles the tools wrote: path -> contents.
    #[serde(default)]
    pub lockfiles: HashMap<String, String>,
}

/// Why a run stopped early.
#[derive(Debug)]
pub enum Stop {
    Cancelled,
    Timeout,
    Budget,
    Error(String),
}

fn request(st: &AppState, method: reqwest::Method, path: &str) -> Option<reqwest::RequestBuilder> {
    let base = st.cfg.warmer_url.as_ref()?;
    let mut req = st
        .admin
        .jobs
        .client
        .request(method, format!("{base}{path}"));
    if let Some(secret) = &st.cfg.warmer_secret {
        req = req.bearer_auth(secret);
    }
    Some(req)
}

/// Run one ecosystem's tools, appending their output to the job log.
pub async fn run(
    st: &AppState,
    req: &RunRequest,
    log: &mut JobLog,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
    budget: Option<Budget>,
) -> Result<RunResult, Stop> {
    let db = &st.auth.db;
    let Some(http) = request(st, reqwest::Method::POST, "/run") else {
        return Err(Stop::Error("GRANARY_WARMER_URL is not set".into()));
    };
    let send = http.json_body(req).send();
    // The warmer may already have started the tools when a stop arrives
    // before its answer, so tell it to stop in that case too.
    let resp = tokio::select! {
        r = send => r.map_err(|e| Stop::Error(format!("the warmer could not be reached: {e}")))?,
        _ = cancel.cancelled() => {
            abort(st, req.job).await;
            return Err(Stop::Cancelled);
        }
        _ = tokio::time::sleep_until(deadline) => {
            abort(st, req.job).await;
            return Err(Stop::Timeout);
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Stop::Error(format!(
            "the warmer answered {status}: {}",
            body.trim()
        )));
    }

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut result: Option<RunResult> = None;
    let mut tick = tokio::time::interval(BUDGET_CHECK);
    let stop = loop {
        tokio::select! {
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    buf.extend_from_slice(&bytes);
                    while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=i).collect();
                        let line = String::from_utf8_lossy(&line).trim_end().to_string();
                        match line.strip_prefix(RESULT_PREFIX) {
                            Some(json) => result = serde_json::from_str(json).ok(),
                            None => log.push(db, &line).await,
                        }
                    }
                }
                Some(Err(e)) => break Some(Stop::Error(format!("lost the connection to the warmer: {e}"))),
                None => break None,
            },
            _ = cancel.cancelled() => break Some(Stop::Cancelled),
            _ = tokio::time::sleep_until(deadline) => break Some(Stop::Timeout),
            _ = tick.tick() => {
                if budget.is_some_and(|b| b.exceeded(st)) {
                    break Some(Stop::Budget);
                }
            }
        }
    };
    if let Some(stop) = stop {
        if !matches!(stop, Stop::Error(_)) {
            abort(st, req.job).await;
        }
        return Err(stop);
    }
    if !buf.is_empty() {
        log.push(db, String::from_utf8_lossy(&buf).trim_end()).await;
    }
    // A budget overrun in the last moments still counts.
    if budget.is_some_and(|b| b.exceeded(st)) {
        return Err(Stop::Budget);
    }
    result.ok_or_else(|| Stop::Error("the warmer stopped without reporting a result".into()))
}

/// Ask the warmer to stop a job's tools.
async fn abort(st: &AppState, job: i64) {
    if let Some(req) = request(st, reqwest::Method::DELETE, &format!("/run/{job}"))
        && let Err(e) = req.timeout(Duration::from_secs(10)).send().await
    {
        tracing::warn!("could not ask the warmer to stop job {job}: {e}");
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Health {
    pub configured: bool,
    pub available: bool,
    pub tools: HashMap<String, String>,
    pub error: Option<String>,
}

/// Whether the warmer is configured and answering. Cached for 30 seconds.
pub async fn health(st: &AppState) -> Health {
    if let Some((at, h)) = st.admin.jobs.health.lock().unwrap().as_ref()
        && at.elapsed() < Duration::from_secs(30)
    {
        return h.clone();
    }
    let h = match request(st, reqwest::Method::GET, "/health") {
        None => Health::default(),
        Some(req) => match req.timeout(Duration::from_secs(5)).send().await {
            Ok(r) if r.status().is_success() => {
                #[derive(Deserialize)]
                struct Body {
                    #[serde(default)]
                    tools: HashMap<String, String>,
                }
                let body: Body = r
                    .bytes()
                    .await
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or(Body {
                        tools: HashMap::new(),
                    });
                Health {
                    configured: true,
                    available: true,
                    tools: body.tools,
                    error: None,
                }
            }
            Ok(r) => Health {
                configured: true,
                error: Some(format!("the warmer answered {}", r.status())),
                ..Default::default()
            },
            Err(e) => Health {
                configured: true,
                error: Some(format!("the warmer is not reachable: {e}")),
                ..Default::default()
            },
        },
    };
    *st.admin.jobs.health.lock().unwrap() = Some((Instant::now(), h.clone()));
    h
}

trait JsonBody {
    fn json_body<T: Serialize>(self, body: &T) -> Self;
}

impl JsonBody for reqwest::RequestBuilder {
    /// reqwest is built without its `json` feature; send JSON by hand.
    fn json_body<T: Serialize>(self, body: &T) -> Self {
        self.header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(body).unwrap())
    }
}
