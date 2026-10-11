//! Go modules straight from a git repository, the way the go command
//! builds them when it downloads from the repository itself (`cmd/go`'s
//! `codeRepo` and `golang.org/x/mod/zip`): which commit a version is, which
//! directory holds the module, which versions exist, and a module zip whose
//! `h1:` hash is the one `go` computes for the same commit.
//!
//! Everything is read from the repo's cached mirror (see `formats::git`).

use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::Path,
};

use axum::http::StatusCode;

use crate::{
    AppState,
    config::Config,
    error::{AppError, AppResult},
    formats::git::{self, Refresh, RepoId},
    gomod,
    goprivate::{Route, Source},
};

/// A module in a git repository.
#[derive(Clone, Debug)]
pub struct Repo {
    pub id: RepoId,
    pub module: String,
    /// The module's directory in the repository ("" for the root), not
    /// counting a major version directory.
    pub code_dir: String,
    /// The path's major version suffix: "/v2", or "".
    pub path_major: String,
}

/// A version of a module: its canonical version, commit and commit time.
#[derive(Clone, Debug)]
pub struct Info {
    pub version: String,
    pub commit: String,
    pub time: i64,
}

impl Info {
    /// The `.info` JSON.
    pub fn json(&self) -> String {
        serde_json::json!({"Version": self.version, "Time": rfc3339(self.time)}).to_string()
    }
}

fn missing(why: impl Into<String>) -> AppError {
    AppError::new(StatusCode::NOT_FOUND, why)
}

/// Where a private module lives: the repository, and the directory in it.
pub fn locate(cfg: &Config, module: &str, route: &Route) -> Result<Repo, String> {
    if module.starts_with("gopkg.in/") {
        return Err("gopkg.in paths can't be private modules".into());
    }
    let (prefix, major) = gomod::split_path_version(module).ok_or("invalid module path")?;
    let parts: Vec<&str> = prefix.split('/').collect();
    let (host, owner, repo, rest) = match &route.source {
        Source::Git => {
            if parts.len() < 3 {
                return Err(format!(
                    "{module} doesn't name a repository (host/owner/repo)"
                ));
            }
            (
                parts[0].to_string(),
                parts[1].to_string(),
                parts[2],
                &parts[3..],
            )
        }
        Source::Mapped { host, owner } => {
            if parts.len() <= route.covered {
                return Err(format!(
                    "{module} doesn't name a repository under its prefix"
                ));
            }
            (
                host.clone(),
                owner.clone(),
                parts[route.covered],
                &parts[route.covered + 1..],
            )
        }
        Source::Gitea { .. } => return Err("not a git source".into()),
    };
    let id = RepoId::new(cfg, &host, &owner, repo)?;
    Ok(Repo {
        id,
        module: module.to_string(),
        code_dir: rest.join("/"),
        path_major: major.to_string(),
    })
}

impl Repo {
    /// Version tags are prefixed with the module's directory.
    fn tag_prefix(&self) -> String {
        if self.code_dir.is_empty() {
            String::new()
        } else {
            format!("{}/", self.code_dir)
        }
    }

    /// Versions of v2 and later may be `+incompatible`: only for a module at
    /// the repository root without a major version suffix.
    fn may_be_incompatible(&self) -> bool {
        self.code_dir.is_empty() && self.path_major.is_empty()
    }
}

// ----------------------------------------------------------------- semver

fn strip_build(v: &str) -> &str {
    v.split('+').next().unwrap_or(v)
}

fn parsed(v: &str) -> Option<semver::Version> {
    semver::Version::parse(strip_build(v).strip_prefix('v')?).ok()
}

