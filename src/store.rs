//! The cache engine shared by every package format.
//!
//! Two kinds of data are cached:
//!
//! * **Files** (crate archives, npm tarballs, wheels) never change once
//!   published. They are downloaded once and kept forever.
//! * **Metadata** (index entries, packuments, simple-API pages) changes as new
//!   versions are published. It is kept for a configurable TTL, revalidated
//!   with `If-None-Match` / `If-Modified-Since`, and served stale if the
//!   upstream registry is unreachable.
//!
//! Concurrent requests for the same uncached item are coalesced: only one
//! upstream fetch happens and the others wait for its result. Downloads run
//! as their own task, so a client that disconnects doesn't abort them.
//!
//! When the format knows the registry's checksum for a file (Cargo's
//! `cksum`, npm's `integrity`, PyPI's `sha256`), the download is verified
//! before it enters the cache, so a corrupted or tampered response from
//! upstream is never stored or served to anyone.

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::{
    error::{AppError, AppResult},
    util::safe_rel_path,
};

const USER_AGENT: &str = concat!("granary/", env!("CARGO_PKG_VERSION"));

/// Largest metadata document accepted from upstream. The biggest npm
/// packuments are a few hundred megabytes; anything past this is refused
/// rather than buffered in memory.
const MAX_META_BYTES: usize = 512 * 1024 * 1024;

/// A checksum published by the registry for a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checksum {
    Sha256(Vec<u8>),
    Sha512(Vec<u8>),
}

impl Checksum {
    pub fn sha256_hex(s: &str) -> Option<Self> {
        let b = hex::decode(s.trim()).ok()?;
        (b.len() == 32).then_some(Self::Sha256(b))
    }

    /// An npm/SRI `integrity` value; only sha512 (and sha256) are trusted.
    pub fn from_sri(s: &str) -> Option<Self> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        s.split_whitespace().find_map(|part| {
            let (alg, b64) = part.split_once('-')?;
            let b = STANDARD.decode(b64.split('?').next()?).ok()?;
            match (alg, b.len()) {
                ("sha512", 64) => Some(Self::Sha512(b)),
                ("sha256", 32) => Some(Self::Sha256(b)),
                _ => None,
            }
        })
    }
}

enum Hasher {
    Sha256(Sha256),
    Sha512(Sha512),
}

