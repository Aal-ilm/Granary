//! Which package names belong to Gitea.
//!
//! Admins map name patterns to a Gitea owner (user or org), per format:
//!
//! | Format | Pattern |
//! |---|---|
//! | npm | a scope, `@dynatheia` |
//! | PyPI | an exact name, or a prefix ending in `*` (`dynatheia-*`), compared after PEP 503 normalization |
//! | Maven | a groupId prefix, `com.dynatheia` (whole segments: `com.dynatheiax` doesn't match) |
//! | Cargo | the whole private registry at `/cargo-private/` |
//!
//! **The dependency confusion rule.** A name matching a pattern is only ever
//! fetched from Gitea; when Gitea doesn't have it, the answer is 404 and the
//! public registry is never asked. A name matching no pattern is never
//! fetched from Gitea. Every format routes through [`Namespaces::owner`].
//!
//! A format's list comes from its environment variable when that is set
//! (`GRANARY_GITEA_NPM` and so on, written `pattern=owner,...`), and the
//! System page can't change it; otherwise admins edit it there.

use std::{collections::BTreeMap, sync::RwLock};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{formats::pypi::normalize, templates::lines::Eco};

/// One pattern and the Gitea owner its names belong to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub pattern: String,
    pub owner: String,
}

/// Rules per format, keyed by `Eco::as_str`.
pub type Lists = BTreeMap<String, Vec<Rule>>;

/// Lists set by the environment, keyed by `Eco::as_str`.
pub type Preset = BTreeMap<String, Vec<Rule>>;

const SETTING: &str = "gitea_namespaces";

pub fn env_var(eco: Eco) -> &'static str {
    match eco {
        Eco::Npm => "GRANARY_GITEA_NPM",
        Eco::Pypi => "GRANARY_GITEA_PYPI",
        Eco::Maven => "GRANARY_GITEA_MAVEN",
        Eco::Cargo => "GRANARY_GITEA_CARGO",
    }
}

/// A Gitea user or org name.
fn valid_owner(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && !s.starts_with(['.', '-'])
}

/// Check and tidy one rule for `eco`.
pub fn check_rule(eco: Eco, pattern: &str, owner: &str) -> Result<Rule, String> {
    let owner = owner.trim();
    if !valid_owner(owner) {
        return Err(format!("{owner:?} is not a Gitea user or org name"));
    }
    let p = pattern.trim();
    let pattern = match eco {
        Eco::Npm => {
            let scope = p.to_ascii_lowercase();
            let ok = scope.strip_prefix('@').is_some_and(|s| {
                !s.is_empty()
                    && s.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
                    })
            });
            if !ok {
                return Err(format!(
                    "npm namespaces are scopes like @dynatheia, not {p:?}"
                ));
            }
            scope
        }
        Eco::Pypi => {
            let (stem, wild) = match p.strip_suffix('*') {
                Some(s) => (s, true),
                None => (p, false),
            };
            let name = normalize(stem);
            let ok = !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && (wild || !name.ends_with('-'));
            if !ok {
                return Err(format!(
                    "PyPI namespaces are project names or prefixes ending in *, like dynatheia-*, not {p:?}"
                ));
            }
            if wild { format!("{name}*") } else { name }
        }
        Eco::Maven => {
            let ok = !p.is_empty()
                && p.split('.').all(|s| {
                    !s.is_empty()
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                });
            if !ok {
                return Err(format!(
                    "Maven namespaces are groupId prefixes like com.dynatheia, not {p:?}"
                ));
            }
            p.to_string()
        }
        Eco::Cargo => {
            if !p.is_empty() && p != "*" {
                return Err("the private Cargo registry takes only an owner".into());
            }
            "*".to_string()
        }
    };
    Ok(Rule {
        pattern,
        owner: owner.to_string(),
    })
}