/// Semantic version order, ignoring build metadata.
fn cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (parsed(a), parsed(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

fn major_of(v: &str) -> u64 {
    parsed(v).map_or(0, |p| p.major)
}

fn is_prerelease(v: &str) -> bool {
    parsed(v).is_some_and(|p| !p.pre.is_empty())
}

/// A pseudo-version's parts: its base version ("" when there is none), its
/// timestamp, and its 12-character revision.
fn parse_pseudo(v: &str) -> Option<(String, String, String)> {
    let core = strip_build(v);
    let (head, rev) = core.rsplit_once('-')?;
    if rev.len() != 12 || !rev.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let (pre, ts) = head.split_at(head.len().checked_sub(14)?);
    if !ts.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let found = |base: String| Some((base, ts.to_string(), rev.to_string()));
    // vX.Y.Z-0.TS-REV: the commit follows the release vX.Y.(Z-1).
    if let Some(next) = pre.strip_suffix("-0.") {
        let p = parsed(next).filter(|p| p.pre.is_empty() && p.patch > 0)?;
        return found(format!("v{}.{}.{}", p.major, p.minor, p.patch - 1));
    }
    // vX.Y.Z-pre.0.TS-REV: the commit follows the pre-release vX.Y.Z-pre.
    if let Some(base) = pre.strip_suffix(".0.") {
        parsed(base).filter(|p| !p.pre.is_empty())?;
        return found(base.to_string());
    }
    // vX.0.0-TS-REV: no version tag before the commit.
    let zero = pre.strip_suffix('-')?;
    parsed(zero).filter(|p| p.minor == 0 && p.patch == 0 && p.pre.is_empty())?;
    found(String::new())
}

/// `module.PseudoVersion`.
fn pseudo_version(major: &str, older: &str, time: i64, commit: &str) -> String {
    let major = if major.is_empty() { "v0" } else { major };
    let segment = format!("{}-{}", timestamp(time), &commit[..12.min(commit.len())]);
    let build = older.find('+').map_or("", |i| &older[i..]);
    let older = strip_build(older);
    if older.is_empty() {
        return format!("{major}.0.0-{segment}");
    }
    if is_prerelease(older) {
        return format!("{older}.0.{segment}{build}");
    }
    let p = parsed(older).expect("a canonical base version");
    format!(
        "v{}.{}.{}-0.{segment}{build}",
        p.major,
        p.minor,
        p.patch + 1
    )
}

fn utc(t: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(t).unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
}

fn timestamp(t: i64) -> String {
    let d = utc(t);
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        d.year(),
        d.month() as u8,
        d.day(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

fn rfc3339(t: i64) -> String {
    let d = utc(t);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year(),
        d.month() as u8,
        d.day(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

// -------------------------------------------------------------------- git

async fn text(st: &AppState, id: &RepoId, args: &[&str]) -> AppResult<Option<String>> {
    Ok(git::out(st, id, args)
        .await?
        .map(|b| String::from_utf8_lossy(&b).trim().to_string()))
}

/// Make sure the mirror is cached and fresh enough.
pub async fn sync(st: &AppState, repo: &Repo) -> AppResult<()> {
    git::ensure(st, &repo.id, Refresh::IfStale, None).await
}

/// Every tag and the commit it points at.
async fn tags(st: &AppState, id: &RepoId) -> AppResult<Vec<(String, String)>> {
    let out = text(
        st,
        id,
        &[
            "for-each-ref",
            "--format=%(refname:strip=2)%09%(objectname)%09%(*objectname)",
            "refs/tags",
        ],
    )
    .await?
    .unwrap_or_default();
    Ok(out
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (tag, obj, peeled) = (f.next()?, f.next()?, f.next().unwrap_or(""));
            let commit = if peeled.is_empty() { obj } else { peeled };
            Some((tag.to_string(), commit.to_string()))
        })
        .collect())
}

async fn commit_time(st: &AppState, id: &RepoId, commit: &str) -> AppResult<i64> {
    text(st, id, &["show", "-s", "--format=%ct", commit])
        .await?
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| missing("unknown revision"))
}

async fn is_ancestor(st: &AppState, id: &RepoId, older: &str, newer: &str) -> AppResult<bool> {
    Ok(
        git::out(st, id, &["merge-base", "--is-ancestor", older, newer])
            .await?
            .is_some(),
    )
}

/// The go.mod's module path (`modfile.ModulePath`).
fn module_path(gomod: &[u8]) -> String {
    for line in String::from_utf8_lossy(gomod).lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("module") {
            if !rest.starts_with([' ', '\t', '"', '`']) {
                continue;
            }
            return rest.trim().trim_matches(['"', '`']).to_string();
        }
    }
    String::new()
}

/// The module's directory at a commit and its go.mod, if it has one
/// (`codeRepo.findDir`).
async fn find_dir(
    st: &AppState,
    repo: &Repo,
    commit: &str,
) -> AppResult<(String, Option<Vec<u8>>)> {
    let join = |a: &str, b: &str| {
        if a.is_empty() {
            b.to_string()
        } else {
            format!("{a}/{b}")
        }
    };
    let is_major = |mpath: &str| {
        gomod::split_path_version(mpath).is_some_and(|(_, major)| major == repo.path_major)
    };
    let file1 = join(&repo.code_dir, "go.mod");
    let gomod1 = git::read_blob(st, &repo.id, commit, &file1, gomod::MAX_GO_MOD).await?;
    let found1 = gomod1.as_deref().is_some_and(|g| is_major(&module_path(g)));
    let short = &commit[..12.min(commit.len())];

    let mut file2 = None;
    if !repo.path_major.is_empty() {
        let dir2 = join(&repo.code_dir, &repo.path_major[1..]);
        let f2 = join(&dir2, "go.mod");
        let gomod2 = git::read_blob(st, &repo.id, commit, &f2, gomod::MAX_GO_MOD).await?;
        let found2 = gomod2.as_deref().is_some_and(|g| is_major(&module_path(g)));
        if found1 && found2 {
            return Err(missing(format!(
                "{file1} and {f2} both have ...{} module paths at revision {short}",
                repo.path_major
            )));
        }
        if found2 {
            return Ok((dir2, gomod2));
        }
        if gomod2.is_some() {
            return Err(missing(format!(
                "{f2} has a module path without {} at revision {short}",
                repo.path_major
            )));
        }
        file2 = Some(f2);
    }
    if found1 {
        return Ok((repo.code_dir.clone(), gomod1));
    }
    if let Some(g) = &gomod1 {
        return Err(missing(format!(
            "{file1} declares module {:?}, not {}, at revision {short}",
            module_path(g),
            repo.module
        )));
    }
    if repo.code_dir.is_empty() && repo.path_major.is_empty() {
        // A repository root without a go.mod is a v0 or v1 module.
        return Ok((String::new(), None));
    }
    Err(missing(match file2 {
        Some(f2) => format!("missing {file1} and {f2} at revision {short}"),
        None => format!("missing {file1} at revision {short}"),
    }))
}

/// Whether the repository root has a go.mod file at a commit.
async fn root_has_go_mod(st: &AppState, repo: &Repo, commit: &str) -> AppResult<bool> {
    Ok(
        git::read_blob(st, &repo.id, commit, "go.mod", gomod::MAX_GO_MOD)
            .await?
            .is_some(),
    )
}

// --------------------------------------------------------------- versions

/// This module's version tags: `(version, commit)` for tags that fit its
/// major version, and those of v2 and later that could be `+incompatible`.
async fn version_tags(
    st: &AppState,
    repo: &Repo,
) -> AppResult<(Vec<(String, String)>, Vec<(String, String)>)> {
    let prefix = repo.tag_prefix();
    let mut fit = Vec::new();
    let mut incompatible = Vec::new();
    for (tag, commit) in tags(st, &repo.id).await? {
        let Some(v) = tag.strip_prefix(&prefix) else {
            continue;
        };
        // Canonical, without build metadata, and not a pseudo-version.
        if !gomod::is_canonical(v) || v.contains('+') || parse_pseudo(v).is_some() {
            continue;
        }
        if gomod::check_path_major(v, &repo.path_major).is_ok() {
            fit.push((v.to_string(), commit));
        } else if repo.may_be_incompatible() && major_of(v) >= 2 {
            incompatible.push((v.to_string(), commit));
        }
    }
    fit.sort_by(|a, b| cmp(&a.0, &b.0));
    incompatible.sort_by(|a, b| cmp(&a.0, &b.0));
    Ok((fit, incompatible))
}

/// The versions `@v/list` gives (`codeRepo.Versions`), oldest first.
pub async fn versions(st: &AppState, repo: &Repo) -> AppResult<Vec<String>> {
    let (fit, incompatible) = version_tags(st, repo).await?;
    let mut list: Vec<String> = fit.iter().map(|(v, _)| v.clone()).collect();
    if incompatible.is_empty() {
        return Ok(list);
    }
    // No +incompatible versions once the latest compatible one has a go.mod.
    if let Some((_, commit)) = fit.last()
        && root_has_go_mod(st, repo, commit).await?
    {
        return Ok(list);
    }
    // A major version whose latest tag has a go.mod is a module of its own.
    let mut skip: HashMap<u64, bool> = HashMap::new();
    for (v, commit) in incompatible.iter().rev() {
        let major = major_of(v);
        if let std::collections::hash_map::Entry::Vacant(e) = skip.entry(major) {
            e.insert(root_has_go_mod(st, repo, commit).await?);
        }
    }
    for (v, _) in &incompatible {
        if !skip[&major_of(v)] {
            list.push(format!("{v}+incompatible"));
        }
    }
    list.sort_by(|a, b| cmp(a, b));
    Ok(list)
}

/// The version of a commit that carries no version tag: a pseudo-version
/// based on the nearest version tag before it.
async fn pseudo_for(st: &AppState, repo: &Repo, commit: &str) -> AppResult<String> {
    let time = commit_time(st, &repo.id, commit).await?;
    let (fit, incompatible) = version_tags(st, repo).await?;
    let mut base = String::new();
    for (v, c) in fit.iter().rev() {
        if is_ancestor(st, &repo.id, c, commit).await? {
            base = v.clone();
            break;
        }
    }
    if base.is_empty() && !incompatible.is_empty() && !root_has_go_mod(st, repo, commit).await? {
        for (v, c) in incompatible.iter().rev() {
            if is_ancestor(st, &repo.id, c, commit).await? {
                base = format!("{v}+incompatible");
                break;
            }
        }
    }
    let major = if base.is_empty() {
        repo.path_major.trim_start_matches('/').to_string()
    } else {
        format!("v{}", major_of(&base))
    };
    Ok(pseudo_version(&major, &base, time, commit))
}

/// The commit a canonical version names, checked the way the go command
/// checks it (`codeRepo.Stat` and `convert`).
async fn stat_version(st: &AppState, repo: &Repo, version: &str) -> AppResult<Info> {
    let core = strip_build(version);
    if let Some((base, ts, rev)) = parse_pseudo(version) {
        let commit = git::resolve_ref(st, &repo.id, Some(&rev))
            .await
            .map_err(|_| missing(format!("unknown revision {rev}")))?;
        if !commit.starts_with(&rev) {
            return Err(missing(format!("unknown revision {rev}")));
        }
        let time = commit_time(st, &repo.id, &commit).await?;
        if timestamp(time) != ts {
            return Err(missing(format!(
                "invalid pseudo-version {version}: its timestamp doesn't match the commit's ({})",
                timestamp(time)
            )));
        }
        if !base.is_empty() {
            let tag = format!("{}{base}", repo.tag_prefix());
            let found = tags(st, &repo.id)
                .await?
                .into_iter()
                .find(|(t, _)| *t == tag);
            let ancestor = match &found {
                Some((_, c)) => is_ancestor(st, &repo.id, c, &commit).await?,
                None => false,
            };
            if !ancestor {
                return Err(missing(format!(
                    "invalid pseudo-version {version}: its base {base} is not a tag before that commit"
                )));
            }
        }
        check_major(st, repo, version, &commit).await?;
        return Ok(Info {
            version: version.to_string(),
            commit,
            time,
        });
    }
    let tag = format!("{}{core}", repo.tag_prefix());
    let commit = tags(st, &repo.id)
        .await?
        .into_iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, c)| c)
        .ok_or_else(|| missing(format!("{}@{version}: no such version", repo.module)))?;
    check_major(st, repo, version, &commit).await?;
    Ok(Info {
        version: version.to_string(),
        time: commit_time(st, &repo.id, &commit).await?,
        commit,
    })
}

