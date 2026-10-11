//! Git pull-through cache.
//!
//! Client setup (or let `make use-granary TOOLS=git` do it):
//!
//! ```text
//! git config --global url."http://HOST:8080/git/github.com/".insteadOf "https://github.com/"
//! ```
//!
//! Repos are served read-only at `/git/{host}/{owner}/{repo}.git` over the
//! smart HTTP protocol, by running `git http-backend` for each request, so
//! shallow clones and partial fetches work. Only fetching is allowed;
//! pushes get a 403.
//!
//! On the first request for a repo, Granary clones it from
//! `https://{host}/{owner}/{repo}.git` with `git clone --mirror`. Later
//! fetches refresh it with `git remote update --prune` once it is older than
//! the metadata TTL, and serve the cached copy if upstream fails. Only hosts
//! in `GRANARY_GIT_HOSTS` are contacted, git never follows redirects, hooks
//! are off, and clones that grow past `GRANARY_GIT_MAX_SIZE` or run longer
//! than `GRANARY_GIT_TIMEOUT_MINS` are stopped and deleted. Submodules and
//! LFS objects are not fetched. Every fetch is pinned to the addresses that
//! passed the private-address rule (see `egress`).
//!
//! Repos on public hosts must be public. The Gitea host (`GRANARY_GITEA_URL`)
//! joins the allowlist on its own: Granary fetches from it with the Gitea
//! token, which git sends only for URLs under the Gitea address, and its
//! repos are private, so serving them follows the access policy.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::mpsc,
};
use tokio_util::io::ReaderStream;

use crate::{
    AppState,
    config::Config,
    error::{AppError, AppResult},
    policy::{Principal, Resource},
    store::{dir_size, now_secs},
};

/// Largest request body passed to git (upload-pack negotiation).
const MAX_REQUEST: usize = 64 * 1024 * 1024;
/// How often a running clone or refresh checks its size.
const SIZE_CHECK: Duration = Duration::from_secs(2);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/git/hosts", get(hosts))
        .route("/git/{*rest}", any(handle))
}

/// The allowed hosts, one per line, for client setup scripts.
async fn hosts(State(st): State<AppState>) -> String {
    st.cfg.git_hosts.iter().map(|h| format!("{h}\n")).collect()
}

// ------------------------------------------------------------------ names

/// A cached repo: `{host}/{owner}/{name}.git`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    /// Without the `.git` suffix.
    pub name: String,
}

/// Owner and repo names: letters, digits, `.`, `-` and `_`; no `..` and no
/// leading `-` or `.`.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('-')
        && !s.starts_with('.')
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

