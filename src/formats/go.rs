//! Go module proxy (the GOPROXY protocol), with the checksum database and
//! the vulnerability database served through it.
//!
//! Client setup (see the README for the HTTPS requirement):
//!
//! ```text
//! go env -w GOPROXY=https://HOST/go GONOPROXY=
//! ```
//!
//! Module paths and versions arrive case-encoded (`!m` for `M`) and are
//! cached that way. Version lists and `@latest` are metadata with the usual
//! TTL; a version's `.info`, `.mod` and `.zip` never change and are cached
//! for good. Before a `.mod` or `.zip` enters the cache, its `h1:` hash must
//! match the checksum database's record for that version, and that record
//! is itself authenticated first (see `sumdb`). A public module without a
//! record is refused unless `GRANARY_GO_ALLOW_UNVERIFIED` is on.
//!
//! The checksum database is served at `/go/sumdb/<name>/`, which `go`
//! finds through `supported` and then uses for every lookup and tile. The
//! vulnerability database is mirrored at `/go/vuln/`.

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};
use bytes::Bytes;

use super::go_private;
use crate::{
    AppState,
    error::{AppError, AppResult},
    gomod,
    policy::Principal,
    store::FileCheck,
    sumdb::{self, Tile},
};

/// Largest lookup answer accepted from the checksum database.
const MAX_LOOKUP: usize = 64 * 1024;
/// Largest tile: 2^8 hashes of 32 bytes.
const MAX_TILE: usize = 256 * 32;

pub fn router() -> Router<AppState> {
    Router::new().route("/go/{*rest}", any(handle))
}

fn missing(why: impl Into<String>) -> AppError {
    AppError::new(StatusCode::NOT_FOUND, why)
}

async fn handle(
    State(st): State<AppState>,
    who: Principal,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> AppResult<Response> {
    if method != Method::GET && method != Method::HEAD {
        return Err(AppError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "the Go module proxy is read-only",
        ));
    }
    // Paths are case-encoded ASCII; anything percent-encoded is refused.
    let rest = uri.path().strip_prefix("/go/").unwrap_or("");
    if rest.contains('%') || rest.contains('\\') || rest.split('/').any(|s| s == "..") {
        return Err(missing("invalid path"));
    }
    if let Some(r) = rest.strip_prefix("sumdb/") {
        return sumdb_proxy(&st, r).await;
    }
    if let Some(r) = rest.strip_prefix("vuln/") {
        return vuln(&st, r).await;
    }
    if rest == "private-prefixes" {
        // For GONOSUMDB, to those who may read private modules.
        let mut body = String::new();
        if st.policy.has_private_access(&who) {
            for p in st.goprivate.patterns() {
                body.push_str(&p);
                body.push('\n');
            }
        }
        return Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response());
    }
    let (escmod, file) = match rest.strip_suffix("/@latest") {
        Some(escmod) => (escmod, "@latest"),
        None => rest
            .rsplit_once("/@v/")
            .ok_or_else(|| missing("not a Go module proxy path"))?,
    };
    let module = module_path(escmod)?;
    let asker = go_private::Asker {
        who: &who,
        headers: &headers,
        path: uri.path(),
    };
    module_file(&st, asker, &module, escmod, file).await
}

/// One file of a module: `@latest`, `list`, or `{version}.{info|mod|zip}`
/// (case-encoded). The one path for clients and Get packages alike, so
/// both use the same cache, the same private routing (with the access
/// policy asked first) and the same checksum verification.
pub async fn module_file(
    st: &AppState,
    asker: go_private::Asker<'_>,
    module: &str,
    escmod: &str,
    file: &str,
) -> AppResult<Response> {
    // A private module only ever comes from its own source.
    if let Some(route) = st.goprivate.route(module) {
        return go_private::serve(st, asker, module, escmod, file, route).await;
    }
    if file == "@latest" {
        let url = format!("{}/{escmod}/@latest", st.cfg.go_upstream);
        let meta = st
            .store
            .meta("go", &format!("{escmod}/@latest"), "raw", &url, "*/*")
            .await?;
        return Ok(([(header::CONTENT_TYPE, "application/json")], meta.body).into_response());
    }
    if file == "list" {
        let url = format!("{}/{escmod}/@v/list", st.cfg.go_upstream);
        let meta = st
            .store
            .meta("go", &format!("{escmod}/@v/list"), "raw", &url, "*/*")
            .await?;
        return Ok((
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            meta.body,
        )
            .into_response());
    }
    let (escver, ext) = file
        .rsplit_once('.')
        .filter(|(_, e)| matches!(*e, "info" | "mod" | "zip"))
        .ok_or_else(|| missing("not a Go module proxy path"))?;
    let version = gomod::unescape(escver).ok_or_else(|| missing("invalid version"))?;
    let url = format!("{}/{escmod}/@v/{escver}.{ext}", st.cfg.go_upstream);
    if ext == "info" && !gomod::is_canonical(&version) {
        // A query (branch, tag, commit): its answer can change.
        if escver.is_empty() || escver.contains('/') {
            return Err(missing("invalid version"));
        }
        let meta = st
            .store
            .meta(
                "go",
                &format!("{escmod}/@v/{escver}.info"),
                "raw",
                &url,
                "*/*",
            )
            .await?;
        return Ok(([(header::CONTENT_TYPE, "application/json")], meta.body).into_response());
    }
    gomod::check(module, &version).map_err(missing)?;
    match ext {
        "info" => {
            st.store
                .file(
                    "go",
                    &format!("{escmod}/@v/{escver}.info"),
                    &url,
                    "application/json",
                    None,
                )
                .await
        }
        "mod" => verified(st, escmod, escver, module, &version, false, &url).await,
        _ => verified(st, escmod, escver, module, &version, true, &url).await,
    }
}