/// A version must fit the module path's major version; `+incompatible` is
/// only for a root module without a go.mod.
async fn check_major(st: &AppState, repo: &Repo, version: &str, commit: &str) -> AppResult<()> {
    let incompatible = version.ends_with("+incompatible");
    if incompatible {
        if !repo.may_be_incompatible() || major_of(version) < 2 {
            return Err(missing(format!(
                "{version}: +incompatible doesn't apply to this module"
            )));
        }
        if root_has_go_mod(st, repo, commit).await? {
            return Err(missing(format!(
                "{version}: the module has a go.mod file, so its path needs a /v{} suffix",
                major_of(version)
            )));
        }
        return Ok(());
    }
    gomod::check_path_major(version, &repo.path_major).map_err(missing)
}

/// What a version, branch, tag or commit resolves to (`.info`).
pub async fn stat(st: &AppState, repo: &Repo, query: &str) -> AppResult<Info> {
    if gomod::is_canonical(query) {
        let info = stat_version(st, repo, query).await?;
        find_dir(st, repo, &info.commit).await?;
        return Ok(info);
    }
    let commit = git::resolve_ref(st, &repo.id, Some(query))
        .await
        .map_err(|_| missing(format!("{}: unknown revision {query}", repo.module)))?;
    stat_commit(st, repo, &commit).await
}

