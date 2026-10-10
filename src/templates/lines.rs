//! Dependency lines: one package per line, in each ecosystem's own syntax.
//!
//! | Ecosystem | Example line |
//! |---|---|
//! | Cargo | `serde = { version = "1", features = ["derive"] }` |
//! | npm | `"react": "^19"` |
//! | PyPI | `fastapi>=0.115` |
//! | Maven | `org.springframework.boot:spring-boot-starter-web:3.4.0` |
//!
//! Blank lines and lines starting with `#` or `//` are ignored. Only
//! dependencies that come from the registry can be warmed, so git, path,
//! file and URL dependencies are refused.

use serde::{Deserialize, Serialize};

use crate::formats::{npm::valid_package_name, pypi::normalize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Eco {
    Cargo,
    Npm,
    Pypi,
    Maven,
}

impl Eco {
    pub const ALL: [Eco; 4] = [Eco::Cargo, Eco::Npm, Eco::Pypi, Eco::Maven];

    pub fn as_str(self) -> &'static str {
        match self {
            Eco::Cargo => "cargo",
            Eco::Npm => "npm",
            Eco::Pypi => "pypi",
            Eco::Maven => "maven",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Eco::Cargo => "Cargo",
            Eco::Npm => "npm",
            Eco::Pypi => "PyPI",
            Eco::Maven => "Maven",
        }
    }
}

/// A problem with one line, numbered from 1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LineError {
    pub line: usize,
    pub text: String,
    pub message: String,
}

/// One valid dependency: its package name and the line as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Dep {
    pub name: String,
    pub line: String,
}

fn is_comment(line: &str) -> bool {
    line.is_empty() || line.starts_with('#') || line.starts_with("//")
}

/// Check every line of one ecosystem's section.
pub fn check(eco: Eco, text: &str) -> (Vec<Dep>, Vec<LineError>) {
    let mut deps: Vec<Dep> = Vec::new();
    let mut errors = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if is_comment(line) {
            continue;
        }
        let err = |message: String| LineError {
            line: i + 1,
            text: line.to_string(),
            message,
        };
        if line.len() > 500 {
            errors.push(err("line is too long".into()));
            continue;
        }
        match check_line(eco, line) {
            Ok(name) => {
                if let Some(first) = deps.iter().position(|d| d.name == name) {
                    errors.push(err(format!(
                        "{name} is already listed (dependency {})",
                        first + 1
                    )));
                } else {
                    deps.push(Dep {
                        name,
                        line: line.to_string(),
                    });
                }
            }
            Err(message) => errors.push(err(message)),
        }
    }
    (deps, errors)
}

/// Check one line; returns the package name it refers to.
pub fn check_line(eco: Eco, line: &str) -> Result<String, String> {
    match eco {
        Eco::Cargo => cargo(line),
        Eco::Npm => npm(line),
        Eco::Pypi => pypi(line),
        Eco::Maven => maven(line).map(|(g, a, _)| format!("{g}:{a}")),
    }
}

// -------------------------------------------------------------------- cargo

fn valid_crate_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn version_req(req: &str) -> Result<(), String> {
    semver::VersionReq::parse(req)
        .map(|_| ())
        .map_err(|e| format!("{req:?} is not a Cargo version requirement: {e}"))
}

