//! "New from repo": find a repo's manifests and lockfiles and turn them
//! into a template.
//!
//! Granary reads these files from the repo's cached mirror at one commit,
//! from the root and up to two levels down: Cargo.toml, Cargo.lock,
//! package.json, package-lock.json, requirements*.txt, pyproject.toml,
//! pom.xml and build.gradle(.kts). Cargo and npm workspace members are
//! included wherever they are. Dependencies that are part of the repo
//! itself (workspace members, path dependencies, Maven modules) are left
//! out, and git dependencies are listed so they can be cached through the
//! git cache. Everything left out is reported with the reason.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use super::{
    lines::{Eco, check_line},
    manifest::{self, cargo_line},
};

/// Largest manifest read.
pub const MAX_MANIFEST: u64 = 2 * 1024 * 1024;
/// Largest lockfile read.
pub const MAX_LOCKFILE: u64 = 16 * 1024 * 1024;
/// Most files read from one repo.
pub const MAX_FILES: usize = 400;

/// Directories never searched: dependencies, build output and virtualenvs.
const SKIP_DIRS: [&str; 7] = [
    "node_modules",
    "target",
    "vendor",
    ".venv",
    "venv",
    "dist",
    ".git",
];

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn is_lockfile(path: &str) -> bool {
    matches!(file_name(path), "Cargo.lock" | "package-lock.json")
}

fn is_manifest(path: &str) -> bool {
    let n = file_name(path);
    matches!(
        n,
        "Cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
    ) || (n.starts_with("requirements") && n.ends_with(".txt"))
}

fn skipped_dir(path: &str) -> bool {
    path.split('/').any(|seg| SKIP_DIRS.contains(&seg))
}

/// Manifests and lockfiles in the root and up to two levels down.
pub fn candidates(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths
        .iter()
        .filter(|p| p.matches('/').count() <= 2)
        .filter(|p| !skipped_dir(p))
        .filter(|p| is_manifest(p) || is_lockfile(p))
        .cloned()
        .collect();
    out.sort();
    out
}

/// Whether `path` matches a workspace glob (`*` within a segment, `**` for
/// any number of segments).
fn glob(pattern: &str, path: &str) -> bool {
    fn seg(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => seg(&p[1..], s) || (!s.is_empty() && seg(p, &s[1..])),
            (Some(a), Some(b)) if a == b => seg(&p[1..], &s[1..]),
            _ => false,
        }
    }
    fn segs(p: &[&str], s: &[&str]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(&"**"), _) => segs(&p[1..], s) || (!s.is_empty() && segs(p, &s[1..])),
            (Some(a), Some(b)) => seg(a.as_bytes(), b.as_bytes()) && segs(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    segs(&p, &s)
}

/// Workspace members' manifests (at any depth) that the candidates point
/// at but don't include yet.
pub fn workspace_members(files: &BTreeMap<String, String>, paths: &[String]) -> Vec<String> {
    let mut want: Vec<(String, String, &str)> = Vec::new(); // (dir, pattern, manifest name)
    for (path, text) in files {
        let dir = dir_of(path).to_string();
        match file_name(path) {
            "Cargo.toml" => {
                if let Ok(t) = text.parse::<toml::Table>()
                    && let Some(members) = t
                        .get("workspace")
                        .and_then(|w| w.get("members"))
                        .and_then(|m| m.as_array())
                {
                    for m in members.iter().filter_map(|m| m.as_str()) {
                        want.push((dir.clone(), m.to_string(), "Cargo.toml"));
                    }
                }
            }
            "package.json" => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                    let list = v["workspaces"]
                        .as_array()
                        .or_else(|| v["workspaces"]["packages"].as_array())
                        .cloned()
                        .unwrap_or_default();
                    for m in list.iter().filter_map(|m| m.as_str()) {
                        want.push((dir.clone(), m.to_string(), "package.json"));
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for p in paths {
        if files.contains_key(p) || skipped_dir(p) {
            continue;
        }
        let name = file_name(p);
        let member_dir = dir_of(p);
        // Patterns are relative to the directory of the manifest naming them.
        let hit = want.iter().any(|(dir, pattern, m)| {
            *m == name
                && member_dir
                    .strip_prefix(dir.as_str())
                    .filter(|r| dir.is_empty() || r.starts_with('/'))
                    .is_some_and(|r| glob(pattern, r.trim_start_matches('/')))
        });
        if hit {
            out.push(p.clone());
        }
    }
    out.sort();
    out
}

/// A manifest that was read.
#[derive(Clone, Debug, Serialize)]
pub struct Found {
    pub path: String,
    pub ecosystem: Option<Eco>,
    /// Packages it added (after removing repeats).
    pub packages: usize,
}

/// Something left out, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub path: String,
    pub item: String,
    pub reason: String,
}

/// A git dependency named in a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitDep {
    pub path: String,
    pub url: String,
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// Whether the host is in GRANARY_GIT_HOSTS.
    pub allowed: bool,
}

/// What was found in a repo.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Scan {
    pub manifests: Vec<Found>,
    pub lockfiles: Vec<String>,
    /// Dependency lines per ecosystem, with a comment naming each file.
    pub cargo: String,
    pub npm: String,
    pub pypi: String,
    pub maven: String,
    pub counts: BTreeMap<String, usize>,
    pub skipped: Vec<Skipped>,
    pub git_deps: Vec<GitDep>,
    pub description: String,
}

