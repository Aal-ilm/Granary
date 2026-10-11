//! Maven repository proxy (Maven 2 layout), for Maven and Gradle.
//!
//! Client setup (`~/.m2/settings.xml`):
//!
//! ```xml
//! <mirrors>
//!   <mirror>
//!     <id>granary</id>
//!     <mirrorOf>central</mirrorOf>
//!     <url>http://HOST:8080/maven</url>
//!   </mirror>
//! </mirrors>
//! ```
//!
//! Release files (jars, POMs, Gradle module files, signatures and the
//! checksum files next to them) never change on Maven Central, so they are
//! cached forever. Before a `.jar` or `.pom` enters the cache it is checked
//! against the `.sha1` upstream publishes for it. `maven-metadata.xml` lists
//! versions and changes as new ones are published, so it (and its checksums)
//! is cached as metadata with the usual TTL and revalidation.
//!
//! Central has no snapshots, so `-SNAPSHOT` paths are refused without asking
//! upstream. Maven and Gradle probe for many files that don't exist, so a 404
//! from upstream is remembered for a few minutes.
//!
//! GroupIds listed as private namespaces (see `namespaces`) come from Gitea's
//! Maven registry instead, and only from there, with the same `.sha1`
//! checks. Their 404s are never remembered, so a package published to Gitea
//! shows up at once. They are cached under `gitea-maven/{owner}/`.

use std::time::Duration;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};

use crate::{
    AppState,
    error::{AppError, AppResult},
    policy::{Principal, Resource, refuse},
    store::Checksum,
    templates::lines::Eco,
};

/// Where a repository path comes from: Maven Central, or Gitea.
struct Origin {
    ns: &'static str,
    owner: Option<String>,
    base: String,
}

impl Origin {
    fn rel(&self, rel: &str) -> String {
        match &self.owner {
            Some(o) => format!("{o}/{rel}"),
            None => rel.to_string(),
        }
    }
}

/// The groupId a repository path belongs to (release file or metadata).
fn group_of(rel: &str) -> Option<String> {
    if let Some((group, ..)) = coordinates(rel) {
        return Some(group);
    }
    let segs: Vec<&str> = rel.split('/').collect();
    (segs.len() >= 3).then(|| segs[..segs.len() - 2].join("."))
}

/// Route a path: private groupIds to Gitea, everything else to Central.
fn origin(st: &AppState, rel: &str) -> Origin {
    let owner = group_of(rel).and_then(|g| st.namespaces.owner(Eco::Maven, &g));
    match (owner, &st.cfg.gitea) {
        (Some(owner), Some(g)) => Origin {
            ns: "gitea-maven",
            base: format!("{}/api/packages/{owner}/maven", g.url),
            owner: Some(owner),
        },
        _ => Origin {
            ns: "maven",
            owner: None,
            base: st.cfg.maven_upstream.clone(),
        },
    }
}

/// The access policy's description of a private path, if it is one.
fn resource(st: &AppState, rel: &str) -> Option<Resource> {
    let o = origin(st, rel);
    let owner = o.owner?;
    let name = match coordinates(rel) {
        Some((group, artifact, ..)) => format!("{group}:{artifact}"),
        None => group_of(rel).unwrap_or_default(),
    };
    Some(Resource::gitea_package("maven", &owner, &name))
}

/// How long a 404 from upstream is remembered.
const NEGATIVE_TTL: Duration = Duration::from_secs(5 * 60);

/// Largest `.sha1` file accepted from upstream.
const MAX_SHA1_BYTES: usize = 1024;

const METADATA: &str = "maven-metadata.xml";
const CHECKSUMS: [&str; 4] = [".sha1", ".md5", ".sha256", ".sha512"];

pub fn router() -> Router<AppState> {
    Router::new().route("/maven/{*path}", any(handle))
}

/// What a request path refers to.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// `maven-metadata.xml` or one of its checksums: cached with a TTL.
    Metadata,
    /// A release file, cached forever. `verify` when upstream's `.sha1` must
    /// match before it is cached.
    Release { verify: bool },
}

/// Check the raw request path and return the repository path under `/maven/`.
/// Rejects traversal, empty segments, backslashes and encoded slashes, and
/// anything outside the characters Maven coordinates use.
fn checked_path(uri: &Uri) -> AppResult<String> {
    let bad = || AppError::bad_request("invalid Maven repository path");
    let raw = uri.path().strip_prefix("/maven/").ok_or_else(bad)?;
    let lower = raw.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") || raw.contains('\\') {
        return Err(bad());
    }
    let rel = percent_decode(raw).ok_or_else(bad)?;
    let ok_chars = rel
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'+' | b'~'));
    let ok_segments = rel
        .split('/')
        .all(|s| !s.is_empty() && s != "." && s != ".." && s.len() <= 255);
    if rel.is_empty() || rel.len() > 1024 || !ok_chars || !ok_segments || rel.contains("..") {
        return Err(bad());
    }
    Ok(rel)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn is_checksum(file: &str) -> bool {
    CHECKSUMS.iter().any(|ext| file.ends_with(ext))
}

