//! JSON API used by the web UI. Every route needs a signed-in session
//! (enforced by `auth::api_guard`); anything that changes or deletes data
//! also needs the admin role.

use std::{cmp::Ordering, collections::BTreeMap, sync::atomic::Ordering as AtomicOrdering};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CleanupPolicy, check_upstreams, cleanup, disk_info, run_task};
use crate::auth::{AuthSession, require_admin};
use crate::{
    AppState,
    error::{AppError, AppResult},
    inventory::{self, Asset, REPOS},
    net::ClientIp,
    policy::{Mode, Principal},
    store::{dir_size, now_secs},
    util::base_url,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/overview", get(overview))
        .route("/api/repos", get(repos))
        .route("/api/repos/{repo}/invalidate", post(invalidate))
        .route("/api/components", get(components))
        .route("/api/component", get(component).delete(delete_component))
        .route("/api/asset", delete(delete_asset))
        .route("/api/cleanup", get(cleanup_get).put(cleanup_put))
        .route("/api/cleanup/preview", post(cleanup_preview))
        .route("/api/cleanup/run", post(cleanup_run))
        .route("/api/tasks", get(tasks))
        .route("/api/tasks/{id}/run", post(task_run))
        .route("/api/system", get(system))
        .route("/api/system/check", post(system_check))
        .route("/api/system/gitea/check", post(gitea_check))
        .route("/api/system/private-access", put(set_private_access))
        .route("/api/logs", get(logs))
}

// -------------------------------------------------------------------- auth

async fn audit(st: &AppState, actor: &str, action: &str, target: &str, ip: ClientIp) {
    crate::audit::record(&st.auth.db, actor, action, target, Some(ip.0), true).await;
}

fn repo_id(s: &str) -> AppResult<&'static str> {
    REPOS
        .iter()
        .find(|r| **r == s)
        .copied()
        .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, format!("no repository named {s:?}")))
}

// ---------------------------------------------------------------- overview

#[derive(Serialize)]
pub(crate) struct RepoUsage {
    pub repo: &'static str,
    pub packages: usize,
    pub files: usize,
    pub size: u64,
    pub meta_size: u64,
}

pub(crate) async fn usage(st: &AppState, inv: &[Asset]) -> Vec<RepoUsage> {
    let meta_dirs: Vec<_> = REPOS.iter().map(|r| st.store.meta_dir(r)).collect();
    let meta_sizes = tokio::task::spawn_blocking(move || {
        meta_dirs.iter().map(|d| dir_size(d)).collect::<Vec<_>>()
    })
    .await
    .unwrap_or_else(|_| vec![0; REPOS.len()]);

    REPOS
        .iter()
        .zip(meta_sizes)
        .map(|(repo, meta_size)| {
            let assets: Vec<&Asset> = inv.iter().filter(|a| a.repo == *repo).collect();
            let packages: std::collections::HashSet<&str> =
                assets.iter().map(|a| a.package.as_str()).collect();
            RepoUsage {
                repo,
                packages: packages.len(),
                files: assets.len(),
                size: assets.iter().map(|a| a.size).sum(),
                meta_size,
            }
        })
        .collect()
}