impl Hasher {
    fn new(c: &Checksum) -> Self {
        match c {
            Checksum::Sha256(_) => Self::Sha256(Sha256::new()),
            Checksum::Sha512(_) => Self::Sha512(Sha512::new()),
        }
    }
    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(h) => h.update(data),
            Self::Sha512(h) => h.update(data),
        }
    }
    fn matches(self, c: &Checksum) -> bool {
        match (self, c) {
            (Self::Sha256(h), Checksum::Sha256(want)) => h.finalize().as_slice() == want.as_slice(),
            (Self::Sha512(h), Checksum::Sha512(want)) => h.finalize().as_slice() == want.as_slice(),
            _ => false,
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub file_hits: AtomicU64,
    pub file_misses: AtomicU64,
    pub meta_hits: AtomicU64,
    pub meta_refreshes: AtomicU64,
    pub meta_stale_served: AtomicU64,
    pub integrity_failures: AtomicU64,
}

pub struct Store {
    root: PathBuf,
    meta_client: reqwest::Client,
    file_client: reqwest::Client,
    meta_ttl: Duration,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    tmp_counter: AtomicU64,
    pub stats: Stats,
    /// Last time each cached file was served, keyed by `ns/rel`.
    access: Mutex<HashMap<String, u64>>,
    access_dirty: AtomicBool,
}

/// A metadata document, as stored from upstream (before any URL rewriting).
pub struct Meta {
    pub body: Bytes,
    pub content_type: String,
}

#[derive(Serialize, Deserialize)]
struct MetaInfo {
    fetched_at: u64,
    content_type: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    pub async fn new(root: PathBuf, meta_ttl: Duration) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(root.join("tmp")).await?;
        // Leftover partial downloads from a crash are useless; clear them.
        let mut rd = tokio::fs::read_dir(root.join("tmp")).await?;
        while let Some(e) = rd.next_entry().await? {
            let _ = tokio::fs::remove_file(e.path()).await;
        }

        let meta_client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()?;
        // Files must arrive byte-for-byte, so never let the client transparently
        // decompress them (some servers mislabel .tar.gz with Content-Encoding).
        let file_client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .no_gzip()
            .no_brotli()
            .build()?;

        let access = match tokio::fs::read(root.join("state/access.json")).await {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };

        Ok(Self {
            root,
            meta_client,
            file_client,
            meta_ttl,
            locks: Mutex::new(HashMap::new()),
            tmp_counter: AtomicU64::new(0),
            stats: Stats::default(),
            access: Mutex::new(access),
            access_dirty: AtomicBool::new(false),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Client for uncached pass-through requests.
    pub fn passthrough_client(&self) -> &reqwest::Client {
        &self.meta_client
    }

    /// Run `f` while holding a per-key lock, so concurrent cache misses for the
    /// same item result in a single upstream fetch.
    async fn with_lock<T>(&self, key: &str, f: impl Future<Output = T>) -> T {
        /// Removes the map entry when the last user is done, even if the
        /// request is cancelled part-way through.
        struct Entry<'a> {
            map: &'a Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
            key: &'a str,
            lock: Arc<tokio::sync::Mutex<()>>,
        }
        impl Drop for Entry<'_> {
            fn drop(&mut self) {
                let mut map = self.map.lock().unwrap();
                // Only the map and our clone remain: nobody else is waiting.
                if Arc::strong_count(&self.lock) == 2 {
                    map.remove(self.key);
                }
            }
        }
        let entry = Entry {
            lock: {
                let mut map = self.locks.lock().unwrap();
                map.entry(key.to_string()).or_default().clone()
            },
            map: &self.locks,
            key,
        };
        let _guard = entry.lock.lock().await;
        f.await
    }

    fn tmp_path(&self) -> PathBuf {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        self.root
            .join("tmp")
            .join(format!("{}-{nanos}-{n}.part", std::process::id()))
    }

    /// Write `data` to `dest` atomically (write temp file, then rename).
    async fn write_atomic(&self, dest: &Path, data: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = self.tmp_path();
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, dest).await
    }

    // ------------------------------------------------------------------ files

    /// Whether a file is already cached (so callers can skip looking up its
    /// checksum).
    pub async fn has_file(&self, ns: &str, rel: &str) -> bool {
        match safe_rel_path(rel) {
            Ok(p) => tokio::fs::try_exists(self.files_dir(ns).join(p))
                .await
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Return a response for an immutable file, downloading it from `url` on
    /// first request. `rel` is the path under `files/<ns>/`. When `expected`
    /// is given, the download must match it to be cached.
    pub async fn file(
        self: &Arc<Self>,
        ns: &'static str,
        rel: &str,
        url: &str,
        content_type: &'static str,
        expected: Option<Checksum>,
    ) -> AppResult<Response> {
        let path = self.root.join("files").join(ns).join(safe_rel_path(rel)?);

        if tokio::fs::try_exists(&path).await? {
            self.stats.file_hits.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(ns, rel, "file hit");
            self.touch(ns, rel);
            return serve_file(&path, content_type).await;
        }

        // Download in a separate task: it finishes (and cleans up after
        // itself) even if this client disconnects, and later requests for
        // the same file then find it cached.
        let task = {
            let store = self.clone();
            let (rel, url, path) = (rel.to_string(), url.to_string(), path.clone());
            tokio::spawn(async move {
                let key = format!("file:{ns}:{rel}");
                store
                    .with_lock(&key, async {
                        // Another request may have finished the download while we waited.
                        if tokio::fs::try_exists(&path).await? {
                            store.stats.file_hits.fetch_add(1, Ordering::Relaxed);
                            return Ok(());
                        }
                        store.stats.file_misses.fetch_add(1, Ordering::Relaxed);
                        tracing::info!("{ns}: downloading {rel} from upstream");
                        store.download(&url, &path, expected.as_ref()).await
                    })
                    .await
            })
        };
        task.await.map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("download task failed: {e}"),
            )
        })??;

        self.touch(ns, rel);
        serve_file(&path, content_type).await
    }

    async fn download(&self, url: &str, dest: &Path, expected: Option<&Checksum>) -> AppResult<()> {
        let resp = self.file_client.get(url).send().await?;
        match resp.status() {
            s if s.is_success() => {}
            StatusCode::NOT_FOUND | StatusCode::GONE => return Err(AppError::not_found()),
            s => {
                return Err(AppError::bad_gateway(format!(
                    "upstream returned {s} for {url}"
                )));
            }
        }

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = self.tmp_path();
        let result: AppResult<()> = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut hasher = expected.map(Hasher::new);
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if let Some(h) = &mut hasher {
                    h.update(&chunk);
                }
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            file.sync_all().await?;
            if let (Some(h), Some(want)) = (hasher, expected)
                && !h.matches(want)
            {
                self.stats.integrity_failures.fetch_add(1, Ordering::Relaxed);
                tracing::error!("checksum mismatch for {url}; the download was discarded");
                return Err(AppError::bad_gateway(
                    "the file from upstream does not match the registry's checksum, so it was not cached",
                ));
            }
            Ok(())
        }
        .await;

        match result {
            Ok(()) => Ok(tokio::fs::rename(&tmp, dest).await?),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    // --------------------------------------------------------------- metadata

    /// Fetch a metadata document with TTL caching. `variant` distinguishes
    /// different representations of the same resource (e.g. npm abbreviated vs
    /// full packuments); `accept` is sent upstream.
    pub async fn meta(
        &self,
        ns: &str,
        rel: &str,
        variant: &str,
        url: &str,
        accept: &str,
    ) -> AppResult<Meta> {
        let base = self.root.join("meta").join(ns).join(safe_rel_path(rel)?);
        let file_name = base.file_name().unwrap().to_string_lossy().to_string();
        let body_path = base.with_file_name(format!("{file_name}.{variant}.body"));
        let info_path = base.with_file_name(format!("{file_name}.{variant}.json"));

        if let Some(m) = self.read_meta_if_fresh(&body_path, &info_path).await {
            self.stats.meta_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(m);
        }

        let key = format!("meta:{ns}:{rel}:{variant}");
        self.with_lock(&key, async {
            if let Some(m) = self.read_meta_if_fresh(&body_path, &info_path).await {
                self.stats.meta_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(m);
            }
            self.refresh_meta(&body_path, &info_path, url, accept).await
        })
        .await
    }

    async fn read_info(&self, info_path: &Path) -> Option<MetaInfo> {
        let raw = tokio::fs::read(info_path).await.ok()?;
        serde_json::from_slice(&raw).ok()
    }

    async fn read_meta_if_fresh(&self, body_path: &Path, info_path: &Path) -> Option<Meta> {
        let info = self.read_info(info_path).await?;
        if now_secs().saturating_sub(info.fetched_at) >= self.meta_ttl.as_secs() {
            return None;
        }
        let body = tokio::fs::read(body_path).await.ok()?;
        Some(Meta {
            body: body.into(),
            content_type: info.content_type,
        })
    }

    async fn refresh_meta(
        &self,
        body_path: &Path,
        info_path: &Path,
        url: &str,
        accept: &str,
    ) -> AppResult<Meta> {
        let cached = self.read_info(info_path).await;
        let mut req = self.meta_client.get(url).header(ACCEPT, accept);
        if let Some(info) = &cached {
            if let Some(etag) = &info.etag {
                req = req.header(IF_NONE_MATCH, etag);
            }
            if let Some(lm) = &info.last_modified {
                req = req.header(IF_MODIFIED_SINCE, lm);
            }
        }

        let serve_stale = |reason: String| async move {
            let info = cached_info_or_none(info_path).await;
            match (info, tokio::fs::read(body_path).await) {
                (Some(info), Ok(body)) => {
                    self.stats.meta_stale_served.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("serving stale metadata for {url}: {reason}");
                    Ok(Meta {
                        body: body.into(),
                        content_type: info.content_type,
                    })
                }
                _ => Err(AppError::bad_gateway(reason)),
            }
        };

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return serve_stale(format!("upstream unreachable: {e}")).await,
        };
        let status = resp.status();

        if status == StatusCode::NOT_MODIFIED {
            if let Some(mut info) = cached
                && let Ok(body) = tokio::fs::read(body_path).await
            {
                info.fetched_at = now_secs();
                let content_type = info.content_type.clone();
                self.write_atomic(info_path, &serde_json::to_vec(&info).unwrap())
                    .await?;
                self.stats.meta_refreshes.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("revalidated {url}");
                return Ok(Meta {
                    body: body.into(),
                    content_type,
                });
            }
            return Err(AppError::bad_gateway(format!("unexpected 304 from {url}")));
        }
        if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Err(AppError::not_found());
        }
        if !status.is_success() {
            return serve_stale(format!("upstream returned {status} for {url}")).await;
        }

        let header_str = |name| {
            resp.headers()
                .get(name)
                .and_then(|v: &HeaderValue| v.to_str().ok())
                .map(String::from)
        };
        let info = MetaInfo {
            fetched_at: now_secs(),
            content_type: header_str(header::CONTENT_TYPE)
                .unwrap_or_else(|| "application/octet-stream".into()),
            etag: header_str(ETAG),
            last_modified: header_str(LAST_MODIFIED),
        };
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_META_BYTES as u64)
        {
            return serve_stale(format!(
                "metadata from {url} is larger than {MAX_META_BYTES} bytes"
            ))
            .await;
        }
        let mut buf = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(c) if buf.len() + c.len() <= MAX_META_BYTES => buf.extend_from_slice(&c),
                Ok(_) => {
                    return serve_stale(format!(
                        "metadata from {url} is larger than {MAX_META_BYTES} bytes"
                    ))
                    .await;
                }
                Err(e) => return serve_stale(format!("upstream body error: {e}")).await,
            }
        }
        let body = Bytes::from(buf);

        // Body first, then info: a crash in between leaves stale-but-valid info.
        self.write_atomic(body_path, &body).await?;
        self.write_atomic(info_path, &serde_json::to_vec(&info).unwrap())
            .await?;
        self.stats.meta_refreshes.fetch_add(1, Ordering::Relaxed);
        tracing::info!("fetched metadata {url}");
        Ok(Meta {
            body,
            content_type: info.content_type,
        })
    }
}