/// The version of a commit: its highest version tag, else a pseudo-version.
async fn stat_commit(st: &AppState, repo: &Repo, commit: &str) -> AppResult<Info> {
    find_dir(st, repo, commit).await?;
    let (fit, incompatible) = version_tags(st, repo).await?;
    let here = |list: &[(String, String)]| {
        list.iter()
            .rev()
            .find(|(_, c)| c == commit)
            .map(|(v, _)| v.clone())
    };
    let version = match here(&fit) {
        Some(v) => v,
        None => match here(&incompatible) {
            Some(v) if !root_has_go_mod(st, repo, commit).await? => format!("{v}+incompatible"),
            _ => pseudo_for(st, repo, commit).await?,
        },
    };
    Ok(Info {
        version,
        commit: commit.to_string(),
        time: commit_time(st, &repo.id, commit).await?,
    })
}

/// `@latest`: the highest release, else the highest pre-release, else the
/// default branch's head.
pub async fn latest(st: &AppState, repo: &Repo) -> AppResult<Info> {
    let list = versions(st, repo).await?;
    let pick = list
        .iter()
        .rev()
        .find(|v| !is_prerelease(v))
        .or(list.last());
    match pick {
        Some(v) => stat(st, repo, v).await,
        None => {
            let head = git::resolve_ref(st, &repo.id, None).await?;
            stat_commit(st, repo, &head).await
        }
    }
}