async fn overview(State(st): State<AppState>, who: Principal) -> Json<Value> {
    let inv = st.admin.inventory(&st.store).await;
    let mut repos = usage(&st, &inv).await;
    // Cached git repos get their own gauge and count toward the budget.
    // Private repos someone can't read are left out of their counts; their
    // bytes still count toward the cache budget.
    let root = crate::formats::git::git_root(&st);
    let git = tokio::task::spawn_blocking(move || crate::formats::git::list(&root))
        .await
        .unwrap_or_default();
    let (git, hidden): (Vec<_>, Vec<_>) = git.into_iter().partition(|r| {
        st.policy.can_read(
            &who,
            crate::formats::git::cached_resource(&st.cfg, r).as_ref(),
        )
    });
    let hidden_size: u64 = hidden.iter().map(|r| r.size).sum();
    repos.push(RepoUsage {
        repo: "git",
        packages: git.len(),
        files: git.len(),
        size: git.iter().map(|r| r.size).sum(),
        meta_size: 0,
    });
    let mut recent: Vec<&Asset> = inv.iter().collect();
    recent.sort_by_key(|a| std::cmp::Reverse(a.cached_at));
    recent.truncate(8);

    let s = &st.store.stats;
    let n = |a: &std::sync::atomic::AtomicU64| a.load(AtomicOrdering::Relaxed);
    let disk = {
        let dir = st.store.root().to_path_buf();
        tokio::task::spawn_blocking(move || disk_info(&dir))
            .await
            .ok()
            .flatten()
    };
    let health = st.admin.health.lock().unwrap().clone();

    let cache_used: u64 = repos.iter().map(|r| r.size + r.meta_size).sum::<u64>() + hidden_size;
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": now_secs().saturating_sub(st.admin.started_at),
        "cache_limit": st.cfg.cache_limit,
        "cache_used": cache_used,
        "repos": repos,
        "stats": {
            "file_hits": n(&s.file_hits),
            "file_downloads": n(&s.file_misses),
            "meta_hits": n(&s.meta_hits),
            "meta_refreshes": n(&s.meta_refreshes),
            "meta_stale_served": n(&s.meta_stale_served),
        },
        "disk": disk,
        "health": health,
        "recent": recent,
    }))
}

// ------------------------------------------------------------ repositories

async fn repos(State(st): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let base = base_url(&st.cfg, &headers);
    let (scheme, host) = base.split_once("://").unwrap_or(("http", &base));
    let (scheme, host) = (scheme.to_string(), host.to_string());
    let auth = st.cfg.client_auth;
    let inv = st.admin.inventory(&st.store).await;
    let usage = usage(&st, &inv).await;
    let health = st.admin.health.lock().unwrap().clone();
    let cfg = &st.cfg;

    let list: Vec<Value> = usage
        .into_iter()
        .map(|u| {
            let h = health.iter().find(|h| h.repo == u.repo);
            let (name, format, path, upstream, setup) = match u.repo {
                "cargo" => (
                    "crates.io",
                    "Cargo",
                    "/cargo/",
                    vec![
                        json!({"label": "Index", "url": cfg.cargo_index}),
                        json!({"label": "Downloads", "url": cfg.cargo_dl}),
                    ],
                    if auth {
                        json!({
                            "file": "~/.cargo/config.toml",
                            "snippet": format!("[registries.granary]\nindex = \"sparse+{base}/cargo/\"\ncredential-provider = \"cargo:token\"\n\n[source.crates-io]\nreplace-with = \"granary\""),
                            "after": "Then run cargo login --registry granary and paste your token.",
                        })
                    } else {
                        json!({
                            "file": "~/.cargo/config.toml",
                            "snippet": format!("[source.crates-io]\nreplace-with = \"granary\"\n\n[source.granary]\nregistry = \"sparse+{base}/cargo/\""),
                        })
                    },
                ),
                "npm" => (
                    "registry.npmjs.org",
                    "npm",
                    "/npm/",
                    vec![json!({"label": "Registry", "url": cfg.npm_upstream})],
                    json!({
                        "file": "~/.npmrc or a project .npmrc",
                        "snippet": if auth {
                            format!("registry={base}/npm/\n//{host}/npm/:_authToken=YOUR_TOKEN")
                        } else {
                            format!("registry={base}/npm/")
                        },
                    }),
                ),
                "maven" => (
                    "Maven Central",
                    "Maven",
                    "/maven",
                    vec![json!({"label": "Repository", "url": cfg.maven_upstream})],
                    maven_setup(&base, auth),
                ),
                _ => (
                    "pypi.org",
                    "PyPI",
                    "/pypi/simple/",
                    vec![
                        json!({"label": "Index", "url": cfg.pypi_index}),
                        json!({"label": "Files", "url": cfg.pypi_files}),
                    ],
                    json!({
                        "file": "pip.conf (or UV_INDEX_URL for uv)",
                        "snippet": if auth {
                            format!("[global]\nindex-url = {scheme}://__token__:YOUR_TOKEN@{host}/pypi/simple/")
                        } else {
                            format!("[global]\nindex-url = {base}/pypi/simple/")
                        },
                    }),
                ),
            };
            json!({
                "id": u.repo,
                "name": name,
                "format": format,
                "kind": "proxy",
                "url": format!("{base}{path}"),
                "upstream": upstream,
                "setup": setup,
                "packages": u.packages,
                "files": u.files,
                "size": u.size,
                "meta_size": u.meta_size,
                "health": h,
            })
        })
        .collect();
    Json(json!({ "base_url": base, "client_auth": auth, "repos": list }))
}

