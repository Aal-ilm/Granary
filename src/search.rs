//! "Get packages": search the public registries, look a package up by its
//! exact name, and cache one version of it.
//!
//! | Ecosystem | Search source |
//! |---|---|
//! | npm | `{npm upstream}/-/v1/search` |
//! | Cargo | crates.io's API (`GRANARY_CARGO_API`) |
//! | PyPI | no search API: exact name lookup only |
//! | Maven | search.maven.org (`GRANARY_MAVEN_SEARCH`) |
//!
//! Exact lookups read the registry metadata through Granary's own cache
//! (the same path clients use), so cached packages can be looked up offline.
//! With `GRANARY_SEARCH=off` or in offline mode, nothing is sent to a search
//! host: the search box does exact lookups instead.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    AppState,
    admin::api::version_cmp,
    auth::{AuthSession, require_admin},
    error::{AppError, AppResult},
    formats::{cargo, maven, npm, pypi},
    inventory::{self, meta_key},
    net::ClientIp,
    templates::lines::{self, Eco},
};

/// How long search results are reused.
const RESULT_TTL: Duration = Duration::from_secs(10 * 60);
/// Most cached result sets.
const MAX_RESULTS: usize = 256;
/// Searches that reach upstream: at most one per second per user.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const HITS: usize = 20;

/// Search results by (ecosystem, lowercased query), with when they arrived.
type ResultCache = HashMap<(Eco, String), (Instant, Vec<Hit>)>;

#[derive(Default)]
pub struct SearchState {
    results: Mutex<ResultCache>,
    last_upstream: Mutex<HashMap<i64, Instant>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub name: String,
    pub latest: Option<String>,
    pub description: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/packages/status", get(status))
        .route("/api/packages/search", get(search))
        .route("/api/packages/versions", get(versions))
        .route("/api/packages/files", get(files))
        .route("/api/packages/cache", post(cache))
}

/// Whether the search box may contact search hosts, and why not.
fn search_mode(st: &AppState) -> (bool, Option<&'static str>) {
    if st.cfg.offline {
        (
            false,
            Some(
                "Offline mode is on, so only packages Granary has already cached can be looked up, by exact name.",
            ),
        )
    } else if !st.cfg.search {
        (
            false,
            Some(
                "Search is turned off (GRANARY_SEARCH=off), so packages are looked up by exact name.",
            ),
        )
    } else {
        (true, None)
    }
}

fn eco_param(s: &str) -> AppResult<Eco> {
    Eco::parse(s).ok_or_else(|| AppError::bad_request(format!("unknown ecosystem {s:?}")))
}

async fn status(State(st): State<AppState>, auth: AuthSession) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    let (search, note) = search_mode(&st);
    let warmer = crate::jobs::warmer::health(&st).await;
    Ok(Json(json!({
        "search": search,
        "offline": st.cfg.offline,
        "warmer": warmer.available && !st.cfg.offline,
        "note": note,
        "hosts": {
            "cargo": st.cfg.cargo_api,
            "npm": st.cfg.npm_upstream,
            "maven": st.cfg.maven_search,
        },
    })))
}

#[derive(Deserialize)]
struct SearchQuery {
    eco: String,
    q: String,
}

async fn search(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<Value>> {
    let user = require_admin(&auth)?.clone();
    let eco = eco_param(&q.eco)?;
    let text = q.q.trim();
    if text.is_empty() || text.len() > 200 {
        return Err(AppError::bad_request("type between 1 and 200 characters"));
    }
    let (search_on, note) = search_mode(&st);
    // PyPI has no search API at all.
    let lookup_only = !search_on || eco == Eco::Pypi;
    crate::audit::record(
        &st.auth.db,
        &user.username,
        if lookup_only {
            "package.lookup"
        } else {
            "package.search"
        },
        &format!("{}: {text}", eco.as_str()),
        Some(ip.0),
        true,
    )
    .await;

    let hits = if lookup_only {
        match lookup(&st, eco, text).await? {
            Some(v) => vec![Hit {
                name: v.name,
                latest: v.latest,
                description: None,
            }],
            None => Vec::new(),
        }
    } else {
        search_upstream(&st, user.id, eco, text).await?
    };

    let inv = st.admin.inventory(&st.store).await;
    let results: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            let cached = cached_versions(&inv, eco, &h.name);
            json!({
                "name": h.name,
                "latest": h.latest,
                "description": h.description,
                "cached_versions": cached,
            })
        })
        .collect();
    let note = if eco == Eco::Pypi && search_on {
        Some("PyPI has no search API, so type the exact project name.")
    } else {
        note
    };
    Ok(Json(json!({
        "mode": if lookup_only { "lookup" } else { "search" },
        "note": note,
        "results": results,
    })))
}