/// The go.mod of a version: the file, or `module <path>` when the module
/// has none.
pub async fn go_mod(st: &AppState, repo: &Repo, info: &Info) -> AppResult<Vec<u8>> {
    let (_, gomod) = find_dir(st, repo, &info.commit).await?;
    Ok(gomod.unwrap_or_else(|| format!("module {}\n", repo.module).into_bytes()))
}

// -------------------------------------------------------------------- zip

/// The Go language version a go.mod declares, as (major, minor).
fn go_directive(gomod: &[u8]) -> Option<(u64, u64)> {
    for line in String::from_utf8_lossy(gomod).lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if let Some(v) = line
            .strip_prefix("go ")
            .or_else(|| line.strip_prefix("go\t"))
        {
            let mut parts = v.trim().split('.');
            let major = parts.next()?.parse().ok()?;
            let minor: String = parts
                .next()
                .unwrap_or("0")
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            return Some((major, minor.parse().unwrap_or(0)));
        }
    }
    None
}

/// `zip.isVendoredPackage`: files of vendored packages are left out. The
/// rule changed with go 1.24, keyed on the module's own `go` directive so
/// older modules keep their hashes.
fn is_vendored(name: &str, go: Option<(u64, u64)>) -> bool {
    let new = go.is_some_and(|v| v >= (1, 24));
    if new && name == "vendor/modules.txt" {
        return true;
    }
    let i = if name.starts_with("vendor/") {
        "vendor/".len()
    } else if let Some(j) = name.find("/vendor/") {
        if new {
            j + "/vendor/".len()
        } else {
            // The pre-1.24 bug, kept: the offset is from the start.
            "/vendor/".len()
        }
    } else {
        return false;
    };
    name.get(i..).is_some_and(|rest| rest.contains('/'))
}