/// settings.xml for Maven, plus a Gradle init script that does the same.
fn maven_setup(base: &str, auth: bool) -> Value {
    let url = format!("{base}/maven");
    let server = if auth {
        "\n<servers>\n  <server>\n    <id>granary</id>\n    <username>token</username>\n    <password>YOUR_TOKEN</password>\n  </server>\n</servers>"
    } else {
        ""
    };
    json!({
        "file": "~/.m2/settings.xml, inside <settings>",
        "snippet": format!("<mirrors>\n  <mirror>\n    <id>granary</id>\n    <mirrorOf>central</mirrorOf>\n    <url>{url}</url>\n  </mirror>\n</mirrors>{server}"),
        "more": [{
            "label": "Gradle",
            "file": "~/.gradle/init.d/granary.gradle",
            "snippet": gradle_init(&url, auth.then_some("YOUR_TOKEN")),
        }],
    })
}

/// A Gradle init script that sends every `mavenCentral()` repository
/// (in settings, plugin management, buildscripts and projects) to Granary.
/// `scripts/granary-client.sh` writes the same script.
fn gradle_init(url: &str, token: Option<&str>) -> String {
    let insecure = if url.starts_with("http://") {
        "\n            repo.allowInsecureProtocol = true"
    } else {
        ""
    };
    let credentials = token
        .map(|t| {
            format!(
                "\n            repo.credentials {{\n                username = 'token'\n                password = '{t}'\n            }}"
            )
        })
        .unwrap_or_default();
    format!(
        "def granary = {{ RepositoryHandler repos ->\n    repos.configureEach {{ repo ->\n        if (repo instanceof MavenArtifactRepository && repo.url.toString().startsWith('https://repo.maven.apache.org/maven2')) {{\n            repo.url = '{url}'{insecure}{credentials}\n        }}\n    }}\n}}\nbeforeSettings {{ settings ->\n    granary(settings.pluginManagement.repositories)\n    granary(settings.dependencyResolutionManagement.repositories)\n}}\nallprojects {{\n    granary(buildscript.repositories)\n    granary(repositories)\n}}"
    )
}

async fn invalidate(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Path(repo): Path<String>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?;
    let repo = repo_id(&repo)?;
    let freed = st.store.invalidate_meta(repo).await?;
    tracing::info!("{repo}: metadata cache invalidated");
    audit(&st, &admin.username, "repo.invalidate", repo, ip).await;
    Ok(Json(json!({ "freed": freed })))
}

// -------------------------------------------------------- browse & search

#[derive(Deserialize)]
struct ListQuery {
    repo: Option<String>,
    q: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn components(
    State(st): State<AppState>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let repo = match q.repo.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => Some(repo_id(r)?),
        None => None,
    };
    let needle = q.q.unwrap_or_default().trim().to_lowercase();
    let inv = st.admin.inventory(&st.store).await;
    let filtered: Vec<Asset> = inv
        .iter()
        .filter(|a| repo.is_none_or(|r| a.repo == r))
        .filter(|a| needle.is_empty() || a.package.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    let mut items = inventory::group(&filtered);

    let desc = q.order.as_deref() == Some("desc");
    match q.sort.as_deref() {
        Some("size") => items.sort_by_key(|c| c.size),
        Some("versions") => items.sort_by_key(|c| c.versions),
        Some("cached") => items.sort_by_key(|c| c.latest_cached),
        Some("used") => items.sort_by_key(|c| c.last_access.unwrap_or(0)),
        _ => {
            // Exact matches first when searching, then alphabetical.
            items.sort_by(|a, b| {
                let ea = a.name.to_lowercase() == needle;
                let eb = b.name.to_lowercase() == needle;
                eb.cmp(&ea)
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.repo.cmp(b.repo))
            });
        }
    }
    if desc {
        items.reverse();
    }