impl RepoId {
    /// Check a host, owner and repo (with or without `.git`) against the
    /// allowlist and the naming rules.
    pub fn new(cfg: &Config, host: &str, owner: &str, repo: &str) -> Result<Self, String> {
        let host = host.to_ascii_lowercase();
        if !cfg.git_hosts.contains(&host) {
            return Err(format!(
                "{host} is not an allowed git host (GRANARY_GIT_HOSTS: {})",
                cfg.git_hosts.join(", ")
            ));
        }
        let name = repo.strip_suffix(".git").unwrap_or(repo);
        if !valid_name(owner) || !valid_name(name) {
            return Err(
                "owner and repo names may only use letters, digits, '.', '-' and '_'".into(),
            );
        }
        Ok(Self {
            host,
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    /// Parse what someone pasted: `https://github.com/owner/repo(.git)`,
    /// with or without a trailing slash, or `github.com/owner/repo`.
    pub fn from_url(cfg: &Config, input: &str) -> Result<Self, String> {
        let s = input.trim();
        let rest = match s.split_once("://") {
            Some(("https", rest)) => rest,
            Some(_) => return Err("only https:// repo URLs are supported".into()),
            None => s,
        };
        let rest = rest.trim_end_matches('/');
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            [host, owner, repo] => Self::new(cfg, host, owner, repo),
            [_, _, _, ..] => Err(
                "give the repo's own URL, like https://github.com/owner/repo (no subgroups or /tree/ paths)"
                    .into(),
            ),
            _ => Err("expected a repo URL like https://github.com/owner/repo".into()),
        }
    }

    pub fn display(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }

    /// Path under `{data}/git/`.
    pub fn rel(&self) -> String {
        format!("{}/{}/{}.git", self.host, self.owner, self.name)
    }

    pub fn dir(&self, st: &AppState) -> PathBuf {
        git_root(st).join(self.rel())
    }

    fn info_path(&self, st: &AppState) -> PathBuf {
        git_root(st).join(format!("{}.granary.json", self.rel()))
    }

    pub fn upstream(&self, cfg: &Config) -> String {
        if let Some(g) = cfg.gitea.as_ref().filter(|g| g.host == self.host) {
            return format!("{}/{}/{}.git", g.url, self.owner, self.name);
        }
        match &cfg.git_upstream_override {
            Some(base) => format!("{base}/{}", self.rel()),
            None => format!("https://{}/{}/{}.git", self.host, self.owner, self.name),
        }
    }

    /// The access policy's description of this repo, when it is private.
    pub fn resource(&self, cfg: &Config) -> Option<Resource> {
        cfg.is_gitea_host(&self.host)
            .then(|| Resource::gitea_repo(&self.owner, &self.name))
    }

    fn lock_key(&self) -> String {
        format!("git:{}", self.rel())
    }
}

pub fn git_root(st: &AppState) -> PathBuf {
    st.store.root().join("git")
}

/// What Granary knows about a cached repo, kept next to it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoInfo {
    pub upstream: String,
    pub cloned_at: u64,
    pub refreshed_at: u64,
    pub default_branch: Option<String>,
    /// Why the last refresh failed, if it did.
    pub last_error: Option<String>,
    /// Fetched from Gitea, so it stays private even if the Gitea settings
    /// change later.
    #[serde(default)]
    pub private: bool,
}

async fn read_info(st: &AppState, id: &RepoId) -> Option<RepoInfo> {
    let raw = tokio::fs::read(id.info_path(st)).await.ok()?;
    serde_json::from_slice(&raw).ok()
}

async fn write_info(st: &AppState, id: &RepoId, info: &RepoInfo) -> std::io::Result<()> {
    let path = id.info_path(st);
    let tmp = path.with_extension("json.part");
    tokio::fs::write(&tmp, serde_json::to_vec(info).unwrap()).await?;
    tokio::fs::rename(&tmp, &path).await
}

// ------------------------------------------------------------ running git

/// A git command that can't run hooks, use the file or ext transports,
/// follow redirects, prompt for credentials, or read any system or user
/// configuration.
pub fn git(st: &AppState) -> Command {
    let home = git_root(st).join(".home");
    let _ = std::fs::create_dir_all(&home);
    let mut cmd = Command::new("git");
    cmd.env_clear();
    for key in [
        "PATH",
        "HTTPS_PROXY",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("HOME", &home)
        .env("LANG", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    for c in [
        "core.hooksPath=/dev/null",
        "protocol.file.allow=never",
        "protocol.ext.allow=never",
        "http.followRedirects=false",
        "credential.helper=",
        "core.askPass=",
        "submodule.recurse=false",
        "fetch.recurseSubmodules=false",
    ] {
        cmd.arg("-c").arg(c);
    }
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    cmd
}

/// A git command that fetches from `id`'s upstream: pinned to the addresses
/// that passed the private-address rule, trusting the extra CA, and for the
/// Gitea host, sending the token. The token goes in through the environment
/// (not the command line, where other processes could see it) and git sends
/// it only for URLs under the Gitea address.
async fn upstream_git(st: &AppState, id: &RepoId) -> AppResult<Command> {
    let upstream = id.upstream(&st.cfg);
    let settings = st
        .store
        .egress()
        .git_settings(&upstream)
        .await
        .map_err(|e| AppError::bad_gateway(format!("can't fetch {}: {e}", id.display())))?;
    let mut cmd = git(st);
    for s in settings {
        cmd.arg("-c").arg(s);
    }
    if let Some(g) = st.cfg.gitea.as_ref().filter(|g| g.host == id.host) {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", format!("http.{}/.extraHeader", g.url))
            .env("GIT_CONFIG_VALUE_0", gitea_auth_header(g));
    }
    Ok(cmd)
}

/// The header git sends to Gitea: Basic auth with the token as the password.
fn gitea_auth_header(g: &crate::config::Gitea) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    format!(
        "Authorization: Basic {}",
        STANDARD.encode(format!("granary:{}", g.token.expose()))
    )
}

/// The forms the Gitea token could take in git's output, to hide them.
fn secrets(cfg: &Config) -> Vec<String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    cfg.gitea
        .iter()
        .flat_map(|g| {
            [
                g.token.expose().to_string(),
                STANDARD.encode(format!("granary:{}", g.token.expose())),
            ]
        })
        .collect()
}

fn hide(line: &str, secrets: &[String]) -> String {
    let mut out = line.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(s.as_str(), "[hidden]");
    }
    out
}