fn file_name_ok(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric() || "!#$%&()+,-.=@[]^_{}~ ".contains(c)
    } else {
        c.is_alphabetic()
    }
}

/// `module.CheckFilePath`.
fn check_file_path(p: &str) -> Result<(), String> {
    if p.is_empty() || p.contains("//") || p.ends_with('/') || p.starts_with('/') {
        return Err(format!("malformed file path {p:?}"));
    }
    for elem in p.split('/') {
        if elem.is_empty() || elem.chars().all(|c| c == '.') || elem.ends_with('.') {
            return Err(format!("malformed file path {p:?}"));
        }
        if let Some(c) = elem.chars().find(|c| !file_name_ok(*c)) {
            return Err(format!("malformed file path {p:?}: invalid char {c:?}"));
        }
        let short = elem.split('.').next().unwrap_or(elem);
        if [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ]
        .iter()
        .any(|b| b.eq_ignore_ascii_case(short))
        {
            return Err(format!(
                "malformed file path {p:?}: {short:?} is reserved on Windows"
            ));
        }
    }
    Ok(())
}

/// One file to put in a module zip.
struct Entry {
    name: String,
    data: Vec<u8>,
    regular: bool,
    symlink: bool,
}

/// `zip.checkFiles`: the files that go in the zip. Vendored packages,
/// nested modules, symbolic links and other irregular files are left out;
/// a bad name, a name collision or an oversized file fails the zip.
fn check_files(files: Vec<Entry>) -> Result<Vec<Entry>, String> {
    let mut have_go_mod: HashSet<String> = HashSet::new();
    let mut go = None;
    for f in &files {
        let (dir, base) = match f.name.rsplit_once('/') {
            Some((d, b)) => (format!("{d}/"), b),
            None => (String::new(), f.name.as_str()),
        };
        if base.eq_ignore_ascii_case("go.mod") && f.regular {
            if base == "go.mod" && dir.is_empty() {
                go = go_directive(&f.data);
            }
            have_go_mod.insert(dir);
        }
    }
    let in_submodule = |p: &str| {
        let mut p = p;
        while let Some((dir, _)) = p.rsplit_once('/') {
            if have_go_mod.contains(&format!("{dir}/")) {
                return true;
            }
            p = dir;
        }
        false
    };
    // Case-insensitive collisions, of files and of the folders above them.
    let mut seen: HashMap<String, (String, bool)> = HashMap::new();
    fn collide(
        seen: &mut HashMap<String, (String, bool)>,
        p: &str,
        is_dir: bool,
    ) -> Result<(), String> {
        let fold = p.to_lowercase();
        match seen.get(&fold) {
            Some((other, other_dir)) => {
                if other != p {
                    return Err(format!(
                        "case-insensitive file name collision: {other:?} and {p:?}"
                    ));
                }
                if is_dir != *other_dir {
                    return Err(format!("entry {p:?} is both a file and a directory"));
                }
                if !is_dir {
                    return Err(format!("multiple entries for file {p:?}"));
                }
            }
            None => {
                seen.insert(fold, (p.to_string(), is_dir));
            }
        }
        match p.rsplit_once('/') {
            Some((parent, _)) => collide(seen, parent, true),
            None => Ok(()),
        }
    }

    let mut left = gomod::MAX_ZIP as i64;
    let mut valid = Vec::new();
    for f in files {
        let p = f.name.as_str();
        if is_vendored(p, go) || in_submodule(p) || p == ".hg_archival.txt" {
            continue;
        }
        check_file_path(p)?;
        if p.eq_ignore_ascii_case("go.mod") && p != "go.mod" {
            return Err("go.mod files must have lowercase names".into());
        }
        collide(&mut seen, p, false)?;
        if f.symlink || !f.regular {
            continue;
        }
        let size = f.data.len() as i64;
        left -= size;
        if left < 0 {
            return Err(format!(
                "module source tree too large (max size is {} bytes)",
                gomod::MAX_ZIP
            ));
        }
        if (p == "go.mod" || p == "LICENSE") && size as u64 > gomod::MAX_GO_MOD {
            return Err(format!("{p} file too large"));
        }
        valid.push(f);
    }
    Ok(valid)
}