/// Parse a git URL from a manifest (`https://host/owner/repo(.git)`, with
/// `git+` prefixes, `#ref` or `@ref` suffixes and npm's `github:` form).
pub fn parse_git_url(raw: &str) -> Option<(String, String, String, String)> {
    let mut s = raw.trim();
    s = s.strip_prefix("git+").unwrap_or(s);
    let s = s.split('#').next().unwrap_or(s);
    let (host, rest) = if let Some(r) = s.strip_prefix("github:") {
        ("github.com".to_string(), r.to_string())
    } else if let Some(r) = s.strip_prefix("gitlab:") {
        ("gitlab.com".to_string(), r.to_string())
    } else {
        let rest = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .or_else(|| s.strip_prefix("git://"))?;
        let rest = rest
            .split_once('@')
            .map_or(rest, |(cred, r)| if cred.contains('/') { rest } else { r });
        let (h, r) = rest.split_once('/')?;
        (h.to_ascii_lowercase(), r.to_string())
    };
    // Drop a pip-style @ref after the repo path.
    let rest = rest
        .split('@')
        .next()
        .unwrap_or(&rest)
        .trim_end_matches('/');
    let mut parts = rest.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    let url = format!("https://{host}/{owner}/{repo}");
    Some((url, host, owner, repo))
}

/// Collects one ecosystem's lines, keeping the first line per package.
#[derive(Default)]
struct Section {
    text: Vec<String>,
    names: HashSet<String>,
    count: usize,
}

impl Section {
    /// Add a file's lines under a comment; returns how many were new.
    fn add(
        &mut self,
        eco: Eco,
        path: &str,
        lines: Vec<String>,
        skipped: &mut Vec<Skipped>,
    ) -> usize {
        let mut fresh = Vec::new();
        for line in lines {
            match check_line(eco, &line) {
                Ok(name) => {
                    if self.names.insert(name) {
                        fresh.push(line);
                    }
                }
                Err(reason) => skipped.push(Skipped {
                    path: path.into(),
                    item: line,
                    reason,
                }),
            }
        }
        let n = fresh.len();
        if n > 0 {
            if !self.text.is_empty() {
                self.text.push(String::new());
            }
            self.text.push(format!("# {path}"));
            self.text.extend(fresh);
            self.count += n;
        }
        n
    }
}