fn cached_versions(inv: &[inventory::Asset], eco: Eco, name: &str) -> usize {
    let name = package_key(eco, name);
    let mut versions: Vec<&str> = inv
        .iter()
        .filter(|a| a.repo == eco.as_str() && a.package == name)
        .map(|a| a.version.as_str())
        .collect();
    versions.sort_unstable();
    versions.dedup();
    versions.len()
}

/// The name a package is listed under in the cache inventory.
fn package_key(eco: Eco, name: &str) -> String {
    match eco {
        Eco::Cargo => name.to_ascii_lowercase(),
        Eco::Pypi => pypi::normalize(name),
        Eco::Npm | Eco::Maven => name.to_string(),
    }
}

async fn search_upstream(st: &AppState, user: i64, eco: Eco, q: &str) -> AppResult<Vec<Hit>> {
    let key = (eco, q.to_lowercase());
    if let Some((at, hits)) = st.admin.search.results.lock().unwrap().get(&key)
        && at.elapsed() < RESULT_TTL
    {
        return Ok(hits.clone());
    }
    {
        let mut last = st.admin.search.last_upstream.lock().unwrap();
        if last.get(&user).is_some_and(|t| t.elapsed() < MIN_INTERVAL) {
            return Err(AppError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "one search per second, please wait a moment",
            ));
        }
        last.insert(user, Instant::now());
    }

    let client = st.store.passthrough_client();
    let get = |url: String, query: Vec<(&'static str, String)>| async move {
        st.store.check_url(&url)?;
        let resp = client
            .get(&url)
            .query(&query)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(AppError::bad_gateway(format!(
                "search failed: {url} returned {}",
                resp.status()
            )));
        }
        let body = resp.bytes().await?;
        serde_json::from_slice::<Value>(&body).map_err(|e| {
            AppError::bad_gateway(format!("search failed: {url} sent invalid JSON: {e}"))
        })
    };
    let s = |v: &Value| v.as_str().map(String::from);
    let hits: Vec<Hit> = match eco {
        Eco::Npm => {
            let v = get(
                format!("{}/-/v1/search", st.cfg.npm_upstream),
                vec![("text", q.into()), ("size", HITS.to_string())],
            )
            .await?;
            v["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| {
                    let p = &o["package"];
                    Some(Hit {
                        name: s(&p["name"])?,
                        latest: s(&p["version"]),
                        description: s(&p["description"]),
                    })
                })
                .collect()
        }
        Eco::Cargo => {
            let v = get(
                format!("{}/crates", st.cfg.cargo_api),
                vec![("q", q.into()), ("per_page", HITS.to_string())],
            )
            .await?;
            v["crates"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some(Hit {
                        name: s(&c["name"])?,
                        latest: s(&c["max_stable_version"]).or_else(|| s(&c["max_version"])),
                        description: s(&c["description"]).map(|d| d.trim().to_string()),
                    })
                })
                .collect()
        }
        Eco::Maven => {
            let v = get(
                st.cfg.maven_search.clone(),
                vec![
                    ("q", q.into()),
                    ("rows", HITS.to_string()),
                    ("wt", "json".into()),
                ],
            )
            .await?;
            v["response"]["docs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| {
                    Some(Hit {
                        name: format!("{}:{}", s(&d["g"])?, s(&d["a"])?),
                        latest: s(&d["latestVersion"]).or_else(|| s(&d["v"])),
                        description: None,
                    })
                })
                .collect()
        }
        Eco::Pypi => Vec::new(),
    };

    let mut cache = st.admin.search.results.lock().unwrap();
    if cache.len() >= MAX_RESULTS {
        cache.retain(|_, (at, _)| at.elapsed() < RESULT_TTL);
        if cache.len() >= MAX_RESULTS
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(key, (Instant::now(), hits.clone()));
    Ok(hits)
}