/// Write a module zip from files (blocking): `module@version/` + each path.
fn write_zip(dest: &Path, module: &str, version: &str, files: &[Entry]) -> Result<(), String> {
    let out = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(out);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    for f in files {
        zip.start_file(format!("{module}@{version}/{}", f.name), opts)
            .map_err(|e| e.to_string())?;
        zip.write_all(&f.data).map_err(|e| e.to_string())?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Read the module's files out of a `git archive` zip (blocking).
fn read_archive(archive: &Path, subdir: &str) -> Result<Vec<Entry>, String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let strip = if subdir.is_empty() {
        "prefix/".to_string()
    } else {
        format!("prefix/{subdir}/")
    };
    let mut files = Vec::new();
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut e = zip.by_index(i).map_err(|e| e.to_string())?;
        let full = e.name().map_err(|e| e.to_string())?.to_string();
        let Some(name) = full.strip_prefix(&strip) else {
            continue;
        };
        if name.is_empty() || name.ends_with('/') {
            continue;
        }
        let mode = e.unix_mode().unwrap_or(0o100644);
        let kind = mode & 0o170000;
        let (symlink, regular) = (kind == 0o120000, kind == 0o100000 || kind == 0);
        let mut data = Vec::new();
        if regular {
            total += e.size();
            if total > gomod::MAX_ZIP {
                return Err(format!(
                    "module source tree too large (max size is {} bytes)",
                    gomod::MAX_ZIP
                ));
            }
            e.read_to_end(&mut data).map_err(|e| e.to_string())?;
        }
        files.push(Entry {
            name: name.to_string(),
            data,
            regular,
            symlink,
        });
    }
    Ok(files)
}