// ------------------------------------------------------------ management

impl Store {
    pub fn files_dir(&self, ns: &str) -> PathBuf {
        self.root.join("files").join(ns)
    }

    pub fn meta_dir(&self, ns: &str) -> PathBuf {
        self.root.join("meta").join(ns)
    }

    /// Record that a cached file was just served.
    pub fn touch(&self, ns: &str, rel: &str) {
        self.access
            .lock()
            .unwrap()
            .insert(format!("{ns}/{rel}"), now_secs());
        self.access_dirty.store(true, Ordering::Relaxed);
    }

    /// When a cached file was last served, if ever since tracking began.
    pub fn last_access(&self, ns: &str, rel: &str) -> Option<u64> {
        self.access
            .lock()
            .unwrap()
            .get(&format!("{ns}/{rel}"))
            .copied()
    }

    /// Snapshot of all access times (cheaper than per-file lookups for scans).
    pub fn access_snapshot(&self) -> HashMap<String, u64> {
        self.access.lock().unwrap().clone()
    }

    /// Persist access times if they changed. Returns whether anything was written.
    pub async fn flush_access(&self) -> std::io::Result<bool> {
        if !self.access_dirty.swap(false, Ordering::Relaxed) {
            return Ok(false);
        }
        let data = serde_json::to_vec(&*self.access.lock().unwrap()).unwrap();
        if let Err(e) = self
            .write_atomic(&self.root.join("state/access.json"), &data)
            .await
        {
            self.access_dirty.store(true, Ordering::Relaxed);
            return Err(e);
        }
        Ok(true)
    }

