//! Warm jobs: fill the cache with every package a template needs, including
//! transitive dependencies, by running the real package tools in the
//! warmer container. The tools talk only to Granary, so everything they
//! download is cached on the way through the normal proxy paths.
//!
//! Jobs are queued in SQLite and run at most `GRANARY_WARM_JOBS` at once.
//! A job stops when it is cancelled, after `GRANARY_WARM_TIMEOUT_MINS`, or
//! when the cache reaches `GRANARY_CACHE_LIMIT`. The last 200 jobs are kept.

pub mod api;
pub mod warmer;

use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    AppState,
    admin::human_bytes,
    inventory::{self, REPOS},
    policy::Resource,
    search,
    store::{dir_size, now_secs},
    templates::{TemplateInput, build, lines::Eco},
};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN ('template', 'package', 'git')),
    template_id INTEGER,
    title TEXT NOT NULL,
    spec TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'done', 'failed', 'cancelled')),
    message TEXT,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    new_packages INTEGER,
    new_files INTEGER,
    new_bytes INTEGER,
    lockfiles TEXT
)";

pub const LOG_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS job_log (
    job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    n INTEGER NOT NULL,
    at INTEGER NOT NULL,
    line TEXT NOT NULL,
    PRIMARY KEY (job_id, n)
)";

/// Finished jobs kept; older ones are deleted with their logs.
const KEEP_JOBS: i64 = 200;
/// Log lines kept per job.
const MAX_LOG_LINES: i64 = 5000;
/// How often a running job checks the cache budget.
const BUDGET_CHECK: Duration = Duration::from_secs(2);

/// Run-time state: the queue's wake-up signal and the running jobs.
pub struct Jobs {
    wake: Notify,
    running: Mutex<HashMap<i64, CancellationToken>>,
    /// For long-lived streaming requests to the warmer (no overall timeout).
    client: reqwest::Client,
    health: Mutex<Option<(Instant, warmer::Health)>>,
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            wake: Notify::new(),
            running: Mutex::default(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client"),
            health: Mutex::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Done,
    Failed,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }
}

/// How a job ended.
struct Outcome {
    status: Status,
    message: Option<String>,
    lockfiles: HashMap<String, HashMap<String, String>>,
    /// What went differently than asked without failing the job, such as a
    /// lockfile that had to be installed without being used as it was.
    warnings: Vec<String>,
}

impl Outcome {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: Status::Failed,
            message: Some(message.into()),
            lockfiles: HashMap::new(),
            warnings: Vec::new(),
        }
    }
}

/// Stored warnings (a JSON list in a text column) as a list in responses.
pub fn warnings_list<S: serde::Serializer>(v: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    let list: Vec<String> = v
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();
    s.collect_seq(list)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct JobRow {
    pub id: i64,
    pub kind: String,
    pub template_id: Option<i64>,
    pub title: String,
    pub status: String,
    pub message: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub new_packages: Option<i64>,
    pub new_files: Option<i64>,
    pub new_bytes: Option<i64>,
    #[serde(serialize_with = "warnings_list")]
    pub warnings: Option<String>,
    /// The private content the job works on (a JSON resource), if any.
    #[serde(serialize_with = "is_set")]
    pub private: Option<String>,
}

impl JobRow {
    pub fn resource(&self) -> Option<Resource> {
        self.private
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok())
    }
}

/// Whether a column is set, as a boolean in responses.
fn is_set<S: serde::Serializer>(v: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bool(v.is_some())
}

pub const JOB_COLUMNS: &str = "id, kind, template_id, title, status, message, created_by, created_at, started_at, finished_at, new_packages, new_files, new_bytes, warnings, private";

/// Who a job is for: the name it shows, and the user whose private access
/// its warmer token gets (none for a schedule nobody owns any more).
pub struct JobBy {
    pub name: String,
    pub user: Option<i64>,
}

impl JobBy {
    pub fn user(u: &crate::auth::User) -> Self {
        Self {
            name: u.username.clone(),
            user: Some(u.id),
        }
    }
}

/// Queue a job. Returns its id.
/// What a warm job installs: a template, and for templates made from a
/// repo, the commit whose lockfiles to use.
#[derive(Clone, Debug, Default, serde::Deserialize, Serialize)]
pub struct WarmSpec {
    #[serde(flatten)]
    pub input: TemplateInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoWarm>,
}

/// A repo template's checkout: the warmer clones it through Granary's git
/// cache and runs the lockfile installs in each lockfile's folder.
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct RepoWarm {
    /// `https://{host}/{owner}/{repo}`
    pub url: String,
    pub commit: String,
    pub locks: Vec<LockDir>,
}

