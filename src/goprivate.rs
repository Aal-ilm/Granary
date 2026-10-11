//! Private Go modules: which module paths are private, where each comes
//! from, and the hashes Granary has pinned for their versions.
//!
//! **Prefixes.** A list of module path patterns in the syntax of `GONOSUMDB`
//! (Go's `path.Match` globs, matched against a path's leading elements), so
//! the same list can be pasted there. Each names its source:
//!
//! | Source | Modules come from |
//! |---|---|
//! | `git` (default) | the repository `host/owner/repo` the path starts with, through the git cache |
//! | `git:<host>/<owner>` | that owner's repositories, for a vanity prefix |
//! | `gitea:<owner>` | Gitea's Go registry for that owner |
//!
//! **The rule.** A path matching a prefix is only ever served from its
//! source: never from the public proxy, and never looked up in the checksum
//! database. A path matching none never reaches a private source.
//!
//! **Pins.** There is no checksum database for private modules, so Granary
//! keeps its own record: the `h1:` hash of a version's zip and go.mod the
//! first time it sees them. A later fetch or rebuild that gives a different
//! hash (a moved tag, or Gitea serving different bytes) is refused and
//! written to the audit log; the version already cached keeps being served.

use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::store::now_secs;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS go_pins (
    module TEXT NOT NULL,
    version TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('zip', 'mod')),
    h1 TEXT NOT NULL,
    source TEXT NOT NULL,
    pinned_at INTEGER NOT NULL,
    PRIMARY KEY (module, version, kind)
)";

const SETTING: &str = "go_private";
pub const ENV: &str = "GRANARY_GO_PRIVATE";

/// Where a private prefix's modules come from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Source {
    /// The repository the module path itself names (`host/owner/repo`).
    Git,
    /// Repositories of `owner` on `host`, for a vanity prefix.
    Mapped { host: String, owner: String },
    /// Gitea's Go registry for `owner`.
    Gitea { owner: String },
}

impl Source {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let name = |v: &str| {
            !v.is_empty()
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        };
        if s.is_empty() || s == "git" {
            return Ok(Source::Git);
        }
        if let Some(rest) = s.strip_prefix("git:") {
            let (host, owner) = rest
                .split_once('/')
                .ok_or("a git source looks like git:host/owner")?;
            if !name(host) || !name(owner) || owner.contains(':') {
                return Err(format!("{rest:?} is not host/owner"));
            }
            return Ok(Source::Mapped {
                host: host.to_ascii_lowercase(),
                owner: owner.to_string(),
            });
        }
        if let Some(owner) = s.strip_prefix("gitea:") {
            if !name(owner) || owner.contains(':') {
                return Err(format!("{owner:?} is not a Gitea user or org name"));
            }
            return Ok(Source::Gitea {
                owner: owner.to_string(),
            });
        }
        Err(format!(
            "{s:?} is not a source (git, git:host/owner or gitea:owner)"
        ))
    }

    pub fn label(&self) -> String {
        match self {
            Source::Git => "git".into(),
            Source::Mapped { host, owner } => format!("git:{host}/{owner}"),
            Source::Gitea { owner } => format!("gitea:{owner}"),
        }
    }

    /// The folder this source's modules are cached under, so a prefix whose
    /// source changes never serves another source's files.
    pub fn cache_key(&self) -> String {
        match self {
            Source::Git => "git".into(),
            Source::Mapped { host, owner } => format!("git-{}-{owner}", host.replace(':', "-")),
            Source::Gitea { owner } => format!("gitea-{owner}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub pattern: String,
    pub source: Source,
}

/// Check and tidy a pattern: path elements of module-path characters and
/// glob syntax, without a trailing slash.
pub fn check_pattern(p: &str) -> Result<String, String> {
    let p = p.trim().trim_end_matches('/');
    let ok = !p.is_empty()
        && p.split('/').all(|e| {
            !e.is_empty()
                && e.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(
                            b,
                            b'-' | b'.' | b'_' | b'~' | b'*' | b'?' | b'[' | b']' | b'^' | b'\\'
                        )
                })
        })
        && glob(p, p.replace(['*', '?', '[', ']', '^', '\\'], "a").as_str()).is_some();
    if ok {
        Ok(p.to_string())
    } else {
        Err(format!(
            "{p:?} is not a module path prefix (like corp.example.com or github.com/acme/*)"
        ))
    }
}

/// Parse `pattern[=source],...`.
pub fn parse_rules(raw: &str) -> Result<Vec<Rule>, String> {
    let mut rules: Vec<Rule> = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (pattern, source) = match item.split_once('=') {
            Some((p, s)) => (p, Source::parse(s)?),
            None => (item, Source::Git),
        };
        let pattern = check_pattern(pattern)?;
        if rules.iter().any(|r| r.pattern == pattern) {
            return Err(format!("{pattern} is listed twice"));
        }
        rules.push(Rule { pattern, source });
    }
    Ok(rules)
}