/// Build the module zip for a version at `dest` (`codeRepo.Zip`).
pub async fn build_zip(st: &AppState, repo: &Repo, info: &Info, dest: &Path) -> AppResult<()> {
    let (dir, _) = find_dir(st, repo, &info.commit).await?;
    let archive = dest.with_extension("git-archive");
    let made = git::archive_zip(st, &repo.id, &info.commit, &dir, &archive).await?;
    let size = tokio::fs::metadata(&archive)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if !made || size > gomod::MAX_ZIP {
        let _ = tokio::fs::remove_file(&archive).await;
        return Err(AppError::bad_gateway(if made {
            format!(
                "{}@{} is too large to be a module",
                repo.module, info.version
            )
        } else {
            format!("git could not archive {}@{}", repo.module, info.version)
        }));
    }
    // A module below the root without its own LICENSE gets the root's.
    let license = if dir.is_empty() {
        None
    } else {
        git::read_blob(st, &repo.id, &info.commit, "LICENSE", gomod::MAX_GO_MOD).await?
    };
    let (module, version) = (repo.module.clone(), info.version.clone());
    let dest = dest.to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let files = read_archive(&archive, &dir);
        let _ = std::fs::remove_file(&archive);
        let mut files = files?;
        if let Some(data) = license
            && !files.iter().any(|f| f.name == "LICENSE")
        {
            files.push(Entry {
                name: "LICENSE".into(),
                data,
                regular: true,
                symlink: false,
            });
        }
        let valid = check_files(files)?;
        write_zip(&dest, &module, &version, &valid)
    })
    .await
    .map_err(|e| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    result.map_err(|e| {
        AppError::bad_gateway(format!(
            "{}@{} can't be made into a module zip: {e}",
            repo.module, info.version
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_versions() {
        let c = "daa7c04131f5aabbccddeeff00112233445566ff";
        // 2019-11-09T02:19:31Z
        let t = 1573265971;
        assert_eq!(timestamp(t), "20191109021931");
        assert_eq!(rfc3339(t), "2019-11-09T02:19:31Z");
        assert_eq!(
            pseudo_version("", "", t, c),
            "v0.0.0-20191109021931-daa7c04131f5"
        );
        assert_eq!(
            pseudo_version("v2", "", t, c),
            "v2.0.0-20191109021931-daa7c04131f5"
        );
        assert_eq!(
            pseudo_version("v1", "v1.2.3", t, c),
            "v1.2.4-0.20191109021931-daa7c04131f5"
        );
        assert_eq!(
            pseudo_version("v1", "v1.2.3-pre", t, c),
            "v1.2.3-pre.0.20191109021931-daa7c04131f5"
        );
        assert_eq!(
            pseudo_version("v2", "v2.0.0+incompatible", t, c),
            "v2.0.1-0.20191109021931-daa7c04131f5+incompatible"
        );
        let p = |v: &str| parse_pseudo(v).map(|(b, _, r)| (b, r));
        assert_eq!(
            p("v0.0.0-20191109021931-daa7c04131f5"),
            Some((String::new(), "daa7c04131f5".into()))
        );
        assert_eq!(
            p("v1.2.4-0.20191109021931-daa7c04131f5"),
            Some(("v1.2.3".into(), "daa7c04131f5".into()))
        );
        assert_eq!(
            p("v1.2.3-pre.0.20191109021931-daa7c04131f5"),
            Some(("v1.2.3-pre".into(), "daa7c04131f5".into()))
        );
        assert_eq!(
            p("v2.0.1-0.20191109021931-daa7c04131f5+incompatible"),
            Some(("v2.0.0".into(), "daa7c04131f5".into()))
        );
        for not in [
            "v1.2.3",
            "v1.2.3-rc.1",
            "v1.2.3-20191109021931-daa7c04131f5",
            "v1.0.0-0.2019-daa7c04131f5",
        ] {
            assert_eq!(parse_pseudo(not), None, "{not}");
        }
    }

    #[test]
    fn vendor_rule_by_go_version() {
        let old = Some((1, 23));
        let new = Some((1, 24));
        // Vendored packages go either way; vendor/modules.txt only from 1.24.
        for go in [None, old, new] {
            assert!(is_vendored("vendor/example.com/x/x.go", go));
            assert!(!is_vendored("vendorx/a/b.go", go));
            assert!(!is_vendored("vendor/README", go));
        }
        assert!(!is_vendored("vendor/modules.txt", old));
        assert!(is_vendored("vendor/modules.txt", new));
        // The pre-1.24 offset bug.
        assert!(is_vendored("pkg/vendor/foo/foo.go", old));
        assert!(is_vendored("pkg/vendor/foo/foo.go", new));
        assert!(is_vendored("pkg/vendor/vendor.go", old));
        assert!(!is_vendored("pkg/vendor/vendor.go", new));
    }

    #[test]
    fn go_mod_parsing() {
        assert_eq!(go_directive(b"module x\n\ngo 1.24.3\n"), Some((1, 24)));
        assert_eq!(go_directive(b"module x\ngo 1.21 // old\n"), Some((1, 21)));
        assert_eq!(go_directive(b"module x\n"), None);
        assert_eq!(
            module_path(b"// c\nmodule example.com/m/v2 // x\n"),
            "example.com/m/v2"
        );
        assert_eq!(module_path(b"module \"example.com/q\"\n"), "example.com/q");
        assert_eq!(module_path(b"modules x\n"), "");
    }

    #[test]
    fn file_rules() {
        let f = |name: &str| Entry {
            name: name.into(),
            data: b"x".to_vec(),
            regular: true,
            symlink: false,
        };
        let names = |v: Vec<Entry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        let ok = check_files(vec![
            f("go.mod"),
            f("a.go"),
            f("sub/go.mod"),
            f("sub/b.go"),
            f("vendor/x.example/y/y.go"),
            f("vendor/modules.txt"),
            Entry {
                symlink: true,
                regular: false,
                ..f("link")
            },
        ])
        .unwrap();
        assert_eq!(names(ok), vec!["go.mod", "a.go", "vendor/modules.txt"]);
        assert!(check_files(vec![f("a.go"), f("A.go")]).is_err());
        assert!(check_files(vec![f("dir/a.go"), f("DIR/b.go")]).is_err());
        assert!(check_files(vec![f("a?.go")]).is_err());
        assert!(check_files(vec![f("aux.go")]).is_err());
        assert!(check_files(vec![f("GO.MOD")]).is_err());
    }
}