/// The module path a case-encoded path element names, or a 404.
fn module_path(escmod: &str) -> AppResult<String> {
    let module = gomod::unescape(escmod).ok_or_else(|| missing("invalid module path"))?;
    gomod::check_path(&module).map_err(|e| missing(format!("invalid module path: {e}")))?;
    Ok(module)
}

/// Where a version's zip (without an extension, so its `.mod` and `.info`
/// are its companions) or go.mod is cached.
pub fn cached_rel(escmod: &str, escver: &str, zip: bool) -> String {
    if zip {
        format!("{escmod}/@v/{escver}")
    } else {
        format!("{escmod}/@v/{escver}.mod")
    }
}

/// Serve a `.mod` or `.zip`, checking a download against the checksum
/// database before it is cached.
async fn verified(
    st: &AppState,
    escmod: &str,
    escver: &str,
    module: &str,
    version: &str,
    zip: bool,
    url: &str,
) -> AppResult<Response> {
    let rel = cached_rel(escmod, escver, zip);
    let ct = if zip {
        "application/zip"
    } else {
        "text/plain; charset=utf-8"
    };
    if st.store.offline() || st.store.has_file("go", &rel).await {
        return st.store.file("go", &rel, url, ct, None).await;
    }
    let lines = lookup_lines(st, escmod, escver).await?;
    let (zip_sum, mod_sum) = lines
        .as_deref()
        .map(|l| sumdb::sums(l, module, version))
        .unwrap_or_default();
    let want = if zip { zip_sum } else { mod_sum };
    let (module, version) = (module.to_string(), version.to_string());
    let check: FileCheck = match want {
        Some(want) => Box::new(move |path| {
            let got = hash(path, &module, &version, zip)?;
            if got == want {
                Ok(true)
            } else {
                Err(format!(
                    "its hash is {got}, but the checksum database records {want}"
                ))
            }
        }),
        None if st.cfg.go_allow_unverified => {
            tracing::warn!(
                "go: {module}@{version} has no checksum database record; caching it unverified (GRANARY_GO_ALLOW_UNVERIFIED is on)"
            );
            // Still a well-formed zip or go.mod of the right size.
            Box::new(move |path| hash(path, &module, &version, zip).map(|_| false))
        }
        None => {
            st.store
                .stats
                .integrity_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                "go: {module}@{version} has no checksum database record, so it was refused"
            );
            return Err(AppError::bad_gateway(format!(
                "{module}@{version} has no record in the checksum database, so Granary can't verify it and won't serve it (GRANARY_GO_ALLOW_UNVERIFIED)"
            )));
        }
    };
    st.store
        .file_checked("go", &rel, url, ct, None, Some(check))
        .await
}

/// The `h1:` hash of a downloaded zip or go.mod (blocking).
fn hash(path: &std::path::Path, module: &str, version: &str, zip: bool) -> Result<String, String> {
    if zip {
        return gomod::zip_hash(path, module, version);
    }
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > gomod::MAX_GO_MOD {
        return Err("the go.mod file is too large".into());
    }
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(gomod::go_mod_hash(&data))
}

// ------------------------------------------------------- checksum database

fn sumdb_name(st: &AppState) -> &str {
    &st.cfg.go_sumdb_key.name
}

/// The go.sum lines the checksum database records for a version,
/// authenticated; `None` when it has no record. Answers are cached once
/// verified (with the tiles that proved them), so they are served offline.
pub async fn lookup_lines(
    st: &AppState,
    escmod: &str,
    escver: &str,
) -> AppResult<Option<Vec<String>>> {
    Ok(lookup_body(st, escmod, escver)
        .await?
        .map(|body| sumdb::record_lines(&body)))
}