// ------------------------------------------------------------------ lookup

/// A package's versions, newest first.
pub struct Versions {
    pub name: String,
    pub latest: Option<String>,
    pub versions: Vec<String>,
}

/// Whether a version is a pre-release (alpha, beta, rc, dev...).
fn is_prerelease(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    if lower.contains("snapshot") {
        return true;
    }
    // Letters other than a post-release or a Maven/Java qualifier such as
    // "-jre", "-android" or ".Final" mean a pre-release.
    let stable_words = ["post", "jre", "android", "final", "release", "ga"];
    let mut word = String::new();
    for c in lower.chars().chain(std::iter::once('.')) {
        if c.is_ascii_alphabetic() {
            word.push(c);
        } else if !word.is_empty() {
            if !stable_words.contains(&word.as_str()) {
                return true;
            }
            word.clear();
        }
    }
    false
}

fn finish(name: String, mut versions: Vec<String>, latest: Option<String>) -> Versions {
    versions.sort_by(|a, b| version_cmp(b, a));
    versions.dedup();
    // A registry's "latest" can be a pre-release (Maven's <release> often
    // is a milestone); prefer the newest stable version then.
    let latest = latest
        .filter(|l| versions.contains(l) && !is_prerelease(l))
        .or_else(|| versions.iter().find(|v| !is_prerelease(v)).cloned())
        .or_else(|| versions.first().cloned());
    Versions {
        name,
        latest,
        versions,
    }
}

/// Look a package up by exact name through the cached registry metadata.
/// `None` when the registry (or, offline, the cache) doesn't have it.
pub async fn lookup(st: &AppState, eco: Eco, name: &str) -> AppResult<Option<Versions>> {
    let found = match eco {
        Eco::Npm => {
            if !npm::valid_package_name(name) {
                return Ok(None);
            }
            let url = format!("{}/{}", st.cfg.npm_upstream, name.replace('/', "%2f"));
            st.store
                .meta("npm", name, "abbr", &url, npm::ABBREVIATED)
                .await
                .map(|m| {
                    let v: Value = serde_json::from_slice(&m.body).unwrap_or_default();
                    let versions = v["versions"]
                        .as_object()
                        .map(|o| o.keys().cloned().collect())
                        .unwrap_or_default();
                    let latest = v["dist-tags"]["latest"].as_str().map(String::from);
                    finish(name.to_string(), versions, latest)
                })
        }
        Eco::Cargo => {
            let name = name.to_ascii_lowercase();
            let Some(key) = meta_key("cargo", &name)
                .filter(|_| lines::check_line(Eco::Cargo, &format!("{name} = \"*\"")).is_ok())
            else {
                return Ok(None);
            };
            let url = format!("{}/{key}", st.cfg.cargo_index);
            st.store
                .meta("cargo", &key, "raw", &url, "*/*")
                .await
                .map(|m| {
                    #[derive(Deserialize)]
                    struct Line {
                        vers: String,
                        #[serde(default)]
                        yanked: bool,
                    }
                    let versions = m
                        .body
                        .split(|b| *b == b'\n')
                        .filter_map(|l| serde_json::from_slice::<Line>(l).ok())
                        .filter(|l| !l.yanked)
                        .map(|l| l.vers)
                        .collect();
                    finish(name.clone(), versions, None)
                })
        }
        Eco::Pypi => {
            let name = pypi::normalize(name);
            if !pypi::valid_project(&name) {
                return Ok(None);
            }
            let url = format!("{}/{name}/", st.cfg.pypi_index);
            st.store
                .meta("pypi", &name, "json", &url, pypi::SIMPLE_JSON)
                .await
                .map(|m| {
                    let v: Value = serde_json::from_slice(&m.body).unwrap_or_default();
                    // PEP 700 lists versions; older indexes only list files.
                    let versions = match v["versions"].as_array() {
                        Some(list) => list
                            .iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect(),
                        None => pypi_files(&v)
                            .into_iter()
                            .filter_map(|(_, f)| {
                                inventory::classify("pypi", &f).map(|(_, ver)| ver)
                            })
                            .collect(),
                    };
                    finish(name.clone(), versions, None)
                })
        }
        Eco::Maven => {
            let Ok((group, artifact, _)) = lines::maven(name) else {
                return Ok(None);
            };
            let rel = format!("{}/{artifact}/maven-metadata.xml", group.replace('.', "/"));
            let url = format!("{}/{rel}", st.cfg.maven_upstream);
            st.store
                .meta("maven", &rel, "raw", &url, "*/*")
                .await
                .map(|m| {
                    let text = String::from_utf8_lossy(&m.body);
                    let (versions, release) = maven_versions(&text);
                    finish(format!("{group}:{artifact}"), versions, release)
                })
        }
    };
    match found {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.status == StatusCode::NOT_FOUND => Ok(None),
        Err(e) => Err(e),
    }
}

