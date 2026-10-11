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
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{
    AppState,
    admin::human_bytes,
    inventory::{self, REPOS},
    search,
    store::{dir_size, now_secs},
    templates::{TemplateInput, build, lines::Eco},
};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN ('template', 'package')),
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
}

impl Outcome {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            status: Status::Failed,
            message: Some(message.into()),
            lockfiles: HashMap::new(),
        }
    }
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
}

pub const JOB_COLUMNS: &str = "id, kind, template_id, title, status, message, created_by, created_at, started_at, finished_at, new_packages, new_files, new_bytes";

/// Queue a job. Returns its id.
pub async fn enqueue(
    st: &AppState,
    kind: &str,
    template_id: Option<i64>,
    title: &str,
    spec: &TemplateInput,
    created_by: &str,
) -> sqlx::Result<i64> {
    let r = sqlx::query(
        "INSERT INTO jobs (kind, template_id, title, spec, status, created_by, created_at)
         VALUES (?, ?, ?, ?, 'queued', ?, ?)",
    )
    .bind(kind)
    .bind(template_id)
    .bind(title)
    .bind(serde_json::to_string(spec).unwrap())
    .bind(created_by)
    .bind(now_secs() as i64)
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
        for repo in REPOS {
            for a in inventory::scan_repo(repo, &store.files_dir(repo), &access) {
                out.insert(
                    format!("{repo}/{}", a.path),
                    (format!("{repo}/{}", a.package), a.size),
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
        dir_size(&root.join("files")) + dir_size(&root.join("meta"))
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
    let before = snapshot(st).await;
    let outcome = run_inner(st, id, &mut log, &cancel).await;
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
    let _ = sqlx::query(
        "UPDATE jobs SET status = ?, message = ?, finished_at = ?, new_packages = ?, new_files = ?, new_bytes = ?, lockfiles = ?
         WHERE id = ?",
    )
    .bind(outcome.status.as_str())
    .bind(&outcome.message)
    .bind(now_secs() as i64)
    .bind(packages.len() as i64)
    .bind(new.len() as i64)
    .bind(bytes as i64)
    .bind(lockfiles)
    .bind(id)
    .execute(db)
    .await;
    if outcome.status == Status::Done {
        let _ = sqlx::query(
            "UPDATE templates SET last_warmed_at = ? WHERE id = (SELECT template_id FROM jobs WHERE id = ? AND kind = 'template')",
        )
        .bind(now_secs() as i64)
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
    let spec: TemplateInput = match serde_json::from_str(&spec) {
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
    let token = match st.auth.warmer_token().await {
        Ok(t) => t,
        Err(e) => return Outcome::failed(format!("could not get the warmer's access token: {e}")),
    };

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

    let deadline = tokio::time::Instant::now() + st.cfg.warm_timeout;
    let mut lockfiles = HashMap::new();
    let mut failures = Vec::new();
    for m in manifests {
        let label = m.eco.label();
        log.push(db, &format!("--- {label} ({})", spec.platforms.join(", ")))
            .await;
        // Maven has no lockfile; keep the pom.xml that was warmed, with
        // every version filled in, for the template's starter.
        let warmed_pom = (m.eco == Eco::Maven)
            .then(|| m.files.iter().find(|(p, _)| p == "pom.xml").cloned())
            .flatten();
        let req = warmer::RunRequest {
            job: id,
            ecosystem: m.eco,
            files: m.files.into_iter().collect(),
            platforms: spec.platforms.clone(),
            allow_sdist: spec.allow_sdist,
            python_version: st.cfg.warm_python.clone(),
            granary_url: st.cfg.warmer_granary_url.clone(),
            token: token.clone(),
        };
        match warmer::run(st, &req, log, cancel, deadline, budget).await {
            Ok(r) if r.ok => {
                let mut locks = r.lockfiles;
                locks.extend(warmed_pom);
                lockfiles.insert(m.eco.as_str().to_string(), locks);
            }
            Ok(r) => {
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
        }
    } else {
        Outcome {
            status: Status::Failed,
            message: Some(failures.join("; ")),
            lockfiles,
        }
    }
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
        match enqueue(
            st,
            "template",
            Some(id),
            &t.input.name,
            &t.input,
            "schedule",
        )
        .await
        {
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