/// Turn the files read from a repo into a template draft.
pub fn scan(files: &BTreeMap<String, String>, allowed_hosts: &[String]) -> Scan {
    let mut out = Scan::default();
    let mut sections: HashMap<Eco, Section> = HashMap::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut git_deps: Vec<GitDep> = Vec::new();
    let mut git_dep =
        |path: &str, raw: &str, skipped: &mut Vec<Skipped>, item: &str| match parse_git_url(raw) {
            Some((url, host, owner, repo)) => {
                let allowed = allowed_hosts.contains(&host);
                skipped.push(Skipped {
                    path: path.into(),
                    item: item.into(),
                    reason: if allowed {
                        format!("git dependency; {url} is cached through the git cache")
                    } else {
                        format!("git dependency on {host}, which is not an allowed git host")
                    },
                });
                if !git_deps.iter().any(|d| d.url == url) {
                    git_deps.push(GitDep {
                        path: path.into(),
                        url,
                        host,
                        owner,
                        repo,
                        allowed,
                    });
                }
            }
            None => skipped.push(Skipped {
                path: path.into(),
                item: item.into(),
                reason: format!("git dependency {raw:?} is not a host/owner/repo URL"),
            }),
        };

    // Parse everything first: workspace settings and the repo's own package
    // names are needed before any dependency can be judged.
    let mut cargo: Vec<(&String, toml::Table)> = Vec::new();
    let mut npm: Vec<(&String, serde_json::Value)> = Vec::new();
    for (path, text) in files {
        if is_lockfile(path) {
            out.lockfiles.push(path.clone());
            continue;
        }
        let parse_error = |e: String| Skipped {
            path: path.clone(),
            item: file_name(path).into(),
            reason: e,
        };
        match file_name(path) {
            "Cargo.toml" => match text.parse::<toml::Table>() {
                Ok(t) => cargo.push((path, t)),
                Err(e) => skipped.push(parse_error(format!(
                    "not a valid Cargo.toml: {}",
                    e.message()
                ))),
            },
            "package.json" => match serde_json::from_str(text) {
                Ok(v) => npm.push((path, v)),
                Err(e) => skipped.push(parse_error(format!("not a valid package.json: {e}"))),
            },
            _ => {}
        }
    }
    let own_crates: HashSet<String> = cargo
        .iter()
        .filter_map(|(_, t)| t.get("package")?.get("name")?.as_str().map(String::from))
        .collect();
    let own_npm: HashSet<String> = npm
        .iter()
        .filter_map(|(_, v)| v["name"].as_str().map(String::from))
        .collect();
    let workspace_roots: Vec<(String, toml::Table)> = cargo
        .iter()
        .filter_map(|(p, t)| {
            let deps = t.get("workspace")?.as_table()?;
            Some((
                dir_of(p).to_string(),
                deps.get("dependencies")
                    .and_then(|d| d.as_table())
                    .cloned()
                    .unwrap_or_default(),
            ))
        })
        .collect();

    // Description: the root manifest's, if it has one.
    for (path, t) in &cargo {
        if dir_of(path).is_empty()
            && let Some(d) = t
                .get("package")
                .and_then(|p| p.get("description"))
                .and_then(|d| d.as_str())
        {
            out.description = d.to_string();
        }
    }

    // ---- Cargo
    for (path, t) in &cargo {
        // The nearest workspace root at or above this manifest.
        let dir = dir_of(path);
        let ws = workspace_roots
            .iter()
            .filter(|(root, _)| {
                root.is_empty() || dir == root || dir.starts_with(&format!("{root}/"))
            })
            .max_by_key(|(root, _)| root.len())
            .map(|(_, deps)| deps);
        let mut tables: Vec<&toml::Table> = Vec::new();
        fn sections_of<'a>(t: &'a toml::Table, out: &mut Vec<&'a toml::Table>) {
            for s in [
                "dependencies",
                "dev-dependencies",
                "dev_dependencies",
                "build-dependencies",
                "build_dependencies",
            ] {
                if let Some(d) = t.get(s).and_then(|v| v.as_table()) {
                    out.push(d);
                }
            }
        }
        sections_of(t, &mut tables);
        if let Some(targets) = t.get("target").and_then(|v| v.as_table()) {
            for tt in targets.values().filter_map(|v| v.as_table()) {
                sections_of(tt, &mut tables);
            }
        }
        let mut lines = Vec::new();
        for table in tables {
            for (name, value) in table {
                let real = value
                    .get("package")
                    .and_then(|p| p.as_str())
                    .unwrap_or(name);
                if own_crates.contains(real) {
                    continue; // another crate in this repo
                }
                let mut value = value.clone();
                if value.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
                    let Some(base) = ws.and_then(|deps| deps.get(name)) else {
                        skipped.push(Skipped {
                            path: (*path).clone(),
                            item: name.clone(),
                            reason: "inherits from a workspace that doesn't list it".into(),
                        });
                        continue;
                    };
                    // The workspace entry, plus the member's extra features.
                    let mut merged = match base {
                        toml::Value::String(v) => {
                            let mut t = toml::Table::new();
                            t.insert("version".into(), toml::Value::String(v.clone()));
                            t
                        }
                        toml::Value::Table(t) => t.clone(),
                        _ => toml::Table::new(),
                    };
                    if let Some(extra) = value.get("features").and_then(|f| f.as_array()) {
                        let mut feats = merged
                            .get("features")
                            .and_then(|f| f.as_array())
                            .cloned()
                            .unwrap_or_default();
                        for f in extra {
                            if !feats.contains(f) {
                                feats.push(f.clone());
                            }
                        }
                        merged.insert("features".into(), toml::Value::Array(feats));
                    }
                    value = toml::Value::Table(merged);
                }
                if let Some(url) = value.get("git").and_then(|g| g.as_str()) {
                    let url = url.to_string();
                    git_dep(path, &url, &mut skipped, name);
                    continue;
                }
                if value.get("path").is_some() && value.get("version").is_none() {
                    skipped.push(Skipped {
                        path: (*path).clone(),
                        item: name.clone(),
                        reason: "local path dependency".into(),
                    });
                    continue;
                }
                // A path dependency with a version: the registry version.
                if let toml::Value::Table(t) = &mut value {
                    t.remove("path");
                }
                match cargo_line(name, &value) {
                    Ok(l) => lines.push(l),
                    Err(reason) => skipped.push(Skipped {
                        path: (*path).clone(),
                        item: name.clone(),
                        reason,
                    }),
                }
            }
        }
        let n = sections
            .entry(Eco::Cargo)
            .or_default()
            .add(Eco::Cargo, path, lines, &mut skipped);
        out.manifests.push(Found {
            path: (*path).clone(),
            ecosystem: Some(Eco::Cargo),
            packages: n,
        });
    }

    // ---- npm
    for (path, v) in &npm {
        if dir_of(path).is_empty() && out.description.is_empty() {
            out.description = v["description"].as_str().unwrap_or("").to_string();
        }
        let mut lines = Vec::new();
        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            let Some(deps) = v[section].as_object() else {
                continue;
            };
            for (name, range) in deps {
                let Some(range) = range.as_str() else {
                    skipped.push(Skipped {
                        path: (*path).clone(),
                        item: name.clone(),
                        reason: "the version is not a string".into(),
                    });
                    continue;
                };
                if own_npm.contains(name) || range.starts_with("workspace:") {
                    continue; // another package in this repo
                }
                if range.starts_with("file:")
                    || range.starts_with("link:")
                    || range.starts_with("portal:")
                {
                    skipped.push(Skipped {
                        path: (*path).clone(),
                        item: name.clone(),
                        reason: "local path dependency".into(),
                    });
                    continue;
                }
                let is_git = range.starts_with("github:")
                    || range.starts_with("gitlab:")
                    || range.starts_with("git")
                    || range.starts_with("https://")
                    || range.starts_with("http://")
                    || (range.contains('/') && !range.starts_with("npm:") && !range.contains(' '));
                if is_git {
                    let raw = if !range.contains(':') {
                        format!("github:{range}")
                    } else {
                        range.to_string()
                    };
                    git_dep(path, &raw, &mut skipped, name);
                    continue;
                }
                lines.push(format!(
                    "{}: {}",
                    serde_json::to_string(name).unwrap(),
                    serde_json::to_string(range).unwrap()
                ));
            }
        }
        let n = sections
            .entry(Eco::Npm)
            .or_default()
            .add(Eco::Npm, path, lines, &mut skipped);
        out.manifests.push(Found {
            path: (*path).clone(),
            ecosystem: Some(Eco::Npm),
            packages: n,
        });
    }

    // ---- PyPI, Maven and Gradle
    let mut own_maven: HashSet<String> = HashSet::new();
    for (path, text) in files {
        if file_name(path) == "pom.xml"
            && let Ok(doc) = roxmltree::Document::parse(text)
        {
            let root = doc.root_element();
            let child = |n: roxmltree::Node, name: &str| {
                n.children()
                    .find(|c| c.has_tag_name(name))
                    .and_then(|c| c.text())
                    .map(|t| t.trim().to_string())
            };
            let group = child(root, "groupId").or_else(|| {
                root.children()
                    .find(|c| c.has_tag_name("parent"))
                    .and_then(|p| child(p, "groupId"))
            });
            if let (Some(g), Some(a)) = (group, child(root, "artifactId")) {
                own_maven.insert(format!("{g}:{a}"));
            }
        }
    }
    for (path, text) in files {
        let name = file_name(path);
        let (eco, result): (Eco, Result<manifest::Lines, String>) = if name
            .starts_with("requirements")
            && name.ends_with(".txt")
        {
            let (lines, skip) = manifest::pypi(text);
            let mut keep = Vec::new();
            for l in lines {
                match l
                    .split_once(" @ ")
                    .or_else(|| l.split_once('@').filter(|(_, u)| u.contains("://")))
                {
                    Some((pkg, url)) if url.contains("git") => {
                        git_dep(path, url.trim(), &mut skipped, pkg.trim())
                    }
                    _ => keep.push(l),
                }
            }
            (Eco::Pypi, Ok((keep, skip)))
        } else if name == "pyproject.toml" {
            (
                Eco::Pypi,
                pyproject(text, path, &mut |raw, item, skipped: &mut Vec<Skipped>| {
                    git_dep(path, raw, skipped, item)
                }),
            )
        } else if name == "pom.xml" {
            (
                Eco::Maven,
                manifest::maven(text).map(|(lines, skip)| {
                    let lines = lines
                        .into_iter()
                        .filter(|l| {
                            let ga: Vec<&str> = l.splitn(3, ':').collect();
                            !own_maven.contains(&format!("{}:{}", ga[0], ga.get(1).unwrap_or(&"")))
                        })
                        .collect();
                    (lines, skip)
                }),
            )
        } else if name == "build.gradle" || name == "build.gradle.kts" {
            (Eco::Maven, Ok(gradle(text)))
        } else {
            continue;
        };
        match result {
            Ok((lines, skip)) => {
                skipped.extend(skip.into_iter().map(|reason| Skipped {
                    path: path.clone(),
                    item: reason.split(':').next().unwrap_or("").trim().to_string(),
                    reason,
                }));
                let n = sections
                    .entry(eco)
                    .or_default()
                    .add(eco, path, lines, &mut skipped);
                out.manifests.push(Found {
                    path: path.clone(),
                    ecosystem: Some(eco),
                    packages: n,
                });
            }
            Err(reason) => skipped.push(Skipped {
                path: path.clone(),
                item: name.into(),
                reason,
            }),
        }
    }

    out.manifests.sort_by(|a, b| a.path.cmp(&b.path));
    for eco in Eco::ALL {
        let s = sections.remove(&eco).unwrap_or_default();
        out.counts.insert(eco.as_str().into(), s.count);
        let text = s.text.join("\n");
        match eco {
            Eco::Cargo => out.cargo = text,
            Eco::Npm => out.npm = text,
            Eco::Pypi => out.pypi = text,
            Eco::Maven => out.maven = text,
        }
    }
    let mut seen = HashSet::new();
    skipped.retain(|s| seen.insert((s.path.clone(), s.item.clone(), s.reason.clone())));
    out.skipped = skipped;
    out.git_deps = git_deps;
    out
}