/// Versions and the release version from a maven-metadata.xml.
pub fn maven_versions(xml: &str) -> (Vec<String>, Option<String>) {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return (Vec::new(), None);
    };
    let versions = doc
        .descendants()
        .filter(|n| {
            n.has_tag_name("version") && n.parent().is_some_and(|p| p.has_tag_name("versions"))
        })
        .filter_map(|n| n.text().map(|t| t.trim().to_string()))
        .filter(|v| !v.is_empty() && !v.contains("SNAPSHOT"))
        .collect();
    let release = doc
        .descendants()
        .find(|n| n.has_tag_name("release"))
        .and_then(|n| n.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    (versions, release)
}

/// (url, filename) of every file in a Simple API JSON page.
fn pypi_files(v: &Value) -> Vec<(String, String)> {
    v["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some((
                f["url"].as_str()?.split('#').next()?.to_string(),
                f["filename"].as_str()?.to_string(),
            ))
        })
        .collect()
}

#[derive(Deserialize)]
struct NameQuery {
    eco: String,
    name: String,
}

async fn versions(
    State(st): State<AppState>,
    auth: AuthSession,
    Query(q): Query<NameQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    let eco = eco_param(&q.eco)?;
    let v = lookup(&st, eco, q.name.trim()).await?.ok_or_else(|| {
        AppError::new(
            StatusCode::NOT_FOUND,
            format!("no package named {}", q.name.trim()),
        )
    })?;
    Ok(Json(
        json!({ "name": v.name, "latest": v.latest, "versions": v.versions }),
    ))
}

#[derive(Deserialize)]
struct FilesQuery {
    name: String,
    version: String,
}

/// One PyPI distribution file of a version.
#[derive(Serialize)]
struct DistFile {
    filename: String,
    /// Path under the files host, as used by /pypi/files/.
    path: String,
    kind: &'static str,
}

async fn pypi_version_files(st: &AppState, name: &str, version: &str) -> AppResult<Vec<DistFile>> {
    let name = pypi::normalize(name);
    if !pypi::valid_project(&name) {
        return Err(AppError::not_found());
    }
    let url = format!("{}/{name}/", st.cfg.pypi_index);
    let m = st
        .store
        .meta("pypi", &name, "json", &url, pypi::SIMPLE_JSON)
        .await?;
    let v: Value = serde_json::from_slice(&m.body).unwrap_or_default();
    let prefix = format!("{}/", st.cfg.pypi_files);
    Ok(pypi_files(&v)
        .into_iter()
        .filter(|(_, f)| inventory::classify("pypi", f).is_some_and(|(_, ver)| ver == version))
        .filter_map(|(url, filename)| {
            let path = url.strip_prefix(&prefix)?.to_string();
            let kind = if filename.ends_with(".whl") {
                "wheel"
            } else {
                "sdist"
            };
            Some(DistFile {
                filename,
                path,
                kind,
            })
        })
        .collect())
}

