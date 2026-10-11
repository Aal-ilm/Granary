//! Private Go modules: paths matching a private prefix (see `goprivate`)
//! are served only from their source, a git repository through the git
//! cache or Gitea's Go registry, and never from the public proxy or the
//! checksum database.
//!
//! They are private content: the access policy is asked first on every
//! request. They are cached apart from public modules, under
//! `go-private/{source}/`.
//!
//! A version's zip and go.mod hashes are pinned the first time they are
//! built or fetched. Anything that later gives a different hash for the same
//! version is refused and written to the audit log.

use std::sync::{Arc, Mutex};

use axum::{
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::{
    AppState,
    error::{AppError, AppResult},
    gomod,
    goprivate::{self, Pin, Route, Source},
    gozip,
    policy::{self, Principal, Resource, refuse},
    store::FileCheck,
};

const NS: &str = "go-private";
const TEXT: &str = "text/plain; charset=utf-8";
const JSON: &str = "application/json";
const ZIP: &str = "application/zip";

fn missing(why: impl Into<String>) -> AppError {
    AppError::new(StatusCode::NOT_FOUND, why)
}

/// The access policy's description of a private module.
pub fn resource(module: &str, source: &Source) -> Resource {
    let mut parts = module.split('/');
    match source {
        Source::Gitea { owner } => Resource::private_module(policy::Source::Gitea, owner, module),
        Source::Mapped { owner, .. } => {
            Resource::private_module(policy::Source::Git, owner, module)
        }
        Source::Git => {
            Resource::private_module(policy::Source::Git, parts.nth(1).unwrap_or(""), module)
        }
    }
}

/// Who is asking, and how to answer them if they may not.
pub struct Asker<'a> {
    pub who: &'a Principal,
    pub headers: &'a HeaderMap,
    pub path: &'a str,
}

/// Serve one request for a private module. `file` is `@latest`, `list`, or
/// `{version}.{info|mod|zip}` (case-encoded).
pub async fn serve(
    st: &AppState,
    asker: Asker<'_>,
    module: &str,
    escmod: &str,
    file: &str,
    route: Route,
) -> AppResult<Response> {
    // Ask who's there before anything else, cached or not.
    let what = resource(module, &route.source);
    if let Some(answer) = refuse(st, asker.who, &what, asker.headers, asker.path) {
        return Ok(answer);
    }
    let key = route.source.cache_key();
    let req = Request {
        module,
        escmod,
        base: format!("{key}/{escmod}"),
        source: route.source.label(),
    };
    match &route.source {
        Source::Gitea { owner } => from_gitea(st, &req, owner, file).await,
        _ => from_git(st, &req, &route, file).await,
    }
}

struct Request<'a> {
    module: &'a str,
    escmod: &'a str,
    /// The module's folder in the cache namespace.
    base: String,
    /// The source's name, for pins and messages.
    source: String,
}

/// What `file` asks for: a version (case-decoded) and its extension.
fn version_file(file: &str) -> AppResult<(String, &str, &str)> {
    let (escver, ext) = file
        .rsplit_once('.')
        .filter(|(_, e)| matches!(*e, "info" | "mod" | "zip"))
        .ok_or_else(|| missing("not a Go module proxy path"))?;
    if escver.is_empty() || escver.contains('/') {
        return Err(missing("invalid version"));
    }
    let version = gomod::unescape(escver).ok_or_else(|| missing("invalid version"))?;
    Ok((version, escver, ext))
}

async fn cached(st: &AppState, rel: &str, ct: &'static str) -> AppResult<Response> {
    st.store.file(NS, rel, "", ct, None).await
}