    let total = items.len();
    let offset = q.offset.unwrap_or(0).min(total);
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let page: Vec<_> = items.into_iter().skip(offset).take(limit).collect();
    Ok(Json(
        json!({ "total": total, "offset": offset, "items": page }),
    ))
}

#[derive(Deserialize)]
struct ComponentQuery {
    repo: String,
    name: String,
}

/// Order versions naturally: numeric runs compare as numbers.
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = s.as_bytes();
        for i in 1..=bytes.len() {
            if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
                out.push((bytes[start].is_ascii_digit(), &s[start..i]));
                start = i;
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(cb.iter()) {
        let ord = match (x, y) {
            ((true, x), (true, y)) => x
                .trim_start_matches('0')
                .len()
                .cmp(&y.trim_start_matches('0').len())
                .then_with(|| x.trim_start_matches('0').cmp(y.trim_start_matches('0'))),
            ((_, x), (_, y)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    // Same prefix: a trailing `-pre` part (1.0.0-rc.1) ranks below the release.
    let common = ca.len().min(cb.len());
    let pre = |c: &[(bool, &str)]| c.get(common).is_some_and(|(_, s)| s.starts_with('-'));
    match (pre(&ca), pre(&cb)) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => ca.len().cmp(&cb.len()),
    }
}

fn upstream_page(st: &AppState, repo: &str, name: &str) -> Option<(String, String)> {
    let cfg = &st.cfg;
    match repo {
        "cargo" if cfg.cargo_index == "https://index.crates.io" => Some((
            "crates.io".into(),
            format!("https://crates.io/crates/{name}"),
        )),
        "npm" if cfg.npm_upstream == "https://registry.npmjs.org" => Some((
            "npmjs.com".into(),
            format!("https://www.npmjs.com/package/{name}"),
        )),
        "pypi" if cfg.pypi_index == "https://pypi.org/simple" => Some((
            "pypi.org".into(),
            format!("https://pypi.org/project/{name}/"),
        )),
        "maven" if cfg.maven_upstream == "https://repo1.maven.org/maven2" => {
            let (group, artifact) = name.split_once(':')?;
            Some((
                "Maven Central".into(),
                format!("https://central.sonatype.com/artifact/{group}/{artifact}"),
            ))
        }
        _ => None,
    }
}

async fn component(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ComponentQuery>,
) -> AppResult<Json<Value>> {
    let repo = repo_id(&q.repo)?;
    let inv = st.admin.inventory(&st.store).await;
    let assets: Vec<&Asset> = inv
        .iter()
        .filter(|a| a.repo == repo && a.package == q.name)
        .collect();
    if assets.is_empty() {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            format!("{} is not cached in {repo}", q.name),
        ));
    }

    let mut by_version: BTreeMap<&str, Vec<&Asset>> = BTreeMap::new();
    for a in &assets {
        by_version.entry(a.version.as_str()).or_default().push(a);
    }
    let base = base_url(&st.cfg, &headers);
    let mut versions: Vec<Value> = by_version
        .into_iter()
        .map(|(v, files)| {
            let files_json: Vec<Value> = files
                .iter()
                .map(|a| {
                    let download = match repo {
                        "cargo" => format!("{base}/cargo/dl/{}/{v}", a.package),
                        "npm" => format!("{base}/npm/{}", a.path),
                        "maven" => format!("{base}/maven/{}", a.path),
                        _ => format!("{base}/pypi/files/{}", a.path),
                    };
                    json!({
                        "file_name": a.file_name,
                        "path": a.path,
                        "size": a.size,
                        "cached_at": a.cached_at,
                        "last_access": a.last_access,
                        "download": download,
                    })
                })
                .collect();
            json!({
                "version": v,
                "size": files.iter().map(|a| a.size).sum::<u64>(),
                "cached_at": files.iter().map(|a| a.cached_at).max(),
                "last_access": files.iter().filter_map(|a| a.last_access).max(),
                "files": files_json,
            })
        })
        .collect();
    versions.sort_by(|a, b| {
        version_cmp(
            b["version"].as_str().unwrap_or(""),
            a["version"].as_str().unwrap_or(""),
        )
    });

    let upstream = upstream_page(&st, repo, &q.name);
    Ok(Json(json!({
        "repo": repo,
        "name": q.name,
        "size": assets.iter().map(|a| a.size).sum::<u64>(),
        "files": assets.len(),
        "versions": versions,
        "upstream": upstream.map(|(label, url)| json!({"label": label, "url": url})),
    })))
}