/// Poetry's caret and tilde ranges, in the form pip understands.
fn poetry_spec(v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || v == "*" {
        return Some(String::new());
    }
    let bump = |ver: &str, caret: bool| -> Option<String> {
        let parts: Vec<u64> = ver
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        let idx = if caret {
            parts
                .iter()
                .position(|p| *p != 0)
                .unwrap_or(parts.len() - 1)
        } else if parts.len() > 1 {
            1
        } else {
            0
        };
        let mut upper: Vec<u64> = parts[..=idx].to_vec();
        upper[idx] += 1;
        Some(format!(
            ">={ver},<{}",
            upper
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(".")
        ))
    };
    if let Some(r) = v.strip_prefix('^') {
        return bump(r.trim(), true);
    }
    if let Some(r) = v.strip_prefix("~=") {
        return Some(format!("~={}", r.trim()));
    }
    if let Some(r) = v.strip_prefix('~') {
        return bump(r.trim(), false);
    }
    if v.starts_with(|c: char| c.is_ascii_digit()) {
        return Some(format!("=={v}"));
    }
    Some(v.to_string())
}

type GitSink<'a> = &'a mut dyn FnMut(&str, &str, &mut Vec<Skipped>);

/// PEP 621 `[project]` dependencies and Poetry's dependency tables.
fn pyproject(text: &str, path: &str, git: GitSink) -> Result<manifest::Lines, String> {
    let doc: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("not a valid pyproject.toml: {}", e.message()))?;
    let mut lines = Vec::new();
    let mut skip = Vec::new();
    let mut extra_skips: Vec<Skipped> = Vec::new();
    if let Some(project) = doc.get("project").and_then(|p| p.as_table()) {
        let mut reqs: Vec<&str> = project
            .get("dependencies")
            .and_then(|d| d.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if let Some(opt) = project
            .get("optional-dependencies")
            .and_then(|o| o.as_table())
        {
            for group in opt.values().filter_map(|g| g.as_array()) {
                reqs.extend(group.iter().filter_map(|v| v.as_str()));
            }
        }
        for r in reqs {
            match r
                .split_once(" @ ")
                .or_else(|| r.split_once('@').filter(|(_, u)| u.contains("://")))
            {
                Some((pkg, url)) => git(url.trim(), pkg.trim(), &mut extra_skips),
                None => lines.push(r.trim().to_string()),
            }
        }
    }
    let poetry = doc.get("tool").and_then(|t| t.get("poetry"));
    let mut tables: Vec<&toml::Table> = Vec::new();
    if let Some(p) = poetry {
        for key in ["dependencies", "dev-dependencies"] {
            if let Some(t) = p.get(key).and_then(|t| t.as_table()) {
                tables.push(t);
            }
        }
        if let Some(groups) = p.get("group").and_then(|g| g.as_table()) {
            for g in groups.values() {
                if let Some(t) = g.get("dependencies").and_then(|t| t.as_table()) {
                    tables.push(t);
                }
            }
        }
    }
    for t in tables {
        for (name, value) in t {
            if name.eq_ignore_ascii_case("python") {
                continue;
            }
            let (version, extras) = match value {
                toml::Value::String(v) => (v.clone(), Vec::new()),
                toml::Value::Table(t) => {
                    if let Some(g) = t.get("git").and_then(|g| g.as_str()) {
                        git(g, name, &mut extra_skips);
                        continue;
                    }
                    if t.contains_key("path") || t.contains_key("url") {
                        skip.push(format!("{name}: local path or URL dependency"));
                        continue;
                    }
                    let extras: Vec<String> = t
                        .get("extras")
                        .and_then(|e| e.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| e.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        t.get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("*")
                            .to_string(),
                        extras,
                    )
                }
                _ => {
                    skip.push(format!("{name}: a dependency form that isn't read"));
                    continue;
                }
            };
            let Some(spec) = poetry_spec(&version) else {
                skip.push(format!(
                    "{name}: version {version:?} isn't understood; add it by hand"
                ));
                continue;
            };
            let extras = if extras.is_empty() {
                String::new()
            } else {
                format!("[{}]", extras.join(","))
            };
            lines.push(format!("{name}{extras}{spec}"));
        }
    }
    // Git dependencies were reported through the sink; keep their reasons.
    skip.extend(
        extra_skips
            .into_iter()
            .map(|s| format!("{}: {}", s.item, s.reason)),
    );
    let _ = path;
    Ok((lines, skip))
}