/// Parse an environment variable's `pattern=owner,...` (for Cargo, just
/// the owner).
pub fn parse_rules(eco: Eco, raw: &str) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let rule = match (eco, item.split_once('=')) {
            (Eco::Cargo, None) => check_rule(eco, "*", item)?,
            (_, Some((p, o))) => check_rule(eco, p, o)?,
            (_, None) => return Err(format!("{item:?} should be pattern=owner")),
        };
        rules.push(rule);
    }
    check_list(eco, &rules)?;
    Ok(rules)
}

/// A list as a whole: no pattern twice, at most one Cargo owner.
pub fn check_list(eco: Eco, rules: &[Rule]) -> Result<(), String> {
    if eco == Eco::Cargo && rules.len() > 1 {
        return Err("the private Cargo registry belongs to one owner".into());
    }
    for (i, r) in rules.iter().enumerate() {
        if rules[..i].iter().any(|x| x.pattern == r.pattern) {
            return Err(format!("{} is listed twice", r.pattern));
        }
    }
    Ok(())
}

fn matches(eco: Eco, pattern: &str, name: &str) -> bool {
    match eco {
        Eco::Npm => name
            .split_once('/')
            .is_some_and(|(scope, _)| scope.eq_ignore_ascii_case(pattern)),
        Eco::Pypi => {
            let name = normalize(name);
            match pattern.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == pattern,
            }
        }
        Eco::Maven => {
            // `name` is a groupId, or `group:artifact`.
            let group = name.split(':').next().unwrap_or(name);
            group == pattern
                || group
                    .strip_prefix(pattern)
                    .is_some_and(|rest| rest.starts_with('.'))
        }
        Eco::Cargo => true,
    }
}

pub struct Namespaces {
    lists: RwLock<Lists>,
    /// Formats whose list the environment sets.
    locked: Vec<Eco>,
    /// Gitea is connected; without it no name is private.
    enabled: bool,
}

impl Namespaces {
    /// The environment's lists, then the saved ones for the other formats.
    pub async fn load(db: &SqlitePool, enabled: bool, preset: &Preset) -> sqlx::Result<Self> {
        let saved: Lists = crate::auth::setting(db, SETTING)
            .await?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut lists = Lists::new();
        let mut locked = Vec::new();
        for eco in Eco::ALL {
            let key = eco.as_str().to_string();
            match preset.get(&key) {
                Some(rules) => {
                    locked.push(eco);
                    lists.insert(key, rules.clone());
                }
                None => {
                    let rules = saved.get(&key).cloned().unwrap_or_default();
                    lists.insert(key, rules);
                }
            }
        }
        Ok(Self {
            lists: RwLock::new(lists),
            locked,
            enabled,
        })
    }

    /// The Gitea owner `name` belongs to, if it is private. For Maven, `name`
    /// is a groupId or `group:artifact`; for Cargo, any crate name.
    pub fn owner(&self, eco: Eco, name: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let lists = self.lists.read().unwrap();
        lists
            .get(eco.as_str())?
            .iter()
            .find(|r| matches(eco, &r.pattern, name))
            .map(|r| r.owner.clone())
    }

    /// The owner of the private Cargo registry, if there is one.
    pub fn cargo_owner(&self) -> Option<String> {
        self.owner(Eco::Cargo, "")
    }