async fn delete_component(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Query(q): Query<ComponentQuery>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?;
    let repo = repo_id(&q.repo)?;
    let inv = st.admin.inventory(&st.store).await;
    let paths: Vec<String> = inv
        .iter()
        .filter(|a| a.repo == repo && a.package == q.name)
        .map(|a| a.path.clone())
        .collect();
    if paths.is_empty() {
        return Err(AppError::not_found());
    }
    let (mut files, mut bytes) = (0, 0);
    for p in &paths {
        if let Ok(freed) = st.store.delete_file(repo, p).await {
            files += 1;
            bytes += freed;
        }
    }
    for key in inventory::meta_keys(repo, &q.name) {
        bytes += st.store.delete_meta(repo, &key).await.unwrap_or(0);
    }
    st.admin.invalidate_inventory();
    let _ = st.store.flush_access().await;
    tracing::info!("{repo}: deleted {} ({files} files)", q.name);
    audit(
        &st,
        &admin.username,
        "package.delete",
        &format!("{repo}/{} ({files} files)", q.name),
        ip,
    )
    .await;
    Ok(Json(json!({ "files": files, "bytes": bytes })))
}

#[derive(Deserialize)]
struct AssetQuery {
    repo: String,
    path: String,
}

async fn delete_asset(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Query(q): Query<AssetQuery>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?;
    let repo = repo_id(&q.repo)?;
    let freed = st.store.delete_file(repo, &q.path).await?;
    st.admin.invalidate_inventory();
    let _ = st.store.flush_access().await;
    tracing::info!("{repo}: deleted {}", q.path);
    audit(
        &st,
        &admin.username,
        "file.delete",
        &format!("{repo}/{}", q.path),
        ip,
    )
    .await;
    Ok(Json(json!({ "files": 1, "bytes": freed })))
}

// ----------------------------------------------------------------- cleanup

async fn cleanup_get(State(st): State<AppState>) -> Json<Value> {
    let s = st.admin.settings.lock().unwrap().clone();
    Json(json!({ "policy": s.cleanup, "last_run": s.last_cleanup }))
}

fn validate_policy(p: &CleanupPolicy) -> AppResult<()> {
    if !(1..=3650).contains(&p.unused_days) {
        return Err(AppError::bad_request(
            "unused_days must be between 1 and 3650",
        ));
    }
    if p.repos.is_empty() {
        return Err(AppError::bad_request("choose at least one repository"));
    }
    for r in &p.repos {
        repo_id(r)?;
    }
    Ok(())
}

async fn cleanup_put(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Json(policy): Json<CleanupPolicy>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?;
    validate_policy(&policy)?;
    st.admin.settings.lock().unwrap().cleanup = policy.clone();
    st.admin.save_settings().await?;
    st.admin.sync_cleanup_task();
    tracing::info!(
        "cleanup policy saved: {} after {} days",
        if policy.enabled {
            "enabled"
        } else {
            "disabled"
        },
        policy.unused_days
    );
    let summary = format!(
        "{}, {} days, {}",
        if policy.enabled {
            "scheduled"
        } else {
            "manual"
        },
        policy.unused_days,
        policy.repos.join("+")
    );
    audit(&st, &admin.username, "cleanup.policy", &summary, ip).await;
    Ok(Json(json!({ "policy": policy })))
}

async fn cleanup_preview(
    State(st): State<AppState>,
    body: Option<Json<CleanupPolicy>>,
) -> AppResult<Json<Value>> {
    let policy = match body {
        Some(Json(p)) => {
            validate_policy(&p)?;
            p
        }
        None => st.admin.settings.lock().unwrap().cleanup.clone(),
    };
    Ok(Json(json!(cleanup(&st, &policy, true).await)))
}

async fn cleanup_run(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
) -> AppResult<Response> {
    let admin = require_admin(&auth)?.username.clone();
    audit(&st, &admin, "task.run", "cleanup", ip).await;
    if !run_task(&st, "cleanup").await {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "cleanup is already running",
        ));
    }
    let last = st.admin.settings.lock().unwrap().last_cleanup.clone();
    Ok(Json(json!(last)).into_response())
}