/// A hash that differs from a version's pin: refuse it, count it, and
/// write it to the audit log.
async fn changed(
    st: &AppState,
    req: &Request<'_>,
    version: &str,
    kind: &str,
    pinned: &str,
    got: &str,
    counted: bool,
) -> AppError {
    if !counted {
        st.store
            .stats
            .integrity_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let what = if kind == "zip" { "zip" } else { "go.mod" };
    let target = format!(
        "{}@{version} {what}: pinned {pinned}, {} now gives {got}; the new one was refused",
        req.module, req.source
    );
    tracing::error!("go: {target}");
    crate::audit::record(
        &st.auth.db,
        "granary",
        "integrity.go_module",
        &target,
        None,
        false,
    )
    .await;
    AppError::bad_gateway(format!(
        "{}@{version} has changed since Granary first saw it: its {what} was {pinned} and is now {got}. A published version must never change, so the new one was refused. If it was republished on purpose, an admin can run `granary go-unpin {} {version}`",
        req.module, req.module
    ))
}

/// Pin a hash on first sight, or refuse one that differs from the pin.
async fn enforce(
    st: &AppState,
    req: &Request<'_>,
    version: &str,
    kind: &str,
    h1: &str,
) -> AppResult<()> {
    let db = &st.auth.db;
    let pin = goprivate::pin(db, req.module, version, kind, h1, &req.source)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match pin {
        Pin::New => {
            tracing::info!("go: pinned {}@{version} {kind} {h1}", req.module);
            Ok(())
        }
        Pin::Same => Ok(()),
        Pin::Changed(old) => Err(changed(st, req, version, kind, &old, h1, false).await),
    }
}

// ----------------------------------------------------------------- git

async fn from_git(
    st: &AppState,
    req: &Request<'_>,
    route: &Route,
    file: &str,
) -> AppResult<Response> {
    let repo = gozip::locate(&st.cfg, req.module, route).map_err(missing)?;
    if file == "@latest" {
        gozip::sync(st, &repo).await?;
        let info = gozip::latest(st, &repo).await?;
        return Ok(([(header::CONTENT_TYPE, JSON)], info.json()).into_response());
    }
    if file == "list" {
        gozip::sync(st, &repo).await?;
        let mut body = gozip::versions(st, &repo).await?.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        return Ok(([(header::CONTENT_TYPE, TEXT)], body).into_response());
    }
    let (version, escver, ext) = version_file(file)?;
    if !gomod::is_canonical(&version) {
        if ext != "info" {
            return Err(missing(format!("{version:?} is not a canonical version")));
        }
        // A branch, tag or commit: what it is can change.
        gozip::sync(st, &repo).await?;
        let info = gozip::stat(st, &repo, &version).await?;
        return Ok(([(header::CONTENT_TYPE, JSON)], info.json()).into_response());
    }
    gomod::check(req.module, &version).map_err(missing)?;
    let zip_rel = format!("{}/@v/{escver}", req.base);
    let (rel, ct) = match ext {
        "zip" => (zip_rel.clone(), ZIP),
        "mod" => (format!("{zip_rel}.mod"), TEXT),
        _ => (format!("{zip_rel}.info"), JSON),
    };
    if st.store.has_file(NS, &rel).await {
        return cached(st, &rel, ct).await;
    }
    // One build at a time for a version.
    let lock = format!("gozip:{}@{version}", req.module);
    st.store
        .with_lock(&lock, async {
            if st.store.has_file(NS, &rel).await {
                return Ok(());
            }
            gozip::sync(st, &repo).await?;
            let info = gozip::stat(st, &repo, &version).await?;
            match ext {
                "info" => st.store.put_file(NS, &rel, info.json().as_bytes()).await,
                "mod" => {
                    let data = gozip::go_mod(st, &repo, &info).await?;
                    enforce(st, req, &version, "mod", &gomod::go_mod_hash(&data)).await?;
                    st.store.put_file(NS, &rel, &data).await
                }
                _ => {
                    let tmp = st.store.new_tmp_path();
                    let built = build(st, req, &repo, &info, &tmp).await;
                    if built.is_err() {
                        let _ = tokio::fs::remove_file(&tmp).await;
                    }
                    built?;
                    st.store.adopt_file(NS, &rel, &tmp).await
                }
            }
        })
        .await?;
    cached(st, &rel, ct).await
}

/// Build a version's zip at `tmp` and check its hash against the pin.
async fn build(
    st: &AppState,
    req: &Request<'_>,
    repo: &gozip::Repo,
    info: &gozip::Info,
    tmp: &std::path::Path,
) -> AppResult<()> {
    gozip::build_zip(st, repo, info, tmp).await?;
    let (path, module, version) = (
        tmp.to_path_buf(),
        req.module.to_string(),
        info.version.clone(),
    );
    let h1 = tokio::task::spawn_blocking(move || gomod::zip_hash(&path, &module, &version))
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| {
            AppError::bad_gateway(format!(
                "{}@{} did not make a valid module zip: {e}",
                req.module, info.version
            ))
        })?;
    enforce(st, req, &info.version, "zip", &h1).await?;
    tracing::info!(
        "go: built {}@{} from {} ({h1})",
        req.module,
        info.version,
        repo.id.display()
    );
    Ok(())
}