    /// Delete one cached file (and a PyPI `.metadata` companion). Returns bytes freed.
    pub async fn delete_file(&self, ns: &str, rel: &str) -> AppResult<u64> {
        let path = self.files_dir(ns).join(safe_rel_path(rel)?);
        let mut freed = 0;
        for (p, key) in [
            (path.clone(), rel.to_string()),
            (
                PathBuf::from(format!("{}.metadata", path.display())),
                format!("{rel}.metadata"),
            ),
        ] {
            if let Ok(meta) = tokio::fs::metadata(&p).await {
                tokio::fs::remove_file(&p).await?;
                freed += meta.len();
                self.access.lock().unwrap().remove(&format!("{ns}/{key}"));
                self.access_dirty.store(true, Ordering::Relaxed);
            }
        }
        if freed == 0 {
            return Err(AppError::not_found());
        }
        remove_empty_parents(&path, &self.files_dir(ns)).await;
        Ok(freed)
    }

    /// Delete every cached variant of one metadata document. Returns bytes freed.
    pub async fn delete_meta(&self, ns: &str, rel: &str) -> AppResult<u64> {
        let base = self.meta_dir(ns).join(safe_rel_path(rel)?);
        let (Some(dir), Some(name)) = (base.parent(), base.file_name()) else {
            return Ok(0);
        };
        let prefix = format!("{}.", name.to_string_lossy());
        let mut freed = 0;
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return Ok(0);
        };
        while let Some(e) = rd.next_entry().await? {
            let fname = e.file_name().to_string_lossy().to_string();
            // Variant files are named `<name>.<variant>.body|json`, where the
            // variant itself contains no dot.
            let is_variant = fname.strip_prefix(&prefix).is_some_and(|rest| {
                ["body", "json"].iter().any(|ext| {
                    rest.strip_suffix(&format!(".{ext}"))
                        .is_some_and(|v| !v.is_empty() && !v.contains('.'))
                })
            });
            if is_variant {
                freed += e.metadata().await.map(|m| m.len()).unwrap_or(0);
                tokio::fs::remove_file(e.path()).await?;
            }
        }
        Ok(freed)
    }

    /// Drop all cached metadata for a repository, forcing a refresh from
    /// upstream on the next request (Nexus calls this "invalidate cache").
    /// Cached files are kept. Returns bytes freed.
    pub async fn invalidate_meta(&self, ns: &str) -> std::io::Result<u64> {
        let dir = self.meta_dir(ns);
        let freed = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || dir_size(&dir)
        })
        .await
        .unwrap_or(0);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(freed),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }
}