/// The file to suggest: a pure-Python wheel if there is one, else the sdist.
fn suggested(files: &[DistFile]) -> Option<&str> {
    files
        .iter()
        .find(|f| f.filename.ends_with("-none-any.whl"))
        .or_else(|| files.iter().find(|f| f.kind == "sdist"))
        .or_else(|| files.first())
        .map(|f| f.path.as_str())
}

async fn files(
    State(st): State<AppState>,
    auth: AuthSession,
    Query(q): Query<FilesQuery>,
) -> AppResult<Json<Value>> {
    require_admin(&auth)?;
    let files = pypi_version_files(&st, &q.name, &q.version).await?;
    let pick = suggested(&files).map(String::from);
    Ok(Json(json!({ "files": files, "suggested": pick })))
}

// ------------------------------------------------------------------- cache

#[derive(Deserialize)]
struct CacheBody {
    eco: String,
    name: String,
    version: String,
    /// PyPI only: which distribution file.
    file: Option<String>,
}

/// Cache one version of a package directly, without its dependencies: the
/// `.crate`, the npm tarball, one PyPI file, or a Maven POM and its artifact.
async fn cache(
    State(st): State<AppState>,
    auth: AuthSession,
    Extension(ip): Extension<ClientIp>,
    Json(body): Json<CacheBody>,
) -> AppResult<Json<Value>> {
    let admin = require_admin(&auth)?.username.clone();
    let eco = eco_param(&body.eco)?;
    let (name, version) = (body.name.trim(), body.version.trim());
    if version.is_empty() || version.len() > 128 {
        return Err(AppError::bad_request("choose a version"));
    }
    let result = cache_one(&st, eco, name, version, body.file.as_deref()).await;
    crate::audit::record(
        &st.auth.db,
        &admin,
        "package.cache",
        &format!("{}/{name}@{version}", eco.as_str()),
        Some(ip.0),
        result.is_ok(),
    )
    .await;
    let files = result?;
    st.admin.invalidate_inventory();
    let bytes: u64 = files.iter().map(|(_, size)| size).sum();
    tracing::info!(
        "{}: cached {name} {version} on request ({} files)",
        eco.as_str(),
        files.len()
    );
    Ok(Json(json!({
        "files": files.iter().map(|(f, s)| json!({"file": f, "size": s})).collect::<Vec<_>>(),
        "bytes": bytes,
    })))
}