// --------------------------------------------------------------- gitea

async fn from_gitea(
    st: &AppState,
    req: &Request<'_>,
    owner: &str,
    file: &str,
) -> AppResult<Response> {
    let Some(g) = &st.cfg.gitea else {
        return Err(missing(
            "this module comes from Gitea's Go registry, but Gitea is not set up (GRANARY_GITEA_URL)",
        ));
    };
    let upstream = format!("{}/api/packages/{owner}/go/{}", g.url, req.escmod);
    if file == "@latest" || file == "list" {
        let (tail, ct) = if file == "list" {
            ("@v/list", TEXT)
        } else {
            ("@latest", JSON)
        };
        let meta = st
            .store
            .meta(
                NS,
                &format!("{}/{tail}", req.base),
                "raw",
                &format!("{upstream}/{tail}"),
                "*/*",
            )
            .await?;
        return Ok(([(header::CONTENT_TYPE, ct)], meta.body).into_response());
    }
    let (version, escver, ext) = version_file(file)?;
    let url = format!("{upstream}/@v/{escver}.{ext}");
    let zip_rel = format!("{}/@v/{escver}", req.base);
    if !gomod::is_canonical(&version) {
        if ext != "info" {
            return Err(missing(format!("{version:?} is not a canonical version")));
        }
        let meta = st
            .store
            .meta(NS, &format!("{zip_rel}.info"), "raw", &url, "*/*")
            .await?;
        return Ok(([(header::CONTENT_TYPE, JSON)], meta.body).into_response());
    }
    gomod::check(req.module, &version).map_err(missing)?;
    if ext == "info" {
        return st
            .store
            .file(NS, &format!("{zip_rel}.info"), &url, JSON, None)
            .await;
    }
    let (zip, kind, rel, ct) = if ext == "zip" {
        (true, "zip", zip_rel, ZIP)
    } else {
        (false, "mod", format!("{zip_rel}.mod"), TEXT)
    };
    if st.store.offline() || st.store.has_file(NS, &rel).await {
        return cached(st, &rel, ct).await;
    }
    // The download is checked against the pin before it is cached; with no
    // pin yet, its hash becomes the pin once it is in.
    let pinned = goprivate::pinned(&st.auth.db, req.module, &version, kind)
        .await
        .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let seen: Arc<Mutex<Option<String>>> = Arc::default();
    let check: FileCheck = {
        let (seen, pinned) = (seen.clone(), pinned.clone());
        let (module, version) = (req.module.to_string(), version.clone());
        Box::new(move |path| {
            let got = if zip {
                gomod::zip_hash(path, &module, &version)?
            } else {
                let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
                if size > gomod::MAX_GO_MOD {
                    return Err("the go.mod file is too large".into());
                }
                gomod::go_mod_hash(&std::fs::read(path).map_err(|e| e.to_string())?)
            };
            *seen.lock().unwrap() = Some(got.clone());
            match pinned {
                Some(old) if old != got => Err(format!("its hash is {got}, but {old} is pinned")),
                Some(_) => Ok(true),
                None => Ok(false),
            }
        })
    };
    let result = st
        .store
        .file_checked(NS, &rel, &url, ct, None, Some(check))
        .await;
    let got = seen.lock().unwrap().clone();
    match (result, pinned, got) {
        (Err(_), Some(old), Some(got)) if old != got => {
            Err(changed(st, req, &version, kind, &old, &got, true).await)
        }
        (Err(e), ..) => Err(e),
        (Ok(resp), None, Some(got)) => {
            // First sight. Someone else may have pinned a different hash in
            // the meantime: then this copy doesn't stay.
            if let Err(e) = enforce(st, req, &version, kind, &got).await {
                let _ = st.store.delete_file(NS, &rel).await;
                return Err(e);
            }
            Ok(resp)
        }
        (Ok(resp), ..) => Ok(resp),
    }
}
