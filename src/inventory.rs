//! Turns the on-disk cache into a list of packages and versions per
//! repository, for the browse, search and cleanup features.

use std::{collections::HashMap, path::Path, time::UNIX_EPOCH};

use serde::Serialize;

use crate::formats::pypi::normalize;

/// The repositories Granary serves, in display order.
pub const REPOS: [&str; 3] = ["cargo", "npm", "pypi"];

/// One cached file.
#[derive(Clone, Debug, Serialize)]
pub struct Asset {
    pub repo: &'static str,
    pub package: String,
    pub version: String,
    /// Path under `files/<repo>/`, as used in delete requests.
    pub path: String,
    pub file_name: String,
    pub size: u64,
    /// When the file entered the cache (unix seconds).
    pub cached_at: u64,
    /// When a client last downloaded it, if recorded.
    pub last_access: Option<u64>,
}

/// Work out which package and version a cached file belongs to.
/// Returns `None` for files that aren't packages (e.g. PyPI `.metadata`).
pub fn classify(repo: &str, rel: &str) -> Option<(String, String)> {
    match repo {
        "cargo" => {
            let (name, file) = rel.split_once('/')?;
            let version = file
                .strip_prefix(&format!("{name}-"))?
                .strip_suffix(".crate")?;
            Some((name.to_string(), version.to_string()))
        }
        "npm" => {
            let (name, file) = rel.split_once("/-/")?;
            let base = name.rsplit('/').next().unwrap_or(name);
            let stem = file.strip_suffix(".tgz").unwrap_or(file);
            let version = stem.strip_prefix(&format!("{base}-")).unwrap_or(stem);
            Some((name.to_string(), version.to_string()))
        }
        "pypi" => {
            let file = rel.rsplit('/').next()?;
            if file.ends_with(".metadata") {
                return None;
            }
            if let Some(stem) = file.strip_suffix(".whl") {
                let mut parts = stem.split('-');
                let name = parts.next()?;
                let version = parts.next()?;
                return Some((normalize(name), version.to_string()));
            }
            let stem = [".tar.gz", ".tar.bz2", ".tar.xz", ".tgz", ".zip", ".egg"]
                .iter()
                .find_map(|ext| file.strip_suffix(ext))
                .unwrap_or(file);
            let (name, version) = stem.rsplit_once('-')?;
            Some((normalize(name), version.to_string()))
        }
        _ => None,
    }
}

/// Scan one repository's files (blocking; run on a blocking thread).
pub fn scan_repo(repo: &'static str, root: &Path, access: &HashMap<String, u64>) -> Vec<Asset> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            // PyPI `.metadata` files normally count toward their wheel. Ones
            // whose wheel was never downloaded (pip and uv often resolve from
            // metadata alone) are listed on their own, so they show up in
            // Browse and can be cleaned up.
            let classified = match rel.strip_suffix(".metadata") {
                Some(dist) if repo == "pypi" => {
                    if path.with_extension("").exists() {
                        continue;
                    }
                    classify(repo, dist)
                }
                _ => classify(repo, &rel),
            };
            let Some((package, version)) = classified else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            let cached_at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // A PyPI file's `.metadata` companion counts toward its size.
            let companion = std::fs::metadata(format!("{}.metadata", path.display()))
                .map(|m| m.len())
                .unwrap_or(0);
            out.push(Asset {
                repo,
                package,
                version,
                file_name: entry.file_name().to_string_lossy().to_string(),
                last_access: access.get(&format!("{repo}/{rel}")).copied(),
                path: rel,
                size: meta.len() + companion,
                cached_at,
            });
        }
    }
    out
}

/// A package with all of its cached versions, for listings.
#[derive(Clone, Debug, Serialize)]
pub struct Component {
    pub repo: &'static str,
    pub name: String,
    pub versions: usize,
    pub files: usize,
    pub size: u64,
    pub latest_cached: u64,
    pub last_access: Option<u64>,
}

pub fn group(assets: &[Asset]) -> Vec<Component> {
    let mut map: HashMap<(&str, &str), Component> = HashMap::new();
    let mut versions: HashMap<(&str, &str), std::collections::HashSet<&str>> = HashMap::new();
    for a in assets {
        let key = (a.repo, a.package.as_str());
        let c = map.entry(key).or_insert_with(|| Component {
            repo: a.repo,
            name: a.package.clone(),
            versions: 0,
            files: 0,
            size: 0,
            latest_cached: 0,
            last_access: None,
        });
        c.files += 1;
        c.size += a.size;
        c.latest_cached = c.latest_cached.max(a.cached_at);
        c.last_access = c.last_access.max(a.last_access);
        versions.entry(key).or_default().insert(a.version.as_str());
    }
    let mut out: Vec<Component> = map
        .into_iter()
        .map(|(key, mut c)| {
            c.versions = versions.get(&key).map_or(0, |v| v.len());
            c
        })
        .collect();
    out.sort_by(|a, b| a.repo.cmp(b.repo).then_with(|| a.name.cmp(&b.name)));
    out
}

/// The metadata cache key for a package, so it can be removed with it.
pub fn meta_key(repo: &str, name: &str) -> Option<String> {
    match repo {
        "cargo" => {
            let n = name.to_ascii_lowercase();
            Some(match n.len() {
                0 => return None,
                1 => format!("1/{n}"),
                2 => format!("2/{n}"),
                3 => format!("3/{}/{n}", &n[..1]),
                _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
            })
        }
        "npm" => Some(name.to_string()),
        "pypi" => Some(normalize(name)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(repo: &str, rel: &str) -> Option<(String, String)> {
        classify(repo, rel)
    }

    #[test]
    fn classifies_files() {
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(
            c("cargo", "serde/serde-1.0.229.crate"),
            pair("serde", "1.0.229")
        );
        assert_eq!(
            c("cargo", "syn/syn-3.0.0-rc.1.crate"),
            pair("syn", "3.0.0-rc.1")
        );
        assert_eq!(c("npm", "chalk/-/chalk-5.6.2.tgz"), pair("chalk", "5.6.2"));
        assert_eq!(
            c("npm", "@types/node/-/node-26.6.4.tgz"),
            pair("@types/node", "26.6.4")
        );
        assert_eq!(
            c(
                "pypi",
                "packages/ab/cd/ef/charset_normalizer-3.5.2-cp313-cp313-manylinux.whl"
            ),
            pair("charset-normalizer", "3.5.2")
        );
        assert_eq!(
            c("pypi", "packages/ab/cd/ef/requests-0.2.0.tar.gz"),
            pair("requests", "0.2.0")
        );
        assert_eq!(
            c(
                "pypi",
                "packages/ab/cd/ef/requests-2.0-py3-none-any.whl.metadata"
            ),
            None
        );
    }

    #[test]
    fn cargo_index_paths() {
        assert_eq!(meta_key("cargo", "a").as_deref(), Some("1/a"));
        assert_eq!(meta_key("cargo", "ab").as_deref(), Some("2/ab"));
        assert_eq!(meta_key("cargo", "syn").as_deref(), Some("3/s/syn"));
        assert_eq!(meta_key("cargo", "Serde").as_deref(), Some("se/rd/serde"));
    }
}