/// Returns (relative path, size) of every file now cached.
pub async fn cache_one(
    st: &AppState,
    eco: Eco,
    name: &str,
    version: &str,
    file: Option<&str>,
) -> AppResult<Vec<(String, u64)>> {
    let mut rels: Vec<String> = Vec::new();
    match eco {
        Eco::Cargo => {
            cargo::fetch_crate(st, name, version).await?;
            let lower = name.to_ascii_lowercase();
            rels.push(format!("{lower}/{lower}-{version}.crate"));
        }
        Eco::Npm => {
            let v = lookup(st, eco, name)
                .await?
                .ok_or_else(AppError::not_found)?;
            if !v.versions.iter().any(|x| x == version) {
                return Err(AppError::new(
                    StatusCode::NOT_FOUND,
                    format!("{name} has no version {version}"),
                ));
            }
            let url = format!("{}/{}", st.cfg.npm_upstream, name.replace('/', "%2f"));
            let meta = st
                .store
                .meta("npm", name, "abbr", &url, npm::ABBREVIATED)
                .await?;
            let doc: Value = serde_json::from_slice(&meta.body).unwrap_or_default();
            let tarball = doc["versions"][version]["dist"]["tarball"]
                .as_str()
                .ok_or_else(|| {
                    AppError::bad_gateway("the registry lists no tarball for that version")
                })?;
            let rel = tarball
                .strip_prefix(&format!("{}/", st.cfg.npm_upstream))
                .ok_or_else(|| {
                    AppError::bad_gateway("the tarball is not on the configured registry")
                })?;
            let (pkg, file) = rel
                .split_once("/-/")
                .ok_or_else(|| AppError::bad_gateway("unexpected tarball URL"))?;
            npm::tarball(st, pkg, file).await?;
            rels.push(format!("{pkg}/-/{file}"));
        }
        Eco::Pypi => {
            let files = pypi_version_files(st, name, version).await?;
            let path = match file {
                Some(f) => f.to_string(),
                None => suggested(&files)
                    .ok_or_else(AppError::not_found)?
                    .to_string(),
            };
            // Only files the project page lists for this version.
            if !files.iter().any(|d| d.path == path) {
                return Err(AppError::bad_request(
                    "that file is not part of this version",
                ));
            }
            pypi::fetch_file(st, &path).await?;
            rels.push(path);
        }
        Eco::Maven => {
            let (group, artifact, version) =
                lines::maven(&format!("{name}:{version}")).map_err(AppError::bad_request)?;
            let version = version.unwrap_or_default();
            let base = format!(
                "{}/{artifact}/{version}/{artifact}-{version}",
                group.replace('.', "/")
            );
            let pom = format!("{base}.pom");
            maven::fetch(st, &pom).await?;
            rels.push(pom.clone());
            let packaging = st
                .store
                .read_file("maven", &pom)
                .await
                .and_then(|b| pom_packaging(&String::from_utf8_lossy(&b)))
                .unwrap_or_else(|| "jar".into());
            let ext = match packaging.as_str() {
                "pom" => None,
                "aar" => Some("aar"),
                "war" => Some("war"),
                // jar, bundle, maven-plugin and the like all ship a jar.
                _ => Some("jar"),
            };
            if let Some(ext) = ext {
                let main = format!("{base}.{ext}");
                maven::fetch(st, &main).await?;
                rels.push(main);
            }
        }
    }
    let dir = st.store.files_dir(eco.as_str());
    let mut out = Vec::new();
    for rel in rels {
        let size = tokio::fs::metadata(dir.join(&rel))
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        out.push((rel, size));
    }
    Ok(out)
}

fn pom_packaging(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let project = doc.root_element();
    project
        .children()
        .find(|c| c.has_tag_name("packaging"))
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prereleases() {
        for pre in [
            "1.0.0-rc.1",
            "2.0b1",
            "1.0.dev3",
            "3.0.0-alpha",
            "1.0-SNAPSHOT",
            "5.0.0-M1",
        ] {
            assert!(is_prerelease(pre), "{pre}");
        }
        for stable in [
            "1.0.0",
            "2.0.post1",
            "33.4.0-jre",
            "33.4.0-android",
            "5.6.Final",
            "1.0",
        ] {
            assert!(!is_prerelease(stable), "{stable}");
        }
    }

    #[test]
    fn latest_skips_prereleases() {
        let v = finish(
            "x".into(),
            vec!["1.0.0".into(), "2.0.0-rc.1".into(), "1.10.0".into()],
            None,
        );
        assert_eq!(v.versions, vec!["2.0.0-rc.1", "1.10.0", "1.0.0"]);
        assert_eq!(v.latest.as_deref(), Some("1.10.0"));
        let v = finish(
            "x".into(),
            vec!["1.0".into(), "1.1".into()],
            Some("1.0".into()),
        );
        assert_eq!(v.latest.as_deref(), Some("1.0"));
        let v = finish(
            "x".into(),
            vec!["4.1.0".into(), "4.2.0-M2".into()],
            Some("4.2.0-M2".into()),
        );
        assert_eq!(v.latest.as_deref(), Some("4.1.0"));
    }

    #[test]
    fn reads_maven_metadata() {
        let xml = "<metadata><versioning><latest>2.0-SNAPSHOT</latest><release>1.1</release><versions><version>1.0</version><version>1.1</version><version>2.0-SNAPSHOT</version></versions></versioning></metadata>";
        let (versions, release) = maven_versions(xml);
        assert_eq!(versions, vec!["1.0", "1.1"]);
        assert_eq!(release.as_deref(), Some("1.1"));
        assert_eq!(
            pom_packaging("<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><packaging>pom</packaging></project>").as_deref(),
            Some("pom")
        );
    }
}