/// Go's `path.Match`: `Some(matched)`, or `None` for a malformed pattern.
/// `*` and `?` never match a slash.
pub fn glob(pattern: &str, name: &str) -> Option<bool> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn class(p: &[char], mut i: usize, c: char) -> Option<(bool, usize)> {
        // p[i] is just past '['.
        let negated = p.get(i) == Some(&'^');
        if negated {
            i += 1;
        }
        let mut matched = false;
        let mut first = true;
        loop {
            let mut lo = *p.get(i)?;
            if lo == ']' && !first {
                return Some((matched != negated, i + 1));
            }
            first = false;
            if lo == '\\' {
                i += 1;
                lo = *p.get(i)?;
            }
            i += 1;
            let mut hi = lo;
            if p.get(i) == Some(&'-') && p.get(i + 1).is_some_and(|c| *c != ']') {
                i += 1;
                hi = *p.get(i)?;
                if hi == '\\' {
                    i += 1;
                    hi = *p.get(i)?;
                }
                i += 1;
            }
            if lo <= c && c <= hi {
                matched = true;
            }
        }
    }
    fn go(p: &[char], n: &[char]) -> Option<bool> {
        let Some(&c) = p.first() else {
            return Some(n.is_empty());
        };
        match c {
            '*' => {
                for k in 0..=n.len() {
                    if k > 0 && n[k - 1] == '/' {
                        break;
                    }
                    if go(&p[1..], &n[k..])? {
                        return Some(true);
                    }
                }
                Some(false)
            }
            '?' => match n.first() {
                Some(x) if *x != '/' => go(&p[1..], &n[1..]),
                _ => {
                    go(&p[1..], &[])?;
                    Some(false)
                }
            },
            '[' => match n.first() {
                Some(x) => {
                    let (hit, next) = class(p, 1, *x)?;
                    if hit && *x != '/' {
                        go(&p[next..], &n[1..])
                    } else {
                        go(&p[next..], &[])?;
                        Some(false)
                    }
                }
                None => {
                    class(p, 1, 'a')?;
                    Some(false)
                }
            },
            '\\' => {
                let lit = *p.get(1)?;
                match n.first() {
                    Some(x) if *x == lit => go(&p[2..], &n[1..]),
                    _ => Some(false),
                }
            }
            _ => match n.first() {
                Some(x) if *x == c => go(&p[1..], &n[1..]),
                _ => Some(false),
            },
        }
    }
    go(&p, &n)
}

/// `module.MatchPrefixPatterns` for one pattern: whether it matches the
/// leading path elements of `target`. Returns how many elements it covers.
pub fn matches_prefix(pattern: &str, target: &str) -> Option<usize> {
    let elems = pattern.split('/').count();
    let parts: Vec<&str> = target.split('/').collect();
    if parts.len() < elems {
        return None;
    }
    let prefix = parts[..elems].join("/");
    (glob(pattern, &prefix) == Some(true)).then_some(elems)
}

/// A private module's source, with what's left of its path after the
/// prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub source: Source,
    /// How many leading path elements the prefix covered.
    pub covered: usize,
}

pub struct GoPrivate {
    rules: RwLock<Vec<Rule>>,
    /// Set by the environment, so the System page can't change it.
    locked: bool,
}

impl GoPrivate {
    pub async fn load(db: &SqlitePool, env: Option<Vec<Rule>>) -> sqlx::Result<Self> {
        let locked = env.is_some();
        let rules = match env {
            Some(r) => r,
            None => crate::auth::setting(db, SETTING)
                .await?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
        };
        Ok(Self {
            rules: RwLock::new(rules),
            locked,
        })
    }

    /// Where a module path comes from, if it is private.
    pub fn route(&self, module: &str) -> Option<Route> {
        self.rules.read().unwrap().iter().find_map(|r| {
            matches_prefix(&r.pattern, module).map(|covered| Route {
                source: r.source.clone(),
                covered,
            })
        })
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.rules.read().unwrap().clone()
    }

    pub fn patterns(&self) -> Vec<String> {
        self.rules().into_iter().map(|r| r.pattern).collect()
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Replace the list from the System page.
    pub async fn set(&self, db: &SqlitePool, rules: Vec<Rule>) -> Result<(), String> {
        if self.locked {
            return Err(format!(
                "{ENV} is set, so this list can only be changed in the environment"
            ));
        }
        let mut tidy: Vec<Rule> = Vec::new();
        for r in rules {
            let pattern = check_pattern(&r.pattern)?;
            if tidy.iter().any(|x| x.pattern == pattern) {
                return Err(format!("{pattern} is listed twice"));
            }
            let source = Source::parse(&r.source.label())?;
            tidy.push(Rule { pattern, source });
        }
        crate::auth::set_setting(db, SETTING, &serde_json::to_string(&tidy).unwrap())
            .await
            .map_err(|e| format!("could not save the setting: {e}"))?;
        *self.rules.write().unwrap() = tidy;
        Ok(())
    }
}

// --------------------------------------------------------------------- pins

/// What checking a hash against its pin found.
#[derive(Debug, PartialEq, Eq)]
pub enum Pin {
    /// First sight: now pinned.
    New,
    /// The same as the pinned hash.
    Same,
    /// Different from the pinned hash, which is given.
    Changed(String),
}

/// The pinned hash of a version's zip (`kind` "zip") or go.mod ("mod").
pub async fn pinned(
    db: &SqlitePool,
    module: &str,
    version: &str,
    kind: &str,
) -> sqlx::Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT h1 FROM go_pins WHERE module = ? AND version = ? AND kind = ?")
            .bind(module)
            .bind(version)
            .bind(kind)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(h,)| h))
}