fn cargo(line: &str) -> Result<String, String> {
    const HINT: &str = "expected a Cargo dependency, like serde = \"1\" or serde = { version = \"1\", features = [\"derive\"] }";
    let doc: toml::Table = line.parse().map_err(|_| HINT.to_string())?;
    if doc.len() != 1 {
        return Err("put one dependency on each line".into());
    }
    let (name, value) = doc.iter().next().unwrap();
    if !valid_crate_name(name) {
        return Err(format!("{name:?} is not a valid crate name"));
    }
    match value {
        toml::Value::String(req) => version_req(req)?,
        toml::Value::Table(t) => {
            for key in t.keys() {
                match key.as_str() {
                    "version" | "features" | "default-features" | "default_features"
                    | "optional" | "package" => {}
                    "git" | "path" | "branch" | "tag" | "rev" | "registry" | "workspace" => {
                        return Err(format!(
                            "{key} dependencies can't be warmed; give a crates.io version instead"
                        ));
                    }
                    other => return Err(format!("unknown dependency key {other:?}")),
                }
            }
            match t.get("version") {
                Some(toml::Value::String(req)) => version_req(req)?,
                Some(_) => return Err("version must be a string, like \"1\"".into()),
                None => return Err("give a version, like version = \"1\" (\"*\" for any)".into()),
            }
            if let Some(f) = t.get("features") {
                let ok = f
                    .as_array()
                    .is_some_and(|a| a.iter().all(|v| v.as_str().is_some()));
                if !ok {
                    return Err("features must be a list of strings".into());
                }
            }
            for key in ["default-features", "default_features", "optional"] {
                if t.get(key).is_some_and(|v| !v.is_bool()) {
                    return Err(format!("{key} must be true or false"));
                }
            }
            if let Some(p) = t.get("package")
                && !p.as_str().is_some_and(valid_crate_name)
            {
                return Err("package must be a crate name".into());
            }
        }
        _ => return Err(HINT.into()),
    }
    // A renamed dependency (`package = "..."`) is still the named crate.
    let real = value
        .get("package")
        .and_then(|p| p.as_str())
        .unwrap_or(name);
    Ok(real.to_ascii_lowercase().replace('_', "-"))
}

// ---------------------------------------------------------------------- npm

fn npm(line: &str) -> Result<String, String> {
    const HINT: &str = "expected \"name\": \"version\", like \"react\": \"^19\"";
    let body = line.trim_end_matches(',');
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&format!("{{{body}}}")).map_err(|_| HINT.to_string())?;
    if map.len() != 1 {
        return Err("put one dependency on each line".into());
    }
    let (name, value) = map.iter().next().unwrap();
    if !valid_package_name(name) {
        return Err(format!("{name:?} is not a valid npm package name"));
    }
    let Some(range) = value.as_str() else {
        return Err(HINT.into());
    };
    let range = range.trim();
    if range.is_empty() {
        return Err("give a version range, like \"^19\" (\"latest\" for the newest)".into());
    }
    // npm:other@1 installs another registry package under this name.
    let check = match range.strip_prefix("npm:") {
        Some(alias) => {
            let (pkg, ver) = match alias.rfind('@').filter(|&i| i > 0) {
                Some(i) => (&alias[..i], &alias[i + 1..]),
                None => (alias, "latest"),
            };
            if !valid_package_name(pkg) {
                return Err(format!("{pkg:?} is not a valid npm package name"));
            }
            ver
        }
        None => range,
    };
    if check.contains(':') || check.contains('/') {
        return Err(
            "only registry versions can be warmed, not git, file, link or URL dependencies".into(),
        );
    }
    let ok = check.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'.' | b'^' | b'~' | b'<' | b'>' | b'=' | b'|' | b'*' | b' ' | b'-' | b'+'
            )
    });
    if !ok {
        return Err(format!("{range:?} is not a version range or dist-tag"));
    }
    Ok(name.clone())
}

// --------------------------------------------------------------------- pypi

fn pep508_name_end(s: &str) -> usize {
    s.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(s.len())
}