/// Why a git run failed.
#[derive(Debug)]
pub enum GitError {
    TooBig(u64),
    Timeout,
    Failed(String),
    Io(std::io::Error),
}

/// Run git, sending its progress lines to `lines`, and stop it if `watch`
/// grows past the size limit or it runs past the timeout.
async fn run_watched(
    st: &AppState,
    mut cmd: Command,
    watch: &Path,
    lines: Option<&mpsc::UnboundedSender<String>>,
) -> Result<(), GitError> {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(GitError::Io)?;
    let mut stderr = child.stderr.take().unwrap();
    let sink = lines.cloned();
    let secrets = secrets(&st.cfg);
    // git rewrites progress lines with \r; keep each line's final state.
    let reader = tokio::spawn(async move {
        let mut tail: Vec<String> = Vec::new();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let emit = |line: &str, tail: &mut Vec<String>| {
            let line = hide(line.rsplit('\r').next().unwrap_or("").trim_end(), &secrets);
            if line.is_empty() {
                return;
            }
            if let Some(s) = &sink {
                let _ = s.send(line.clone());
            }
            tail.push(line);
            if tail.len() > 20 {
                tail.remove(0);
            }
        };
        while let Ok(n) = stderr.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                emit(&String::from_utf8_lossy(&line), &mut tail);
            }
        }
        emit(&String::from_utf8_lossy(&buf), &mut tail);
        tail.join("\n")
    });

    let deadline = tokio::time::Instant::now() + st.cfg.git_timeout;
    let mut tick = tokio::time::interval(SIZE_CHECK);
    let outcome = loop {
        tokio::select! {
            status = child.wait() => {
                break match status {
                    Ok(s) if s.success() => Ok(()),
                    Ok(_) => Err(GitError::Failed(String::new())),
                    Err(e) => Err(GitError::Io(e)),
                };
            }
            _ = tokio::time::sleep_until(deadline) => break Err(GitError::Timeout),
            _ = tick.tick() => {
                let dir = watch.to_path_buf();
                let size = tokio::task::spawn_blocking(move || dir_size(&dir)).await.unwrap_or(0);
                if size > st.cfg.git_max_size {
                    break Err(GitError::TooBig(size));
                }
            }
        }
    };
    // A fetch can finish between size checks; measure the result too.
    let outcome = match outcome {
        Ok(()) => {
            let dir = watch.to_path_buf();
            let size = tokio::task::spawn_blocking(move || dir_size(&dir))
                .await
                .unwrap_or(0);
            if size > st.cfg.git_max_size {
                Err(GitError::TooBig(size))
            } else {
                Ok(())
            }
        }
        err => err,
    };
    if outcome.is_err() {
        let _ = child.kill().await;
    }
    let tail = reader.await.unwrap_or_default();
    match outcome {
        Err(GitError::Failed(_)) => Err(GitError::Failed(tail)),
        other => other,
    }
}

