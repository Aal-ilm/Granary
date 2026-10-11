//! Management features behind the web UI: persisted settings, cleanup
//! policies, scheduled tasks, upstream health checks and inventory caching.

pub mod api;
pub mod git;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    AppState,
    auth::SqliteSessionStore,
    config::Config,
    inventory::{self, Asset, REPOS},
    logbuf::LogBuffer,
    store::{Store, now_secs},
};

const DAY: u64 = 86_400;
const INVENTORY_TTL: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- settings

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanupPolicy {
    /// Run cleanup automatically once a day.
    pub enabled: bool,
    /// Remove packages nobody has downloaded in this many days.
    pub unused_days: u32,
    /// Which repositories the policy applies to.
    pub repos: Vec<String>,
    /// Also drop cached metadata that hasn't been requested in `unused_days`.
    pub include_metadata: bool,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            unused_days: 90,
            repos: REPOS.iter().map(|s| s.to_string()).collect(),
            include_metadata: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupItem {
    pub repo: String,
    /// The cache namespace, when it isn't the repository's own (a private
    /// package's `gitea-` namespace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ns: Option<String>,
    pub package: String,
    pub version: String,
    pub path: String,
    pub size: u64,
    pub last_used: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CleanupReport {
    pub at: u64,
    pub dry_run: bool,
    pub unused_days: u32,
    pub files: usize,
    pub bytes: u64,
    pub meta_files: usize,
    pub meta_bytes: u64,
    /// Up to 500 of the affected files (largest first).
    pub items: Vec<CleanupItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub cleanup: CleanupPolicy,
    pub last_cleanup: Option<CleanupReport>,
}

// ------------------------------------------------------------------- tasks

#[derive(Clone, Debug, Serialize)]
pub struct TaskStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub schedule: String,
    pub enabled: bool,
    pub running: bool,
    pub interval_secs: u64,
    pub last_run: Option<u64>,
    pub last_duration_ms: Option<u64>,
    pub last_result: Option<String>,
    pub last_ok: Option<bool>,
    pub next_run: Option<u64>,
}

fn task(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    interval_secs: u64,
    first_delay: u64,
) -> TaskStatus {
    TaskStatus {
        id,
        name,
        description,
        schedule: describe_interval(interval_secs),
        enabled: true,
        running: false,
        interval_secs,
        last_run: None,
        last_duration_ms: None,
        last_result: None,
        last_ok: None,
        next_run: Some(now_secs() + first_delay),
    }
}

fn describe_interval(secs: u64) -> String {
    match secs {
        s if s % DAY == 0 && s / DAY == 1 => "Every day".into(),
        s if s % DAY == 0 => format!("Every {} days", s / DAY),
        s if s % 3600 == 0 && s / 3600 == 1 => "Every hour".into(),
        s if s % 3600 == 0 => format!("Every {} hours", s / 3600),
        s if s % 60 == 0 => format!("Every {} minutes", s / 60),
        s => format!("Every {s} seconds"),
    }
}

// ------------------------------------------------------------------ health

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamHealth {
    pub repo: &'static str,
    pub url: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub error: Option<String>,
    pub checked_at: u64,
    /// Not contacted because offline mode is on.
    pub offline: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiskInfo {
    pub total: u64,
    pub free: u64,
}

pub fn disk_info(path: &Path) -> Option<DiskInfo> {
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out pointer.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let frsize = st.f_frsize as u64;
        Some(DiskInfo {
            total: st.f_blocks as u64 * frsize,
            free: st.f_bavail as u64 * frsize,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

// ------------------------------------------------------------------- admin

pub struct Admin {
    pub settings: Mutex<Settings>,
    pub tasks: Mutex<Vec<TaskStatus>>,
    pub health: Mutex<Vec<UpstreamHealth>>,
    pub logs: LogBuffer,
    /// Search results and per-user limits for "Get packages".
    pub search: crate::search::SearchState,
    /// The warm job queue.
    pub jobs: crate::jobs::Jobs,
    pub started_at: u64,
    inventory: Mutex<Option<(Instant, Arc<Vec<Asset>>)>>,
    settings_path: PathBuf,
}

impl Admin {
    pub async fn new(data_dir: &Path, logs: LogBuffer) -> anyhow::Result<Self> {
        let settings_path = data_dir.join("state/settings.json");
        let settings = match tokio::fs::read(&settings_path).await {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable settings file: {e}");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        let enabled = settings.cleanup.enabled;
        let mut cleanup = task(
            "cleanup",
            "Clean up unused packages",
            "Deletes cached packages and metadata that match the cleanup policy.",
            DAY,
            3600,
        );
        cleanup.enabled = enabled;
        // Carry the last cleanup across restarts so the schedule stays daily.
        if let Some(last) = &settings.last_cleanup {
            cleanup.last_run = Some(last.at);
            cleanup.last_ok = Some(true);
            cleanup.last_result = Some(format!(
                "Removed {} files and {} metadata entries, freeing {}",
                last.files,
                last.meta_files,
                human_bytes(last.bytes + last.meta_bytes)
            ));
            cleanup.next_run = Some((last.at + DAY).max(now_secs() + 60));
        }
        if !enabled {
            cleanup.next_run = None;
        }
        let tasks = vec![
            cleanup,
            task(
                "health",
                "Check upstream registries",
                "Contacts each upstream registry to confirm it is reachable.",
                300,
                2,
            ),
            task(
                "flush",
                "Save download history",
                "Writes the last-downloaded time of each package to disk.",
                30,
                30,
            ),
            task(
                "sessions",
                "Remove expired sign-ins",
                "Deletes sessions that have timed out and audit entries past their retention period.",
                3600,
                60,
            ),
        ];
        Ok(Self {
            settings: Mutex::new(settings),
            tasks: Mutex::new(tasks),
            health: Mutex::new(Vec::new()),
            logs,
            started_at: now_secs(),
            inventory: Mutex::new(None),
            search: Default::default(),
            jobs: Default::default(),
            settings_path,
        })
    }

    pub async fn save_settings(&self) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(&*self.settings.lock().unwrap()).unwrap();
        if let Some(parent) = self.settings_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = self.settings_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &self.settings_path).await
    }

    /// All cached files, rescanned at most every few seconds.
    pub async fn inventory(&self, store: &Store) -> Arc<Vec<Asset>> {
        if let Some((at, inv)) = &*self.inventory.lock().unwrap()
            && at.elapsed() < INVENTORY_TTL
        {
            return inv.clone();
        }
        let access = store.access_snapshot();
        let mut dirs: Vec<(&'static str, &'static str, PathBuf, bool)> = REPOS
            .iter()
            .map(|r| (*r, *r, store.files_dir(r), false))
            .collect();
        dirs.extend(
            inventory::PRIVATE
                .iter()
                .map(|(r, ns)| (*r, *ns, store.files_dir(ns), true)),
        );
        let assets = tokio::task::spawn_blocking(move || {
            dirs.iter()
                .flat_map(|(repo, ns, dir, private)| {
                    inventory::scan_repo(repo, ns, dir, &access, *private)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let inv = Arc::new(assets);
        *self.inventory.lock().unwrap() = Some((Instant::now(), inv.clone()));
        inv
    }

    pub fn invalidate_inventory(&self) {
        *self.inventory.lock().unwrap() = None;
    }

    /// Enable or disable the scheduled cleanup task to match the policy.
    pub fn sync_cleanup_task(&self) {
        let enabled = self.settings.lock().unwrap().cleanup.enabled;
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(t) = tasks.iter_mut().find(|t| t.id == "cleanup") {
            t.enabled = enabled;
            t.next_run = if enabled {
                Some(
                    t.last_run
                        .map_or(now_secs() + 3600, |l| l + t.interval_secs),
                )
            } else {
                None
            };
        }
    }
}

// ----------------------------------------------------------------- cleanup

/// Work out (and unless `dry_run`, perform) a cleanup under `policy`.
pub async fn cleanup(st: &AppState, policy: &CleanupPolicy, dry_run: bool) -> CleanupReport {
    let now = now_secs();
    let cutoff = now.saturating_sub(policy.unused_days as u64 * DAY);
    let inv = st.admin.inventory(&st.store).await;

    let mut items: Vec<CleanupItem> = inv
        .iter()
        .filter(|a| policy.repos.iter().any(|r| r == a.repo))
        .filter_map(|a| {
            let last_used = a.last_access.unwrap_or(a.cached_at).max(a.cached_at);
            (last_used < cutoff).then(|| CleanupItem {
                repo: a.repo.to_string(),
                ns: a.private.then(|| a.ns.to_string()),
                package: a.package.clone(),
                version: a.version.clone(),
                path: a.path.clone(),
                size: a.size,
                last_used,
            })
        })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.size));

    let mut report = CleanupReport {
        at: now,
        dry_run,
        unused_days: policy.unused_days,
        ..Default::default()
    };

    for item in &items {
        if dry_run {
            report.files += 1;
            report.bytes += item.size;
        } else if let Ok(freed) = st
            .store
            .delete_file(item.ns.as_deref().unwrap_or(&item.repo), &item.path)
            .await
        {
            report.files += 1;
            report.bytes += freed;
        }
    }

    if policy.include_metadata {
        // Each repository's own metadata, and its private packages' too.
        let namespaces = REPOS
            .iter()
            .filter(|r| policy.repos.iter().any(|p| p == *r))
            .flat_map(|r| [Some(*r), inventory::private_ns(r)])
            .flatten()
            .collect::<Vec<&'static str>>();
        for ns in namespaces {
            let dir = st.store.meta_dir(ns);
            let expired = tokio::task::spawn_blocking(move || expired_meta(&dir, cutoff))
                .await
                .unwrap_or_default();
            for (info, body, size) in expired {
                if dry_run {
                    report.meta_files += 1;
                    report.meta_bytes += size;
                    continue;
                }
                let ok_body = tokio::fs::remove_file(&body).await.is_ok();
                let ok_info = tokio::fs::remove_file(&info).await.is_ok();
                if ok_body || ok_info {
                    report.meta_files += 1;
                    report.meta_bytes += size;
                }
            }
        }
    }

    items.truncate(500);
    report.items = items;
    if !dry_run {
        st.admin.invalidate_inventory();
        let _ = st.store.flush_access().await;
        tracing::info!(
            "cleanup removed {} files ({} bytes) and {} metadata entries",
            report.files,
            report.bytes,
            report.meta_files
        );
    }
    report
}

/// Metadata entries not refreshed since `cutoff`: (info path, body path, size).
fn expired_meta(dir: &Path, cutoff: u64) -> Vec<(PathBuf, PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(raw) = std::fs::read(&path) else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(fetched) = v.get("fetched_at").and_then(|f| f.as_u64()) else {
                continue;
            };
            if fetched < cutoff {
                let body = path.with_extension("body");
                let size =
                    raw.len() as u64 + std::fs::metadata(&body).map(|m| m.len()).unwrap_or(0);
                out.push((path, body, size));
            }
        }
    }
    out
}

// ------------------------------------------------------------ health check

pub async fn check_upstreams(st: &AppState) -> Vec<UpstreamHealth> {
    let cfg: &Config = &st.cfg;
    let targets = [
        (
            "cargo",
            reqwest::Method::GET,
            format!("{}/config.json", cfg.cargo_index),
        ),
        (
            "npm",
            reqwest::Method::GET,
            format!("{}/-/ping", cfg.npm_upstream),
        ),
        (
            "pypi",
            reqwest::Method::HEAD,
            format!("{}/pip/", cfg.pypi_index),
        ),
        (
            "maven",
            reqwest::Method::HEAD,
            format!("{}/", cfg.maven_upstream),
        ),
    ];
    if st.store.offline() {
        // Offline mode: report each upstream as deliberately not contacted.
        let results: Vec<UpstreamHealth> = targets
            .into_iter()
            .map(|(repo, _, url)| UpstreamHealth {
                repo,
                url,
                ok: false,
                status: None,
                latency_ms: 0,
                error: Some("offline mode".into()),
                checked_at: now_secs(),
                offline: true,
            })
            .collect();
        *st.admin.health.lock().unwrap() = results.clone();
        return results;
    }
    let client = st.store.passthrough_client();
    let checks = targets.into_iter().map(|(repo, method, url)| async move {
        let start = Instant::now();
        if let Err(e) = st.store.check_url(&url) {
            return UpstreamHealth {
                repo,
                ok: false,
                status: None,
                latency_ms: 0,
                error: Some(e.message),
                checked_at: now_secs(),
                url,
                offline: false,
            };
        }
        let res = client
            .request(method, &url)
            .timeout(Duration::from_secs(8))
            .send()
            .await;
        let latency_ms = start.elapsed().as_millis() as u64;
        match res {
            Ok(r) => UpstreamHealth {
                repo,
                ok: r.status().as_u16() < 500,
                status: Some(r.status().as_u16()),
                latency_ms,
                error: None,
                checked_at: now_secs(),
                url,
                offline: false,
            },
            Err(e) => UpstreamHealth {
                repo,
                ok: false,
                status: None,
                latency_ms,
                error: Some(short_error(&e)),
                checked_at: now_secs(),
                url,
                offline: false,
            },
        }
    });
    let results = futures_util::future::join_all(checks).await;
    for r in results.iter().filter(|r| !r.ok) {
        tracing::warn!(
            "upstream {} unreachable: {}",
            r.url,
            r.error
                .clone()
                .unwrap_or_else(|| format!("HTTP {}", r.status.unwrap_or(0)))
        );
    }
    *st.admin.health.lock().unwrap() = results.clone();
    results
}

fn short_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        // The innermost cause says why: refused, a certificate problem, or
        // an address the private-address rule doesn't allow.
        let mut cause: &dyn std::error::Error = e;
        while let Some(next) = cause.source() {
            cause = next;
        }
        format!("connection failed: {cause}")
    } else {
        e.to_string()
    }
}

// --------------------------------------------------------------- scheduler

/// Run one task now. Returns false if it was already running or is unknown.
pub async fn run_task(st: &AppState, id: &str) -> bool {
    run_task_with(st, id, None).await
}

async fn run_task_with(st: &AppState, id: &str, sessions: Option<&SqliteSessionStore>) -> bool {
    {
        let mut tasks = st.admin.tasks.lock().unwrap();
        let Some(t) = tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if t.running {
            return false;
        }
        t.running = true;
    }
    let start = Instant::now();
    let (ok, result) = match id {
        "cleanup" => {
            let policy = st.admin.settings.lock().unwrap().cleanup.clone();
            let report = cleanup(st, &policy, false).await;
            let msg = format!(
                "Removed {} files and {} metadata entries, freeing {}",
                report.files,
                report.meta_files,
                human_bytes(report.bytes + report.meta_bytes)
            );
            st.admin.settings.lock().unwrap().last_cleanup = Some(report);
            let _ = st.admin.save_settings().await;
            (true, msg)
        }
        "health" if st.store.offline() => {
            check_upstreams(st).await;
            (true, "offline mode: upstreams not contacted".to_string())
        }
        "health" => {
            let r = check_upstreams(st).await;
            let up = r.iter().filter(|h| h.ok).count();
            (
                up == r.len(),
                format!("{up} of {} upstreams reachable", r.len()),
            )
        }
        "sessions" => {
            let store = sessions
                .cloned()
                .unwrap_or_else(|| SqliteSessionStore::new(st.auth.db.clone()));
            let pruned = crate::audit::prune(&st.auth.db, st.cfg.audit_days).await;
            match (store.delete_expired().await, pruned) {
                (Ok(n), Ok(a)) => (
                    true,
                    format!("Removed {n} expired sign-ins and {a} old audit entries"),
                ),
                (Err(e), _) | (_, Err(e)) => (false, format!("Could not clean up: {e}")),
            }
        }
        "flush" => match st.store.flush_access().await {
            Ok(true) => (true, "Saved".into()),
            Ok(false) => (true, "Nothing new to save".into()),
            Err(e) => (false, format!("Could not save: {e}")),
        },
        _ => (false, "Unknown task".into()),
    };
    let mut tasks = st.admin.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        let now = now_secs();
        t.running = false;
        t.last_run = Some(now);
        t.last_duration_ms = Some(start.elapsed().as_millis() as u64);
        t.last_ok = Some(ok);
        t.last_result = Some(result);
        t.next_run = t.enabled.then_some(now + t.interval_secs);
    }
    true
}

/// Background loop that runs due tasks.
pub fn spawn_scheduler(st: AppState, sessions: SqliteSessionStore) {
    tokio::spawn(async move {
        if st.store.offline() {
            // No network involved: just records that upstreams are off limits,
            // so the web UI shows offline mode straight away.
            check_upstreams(&st).await;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            let now = now_secs();
            let due: Vec<&'static str> = st
                .admin
                .tasks
                .lock()
                .unwrap()
                .iter()
                .filter(|t| t.enabled && !t.running && t.next_run.is_some_and(|n| n <= now))
                .map(|t| t.id)
                .collect();
            for id in due {
                let st = st.clone();
                let sessions = sessions.clone();
                tokio::spawn(async move {
                    run_task_with(&st, id, Some(&sessions)).await;
                });
            }
        }
    });
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