#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct LockDir {
    pub ecosystem: Eco,
    /// Folder holding the lockfile, relative to the repo root ("" for the root).
    pub dir: String,
}

impl RepoWarm {
    /// From a template's source; `None` when it has no lockfiles to use.
    pub fn from_source(s: &crate::templates::RepoSource) -> Option<Self> {
        let locks: Vec<LockDir> = s
            .lockfiles()
            .into_iter()
            .filter_map(|p| {
                let (dir, name) = p.rsplit_once('/').unwrap_or(("", p));
                let ecosystem = match name {
                    "Cargo.lock" => Eco::Cargo,
                    "package-lock.json" => Eco::Npm,
                    _ => return None,
                };
                Some(LockDir {
                    ecosystem,
                    dir: dir.to_string(),
                })
            })
            .collect();
        (!locks.is_empty()).then(|| Self {
            url: s.url.clone(),
            commit: s.commit.clone(),
            locks,
        })
    }
}

/// What a new warm job holds.
pub struct NewJob<'a> {
    pub kind: &'a str,
    pub template_id: Option<i64>,
    pub title: &'a str,
    pub spec: &'a TemplateInput,
    pub repo: Option<RepoWarm>,
    /// The private content it works on, so only those who may read that
    /// see the job.
    pub private: Option<Resource>,
}

pub async fn enqueue(st: &AppState, job: NewJob<'_>, by: &JobBy) -> sqlx::Result<i64> {
    let spec = WarmSpec {
        input: job.spec.clone(),
        repo: job.repo,
    };
    let r = sqlx::query(
        "INSERT INTO jobs (kind, template_id, title, spec, status, created_by, created_at, acting_for, private)
         VALUES (?, ?, ?, ?, 'queued', ?, ?, ?, ?)",
    )
    .bind(job.kind)
    .bind(job.template_id)
    .bind(job.title)
    .bind(serde_json::to_string(&spec).unwrap())
    .bind(&by.name)
    .bind(now_secs() as i64)
    .bind(by.user)
    .bind(job.private.map(|r| serde_json::to_string(&r).unwrap()))
    .execute(&st.auth.db)
    .await?;
    st.admin.jobs.wake.notify_one();
    Ok(r.last_insert_rowid())
}