// ------------------------------------------------------------------- tasks

async fn tasks(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "tasks": *st.admin.tasks.lock().unwrap() }))
}

async fn task_run(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let admin = require_admin(&auth)?.username.clone();
    let known = st
        .admin
        .tasks
        .lock()
        .unwrap()
        .iter()
        .find(|t| t.id == id)
        .map(|t| (t.id, t.running));
    let Some((id, running)) = known else {
        return Err(AppError::not_found());
    };
    if running {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "task is already running",
        ));
    }
    audit(&st, &admin, "task.run", id, ip).await;
    tokio::spawn(async move {
        run_task(&st, id).await;
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "started": true }))).into_response())
}

// ------------------------------------------------------------------ system

async fn system(State(st): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let cfg = &st.cfg;
    let disk = {
        let dir = st.store.root().to_path_buf();
        tokio::task::spawn_blocking(move || disk_info(&dir))
            .await
            .ok()
            .flatten()
    };
    let data_dir = std::fs::canonicalize(&cfg.data_dir).unwrap_or_else(|_| cfg.data_dir.clone());
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": st.admin.started_at,
        "uptime_secs": now_secs().saturating_sub(st.admin.started_at),
        "disk": disk,
        "health": *st.admin.health.lock().unwrap(),
        "config": [
            {"key": "GRANARY_LISTEN", "label": "Listen address", "value": cfg.listen},
            {"key": "GRANARY_PUBLIC_URL", "label": "Public URL", "value": cfg.public_url.clone().unwrap_or_else(|| format!("{} (from request)", base_url(cfg, &headers)))},
            {"key": "GRANARY_DATA_DIR", "label": "Data directory", "value": data_dir.display().to_string()},
            {"key": "GRANARY_CACHE_LIMIT", "label": "Cache budget", "value": cfg.cache_limit.map_or_else(|| "None (the whole disk)".to_string(), super::human_bytes)},
            {"key": "GRANARY_META_TTL_SECS", "label": "Metadata freshness", "value": format!("{} seconds", cfg.meta_ttl.as_secs())},
            {"key": "GRANARY_OFFLINE", "label": "Offline mode", "value": if cfg.offline { "On: only cached packages are served, upstreams are never contacted" } else { "Off" }},
            {"key": "GRANARY_CLIENT_AUTH", "label": "Package downloads", "value": if cfg.client_auth { "Need an access token" } else { "Open to anyone who can reach Granary" }},
            {"key": "GRANARY_SESSION_HOURS", "label": "Sign out after inactivity", "value": format!("{} hours", cfg.session_idle.as_secs() / 3600)},
            {"key": "GRANARY_COOKIE_SECURE", "label": "HTTPS-only session cookie", "value": if cfg.cookie_secure { "Yes" } else { "No" }},
            {"key": "GRANARY_CARGO_INDEX", "label": "Cargo index", "value": cfg.cargo_index},
            {"key": "GRANARY_CARGO_DL", "label": "Cargo downloads", "value": cfg.cargo_dl},
            {"key": "GRANARY_NPM_UPSTREAM", "label": "npm registry", "value": cfg.npm_upstream},
            {"key": "GRANARY_NPM_REWRITE_TARBALLS", "label": "npm tarball URLs", "value": if cfg.npm_rewrite_tarballs { "Rewritten to point at Granary" } else { "Kept as the registry lists them, so lockfiles stay portable" }},
            {"key": "GRANARY_PYPI_INDEX", "label": "PyPI index", "value": cfg.pypi_index},
            {"key": "GRANARY_PYPI_FILES", "label": "PyPI files", "value": cfg.pypi_files},
            {"key": "GRANARY_SEARCH", "label": "Package search", "value": if !cfg.search { "Off: exact name lookup only" } else if cfg.offline { "On, but offline mode allows exact lookups of cached packages only" } else { "On" }},
            {"key": "GRANARY_CARGO_API", "label": "Cargo search", "value": cfg.cargo_api},
            {"key": "GRANARY_MAVEN_SEARCH", "label": "Maven search", "value": cfg.maven_search},
            {"key": "GRANARY_MAVEN_UPSTREAM", "label": "Maven repository", "value": cfg.maven_upstream},
            {"key": "GRANARY_GITEA_URL", "label": "Gitea", "value": cfg.gitea.as_ref().map_or("Not connected", |g| g.url.as_str())},
            {"key": "GRANARY_PRIVATE_ACCESS", "label": "Private access", "value": match cfg.private_access {
                Some(m) => format!("{}: {}", m.as_str(), m.describe()),
                None => "Not set (chosen on this page)".to_string(),
            }},
            {"key": "GRANARY_PRIVATE_HOSTS", "label": "Internal hosts allowed", "value": if cfg.private_hosts.is_empty() { "None besides the Gitea host".to_string() } else { cfg.private_hosts.join(", ") }},
            {"key": "GRANARY_EXTRA_CA", "label": "Extra root certificates", "value": cfg.extra_ca.as_ref().map_or_else(|| "None".to_string(), |p| p.display().to_string())},
        ],
        "gitea": cfg.gitea.as_ref().map(|g| json!({
            "url": g.url,
            "host": g.host,
            "status": st.gitea.status(),
        })),
        "private_access": private_access_json(&st),
    }))
}