fn pypi(line: &str) -> Result<String, String> {
    // pip allows a trailing comment after whitespace.
    let line = match line.find(" #") {
        Some(i) => line[..i].trim(),
        None => line,
    };
    if line.starts_with('-') {
        return Err(
            "pip options such as -r, -e and --index-url aren't supported; list one package per line"
                .into(),
        );
    }
    let (req, marker) = match line.split_once(';') {
        Some((r, m)) => (r.trim(), Some(m.trim())),
        None => (line, None),
    };
    if marker.is_some_and(str::is_empty) {
        return Err("nothing after ';' (expected an environment marker)".into());
    }
    if req.contains('@') || req.contains("://") {
        return Err("direct URL requirements can't be warmed; give a version instead".into());
    }
    let end = pep508_name_end(req);
    let name = &req[..end];
    let ends_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    if name.is_empty() || !ends_ok(name.chars().next()) || !ends_ok(name.chars().last()) {
        return Err("expected a requirement, like fastapi>=0.115".into());
    }
    let mut rest = req[end..].trim_start();
    if let Some(after) = rest.strip_prefix('[') {
        let Some((extras, tail)) = after.split_once(']') else {
            return Err("missing ']' after the extras".into());
        };
        for extra in extras.split(',').map(str::trim) {
            if extra.is_empty() || pep508_name_end(extra) != extra.len() {
                return Err(format!("{extra:?} is not a valid extra"));
            }
        }
        rest = tail.trim_start();
    }
    let rest = rest
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or(rest)
        .trim();
    if !rest.is_empty() {
        for spec in rest.split(',').map(str::trim) {
            let op = ["===", "~=", "==", "!=", "<=", ">=", "<", ">"]
                .into_iter()
                .find(|op| spec.starts_with(op))
                .ok_or_else(|| format!("{spec:?} is not a version specifier, like >=1.0"))?;
            let version = spec[op.len()..].trim();
            let ok = !version.is_empty()
                && version.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'.' | b'*' | b'+' | b'!' | b'_' | b'-')
                });
            if !ok {
                return Err(format!("{spec:?} is not a version specifier, like >=1.0"));
            }
        }
    }
    Ok(normalize(name))
}

// -------------------------------------------------------------------- maven