/// Cancel a queued or running job. Returns false if it had already ended.
pub async fn cancel(st: &AppState, id: i64, by: &str) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "UPDATE jobs SET status = 'cancelled', message = ?, finished_at = ? WHERE id = ? AND status = 'queued'",
    )
    .bind(format!("Cancelled by {by} before it started"))
    .bind(now_secs() as i64)
    .bind(id)
    .execute(&st.auth.db)
    .await?;
    if r.rows_affected() == 1 {
        return Ok(true);
    }
    let token = st.admin.jobs.running.lock().unwrap().get(&id).cloned();
    match token {
        Some(t) => {
            let mut log = JobLog::resume(&st.auth.db, id).await;
            log.push(&st.auth.db, &format!("Cancel requested by {by}"))
                .await;
            t.cancel();
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Start the queue. Jobs left running by a previous process are marked failed.
pub fn spawn_runner(st: AppState) {
    tokio::spawn(async move {
        let _ = sqlx::query(
            "UPDATE jobs SET status = 'failed', message = 'Granary restarted while this job was running', finished_at = ?
             WHERE status = 'running'",
        )
        .bind(now_secs() as i64)
        .execute(&st.auth.db)
        .await;
        // Tokens of jobs that were running when Granary stopped.
        if let Err(e) = st.auth.drop_job_token(None).await {
            tracing::warn!("removing old job tokens: {e}");
        }
        let mut last_schedule_check: Option<Instant> = None;
        loop {
            if last_schedule_check.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                last_schedule_check = Some(Instant::now());
                queue_scheduled(&st).await;
            }
            start_due(&st).await;
            tokio::select! {
                _ = st.admin.jobs.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    });
}

async fn start_due(st: &AppState) {
    loop {
        if st.admin.jobs.running.lock().unwrap().len() >= st.cfg.warm_jobs {
            return;
        }
        let next: Option<(i64,)> =
            match sqlx::query_as("SELECT id FROM jobs WHERE status = 'queued' ORDER BY id LIMIT 1")
                .fetch_optional(&st.auth.db)
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("job queue: {e}");
                    return;
                }
            };
        let Some((id,)) = next else { return };
        let claimed = sqlx::query(
            "UPDATE jobs SET status = 'running', started_at = ? WHERE id = ? AND status = 'queued'",
        )
        .bind(now_secs() as i64)
        .bind(id)
        .execute(&st.auth.db)
        .await
        .is_ok_and(|r| r.rows_affected() == 1);
        if !claimed {
            continue;
        }
        let token = CancellationToken::new();
        st.admin
            .jobs
            .running
            .lock()
            .unwrap()
            .insert(id, token.clone());
        let st = st.clone();
        tokio::spawn(async move {
            run(&st, id, token).await;
            st.admin.jobs.running.lock().unwrap().remove(&id);
            st.admin.jobs.wake.notify_one();
        });
    }
}

/// Appends a job's log lines, numbered, up to the per-job limit.
pub struct JobLog {
    id: i64,
    n: i64,
}

impl JobLog {
    async fn resume(db: &SqlitePool, id: i64) -> Self {
        let (n,): (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(n), 0) FROM job_log WHERE job_id = ?")
                .bind(id)
                .fetch_one(db)
                .await
                .unwrap_or((0,));
        Self { id, n }
    }

    pub async fn push(&mut self, db: &SqlitePool, line: &str) {
        if self.n > MAX_LOG_LINES {
            return;
        }
        self.n += 1;
        let line = if self.n > MAX_LOG_LINES {
            "(the log is too long; later lines were not kept)".to_string()
        } else {
            line.chars().take(2000).collect()
        };
        if let Err(e) = sqlx::query("INSERT INTO job_log (job_id, n, at, line) VALUES (?, ?, ?, ?)")
            .bind(self.id)
            .bind(self.n)
            .bind(now_secs() as i64)
            .bind(line)
            .execute(db)
            .await
        {
            tracing::warn!("job {}: could not save a log line: {e}", self.id);
        }
    }
}

/// Every cached file: "repo/path" -> (package, size).
async fn snapshot(st: &AppState) -> HashMap<String, (String, u64)> {
    let store = st.store.clone();
    tokio::task::spawn_blocking(move || {
        let access = HashMap::new();
        let mut out = HashMap::new();
        let namespaces = REPOS
            .iter()
            .map(|r| (*r, *r, false))
            .chain(inventory::PRIVATE.iter().map(|(r, ns)| (*r, *ns, true)));
        for (repo, ns, private) in namespaces {
            for a in inventory::scan_repo(repo, ns, &store.files_dir(ns), &access, private) {
                out.insert(
                    format!("{ns}/{}", a.path),
                    (format!("{ns}/{}", a.package), a.size),
                );
            }
        }
        out
    })
    .await
    .unwrap_or_default()
}

/// Bytes the cache uses now: files plus metadata.
pub async fn cache_used(st: &AppState) -> u64 {
    let root = st.store.root().to_path_buf();
    tokio::task::spawn_blocking(move || {
        dir_size(&root.join("files")) + dir_size(&root.join("meta")) + dir_size(&root.join("git"))
    })
    .await
    .unwrap_or(0)
}

/// The budget a running job checks against.
#[derive(Clone, Copy)]
pub struct Budget {
    limit: u64,
    used_at_start: u64,
    counter_at_start: u64,
}

impl Budget {
    pub fn exceeded(&self, st: &AppState) -> bool {
        let added = st
            .store
            .stats
            .bytes_cached
            .load(Ordering::Relaxed)
            .saturating_sub(self.counter_at_start);
        self.used_at_start + added >= self.limit
    }
}

async fn run(st: &AppState, id: i64, cancel: CancellationToken) {
    let db = &st.auth.db;
    let mut log = JobLog::resume(db, id).await;
    let kind: Option<(String,)> = sqlx::query_as("SELECT kind FROM jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if kind.is_some_and(|(k,)| k == "git") {
        return run_git(st, id, &mut log, &cancel).await;
    }
    let before = snapshot(st).await;
    let outcome = run_inner(st, id, &mut log, &cancel).await;
    if let Err(e) = st.auth.drop_job_token(Some(id)).await {
        tracing::warn!("job {id}: could not remove its token: {e}");
    }
    let after = snapshot(st).await;

    let new: Vec<&(String, u64)> = after
        .iter()
        .filter(|(k, _)| !before.contains_key(*k))
        .map(|(_, v)| v)
        .collect();
    let packages: HashSet<&str> = new.iter().map(|(p, _)| p.as_str()).collect();
    let bytes: u64 = new.iter().map(|(_, s)| s).sum();
    log.push(
        db,
        &format!(
            "{}: {} new packages, {} new files, {}",
            match outcome.status {
                Status::Done => "Finished",
                Status::Failed => "Failed",
                Status::Cancelled => "Cancelled",
            },
            packages.len(),
            new.len(),
            human_bytes(bytes)
        ),
    )
    .await;

    let lockfiles =
        (!outcome.lockfiles.is_empty()).then(|| serde_json::to_string(&outcome.lockfiles).unwrap());
    let warnings =
        (!outcome.warnings.is_empty()).then(|| serde_json::to_string(&outcome.warnings).unwrap());
    if !outcome.warnings.is_empty() {
        log.push(
            db,
            &format!(
                "Finished with {} warning(s); see above.",
                outcome.warnings.len()
            ),
        )
        .await;
    }
    let _ = sqlx::query(
        "UPDATE jobs SET status = ?, message = ?, finished_at = ?, new_packages = ?, new_files = ?, new_bytes = ?, lockfiles = ?, warnings = ?
         WHERE id = ?",
    )
    .bind(outcome.status.as_str())
    .bind(&outcome.message)
    .bind(now_secs() as i64)
    .bind(packages.len() as i64)
    .bind(new.len() as i64)
    .bind(bytes as i64)
    .bind(lockfiles)
    .bind(&warnings)
    .bind(id)
    .execute(db)
    .await;
    if outcome.status == Status::Done {
        // The template card shows the latest warm's warnings, or none.
        let _ = sqlx::query(
            "UPDATE templates SET last_warmed_at = ?, last_warm_warnings = ?
             WHERE id = (SELECT template_id FROM jobs WHERE id = ? AND kind = 'template')",
        )
        .bind(now_secs() as i64)
        .bind(&warnings)
        .bind(id)
        .execute(db)
        .await;
    }
    let _ = sqlx::query(
        "DELETE FROM jobs WHERE status NOT IN ('queued', 'running')
         AND id NOT IN (SELECT id FROM jobs ORDER BY id DESC LIMIT ?)",
    )
    .bind(KEEP_JOBS)
    .execute(db)
    .await;
    st.admin.invalidate_inventory();
    tracing::info!(
        "warm job {id} {}: {} new files, {}",
        outcome.status.as_str(),
        new.len(),
        human_bytes(bytes)
    );
}

async fn run_inner(
    st: &AppState,
    id: i64,
    log: &mut JobLog,
    cancel: &CancellationToken,
) -> Outcome {
    let db = &st.auth.db;
    let row: Option<(String, String)> = sqlx::query_as("SELECT title, spec FROM jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some((title, spec)) = row else {
        return Outcome::failed("the job disappeared");
    };
    let WarmSpec { input: spec, repo } = match serde_json::from_str(&spec) {
        Ok(s) => s,
        Err(e) => return Outcome::failed(format!("the job's template could not be read: {e}")),
    };
    log.push(db, &format!("Warming {title}")).await;

    if st.cfg.warmer_url.is_none() {
        return Outcome::failed("warming needs the warmer service (GRANARY_WARMER_URL is not set)");
    }
    if st.cfg.offline {
        return Outcome::failed("offline mode is on, so nothing new can be fetched");
    }
    let budget = match st.cfg.cache_limit {
        Some(limit) => {
            let used = cache_used(st).await;
            if used >= limit {
                return Outcome::failed(format!(
                    "the cache already uses {} of its {} budget (GRANARY_CACHE_LIMIT)",
                    human_bytes(used),
                    human_bytes(limit)
                ));
            }
            log.push(
                db,
                &format!(
                    "Cache budget: {} of {} used",
                    human_bytes(used),
                    human_bytes(limit)
                ),
            )
            .await;
            Some(Budget {
                limit,
                used_at_start: used,
                counter_at_start: st.store.stats.bytes_cached.load(Ordering::Relaxed),
            })
        }
        None => None,
    };
    let token = match warmer_token(st, id).await {
        Ok(t) => t,
        Err(e) => return Outcome::failed(format!("could not get the warmer's access token: {e}")),
    };
    let cargo_private = cargo_private_index(st, id).await;

    // Maven needs versions; use the latest release for lines without one.
    let mut maven_versions = HashMap::new();
    for ga in build::unversioned_maven(&spec) {
        match search::lookup(st, Eco::Maven, &ga).await {
            Ok(Some(v)) if v.latest.is_some() => {
                let latest = v.latest.unwrap();
                log.push(
                    db,
                    &format!("Maven: {ga} has no version, using the latest release, {latest}"),
                )
                .await;
                maven_versions.insert(ga, latest);
            }
            Ok(_) => return Outcome::failed(format!("Maven: no release of {ga} was found")),
            Err(e) => {
                return Outcome::failed(format!("Maven: could not look up {ga}: {}", e.message));
            }
        }
    }
    let manifests = match build::manifests(&spec, &maven_versions) {
        Ok(m) => m,
        Err(e) => return Outcome::failed(e),
    };

    // A repo template with lockfiles installs exactly those versions, in a
    // checkout of the repo at its commit; other ecosystems use the lines.
    let mut steps: Vec<Step> = Vec::new();
    if let Some(r) = &repo {
        let checkout = match prepare_checkout(st, r, log).await {
            Ok(url) => url,
            Err(e) => return Outcome::failed(e),
        };
        for lock in &r.locks {
            steps.push(Step::Lock {
                eco: lock.ecosystem,
                repo: warmer::RepoCheckout {
                    url: checkout.clone(),
                    commit: r.commit.clone(),
                    dir: lock.dir.clone(),
                },
            });
        }
    }
    let locked: Vec<Eco> = steps.iter().map(Step::eco).collect();
    steps.extend(
        manifests
            .into_iter()
            .filter(|m| !locked.contains(&m.eco))
            .map(Step::Manifest),
    );

    let deadline = tokio::time::Instant::now() + st.cfg.warm_timeout;
    let mut lockfiles = HashMap::new();
    let mut failures = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for step in steps {
        let eco = step.eco();
        let label = step.label();
        log.push(db, &format!("--- {label} ({})", spec.platforms.join(", ")))
            .await;
        let (files, checkout) = match step {
            Step::Manifest(m) => (m.files, None),
            Step::Lock { repo, .. } => (Vec::new(), Some(repo)),
        };
        // Maven has no lockfile; keep the pom.xml that was warmed, with
        // every version filled in, for the template's starter.
        let warmed_pom = (eco == Eco::Maven)
            .then(|| files.iter().find(|(p, _)| p == "pom.xml").cloned())
            .flatten();
        let req = warmer::RunRequest {
            job: id,
            ecosystem: eco,
            files: files.into_iter().collect(),
            platforms: spec.platforms.clone(),
            allow_sdist: spec.allow_sdist,
            python_version: st.cfg.warm_python.clone(),
            granary_url: st.cfg.warmer_granary_url.clone(),
            token: token.clone(),
            repo: checkout,
            git_hosts: st.cfg.git_hosts.clone(),
            cargo_private: (eco == Eco::Cargo).then(|| cargo_private.clone()).flatten(),
        };
        match warmer::run(st, &req, log, cancel, deadline, budget).await {
            Ok(r) if r.ok => {
                warnings.extend(r.warnings);
                let mut locks = r.lockfiles;
                locks.extend(warmed_pom);
                lockfiles.insert(eco.as_str().to_string(), locks);
            }
            Ok(r) => {
                warnings.extend(r.warnings.clone());
                let why = r
                    .error
                    .unwrap_or_else(|| "the tools reported an error".into());
                log.push(db, &format!("{label} failed: {why}")).await;
                failures.push(format!("{label}: {why}"));
            }
            Err(warmer::Stop::Cancelled) => {
                return Outcome {
                    status: Status::Cancelled,
                    message: Some("Cancelled while running".into()),
                    lockfiles: HashMap::new(),
                    warnings,
                };
            }
            Err(warmer::Stop::Timeout) => {
                return Outcome::failed(format!(
                    "stopped after the {} minute time limit (GRANARY_WARM_TIMEOUT_MINS)",
                    st.cfg.warm_timeout.as_secs() / 60
                ));
            }
            Err(warmer::Stop::Budget) => {
                return Outcome::failed(format!(
                    "stopped because the cache reached its {} budget (GRANARY_CACHE_LIMIT)",
                    human_bytes(st.cfg.cache_limit.unwrap_or(0))
                ));
            }
            Err(warmer::Stop::Error(e)) => {
                log.push(db, &format!("{label} failed: {e}")).await;
                failures.push(format!("{label}: {e}"));
            }
        }
    }
    if failures.is_empty() {
        Outcome {
            status: Status::Done,
            message: None,
            lockfiles,
            warnings,
        }
    } else {
        Outcome {
            status: Status::Failed,
            message: Some(failures.join("; ")),
            lockfiles,
            warnings,
        }
    }
}

/// The warmer's token for a job. A job started by someone gets a token of
/// its own that acts for them, so the warmer reads private content only if
/// they may, and only while the job runs. Others use the standing token,
/// which never reads private content.
async fn warmer_token(st: &AppState, id: i64) -> anyhow::Result<String> {
    let starter: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT acting_for FROM jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&st.auth.db)
            .await?;
    match starter.and_then(|(u,)| u) {
        // A little longer than the job may run; it is removed when it ends.
        Some(user) => {
            st.auth
                .job_token(id, user, st.cfg.warm_timeout + Duration::from_secs(600))
                .await
        }
        None => st.auth.warmer_token().await,
    }
}