/// Pin a hash on first sight, or compare it with the pin. Two requests
/// racing to pin the same version agree or one of them sees `Changed`.
pub async fn pin(
    db: &SqlitePool,
    module: &str,
    version: &str,
    kind: &str,
    h1: &str,
    source: &str,
) -> sqlx::Result<Pin> {
    let inserted = sqlx::query(
        "INSERT INTO go_pins (module, version, kind, h1, source, pinned_at) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (module, version, kind) DO NOTHING",
    )
    .bind(module)
    .bind(version)
    .bind(kind)
    .bind(h1)
    .bind(source)
    .bind(now_secs() as i64)
    .execute(db)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Pin::New);
    }
    Ok(match pinned(db, module, version, kind).await? {
        Some(old) if old == h1 => Pin::Same,
        Some(old) => Pin::Changed(old),
        None => Pin::New,
    })
}

/// Forget a version's pins (`granary go-unpin`), for a version that was
/// deliberately republished. Returns how many were removed.
pub async fn unpin(db: &SqlitePool, module: &str, version: &str) -> sqlx::Result<u64> {
    Ok(
        sqlx::query("DELETE FROM go_pins WHERE module = ? AND version = ?")
            .bind(module)
            .bind(version)
            .execute(db)
            .await?
            .rows_affected(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_like_path_match() {
        let m = |p: &str, n: &str| glob(p, n);
        assert_eq!(m("abc", "abc"), Some(true));
        assert_eq!(m("*", "abc"), Some(true));
        assert_eq!(m("*c", "abc"), Some(true));
        assert_eq!(m("a*", "a"), Some(true));
        assert_eq!(m("a*", "ab/c"), Some(false));
        assert_eq!(m("a*/b", "abc/b"), Some(true));
        assert_eq!(m("a*/b", "a/c/b"), Some(false));
        assert_eq!(m("a?c", "abc"), Some(true));
        assert_eq!(m("a?c", "a/c"), Some(false));
        assert_eq!(m("a[b-d]c", "acc"), Some(true));
        assert_eq!(m("a[^b-d]c", "acc"), Some(false));
        assert_eq!(m("a[^b-d]c", "aec"), Some(true));
        assert_eq!(m("a\\*c", "a*c"), Some(true));
        assert_eq!(m("a\\*c", "abc"), Some(false));
        assert_eq!(m("a[", "a"), None);
        assert_eq!(m("a[b", "ab"), None);
        assert_eq!(m("", ""), Some(true));
    }

    #[test]
    fn prefixes_match_like_gonosumdb() {
        let hit = |p: &str, t: &str| matches_prefix(p, t).is_some();
        assert!(hit("corp.example.com", "corp.example.com"));
        assert!(hit("corp.example.com", "corp.example.com/team/mod"));
        assert!(!hit("corp.example.com", "corp.example.com.evil/x"));
        assert!(!hit("corp.example.com", "xcorp.example.com/x"));
        assert!(hit("*.corp.example.com", "git.corp.example.com/x"));
        assert!(!hit("*.corp.example.com", "corp.example.com/x"));
        assert!(hit("github.com/acme/*", "github.com/acme/tool/v2"));
        assert!(!hit("github.com/acme/*", "github.com/acme"));
        assert!(!hit("github.com/acme", "github.com/acmeco/tool"));
        assert_eq!(
            matches_prefix("github.com/acme", "github.com/acme/tool"),
            Some(2)
        );
    }

    #[test]
    fn rules_and_sources() {
        let rules = parse_rules(
            "gitea.example.lan, go.example.com=git:gitea.example.lan/acme, corp.example.com/pkg/*=gitea:acme",
        )
        .unwrap();
        assert_eq!(rules[0].source, Source::Git);
        assert_eq!(
            rules[1].source,
            Source::Mapped {
                host: "gitea.example.lan".into(),
                owner: "acme".into()
            }
        );
        assert_eq!(
            rules[2].source,
            Source::Gitea {
                owner: "acme".into()
            }
        );
        assert_eq!(rules[1].source.cache_key(), "git-gitea.example.lan-acme");
        for bad in [
            "a b",
            "x.example=svn",
            "x.example=git:hostonly",
            "x.example=gitea:",
            "x.example,x.example",
            "x.example/[",
            "/x.example",
        ] {
            assert!(parse_rules(bad).is_err(), "{bad:?}");
        }
    }
}