/// Split a release file path into group, artifact, version and file name,
/// following the Maven 2 layout `group/path/artifact/version/artifact-version[-classifier].ext`.
pub fn coordinates(rel: &str) -> Option<(String, &str, &str, &str)> {
    let segs: Vec<&str> = rel.split('/').collect();
    let n = segs.len();
    if n < 4 {
        return None;
    }
    let (artifact, version, file) = (segs[n - 3], segs[n - 2], segs[n - 1]);
    let rest = file.strip_prefix(artifact)?.strip_prefix('-')?;
    let rest = rest.strip_prefix(version)?;
    // Next comes `-classifier` or `.ext`; an extension never starts with a
    // digit, which keeps version 1.0 from matching `a-1.0.1.jar`.
    let ok = match rest.as_bytes() {
        [b'-', c, ..] | [b'.', c, ..] => !c.is_ascii_digit() || rest.starts_with('-'),
        _ => false,
    };
    if !ok {
        return None;
    }
    Some((segs[..n - 3].join("."), artifact, version, file))
}

fn classify(rel: &str) -> Option<Kind> {
    let file = rel.rsplit('/').next()?;
    if file == METADATA
        || CHECKSUMS
            .iter()
            .any(|ext| file == format!("{METADATA}{ext}"))
    {
        return (rel.contains('/')).then_some(Kind::Metadata);
    }
    coordinates(rel)?;
    let verify = !is_checksum(file) && (file.ends_with(".jar") || file.ends_with(".pom"));
    Some(Kind::Release { verify })
}

fn content_type(file: &str) -> &'static str {
    let ext = file.rsplit('.').next().unwrap_or("");
    match ext {
        "jar" | "war" | "aar" => "application/java-archive",
        "pom" | "xml" => "application/xml",
        "module" => "application/json",
        "asc" => "application/pgp-signature",
        "sha1" | "md5" | "sha256" | "sha512" => "text/plain",
        _ => "application/octet-stream",
    }
}

async fn handle(
    State(st): State<AppState>,
    who: Principal,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> AppResult<Response> {
    if method != Method::GET && method != Method::HEAD {
        let mut resp = AppError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "this is a read-only proxy; deploying to it is not supported",
        )
        .into_response();
        resp.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return Ok(resp);
    }
    let rel = checked_path(&uri)?;
    // Private groupIds: ask who's there before anything else.
    if let Some(r) = resource(&st, &rel)
        && let Some(answer) = refuse(&st, &who, &r, &headers, uri.path())
    {
        return Ok(answer);
    }
    fetch(&st, &rel).await
}

/// Serve one repository path (already checked), with the snapshot rule,
/// the negative cache and checksum verification.
pub async fn fetch(st: &AppState, rel: &str) -> AppResult<Response> {
    if rel.contains("-SNAPSHOT") {
        return Err(AppError::new(
            StatusCode::NOT_FOUND,
            "Maven Central has no snapshots",
        ));
    }
    let kind = classify(rel).ok_or_else(AppError::not_found)?;
    let o = origin(st, rel);

    // Private paths never use the negative cache: a package just published
    // to Gitea shows up at once.
    let missing_key = format!("maven:{rel}");
    let public = o.owner.is_none();
    if public && st.store.known_missing(&missing_key) {
        return Err(AppError::not_found());
    }
    let result = match kind {
        Kind::Metadata => metadata(st, &o, rel).await,
        Kind::Release { verify } => release(st, &o, rel, verify).await,
    };
    // Only upstream's own 404s are remembered; an offline miss isn't one.
    if let Err(e) = &result
        && e.status == StatusCode::NOT_FOUND
        && !st.store.offline()
        && public
    {
        st.store.remember_missing(&missing_key, NEGATIVE_TTL);
    }
    result
}

/// A `maven-metadata.xml` (or other metadata path), from wherever its
/// groupId comes from.
pub async fn metadata_doc(st: &AppState, rel: &str) -> AppResult<crate::store::Meta> {
    let o = origin(st, rel);
    let url = format!("{}/{rel}", o.base);
    st.store.meta(o.ns, &o.rel(rel), "raw", &url, "*/*").await
}

/// Where a release file is cached: its namespace and path there.
pub fn cached_at(st: &AppState, rel: &str) -> (&'static str, String) {
    let o = origin(st, rel);
    (o.ns, o.rel(rel))
}