/// The private Cargo registry's index for the warmer, when there is one and
/// the job's starter may read it (the job's token acts for them, so the
/// registry would refuse it otherwise).
async fn cargo_private_index(st: &AppState, id: i64) -> Option<String> {
    st.cfg.gitea.as_ref()?;
    let owner = st.namespaces.cargo_owner()?;
    let (acting_for,): (Option<i64>,) = sqlx::query_as("SELECT acting_for FROM jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(&st.auth.db)
        .await
        .ok()??;
    let starter = st.auth.user_by_id(acting_for?).await.ok()??;
    let who = crate::policy::Principal::Job { starter };
    let registry = Resource::gitea_package("cargo", &owner, "");
    st.policy
        .can_read_private(&who, &registry)
        .then(|| format!("{}/cargo-private/", st.cfg.warmer_granary_url))
}

/// One run of the warmer within a job.
enum Step {
    /// Generated manifests from the template's lines.
    Manifest(build::Manifest),
    /// A lockfile install in the repo's checkout.
    Lock {
        eco: Eco,
        repo: warmer::RepoCheckout,
    },
}

impl Step {
    fn eco(&self) -> Eco {
        match self {
            Step::Manifest(m) => m.eco,
            Step::Lock { eco, .. } => *eco,
        }
    }

    fn label(&self) -> String {
        match self {
            Step::Manifest(m) => m.eco.label().to_string(),
            Step::Lock { eco, repo } => {
                let file = if *eco == Eco::Cargo {
                    "Cargo.lock"
                } else {
                    "package-lock.json"
                };
                let at = if repo.dir.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", repo.dir)
                };
                format!("{} from the repo's {file}{at}", eco.label())
            }
        }
    }
}