fn to_app_error(id: &RepoId, e: GitError, cfg: &Config) -> AppError {
    match e {
        GitError::TooBig(size) => AppError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            format!(
                "{} grew to {}, past the {} limit for cached repos (GRANARY_GIT_MAX_SIZE), so it was stopped and not cached",
                id.display(),
                crate::admin::human_bytes(size),
                crate::admin::human_bytes(cfg.git_max_size)
            ),
        ),
        GitError::Timeout => AppError::new(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "fetching {} took longer than {} minutes (GRANARY_GIT_TIMEOUT_MINS) and was stopped",
                id.display(),
                cfg.git_timeout.as_secs() / 60
            ),
        ),
        GitError::Failed(out) => {
            let lower = out.to_ascii_lowercase();
            // Missing and private repos look the same from outside.
            if lower.contains("not found")
                || lower.contains("could not read username")
                || lower.contains("authentication")
                || lower.contains(" 404")
            {
                let why = if cfg.is_gitea_host(&id.host) {
                    "was not found on Gitea, or Granary's Gitea token can't read it"
                } else {
                    "was not found upstream, or is private (only public repos can be cached)"
                };
                AppError::new(StatusCode::NOT_FOUND, format!("{} {why}", id.display()))
            } else {
                let last = out.lines().last().unwrap_or("git failed").to_string();
                AppError::bad_gateway(format!("fetching {} failed: {last}", id.display()))
            }
        }
        GitError::Io(e) => AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not run git: {e}"),
        ),
    }
}

async fn default_branch(st: &AppState, dir: &Path) -> Option<String> {
    let out = git(st)
        .arg("-C")
        .arg(dir)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Clone a repo into the cache. Call with the repo's lock held.
async fn clone_locked(
    st: &AppState,
    id: &RepoId,
    lines: Option<&mpsc::UnboundedSender<String>>,
) -> AppResult<()> {
    if st.store.offline() {
        return Err(AppError::offline(&id.display()));
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = st.store.root().join("tmp").join(format!("git-{nanos}"));
    let upstream = id.upstream(&st.cfg);
    tracing::info!("git: cloning {upstream}");
    let mut cmd = upstream_git(st, id).await?;
    cmd.args(["clone", "--mirror", "--progress", "--", &upstream])
        .arg(&tmp);
    let result = run_watched(st, cmd, &tmp, lines).await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        let err = to_app_error(id, e, &st.cfg);
        tracing::warn!("git: could not cache {}: {}", id.display(), err.message);
        return Err(err);
    }
    let dir = id.dir(st);
    if let Some(parent) = dir.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let now = now_secs();
    let info = RepoInfo {
        upstream,
        cloned_at: now,
        refreshed_at: now,
        default_branch: default_branch(st, &tmp).await,
        last_error: None,
        private: id.resource(&st.cfg).is_some(),
    };
    tokio::fs::rename(&tmp, &dir).await?;
    write_info(st, id, &info).await?;
    let size = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || dir_size(&dir)
    })
    .await
    .unwrap_or(0);
    st.store
        .stats
        .bytes_cached
        .fetch_add(size, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        "git: cached {} ({})",
        id.display(),
        crate::admin::human_bytes(size)
    );
    Ok(())
}