fn private_access_json(st: &AppState) -> Value {
    json!({
        "mode": st.policy.mode().as_str(),
        "locked": st.policy.locked(),
        "modes": Mode::ALL.iter().map(|m| json!({"id": m.as_str(), "label": m.describe()})).collect::<Vec<_>>(),
    })
}

/// "Test connection" for Gitea.
async fn gitea_check(State(st): State<AppState>, auth: AuthSession) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    let status = st.gitea.check(&st.cfg).await.ok_or_else(|| {
        AppError::new(
            StatusCode::CONFLICT,
            "Gitea is not set up (GRANARY_GITEA_URL)",
        )
    })?;
    Ok(Json(json!({ "status": status })))
}

#[derive(Deserialize)]
struct PrivateAccessBody {
    mode: String,
}

/// Change who may read private content, unless the environment decides.
async fn set_private_access(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Json(body): Json<PrivateAccessBody>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.username.clone();
    let mode = Mode::parse(&body.mode)
        .ok_or_else(|| AppError::bad_request("mode must be signed-in or scoped"))?;
    st.policy
        .set_mode(&st.auth.db, mode)
        .await
        .map_err(|e| AppError::new(StatusCode::CONFLICT, e))?;
    tracing::info!("{admin} set private access to {}", mode.as_str());
    audit(&st, &admin, "system.private_access", mode.as_str(), ip).await;
    Ok(Json(private_access_json(&st)))
}

async fn system_check(State(st): State<AppState>, auth: AuthSession) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    Ok(Json(json!({ "health": check_upstreams(&st).await })))
}

// -------------------------------------------------------------------- logs

#[derive(Deserialize)]
struct LogQuery {
    level: Option<String>,
    q: Option<String>,
}

async fn logs(
    State(st): State<AppState>,
    auth: AuthSession,
    Query(q): Query<LogQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    let rank = |l: &str| match l {
        "ERROR" | "error" => 0,
        "WARN" | "warn" => 1,
        "INFO" | "info" => 2,
        "DEBUG" | "debug" => 3,
        _ => 4,
    };
    let max = q.level.as_deref().map_or(4, rank);
    let needle = q.q.unwrap_or_default().to_lowercase();
    let entries: Vec<_> = st
        .admin
        .logs
        .entries()
        .into_iter()
        .filter(|e| rank(e.level) <= max)
        .filter(|e| needle.is_empty() || e.message.to_lowercase().contains(&needle))
        .take(500)
        .collect();
    Ok(Json(json!({ "entries": entries })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_version_order() {
        let mut v = ["1.10.0", "1.2.0", "1.9.3", "0.9", "1.10.0-rc.1"];
        v.sort_by(|a, b| version_cmp(b, a));
        assert_eq!(v[0], "1.10.0");
        assert_eq!(v[1], "1.10.0-rc.1");
        assert_eq!(v[2], "1.9.3");
        assert_eq!(v.last(), Some(&"0.9"));
    }
}