/// Make sure the repo and its commit are in the git cache; returns the URL
/// the warmer clones it from (Granary's own /git/ path, never upstream).
async fn prepare_checkout(st: &AppState, r: &RepoWarm, log: &mut JobLog) -> Result<String, String> {
    use crate::formats::git::{self, Refresh, RepoId};
    let id = RepoId::from_url(&st.cfg, &r.url)?;
    if !git::has_commit(st, &id, &r.commit).await {
        log.push(
            &st.auth.db,
            &format!("Fetching {} into the git cache", id.display()),
        )
        .await;
        let refresh = if id.dir(st).exists() {
            Refresh::Now
        } else {
            Refresh::Never
        };
        git::ensure(st, &id, refresh, None)
            .await
            .map_err(|e| e.message)?;
        if !git::has_commit(st, &id, &r.commit).await {
            return Err(format!(
                "{} no longer has commit {}",
                id.display(),
                &r.commit[..12.min(r.commit.len())]
            ));
        }
    }
    Ok(format!("{}/git/{}", st.cfg.warmer_granary_url, id.rel()))
}

/// What a git job does.
#[derive(Debug, serde::Deserialize, Serialize)]
pub struct GitJob {
    /// "clone" (cache it, or refresh it if already cached) or "refresh".
    pub action: String,
    pub repo: crate::formats::git::RepoId,
}