/// Refresh a cached repo from upstream. Call with the repo's lock held.
/// On failure the cached copy stays as it was.
async fn refresh_locked(
    st: &AppState,
    id: &RepoId,
    lines: Option<&mpsc::UnboundedSender<String>>,
) -> AppResult<()> {
    if st.store.offline() {
        return Err(AppError::offline(&id.display()));
    }
    let dir = id.dir(st);
    let before = tokio::task::spawn_blocking({
        let dir = dir.clone();
        move || dir_size(&dir)
    })
    .await
    .unwrap_or(0);
    let mut cmd = upstream_git(st, id).await?;
    cmd.arg("-C")
        .arg(&dir)
        .args(["remote", "update", "--prune"]);
    let result = run_watched(st, cmd, &dir, lines).await;
    let mut info = read_info(st, id).await.unwrap_or_default();
    match result {
        Ok(()) => {
            info.refreshed_at = now_secs();
            info.last_error = None;
            info.default_branch = default_branch(st, &dir).await.or(info.default_branch);
            write_info(st, id, &info).await?;
            let after = tokio::task::spawn_blocking(move || dir_size(&dir))
                .await
                .unwrap_or(before);
            st.store.stats.bytes_cached.fetch_add(
                after.saturating_sub(before),
                std::sync::atomic::Ordering::Relaxed,
            );
            tracing::debug!("git: refreshed {}", id.display());
            Ok(())
        }
        Err(e) => {
            let err = to_app_error(id, e, &st.cfg);
            info.last_error = Some(err.message.clone());
            let _ = write_info(st, id, &info).await;
            Err(err)
        }
    }
}

/// When a repo should be brought up to date.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Only clone it if it isn't cached.
    Never,
    /// Refresh it if it's older than the metadata TTL.
    IfStale,
    /// Refresh it now.
    Now,
}

/// Make sure a repo is cached (and fresh enough), cloning or refreshing it
/// under its lock so concurrent requests cause one upstream fetch. A failed
/// refresh of a cached repo is logged and the cached copy is served.
pub async fn ensure(
    st: &AppState,
    id: &RepoId,
    refresh: Refresh,
    lines: Option<&mpsc::UnboundedSender<String>>,
) -> AppResult<()> {
    let dir = id.dir(st);
    let stale = |info: Option<RepoInfo>| match refresh {
        Refresh::Never => false,
        Refresh::Now => true,
        Refresh::IfStale => info
            .is_none_or(|i| now_secs().saturating_sub(i.refreshed_at) >= st.cfg.meta_ttl.as_secs()),
    };
    if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
        if st.store.offline() || !stale(read_info(st, id).await) {
            return Ok(());
        }
    } else if st.store.offline() {
        return Err(AppError::offline(&id.display()));
    }
    st.store
        .with_lock(&id.lock_key(), async {
            if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
                return clone_locked(st, id, lines).await;
            }
            // Someone else may have refreshed it while we waited.
            if !stale(read_info(st, id).await) {
                return Ok(());
            }
            match refresh_locked(st, id, lines).await {
                Ok(()) => Ok(()),
                Err(e) if refresh == Refresh::Now => Err(e),
                Err(e) => {
                    tracing::warn!(
                        "git: serving the cached {}: refresh failed: {}",
                        id.display(),
                        e.message
                    );
                    Ok(())
                }
            }
        })
        .await
}

/// Delete a cached repo. Returns bytes freed.
pub async fn delete(st: &AppState, id: &RepoId) -> AppResult<u64> {
    st.store
        .with_lock(&id.lock_key(), async {
            let dir = id.dir(st);
            if !tokio::fs::try_exists(&dir).await.unwrap_or(false) {
                return Err(AppError::not_found());
            }
            let size = tokio::task::spawn_blocking({
                let dir = dir.clone();
                move || dir_size(&dir)
            })
            .await
            .unwrap_or(0);
            tokio::fs::remove_dir_all(&dir).await?;
            let _ = tokio::fs::remove_file(id.info_path(st)).await;
            // Tidy now-empty owner and host directories.
            for d in [dir.parent(), dir.parent().and_then(Path::parent)]
                .into_iter()
                .flatten()
            {
                let _ = tokio::fs::remove_dir(d).await;
            }
            Ok(size)
        })
        .await
}