/// A lookup answer, verified (from the cache, or fetched and verified).
async fn lookup_body(st: &AppState, escmod: &str, escver: &str) -> AppResult<Option<Bytes>> {
    let name = sumdb_name(st).to_string();
    let rel = format!("{name}/lookup/{escmod}@{escver}");
    if let Some(cached) = st.store.read_file("go-sumdb", &rel).await {
        return Ok(Some(cached.into()));
    }
    if st.store.offline() {
        return Err(AppError::offline(&format!(
            "the checksum database record for {escmod}@{escver}"
        )));
    }
    let url = format!("{}/lookup/{escmod}@{escver}", st.cfg.go_sumdb);
    let Some(body) = st.store.fetch_small(&url, MAX_LOOKUP).await? else {
        return Ok(None);
    };
    // Cached tiles first; if they don't prove the record, fetch them again.
    let mut last = String::new();
    for fresh in [false, true] {
        let reader = |tiles: Vec<Tile>| read_tiles(st, tiles, fresh);
        match sumdb::verify_lookup(&body, &st.cfg.go_sumdb_key, reader).await {
            Ok(v) => {
                for (tile, data) in &v.tiles {
                    let trel = format!("{name}/{}", tile.path());
                    if fresh {
                        let _ = st.store.delete_file("go-sumdb", &trel).await;
                    }
                    let _ = st.store.put_file("go-sumdb", &trel, data).await;
                }
                st.store.put_file("go-sumdb", &rel, &body).await?;
                return Ok(Some(body));
            }
            Err(e) => last = e,
        }
    }
    st.store
        .stats
        .integrity_failures
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::error!(
        "go: the checksum database's answer for {escmod}@{escver} failed verification: {last}"
    );
    Err(AppError::bad_gateway(format!(
        "the checksum database's answer for {escmod}@{escver} could not be verified: {last}"
    )))
}

/// Tile data, from the cache unless `fresh`, else from the database.
async fn read_tiles(st: &AppState, tiles: Vec<Tile>, fresh: bool) -> Result<Vec<Vec<u8>>, String> {
    let name = sumdb_name(st);
    let mut out = Vec::with_capacity(tiles.len());
    for t in tiles {
        let path = t.path();
        if !fresh
            && let Some(data) = st
                .store
                .read_file("go-sumdb", &format!("{name}/{path}"))
                .await
        {
            out.push(data);
            continue;
        }
        let url = format!("{}/{path}", st.cfg.go_sumdb);
        let data = st
            .store
            .fetch_small(&url, MAX_TILE)
            .await
            .map_err(|e| e.message)?
            .ok_or_else(|| format!("the checksum database has no {path}"))?;
        out.push(data.to_vec());
    }
    Ok(out)
}

/// Whether a path is a tile path: `tile/H/L/[xNNN/]NNN[.p/W]`, `L` a number
/// or `data`.
fn valid_tile_path(p: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit());
    let Some(rest) = p.strip_prefix("tile/") else {
        return false;
    };
    let (main, partial) = match rest.split_once(".p/") {
        Some((m, w)) => (m, Some(w)),
        None => (rest, None),
    };
    if partial.is_some_and(|w| !digits(w) || w == "0") {
        return false;
    }
    let parts: Vec<&str> = main.split('/').collect();
    if parts.len() < 3 || !digits(parts[0]) || !(parts[1] == "data" || digits(parts[1])) {
        return false;
    }
    let n = &parts[2..];
    n.iter().enumerate().all(|(i, s)| {
        if i + 1 < n.len() {
            s.strip_prefix('x').is_some_and(digits) && s.len() == 4
        } else {
            digits(s) && s.len() == 3
        }
    })
}

async fn sumdb_proxy(st: &AppState, rest: &str) -> AppResult<Response> {
    let name = sumdb_name(st);
    let Some(op) = rest.strip_prefix(name).and_then(|r| r.strip_prefix('/')) else {
        // Another database: let `go` reach it however it would otherwise.
        return Err(missing(
            "this proxy only serves the checksum database it trusts",
        ));
    };
    let text = [(header::CONTENT_TYPE, "text/plain; charset=utf-8")];
    if op == "supported" {
        return Ok((text, "").into_response());
    }
    if op == "latest" {
        let url = format!("{}/latest", st.cfg.go_sumdb);
        let meta = st
            .store
            .meta("go-sumdb", &format!("{name}/latest"), "raw", &url, "*/*")
            .await?;
        return Ok((text, meta.body).into_response());
    }
    if let Some(target) = op.strip_prefix("lookup/") {
        let (escmod, escver) = target
            .rsplit_once('@')
            .ok_or_else(|| missing("invalid lookup"))?;
        let module = module_path(escmod)?;
        let version = gomod::unescape(escver).ok_or_else(|| missing("invalid version"))?;
        gomod::check(&module, &version).map_err(missing)?;
        // Nothing about a private module goes to the checksum database.
        if st.goprivate.route(&module).is_some() {
            return Err(missing(format!(
                "{module} is a private module, which the checksum database is never asked about. Add its prefix to GONOSUMDB (go env -w GONOSUMDB=...)"
            )));
        }
        return match lookup_body(st, escmod, escver).await? {
            Some(body) => Ok((text, body).into_response()),
            None => Err(missing(format!(
                "the checksum database has no record of {module}@{version}"
            ))),
        };
    }
    if valid_tile_path(op) {
        let url = format!("{}/{op}", st.cfg.go_sumdb);
        return st
            .store
            .file(
                "go-sumdb",
                &format!("{name}/{op}"),
                &url,
                "application/octet-stream",
                None,
            )
            .await;
    }
    Err(missing("not a checksum database path"))
}