/// Queue a git job ("Add repo" or "Refresh").
pub async fn enqueue_git(st: &AppState, job: &GitJob, by: &JobBy) -> sqlx::Result<i64> {
    let title = format!("Git {}", job.repo.display());
    let private = job.repo.resource(&st.cfg);
    let r = sqlx::query(
        "INSERT INTO jobs (kind, title, spec, status, created_by, created_at, acting_for, private)
         VALUES ('git', ?, ?, 'queued', ?, ?, ?, ?)",
    )
    .bind(title)
    .bind(serde_json::to_string(job).unwrap())
    .bind(&by.name)
    .bind(now_secs() as i64)
    .bind(by.user)
    .bind(private.map(|r| serde_json::to_string(&r).unwrap()))
    .execute(&st.auth.db)
    .await?;
    st.admin.jobs.wake.notify_one();
    Ok(r.last_insert_rowid())
}

/// Clone or refresh a git repo in Granary itself, streaming git's progress
/// into the job log.
async fn run_git(st: &AppState, id: i64, log: &mut JobLog, cancel: &CancellationToken) {
    use crate::formats::git::{self, Refresh};
    let db = &st.auth.db;
    let spec: Option<(String,)> = sqlx::query_as("SELECT spec FROM jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    let Some(job) = spec.and_then(|(s,)| serde_json::from_str::<GitJob>(&s).ok()) else {
        finish_git(
            st,
            id,
            Status::Failed,
            Some("the job could not be read".into()),
            0,
            0,
        )
        .await;
        return;
    };
    let dir = job.repo.dir(st);
    let existed = tokio::fs::try_exists(&dir).await.unwrap_or(false);
    let size = |dir: std::path::PathBuf| async move {
        tokio::task::spawn_blocking(move || dir_size(&dir))
            .await
            .unwrap_or(0)
    };
    let before = if existed { size(dir.clone()).await } else { 0 };
    log.push(
        db,
        &format!(
            "{} {} from {}",
            if existed { "Refreshing" } else { "Cloning" },
            job.repo.display(),
            job.repo.upstream(&st.cfg)
        ),
    )
    .await;
    log.push(db, "Submodules and Git LFS objects are not fetched.")
        .await;

    // git's progress arrives on a channel while the job log is written here.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let work = git::ensure(st, &job.repo, Refresh::Now, Some(&tx));
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            r = &mut work => break Some(r),
            Some(line) = rx.recv() => push_git_line(db, log, &line).await,
            _ = cancel.cancelled() => break None,
        }
    };
    // Lines sent just before git finished.
    while let Ok(line) = rx.try_recv() {
        push_git_line(db, log, &line).await;
    }
    let after = if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        size(dir).await
    } else {
        0
    };
    let added = after.saturating_sub(before);
    let (status, message) = match result {
        None => (
            Status::Cancelled,
            Some("Cancelled while running".to_string()),
        ),
        Some(Ok(())) => (Status::Done, None),
        Some(Err(e)) => (Status::Failed, Some(e.message)),
    };
    log.push(
        db,
        &format!(
            "{}: {} newly cached, {} in total",
            match status {
                Status::Done => "Finished",
                Status::Failed => "Failed",
                Status::Cancelled => "Cancelled",
            },
            human_bytes(added),
            human_bytes(after)
        ),
    )
    .await;
    finish_git(
        st,
        id,
        status,
        message,
        i64::from(!existed && after > 0),
        added,
    )
    .await;
}