/// Gradle dependency declarations with string coordinates. Version catalogs,
/// variables, map notation and project dependencies are left out.
fn gradle(text: &str) -> manifest::Lines {
    const CONFIGS: [&str; 14] = [
        "implementation",
        "api",
        "compileOnly",
        "runtimeOnly",
        "testImplementation",
        "testRuntimeOnly",
        "testCompileOnly",
        "annotationProcessor",
        "kapt",
        "ksp",
        "compile",
        "testCompile",
        "runtime",
        "classpath",
    ];
    let mut lines = Vec::new();
    let mut skip = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let Some(config) = CONFIGS.iter().find(|c| {
            line.strip_prefix(**c).is_some_and(|rest| {
                rest.starts_with('(')
                    || rest.starts_with(' ')
                    || rest.starts_with('"')
                    || rest.starts_with('\'')
            })
        }) else {
            continue;
        };
        let rest = &line[config.len()..];
        if rest.contains("project(") {
            continue; // another module in this build
        }
        if rest.contains("libs.") {
            skip.push(format!(
                "{}: uses the version catalog, which isn't read",
                rest.trim_matches(['(', ')', ' '])
            ));
            continue;
        }
        // Map notation: group: '...', name: '...' (Groovy) or group = "..." (Kotlin).
        if rest.contains("name:") || rest.contains("name =") {
            skip.push(format!(
                "{}: map notation isn't read; add it by hand",
                rest.trim()
            ));
            continue;
        }
        // The first quoted string holds the coordinates.
        let quoted = rest.find(['"', '\'']).and_then(|start| {
            let q = rest.as_bytes()[start] as char;
            rest[start + 1..]
                .find(q)
                .map(|end| &rest[start + 1..start + 1 + end])
        });
        let Some(coords) = quoted else { continue };
        if coords.contains('$') {
            skip.push(format!(
                "{coords}: the version is a variable; add it by hand"
            ));
            continue;
        }
        let parts: Vec<&str> = coords.split(':').collect();
        match parts.len() {
            2 | 3 => lines.push(coords.to_string()),
            4 => lines.push(parts[..3].join(":")),
            _ => skip.push(format!("{coords}: not group:artifact:version")),
        }
    }
    (lines, skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect()
    }

    fn hosts() -> Vec<String> {
        vec!["github.com".into()]
    }

    #[test]
    fn finds_candidates_and_members() {
        let paths: Vec<String> = [
            "Cargo.toml",
            "Cargo.lock",
            "src/main.rs",
            "crates/core/Cargo.toml",
            "crates/deep/nested/Cargo.toml",
            "web/package.json",
            "web/packages/ui/package.json",
            "web/node_modules/x/package.json",
            "py/requirements-dev.txt",
            "a/b/c/requirements.txt",
            "target/debug/Cargo.toml",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            candidates(&paths),
            vec![
                "Cargo.lock",
                "Cargo.toml",
                "crates/core/Cargo.toml",
                "py/requirements-dev.txt",
                "web/package.json"
            ]
        );
        let read = files(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\", \"crates/deep/**\"]",
            ),
            ("web/package.json", r#"{"workspaces": ["packages/*"]}"#),
        ]);
        assert_eq!(
            workspace_members(&read, &paths),
            vec![
                "crates/core/Cargo.toml",
                "crates/deep/nested/Cargo.toml",
                "web/packages/ui/package.json"
            ]
        );
    }

    #[test]
    fn globs() {
        assert!(glob("crates/*", "crates/core"));
        assert!(!glob("crates/*", "crates/a/b"));
        assert!(glob("crates/**", "crates/a/b"));
        assert!(glob("./packages/*/", "packages/x"));
        assert!(glob("lib-*", "lib-core"));
        assert!(!glob("lib-*", "core"));
    }

    #[test]
    fn git_urls() {
        let p = |s: &str| parse_git_url(s).map(|(u, ..)| u);
        assert_eq!(
            p("https://github.com/dtolnay/itoa").as_deref(),
            Some("https://github.com/dtolnay/itoa")
        );
        assert_eq!(
            p("git+https://github.com/a/b.git#v1").as_deref(),
            Some("https://github.com/a/b")
        );
        assert_eq!(
            p("github:a/b#main").as_deref(),
            Some("https://github.com/a/b")
        );
        assert_eq!(
            p("git+https://github.com/a/b@v2").as_deref(),
            Some("https://github.com/a/b")
        );
        assert_eq!(p("https://gitlab.com/group/sub/repo"), None);
    }

    #[test]
    fn cargo_and_npm_workspaces() {
        let f = files(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\n[workspace.dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\ntokio = \"1\"\n",
            ),
            ("Cargo.lock", "version = 4"),
            (
                "crates/core/Cargo.toml",
                "[package]\nname = \"demo-core\"\ndescription = \"x\"\n[dependencies]\nserde = { workspace = true, features = [\"rc\"] }\nanyhow = \"1\"\nitoa = { git = \"https://github.com/dtolnay/itoa\" }\nmissing = { workspace = true }\n",
            ),
            (
                "crates/cli/Cargo.toml",
                "[package]\nname = \"demo-cli\"\n[dependencies]\ndemo-core = { path = \"../core\" }\nclap = { version = \"4\", features = [\"derive\"] }\nanyhow = \"1.0.80\"\nlocal = { path = \"../../elsewhere\" }\n",
            ),
            (
                "web/package.json",
                r#"{"name":"web","private":true,"workspaces":["packages/*"],"devDependencies":{"typescript":"^5"}}"#,
            ),
            ("web/package-lock.json", "{}"),
            (
                "web/packages/ui/package.json",
                r#"{"name":"@demo/ui","dependencies":{"react":"^19","@demo/utils":"workspace:*","left":"github:acme/left-pad","far":"git+https://gitlab.example/a/b.git"}}"#,
            ),
            (
                "web/packages/utils/package.json",
                r#"{"name":"@demo/utils","dependencies":{"lodash":"^4","@demo/ui":"1.0.0","here":"file:../here"}}"#,
            ),
            (
                "requirements.txt",
                "fastapi>=0.115\npylib @ git+https://github.com/acme/pylib\n",
            ),
        ]);
        let s = scan(&f, &hosts());
        assert_eq!(s.lockfiles, vec!["Cargo.lock", "web/package-lock.json"]);
        assert_eq!(s.counts["cargo"], 3, "{}", s.cargo);
        assert!(
            s.cargo
                .contains(r#"serde = { version = "1", features = ["derive", "rc"] }"#),
            "{}",
            s.cargo
        );
        // Files are read in path order and the first mention of a crate wins.
        assert!(s.cargo.contains("anyhow = \"1.0.80\""), "{}", s.cargo);
        assert!(s.cargo.contains("# crates/cli/Cargo.toml"));
        assert!(!s.cargo.contains("demo-core"));
        assert_eq!(s.counts["npm"], 3, "{}", s.npm);
        assert!(!s.npm.contains("@demo/"));
        assert_eq!(s.counts["pypi"], 1);
        assert_eq!(
            s.description, "",
            "only the root manifest's description is used"
        );

        let urls: Vec<&str> = s.git_deps.iter().map(|d| d.url.as_str()).collect();
        // npm dependencies come in name order.
        assert_eq!(
            urls,
            vec![
                "https://github.com/dtolnay/itoa",
                "https://gitlab.example/a/b",
                "https://github.com/acme/left-pad",
                "https://github.com/acme/pylib"
            ]
        );
        assert!(!s.git_deps[1].allowed);
        assert!(s.git_deps[0].allowed);
        let reasons: Vec<(&str, &str)> = s
            .skipped
            .iter()
            .map(|k| (k.item.as_str(), k.reason.as_str()))
            .collect();
        assert!(
            reasons
                .iter()
                .any(|(i, r)| *i == "missing" && r.contains("workspace")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|(i, r)| *i == "local" && r.contains("local path"))
        );
        assert!(
            reasons
                .iter()
                .any(|(i, r)| *i == "here" && r.contains("local path"))
        );
        assert!(
            reasons
                .iter()
                .any(|(i, r)| *i == "far" && r.contains("not an allowed git host"))
        );
    }

    #[test]
    fn pyproject_pep621_and_poetry() {
        let s = scan(
            &files(&[(
                "pyproject.toml",
                r#"
[project]
name = "x"
dependencies = ["httpx>=0.27", "lib @ git+https://github.com/acme/lib"]
[project.optional-dependencies]
test = ["pytest"]
[tool.poetry.dependencies]
python = "^3.11"
requests = "^2.31.0"
django = { version = "~4.2", extras = ["bcrypt"] }
local = { path = "../local" }
zero = "^0.3.1"
pinned = "1.2.3"
any = "*"
"#,
            )]),
            &hosts(),
        );
        for want in [
            "httpx>=0.27",
            "pytest",
            "requests>=2.31.0,<3",
            "django[bcrypt]>=4.2,<4.3",
            "zero>=0.3.1,<0.4",
            "pinned==1.2.3",
            "any",
        ] {
            assert!(s.pypi.lines().any(|l| l == want), "{want} in {}", s.pypi);
        }
        assert_eq!(s.git_deps[0].url, "https://github.com/acme/lib");
        assert!(s.skipped.iter().any(|k| k.reason.contains("local path")));
    }

    #[test]
    fn gradle_coordinates() {
        let (lines, skip) = gradle(
            r#"
dependencies {
    implementation("org.springframework.boot:spring-boot-starter-web:3.4.0")
    implementation 'com.google.guava:guava:33.4.0-jre'
    testImplementation(platform("org.junit:junit-bom:5.11.0"))
    implementation(project(":core"))
    implementation(libs.jackson.databind)
    api "org.slf4j:slf4j-api:$slf4jVersion"
    compileOnly group: 'org.projectlombok', name: 'lombok', version: '1.18.34'
    runtimeOnly("org.postgresql:postgresql")
}
"#,
        );
        assert_eq!(
            lines,
            vec![
                "org.springframework.boot:spring-boot-starter-web:3.4.0",
                "com.google.guava:guava:33.4.0-jre",
                "org.junit:junit-bom:5.11.0",
                "org.postgresql:postgresql",
            ]
        );
        assert_eq!(skip.len(), 3, "{skip:?}");
        assert!(skip[2].contains("map notation"));
    }

    #[test]
    fn maven_modules_are_internal() {
        let s = scan(
            &files(&[
                (
                    "pom.xml",
                    "<project><groupId>com.acme</groupId><artifactId>parent</artifactId><dependencies><dependency><groupId>com.acme</groupId><artifactId>core</artifactId><version>1</version></dependency><dependency><groupId>junit</groupId><artifactId>junit</artifactId><version>4.13.2</version></dependency></dependencies></project>",
                ),
                (
                    "core/pom.xml",
                    "<project><parent><groupId>com.acme</groupId></parent><artifactId>core</artifactId></project>",
                ),
            ]),
            &hosts(),
        );
        assert_eq!(s.counts["maven"], 1, "{}", s.maven);
        assert!(s.maven.contains("junit:junit:4.13.2"));
    }
}