/// Split a Maven line into group, artifact and an optional version.
pub fn maven(line: &str) -> Result<(String, String, Option<String>), String> {
    const HINT: &str = "expected group:artifact or group:artifact:version";
    let parts: Vec<&str> = line.split(':').map(str::trim).collect();
    if !(2..=3).contains(&parts.len()) {
        return Err(HINT.into());
    }
    let ident = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let (group, artifact) = (parts[0], parts[1]);
    if !ident(group) || group.split('.').any(str::is_empty) {
        return Err(format!("{group:?} is not a valid groupId"));
    }
    if !ident(artifact) {
        return Err(format!("{artifact:?} is not a valid artifactId"));
    }
    let version = match parts.get(2) {
        None => None,
        Some(v) => {
            let ok = !v.is_empty()
                && v.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(
                            b,
                            b'.' | b'_' | b'-' | b'+' | b'[' | b']' | b'(' | b')' | b','
                        )
                });
            if !ok {
                return Err(format!("{v:?} is not a version or version range"));
            }
            if v.contains("SNAPSHOT") {
                return Err("Maven Central has no snapshots".into());
            }
            Some(v.to_string())
        }
    };
    Ok((group.to_string(), artifact.to_string(), version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(eco: Eco, line: &str) -> String {
        check_line(eco, line).unwrap_or_else(|e| panic!("{line:?} should be valid: {e}"))
    }

    fn bad(eco: Eco, line: &str) -> String {
        check_line(eco, line).expect_err(&format!("{line:?} should be rejected"))
    }

    #[test]
    fn cargo_lines() {
        assert_eq!(ok(Eco::Cargo, r#"serde = "1""#), "serde");
        assert_eq!(
            ok(
                Eco::Cargo,
                r#"serde = { version = "1", features = ["derive"] }"#
            ),
            "serde"
        );
        assert_eq!(
            ok(
                Eco::Cargo,
                r#"tokio = { version = "*", features = ["full"], default-features = false }"#
            ),
            "tokio"
        );
        assert_eq!(
            ok(
                Eco::Cargo,
                r#"json = { package = "serde_json", version = "1" }"#
            ),
            "serde-json"
        );
        assert!(
            bad(
                Eco::Cargo,
                r#"serde = { git = "https://github.com/serde-rs/serde" }"#
            )
            .contains("git")
        );
        assert!(bad(Eco::Cargo, r#"x = { path = "../x" }"#).contains("path"));
        assert!(bad(Eco::Cargo, r#"serde = { features = ["derive"] }"#).contains("version"));
        assert!(bad(Eco::Cargo, r#"serde = "not a version""#).contains("version requirement"));
        assert!(bad(Eco::Cargo, "serde").contains("expected"));
        assert!(bad(Eco::Cargo, r#"a = "1"; b = "2""#).contains("expected"));
        assert!(
            bad(
                Eco::Cargo,
                r#"serde = { version = "1", features = "derive" }"#
            )
            .contains("features")
        );
        assert!(bad(Eco::Cargo, r#"9lives = "1""#).contains("crate name"));
    }

    #[test]
    fn npm_lines() {
        assert_eq!(ok(Eco::Npm, r#""react": "^19""#), "react");
        assert_eq!(
            ok(Eco::Npm, r#""@vitejs/plugin-react": "latest","#),
            "@vitejs/plugin-react"
        );
        assert_eq!(ok(Eco::Npm, r#""ts": ">=5 <6 || 4.9.x""#), "ts");
        assert_eq!(ok(Eco::Npm, r#""lodash4": "npm:lodash@4""#), "lodash4");
        assert!(bad(Eco::Npm, r#""x": "git+https://github.com/a/b.git""#).contains("registry"));
        assert!(bad(Eco::Npm, r#""x": "file:../x""#).contains("registry"));
        assert!(bad(Eco::Npm, r#""x": "user/repo""#).contains("registry"));
        assert!(bad(Eco::Npm, r#""x": """#).contains("version range"));
        assert!(bad(Eco::Npm, "react ^19").contains("expected"));
        assert!(bad(Eco::Npm, r#""../evil": "1""#).contains("package name"));
        assert!(bad(Eco::Npm, r#""a": "1", "b": "2""#).contains("one dependency"));
    }

    #[test]
    fn pypi_lines() {
        assert_eq!(ok(Eco::Pypi, "fastapi>=0.115"), "fastapi");
        assert_eq!(ok(Eco::Pypi, "Django"), "django");
        assert_eq!(ok(Eco::Pypi, "uvicorn[standard]>=0.30,<1"), "uvicorn");
        assert_eq!(ok(Eco::Pypi, "pywin32; sys_platform == 'win32'"), "pywin32");
        assert_eq!(ok(Eco::Pypi, "numpy==2.*  # pinned major"), "numpy");
        assert_eq!(ok(Eco::Pypi, "zope.interface~=7.0"), "zope-interface");
        assert!(bad(Eco::Pypi, "-r other.txt").contains("options"));
        assert!(bad(Eco::Pypi, "-e .").contains("options"));
        assert!(bad(Eco::Pypi, "pkg @ https://example.com/pkg.whl").contains("URL"));
        assert!(bad(Eco::Pypi, "fastapi >> 1").contains("specifier"));
        assert!(bad(Eco::Pypi, "uvicorn[standard").contains("]"));
        assert!(bad(Eco::Pypi, "pkg;").contains("marker"));
        assert!(bad(Eco::Pypi, ">=1.0").contains("expected"));
    }

    #[test]
    fn maven_lines() {
        assert_eq!(
            ok(
                Eco::Maven,
                "org.springframework.boot:spring-boot-starter-web:3.4.0"
            ),
            "org.springframework.boot:spring-boot-starter-web"
        );
        assert_eq!(
            ok(Eco::Maven, "com.google.guava:guava"),
            "com.google.guava:guava"
        );
        assert_eq!(ok(Eco::Maven, "junit:junit:[4.13,5)"), "junit:junit");
        assert!(bad(Eco::Maven, "guava").contains("expected"));
        assert!(bad(Eco::Maven, "a:b:c:d").contains("expected"));
        assert!(bad(Eco::Maven, "org..x:y").contains("groupId"));
        assert!(bad(Eco::Maven, "org.x:y z").contains("artifactId"));
        assert!(bad(Eco::Maven, "org.x:y:1.0-SNAPSHOT").contains("snapshots"));
        assert!(bad(Eco::Maven, "org.x:y:1.0/../../").contains("version"));
    }

    #[test]
    fn sections_skip_comments_and_catch_duplicates() {
        let text = "# web\nfastapi\n\n// tests\npytest>=8\nFastAPI>=1\nnot valid!!\n";
        let (deps, errors) = check(Eco::Pypi, text);
        assert_eq!(deps.len(), 2);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].line, 6);
        assert!(errors[0].message.contains("already listed"));
        assert_eq!(errors[1].line, 7);
    }
}