/// git's "Cloning into '<server path>'" line only reveals where Granary
/// keeps its data; the job's first line already says what's being cloned.
async fn push_git_line(db: &SqlitePool, log: &mut JobLog, line: &str) {
    if !line.starts_with("Cloning into") {
        log.push(db, line).await;
    }
}

async fn finish_git(
    st: &AppState,
    id: i64,
    status: Status,
    message: Option<String>,
    repos: i64,
    bytes: u64,
) {
    let _ = sqlx::query(
        "UPDATE jobs SET status = ?, message = ?, finished_at = ?, new_packages = ?, new_files = 0, new_bytes = ? WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(message)
    .bind(now_secs() as i64)
    .bind(repos)
    .bind(bytes as i64)
    .bind(id)
    .execute(&st.auth.db)
    .await;
}

/// Databases from before git jobs only allow two kinds of job, and SQLite
/// can't change a CHECK constraint in place, so the table is rebuilt with
/// every job kept. Foreign keys are off meanwhile so job logs survive.
pub async fn migrate(db: &SqlitePool) -> sqlx::Result<()> {
    allow_git_jobs(db).await?;
    // Added after the rebuild above, which copies columns by position from
    // older databases.
    crate::auth::add_column(db, "jobs", "warnings", "TEXT").await?;
    crate::auth::add_column(db, "jobs", "private", "TEXT").await?;
    crate::auth::add_column(
        db,
        "jobs",
        "acting_for",
        "INTEGER REFERENCES users(id) ON DELETE SET NULL",
    )
    .await
}

async fn allow_git_jobs(db: &SqlitePool) -> sqlx::Result<()> {
    let sql: Option<(String,)> =
        sqlx::query_as("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'jobs'")
            .fetch_optional(db)
            .await?;
    if sql.is_none_or(|(s,)| s.contains("'git'")) {
        return Ok(());
    }
    let mut conn = db.acquire().await?;
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await?;
    let result = async {
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        sqlx::query(sqlx::AssertSqlSafe(SCHEMA.replace(
            "CREATE TABLE IF NOT EXISTS jobs",
            "CREATE TABLE jobs_new",
        )))
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO jobs_new SELECT * FROM jobs")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE jobs").execute(&mut *tx).await?;
        sqlx::query("ALTER TABLE jobs_new RENAME TO jobs")
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await?;
    result?;
    tracing::info!("jobs table updated for git jobs");
    Ok(())
}

/// Whether a template on a schedule is due: its interval has passed since
/// it was last warmed or last queued by the schedule, whichever is later.
pub fn schedule_due(
    schedule: &str,
    last_warmed: Option<i64>,
    last_queued: Option<i64>,
    now: i64,
) -> bool {
    let Some(&(_, every)) = crate::templates::SCHEDULES
        .iter()
        .find(|(id, secs)| *id == schedule && *secs > 0)
    else {
        return false;
    };
    let last = last_warmed.max(last_queued).unwrap_or(0);
    now - last >= every as i64
}

/// Queue a warm job for every scheduled template that is due. Nothing is
/// queued while the warmer is unavailable or offline mode is on; the
/// templates are simply picked up once it's back.
pub async fn queue_scheduled(st: &AppState) -> usize {
    if st.cfg.offline || st.cfg.warmer_url.is_none() {
        return 0;
    }
    let due: Vec<(i64, String, Option<i64>, Option<i64>)> = match sqlx::query_as(
        "SELECT t.id, t.schedule, t.last_warmed_at, t.schedule_queued_at FROM templates t
         WHERE t.schedule != 'off'
         AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.template_id = t.id AND j.status IN ('queued', 'running'))",
    )
    .fetch_all(&st.auth.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("template schedule: {e}");
            return 0;
        }
    };
    let now = now_secs() as i64;
    let due: Vec<i64> = due
        .into_iter()
        .filter(|(_, schedule, warmed, queued)| schedule_due(schedule, *warmed, *queued, now))
        .map(|(id, ..)| id)
        .collect();
    if due.is_empty() || !warmer::health(st).await.available {
        return 0;
    }
    let mut queued = 0;
    for id in due {
        let Ok(Some(t)) = crate::templates::get(&st.auth.db, id).await else {
            continue;
        };
        let _ = sqlx::query("UPDATE templates SET schedule_queued_at = ? WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(&st.auth.db)
            .await;
        let repo = t.source.as_ref().and_then(RepoWarm::from_source);
        // Scheduled warms act for whoever set the schedule.
        let by = JobBy {
            name: "schedule".into(),
            user: crate::templates::schedule_by(&st.auth.db, id)
                .await
                .ok()
                .flatten(),
        };
        let job = NewJob {
            kind: "template",
            template_id: Some(id),
            title: &t.input.name,
            spec: &t.input,
            repo,
            private: t.resource(&st.cfg),
        };
        match enqueue(st, job, &by).await {
            Ok(job) => {
                tracing::info!(
                    "{}: scheduled {} warm queued as job {job}",
                    t.input.name,
                    t.schedule
                );
                queued += 1;
            }
            Err(e) => tracing::warn!("{}: could not queue the scheduled warm: {e}", t.input.name),
        }
    }
    queued
}