    /// Every owner with names in `eco`.
    pub fn owners(&self, eco: Eco) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut owners: Vec<String> = self
            .lists
            .read()
            .unwrap()
            .get(eco.as_str())
            .into_iter()
            .flatten()
            .map(|r| r.owner.clone())
            .collect();
        owners.sort();
        owners.dedup();
        owners
    }

    pub fn lists(&self) -> Lists {
        self.lists.read().unwrap().clone()
    }

    pub fn locked(&self, eco: Eco) -> bool {
        self.locked.contains(&eco)
    }

    /// Replace one format's list from the System page.
    pub async fn set(&self, db: &SqlitePool, eco: Eco, rules: Vec<Rule>) -> Result<(), String> {
        if self.locked(eco) {
            return Err(format!(
                "{} is set, so this list can only be changed in the environment",
                env_var(eco)
            ));
        }
        let rules = rules
            .iter()
            .map(|r| check_rule(eco, &r.pattern, &r.owner))
            .collect::<Result<Vec<_>, _>>()?;
        check_list(eco, &rules)?;
        // Save only the lists the page controls.
        let mut saved = self.lists();
        saved.insert(eco.as_str().to_string(), rules.clone());
        for e in &self.locked {
            saved.remove(e.as_str());
        }
        crate::auth::set_setting(db, SETTING, &serde_json::to_string(&saved).unwrap())
            .await
            .map_err(|e| format!("could not save the setting: {e}"))?;
        self.lists
            .write()
            .unwrap()
            .insert(eco.as_str().to_string(), rules);
        Ok(())
    }

    /// For tests: these lists, nothing locked.
    #[cfg(test)]
    pub fn with(lists: &[(Eco, &str)]) -> Self {
        let mut map = Lists::new();
        for (eco, raw) in lists {
            map.insert(eco.as_str().to_string(), parse_rules(*eco, raw).unwrap());
        }
        Self {
            lists: RwLock::new(map),
            locked: Vec::new(),
            enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_names() {
        let ns = Namespaces::with(&[
            (Eco::Npm, "@dynatheia=dyn, @acme=acme-org"),
            (Eco::Pypi, "dynatheia-*=dyn, internal-tool=tools"),
            (Eco::Maven, "com.dynatheia=dyn"),
            (Eco::Cargo, "dyn"),
        ]);
        let o = |eco, name| ns.owner(eco, name);
        assert_eq!(o(Eco::Npm, "@dynatheia/ui").as_deref(), Some("dyn"));
        assert_eq!(o(Eco::Npm, "@Acme/x").as_deref(), Some("acme-org"));
        assert_eq!(o(Eco::Npm, "dynatheia-ui"), None);
        assert_eq!(o(Eco::Npm, "@dynatheiax/ui"), None);
        // PEP 503: these are all the same project.
        for name in [
            "dynatheia-foo",
            "Dynatheia_Foo",
            "dynatheia.foo",
            "DYNATHEIA__foo",
        ] {
            assert_eq!(o(Eco::Pypi, name).as_deref(), Some("dyn"), "{name}");
        }
        assert_eq!(o(Eco::Pypi, "Internal.Tool").as_deref(), Some("tools"));
        assert_eq!(o(Eco::Pypi, "internal-tools"), None);
        assert_eq!(o(Eco::Pypi, "dynatheia"), None);
        assert_eq!(o(Eco::Maven, "com.dynatheia").as_deref(), Some("dyn"));
        assert_eq!(
            o(Eco::Maven, "com.dynatheia.core:lib").as_deref(),
            Some("dyn")
        );
        assert_eq!(o(Eco::Maven, "com.dynatheiax:lib"), None);
        assert_eq!(o(Eco::Maven, "com:dynatheia"), None);
        assert_eq!(ns.cargo_owner().as_deref(), Some("dyn"));
        assert_eq!(ns.owners(Eco::Npm), vec!["acme-org", "dyn"]);
    }

    #[test]
    fn checks_rules() {
        assert_eq!(
            parse_rules(Eco::Pypi, "Dynatheia_*=dyn").unwrap()[0].pattern,
            "dynatheia-*"
        );
        for (eco, bad) in [
            (Eco::Npm, "dynatheia=dyn"),
            (Eco::Npm, "@dyn"),
            (Eco::Npm, "@dyn=bad owner"),
            (Eco::Pypi, "a/b=dyn"),
            (Eco::Pypi, "dyn-=dyn"),
            (Eco::Maven, "com..x=dyn"),
            (Eco::Maven, "com.x=dyn,com.x=other"),
            (Eco::Cargo, "a,b"),
        ] {
            assert!(parse_rules(eco, bad).is_err(), "{bad:?}");
        }
    }
}