// --------------------------------------------------- reading files at a ref

/// Branch, tag or commit names: letters, digits and `._/-`, no leading `-`
/// and no `..`.
pub fn valid_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 200
        && !r.starts_with('-')
        && !r.contains("..")
        && !r.ends_with('/')
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

async fn git_out(st: &AppState, id: &RepoId, args: &[&str]) -> AppResult<Option<Vec<u8>>> {
    let out = git(st)
        .arg("-C")
        .arg(id.dir(st))
        .args(args)
        .output()
        .await
        .map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not run git: {e}"),
            )
        })?;
    Ok(out.status.success().then_some(out.stdout))
}

/// The commit a ref (or the default branch, when `None`) points at in the
/// cached mirror.
pub async fn resolve_ref(st: &AppState, id: &RepoId, r: Option<&str>) -> AppResult<String> {
    let r = r.map(str::trim).filter(|r| !r.is_empty()).unwrap_or("HEAD");
    if r != "HEAD" && !valid_ref(r) {
        return Err(AppError::bad_request(format!(
            "{r:?} is not a branch, tag or commit name"
        )));
    }
    let spec = format!("{r}^{{commit}}");
    let out = git_out(
        st,
        id,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .await?;
    out.map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|c| c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("{} has no branch, tag or commit named {r}", id.display()),
            )
        })
}

/// Every file path at a commit.
pub async fn list_files(st: &AppState, id: &RepoId, commit: &str) -> AppResult<Vec<String>> {
    let out = git_out(
        st,
        id,
        &[
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            "--end-of-options",
            commit,
        ],
    )
    .await?
    .ok_or_else(AppError::not_found)?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).to_string())
        .collect())
}

/// One file's contents at a commit, if it exists and is at most `max` bytes.
pub async fn read_file(
    st: &AppState,
    id: &RepoId,
    commit: &str,
    path: &str,
    max: u64,
) -> AppResult<Option<String>> {
    let object = format!("{commit}:{path}");
    let Some(size) = git_out(st, id, &["cat-file", "-s", "--end-of-options", &object]).await?
    else {
        return Ok(None);
    };
    let size: u64 = String::from_utf8_lossy(&size)
        .trim()
        .parse()
        .unwrap_or(u64::MAX);
    if size > max {
        return Ok(None);
    }
    Ok(
        git_out(st, id, &["cat-file", "blob", "--end-of-options", &object])
            .await?
            .map(|b| String::from_utf8_lossy(&b).to_string()),
    )
}

/// Whether the cached mirror has this commit.
pub async fn has_commit(st: &AppState, id: &RepoId, commit: &str) -> bool {
    let spec = format!("{commit}^{{commit}}");
    matches!(
        git_out(st, id, &["cat-file", "-e", "--end-of-options", &spec]).await,
        Ok(Some(_))
    )
}

/// A cached repo, for listings.
#[derive(Clone, Debug, Serialize)]
pub struct Cached {
    #[serde(flatten)]
    pub id: RepoId,
    #[serde(flatten)]
    pub info: RepoInfo,
    pub size: u64,
}

/// The access policy's description of a cached repo, when it is private:
/// fetched from Gitea, or on the Gitea host now.
pub fn cached_resource(cfg: &Config, c: &Cached) -> Option<Resource> {
    (c.info.private || cfg.is_gitea_host(&c.id.host))
        .then(|| Resource::gitea_repo(&c.id.owner, &c.id.name))
}