/// The lockfiles from a template's last successful warm, per ecosystem,
/// and when that warm finished.
pub async fn last_lockfiles(
    db: &SqlitePool,
    template_id: i64,
) -> (HashMap<String, HashMap<String, String>>, Option<i64>) {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT lockfiles, finished_at FROM jobs WHERE template_id = ? AND kind = 'template' AND status = 'done'
         AND lockfiles IS NOT NULL ORDER BY id DESC LIMIT 1",
    )
    .bind(template_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    match row {
        Some((json, at)) => (serde_json::from_str(&json).unwrap_or_default(), at),
        None => (HashMap::new(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::schedule_due;

    #[test]
    fn schedules() {
        let day = 86_400;
        assert!(!schedule_due("off", None, None, 10 * day));
        assert!(
            schedule_due("daily", None, None, 10 * day),
            "never warmed: due now"
        );
        assert!(!schedule_due(
            "daily",
            Some(10 * day),
            None,
            10 * day + 3600
        ));
        assert!(schedule_due("daily", Some(10 * day), None, 11 * day));
        // Queued (even if that run failed) counts, so a broken template
        // isn't retried every minute.
        assert!(!schedule_due(
            "daily",
            Some(day),
            Some(10 * day),
            10 * day + 60
        ));
        assert!(!schedule_due("weekly", Some(10 * day), None, 16 * day));
        assert!(schedule_due("weekly", Some(10 * day), None, 17 * day));
        assert!(!schedule_due("hourly", None, None, 10 * day));
    }
}