/// Total size of all files under `dir` (blocking).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

/// Remove now-empty directories between `path` and `stop` (exclusive).
async fn remove_empty_parents(path: &Path, stop: &Path) {
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if dir == stop || !dir.starts_with(stop) {
            break;
        }
        if tokio::fs::remove_dir(dir).await.is_err() {
            break; // not empty, or already gone
        }
        cur = dir.parent();
    }
}

async fn cached_info_or_none(info_path: &Path) -> Option<MetaInfo> {
    let raw = tokio::fs::read(info_path).await.ok()?;
    serde_json::from_slice(&raw).ok()
}

async fn serve_file(path: &Path, content_type: &'static str) -> AppResult<Response> {
    let file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, len)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from_stream(ReaderStream::new(file)))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        let mut h = Hasher::new(&Checksum::Sha256(vec![]));
        h.update(b"abc");
        let want = Checksum::sha256_hex(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .unwrap();
        assert!(h.matches(&want));
        let sri = Checksum::from_sri(
            "sha512-3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==",
        )
        .unwrap();
        let mut h = Hasher::new(&sri);
        h.update(b"abc");
        assert!(h.matches(&sri));
        assert!(Checksum::from_sri("sha1-qZk+NkcGgWq6PiVxeFDCbJzQ2J0=").is_none());
        assert!(Checksum::sha256_hex("abcd").is_none());
    }
}