/// Every cached repo (blocking; run on a blocking thread).
pub fn list(root: &Path) -> Vec<Cached> {
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Vec<(String, PathBuf)> {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
            .filter(|(n, _)| !n.starts_with('.'))
            .collect()
    };
    for (host, hp) in dirs(root) {
        for (owner, op) in dirs(&hp) {
            for (repo, rp) in dirs(&op) {
                let Some(name) = repo.strip_suffix(".git") else {
                    continue;
                };
                let info = std::fs::read(op.join(format!("{repo}.granary.json")))
                    .ok()
                    .and_then(|raw| serde_json::from_slice(&raw).ok())
                    .unwrap_or_default();
                out.push(Cached {
                    id: RepoId {
                        host: host.clone(),
                        owner: owner.clone(),
                        name: name.to_string(),
                    },
                    info,
                    size: dir_size(&rp),
                });
            }
        }
    }
    out.sort_by_key(|r| r.id.display());
    out
}

// ---------------------------------------------------------------- serving

async fn handle(
    State(st): State<AppState>,
    who: Principal,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> AppResult<Response> {
    let raw = uri.path().strip_prefix("/git/").unwrap_or("");
    // Repo paths never need percent-encoding; refuse any to avoid tricks.
    if raw.contains('%') || raw.contains('\\') {
        return Err(AppError::bad_request("invalid git path"));
    }
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.len() < 4
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Err(AppError::not_found());
    }
    let (host, owner, repo) = (parts[0], parts[1], parts[2]);
    let tail = parts[3..].join("/");
    let service = uri
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("service="));

    if tail == "git-receive-pack" || service == Some("git-receive-pack") {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "Granary's git cache is read-only; push to the upstream repo instead",
        ));
    }
    let refresh = match (&method, tail.as_str(), service) {
        (&Method::GET, "info/refs", Some("git-upload-pack")) => Refresh::IfStale,
        (&Method::POST, "git-upload-pack", None) => Refresh::Never,
        (_, "info/refs", None) => {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "only git's smart HTTP protocol is supported; please upgrade git",
            ));
        }
        _ => return Err(AppError::not_found()),
    };
    let id = RepoId::new(&st.cfg, host, owner, repo)
        .map_err(|e| AppError::new(StatusCode::NOT_FOUND, e))?;
    // Private repos: ask who's there before anything else, whether or not
    // the repo exists, so the answer never tells an outsider a name.
    if let Some(r) = id.resource(&st.cfg)
        && !st.policy.can_read_private(&who, &r)
    {
        if who.is_anonymous() {
            return Ok(crate::auth::challenge(&st, &headers, uri.path()));
        }
        return Err(AppError::not_found());
    }
    ensure(&st, &id, refresh, None).await?;

    let mut cmd = git(&st);
    // Fetching one commit by its SHA (what the warmer does for a repo
    // template) needs allowReachableSHA1InWant; the repos are public.
    cmd.arg("-c")
        .arg("uploadpack.allowFilter=true")
        .arg("-c")
        .arg("uploadpack.allowReachableSHA1InWant=true")
        .arg("http-backend");
    let path_info = format!("/{}/{tail}", id.rel());
    cgi(
        cmd,
        &git_root(&st),
        &method,
        &path_info,
        uri.query().unwrap_or(""),
        &headers,
        body,
    )
    .await
}