async fn metadata(st: &AppState, o: &Origin, rel: &str) -> AppResult<Response> {
    let url = format!("{}/{rel}", o.base);
    let meta = st.store.meta(o.ns, &o.rel(rel), "raw", &url, "*/*").await?;
    Ok(([(header::CONTENT_TYPE, content_type(rel))], meta.body).into_response())
}

async fn release(st: &AppState, o: &Origin, rel: &str, verify: bool) -> AppResult<Response> {
    let url = format!("{}/{rel}", o.base);
    let ct = content_type(rel);
    let (ns, cached) = (o.ns, o.rel(rel));
    // Offline, there's no checksum to fetch; the store serves the file if it
    // is cached and otherwise says it is offline.
    if !verify || st.store.offline() || st.store.has_file(ns, &cached).await {
        return st.store.file(ns, &cached, &url, ct, None).await;
    }

    // Verify against upstream's .sha1, and keep that file next to the
    // artifact once the artifact is cached.
    let sha1_rel = format!("{cached}.sha1");
    let sha1 = match st.store.read_file(ns, &sha1_rel).await {
        Some(b) => Some(b.into()),
        None => {
            st.store
                .fetch_small(&format!("{url}.sha1"), MAX_SHA1_BYTES)
                .await?
        }
    };
    // Without a .sha1 upstream, the store caches the file unverified and
    // says so in the log once the download succeeds.
    let expected = sha1
        .as_ref()
        .map(|b| {
            Checksum::sha1_file(&String::from_utf8_lossy(b)).ok_or_else(|| {
                AppError::bad_gateway(format!("upstream's checksum file for {rel} is not a SHA-1"))
            })
        })
        .transpose()?;
    let resp = st.store.file(ns, &cached, &url, ct, expected).await?;
    if let Some(b) = sha1
        && let Err(e) = st.store.put_file(ns, &sha1_rel, &b).await
    {
        tracing::warn!("maven: could not cache {sha1_rel}: {}", e.message);
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> AppResult<String> {
        checked_path(&p.parse::<Uri>().unwrap())
    }

    #[test]
    fn rejects_bad_paths() {
        for bad in [
            "/maven/org/../etc/passwd",
            "/maven/org//x/1/x-1.jar",
            "/maven/org/x/1/x-1.jar/",
            "/maven/org%2fx/1/x-1.jar",
            "/maven/org%2Fx/1/x-1.jar",
            "/maven/org%5cx/1/x-1.jar",
            "/maven/org/%2e%2e/x",
            "/maven/./org/x",
        ] {
            assert!(path(bad).is_err(), "{bad:?} should be rejected");
        }
        assert_eq!(
            path("/maven/org/slf4j/slf4j-api/2.0.17/slf4j-api-2.0.17.jar").unwrap(),
            "org/slf4j/slf4j-api/2.0.17/slf4j-api-2.0.17.jar"
        );
    }

    #[test]
    fn classifies_paths() {
        let jar = "com/google/guava/guava/33.4.0-jre/guava-33.4.0-jre.jar";
        assert_eq!(classify(jar), Some(Kind::Release { verify: true }));
        assert_eq!(
            classify("com/google/guava/guava/33.4.0-jre/guava-33.4.0-jre-sources.jar"),
            Some(Kind::Release { verify: true })
        );
        assert_eq!(
            classify("com/google/guava/guava/33.4.0-jre/guava-33.4.0-jre.module"),
            Some(Kind::Release { verify: false })
        );
        assert_eq!(
            classify(&format!("{jar}.sha1")),
            Some(Kind::Release { verify: false })
        );
        assert_eq!(
            classify("com/google/guava/guava/maven-metadata.xml"),
            Some(Kind::Metadata)
        );
        assert_eq!(
            classify("com/google/guava/guava/maven-metadata.xml.sha256"),
            Some(Kind::Metadata)
        );
        // Not in the Maven layout.
        assert_eq!(
            classify("com/google/guava/guava/33.4.0-jre/other-1.jar"),
            None
        );
        assert_eq!(classify("com/google/guava"), None);
        assert_eq!(classify("maven-metadata.xml"), None);
    }

    #[test]
    fn splits_coordinates() {
        let (g, a, v, f) =
            coordinates("org/apache/commons/commons-lang3/3.17.0/commons-lang3-3.17.0.pom")
                .unwrap();
        assert_eq!(
            (g.as_str(), a, v, f),
            (
                "org.apache.commons",
                "commons-lang3",
                "3.17.0",
                "commons-lang3-3.17.0.pom"
            )
        );
        // "1.0" must not match version "1.0.1".
        assert!(coordinates("g/a/1.0/a-1.0.1.jar").is_none());
    }
}