// ------------------------------------------------- vulnerability database

/// Whether a path is one the vulnerability database serves.
fn valid_vuln_path(p: &str) -> bool {
    let p = p.strip_suffix(".gz").unwrap_or(p);
    if matches!(
        p,
        "index/db.json" | "index/modules.json" | "index/vulns.json"
    ) {
        return true;
    }
    p.strip_prefix("ID/")
        .and_then(|id| id.strip_suffix(".json"))
        .is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 64
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

async fn vuln(st: &AppState, path: &str) -> AppResult<Response> {
    if !valid_vuln_path(path) {
        return Err(missing("not a vulnerability database path"));
    }
    let url = format!("{}/{path}", st.cfg.go_vulndb);
    let meta = st.store.meta("go-vuln", path, "raw", &url, "*/*").await?;
    let ct = if path.ends_with(".gz") {
        "application/gzip"
    } else {
        "application/json"
    };
    Ok(([(header::CONTENT_TYPE, ct)], meta.body).into_response())
}

/// A vulnerability database file: its path, and its contents plain and
/// gzipped.
type VulnFile = (String, Vec<u8>, Vec<u8>);

/// The daily task: download the whole vulnerability database and store
/// every file, plain and gzipped, so it is all available offline.
pub async fn sync_vulndb(st: &AppState) -> Result<String, String> {
    if st.store.offline() {
        return Ok("offline mode: not contacted".into());
    }
    let url = format!("{}/vulndb.zip", st.cfg.go_vulndb);
    st.store.check_url(&url).map_err(|e| e.message)?;
    let resp = st
        .store
        .passthrough_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| crate::error::error_chain(&e))?;
    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n > 256 << 20) {
        return Err(format!("{url} is unexpectedly large"));
    }
    let data = resp.bytes().await.map_err(|e| e.to_string())?;
    let files = tokio::task::spawn_blocking(move || -> Result<Vec<VulnFile>, String> {
        use std::io::{Read, Write};
        let mut zip =
            zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
            let name = f.name().map_err(|e| e.to_string())?.to_string();
            if !valid_vuln_path(&name) || name.ends_with(".gz") {
                continue;
            }
            let mut json = Vec::new();
            f.by_ref()
                .take(64 << 20)
                .read_to_end(&mut json)
                .map_err(|e| e.to_string())?;
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(&json).map_err(|e| e.to_string())?;
            let gz = gz.finish().map_err(|e| e.to_string())?;
            out.push((name, json, gz));
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())??;
    for (name, json, gz) in &files {
        st.store
            .put_meta("go-vuln", name, "raw", json, "application/json")
            .await
            .map_err(|e| e.message)?;
        st.store
            .put_meta(
                "go-vuln",
                &format!("{name}.gz"),
                "raw",
                gz,
                "application/json",
            )
            .await
            .map_err(|e| e.message)?;
    }
    Ok(format!(
        "Stored {} files from the Go vulnerability database",
        files.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in [
            "tile/8/0/001",
            "tile/8/2/x123/456.p/7",
            "tile/8/data/000.p/1",
        ] {
            assert!(valid_tile_path(ok), "{ok}");
        }
        for bad in [
            "tile/8/0/1",
            "tile/8/0/001.p/0",
            "tile/8/x/001",
            "tile/8/0/x12/001",
            "tiles/8/0/001",
            "tile/8/0/001/",
        ] {
            assert!(!valid_tile_path(bad), "{bad}");
        }
        for ok in [
            "index/db.json",
            "index/modules.json.gz",
            "ID/GO-2022-0969.json",
            "ID/GO-2022-0969.json.gz",
        ] {
            assert!(valid_vuln_path(ok), "{ok}");
        }
        for bad in [
            "index/other.json",
            "ID/../x.json",
            "ID/GO-1.txt",
            "vulndb.zip",
        ] {
            assert!(!valid_vuln_path(bad), "{bad}");
        }
    }
}