/// Run `git http-backend` as a CGI program for one request and stream its
/// answer back. `cmd` is a prepared `git ... http-backend` command.
pub async fn cgi(
    mut cmd: Command,
    project_root: &Path,
    method: &Method,
    path_info: &str,
    query: &str,
    headers: &HeaderMap,
    body: Body,
) -> AppResult<Response> {
    let header = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    cmd.env("GIT_PROJECT_ROOT", project_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("REQUEST_METHOD", method.as_str())
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", header(header::CONTENT_TYPE))
        .env("HTTP_CONTENT_ENCODING", header(header::CONTENT_ENCODING))
        .env("REMOTE_ADDR", "127.0.0.1");
    if let Some(p) = headers.get("git-protocol").and_then(|v| v.to_str().ok()) {
        cmd.env("GIT_PROTOCOL", p);
    }
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
    {
        cmd.env("CONTENT_LENGTH", len);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not run git: {e}"),
            )
        })?;

    // Feed the request body in while the answer is read.
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        let mut stream = body.into_data_stream();
        let mut total = 0usize;
        while let Some(Ok(chunk)) = stream.next().await {
            total += chunk.len();
            if total > MAX_REQUEST || stdin.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = stdin.shutdown().await;
    });
    let mut stderr = child.stderr.take().unwrap();
    tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        if !s.trim().is_empty() {
            tracing::debug!("git http-backend: {}", s.trim());
        }
    });

    // CGI headers end at the first blank line.
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let split = loop {
        let n = stdout.read(&mut chunk).await?;
        if n == 0 {
            return Err(AppError::bad_gateway(
                "git http-backend ended without an answer",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break (i, 4);
        }
        if let Some(i) = buf.windows(2).position(|w| w == b"\n\n") {
            break (i, 2);
        }
        if buf.len() > 64 * 1024 {
            return Err(AppError::bad_gateway(
                "git http-backend sent oversized headers",
            ));
        }
    };
    let head = String::from_utf8_lossy(&buf[..split.0]).to_string();
    let rest = Bytes::copy_from_slice(&buf[split.0 + split.1..]);
    let mut resp = Response::builder().status(StatusCode::OK);
    for line in head.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k.eq_ignore_ascii_case("status") {
            let code = v
                .split_whitespace()
                .next()
                .and_then(|c| c.parse().ok())
                .unwrap_or(500);
            resp = resp.status(code);
        } else if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(k.trim().as_bytes()),
            HeaderValue::from_str(v),
        ) {
            resp = resp.header(name, value);
        }
    }
    // Reap git when it's done.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    let body = futures_util::stream::once(async move { Ok::<_, std::io::Error>(rest) })
        .chain(ReaderStream::new(stdout));
    Ok(resp.body(Body::from_stream(body)).unwrap().into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::tests::test_config;

    fn cfg() -> Config {
        let mut c = test_config();
        c.git_hosts = vec!["github.com".into(), "codeberg.org".into()];
        c
    }

    #[test]
    fn names() {
        for ok in ["serde", "serde-rs", "a.b_c-1", "x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            ".hidden",
            "a..b",
            "a/b",
            "a b",
            "ü",
            &"x".repeat(101),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn refs() {
        for ok in ["main", "v1.2.3", "release/2026-10", "0123abc", "feature_x"] {
            assert!(valid_ref(ok), "{ok}");
        }
        for bad in ["", "-x", "a..b", "a b", "main/", "$(x)", "a;b", "@{-1}"] {
            assert!(!valid_ref(bad), "{bad}");
        }
    }

    #[test]
    fn repo_ids() {
        let c = cfg();
        let id = RepoId::new(&c, "GitHub.com", "serde-rs", "serde.git").unwrap();
        assert_eq!(id.rel(), "github.com/serde-rs/serde.git");
        assert_eq!(id.upstream(&c), "https://github.com/serde-rs/serde.git");
        assert!(RepoId::new(&c, "evil.example", "a", "b").is_err());
        assert!(RepoId::new(&c, "github.com", "..", "b").is_err());
        assert!(RepoId::new(&c, "github.com", "a", "-b").is_err());

        for url in [
            "https://github.com/serde-rs/serde",
            "https://github.com/serde-rs/serde.git",
            "https://github.com/serde-rs/serde/",
            "github.com/serde-rs/serde",
        ] {
            assert_eq!(RepoId::from_url(&c, url).unwrap(), id, "{url}");
        }
        assert!(RepoId::from_url(&c, "http://github.com/a/b").is_err());
        assert!(RepoId::from_url(&c, "git@github.com:a/b.git").is_err());
        assert!(RepoId::from_url(&c, "https://github.com/a/b/tree/main").is_err());
        assert!(RepoId::from_url(&c, "https://127.0.0.1/a/b").is_err());
    }
}
