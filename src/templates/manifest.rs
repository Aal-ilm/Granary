//! "Import from manifest": read the dependency sections of a Cargo.toml,
//! package.json, requirements.txt or pom.xml and turn them into template
//! lines. Every line produced is checked like a typed one; anything that
//! can't be warmed (path, git, URL dependencies...) is reported as skipped.

use serde::Serialize;

use super::lines::{Eco, check_line};

#[derive(Debug, Serialize)]
pub struct Imported {
    pub ecosystem: Eco,
    pub lines: Vec<String>,
    /// Dependencies left out, each with the reason.
    pub skipped: Vec<String>,
}

/// Largest manifest accepted.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// Work out the manifest type from its file name, or from its content when
/// pasted, then import it.
pub fn import(filename: Option<&str>, content: &str) -> Result<Imported, String> {
    if content.len() > MAX_MANIFEST_BYTES {
        return Err("that file is larger than 1 MB".into());
    }
    let name = filename
        .map(|f| {
            f.rsplit(['/', '\\'])
                .next()
                .unwrap_or(f)
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    let trimmed = content.trim_start();
    let eco = if name == "cargo.toml" {
        Eco::Cargo
    } else if name == "package.json" {
        Eco::Npm
    } else if name == "pom.xml" || name.ends_with(".pom") {
        Eco::Maven
    } else if name.ends_with(".txt") || name.ends_with(".in") {
        Eco::Pypi
    } else if trimmed.starts_with('<') {
        Eco::Maven
    } else if trimmed.starts_with('{') {
        Eco::Npm
    } else if content.parse::<toml::Table>().is_ok_and(|t| {
        ["package", "dependencies", "workspace", "dev-dependencies"]
            .iter()
            .any(|k| t.contains_key(*k))
    }) {
        Eco::Cargo
    } else {
        Eco::Pypi
    };
    let (raw, mut skipped) = match eco {
        Eco::Cargo => cargo(content)?,
        Eco::Npm => npm(content)?,
        Eco::Pypi => pypi(content),
        Eco::Maven => maven(content)?,
    };
    // Check every produced line the same way a typed line is checked, and
    // drop repeats (the same package in dependencies and dev-dependencies).
    let mut lines: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for line in raw {
        match check_line(eco, &line) {
            Ok(n) if names.contains(&n) => {}
            Ok(n) => {
                names.push(n);
                lines.push(line);
            }
            Err(e) => skipped.push(format!("{line}: {e}")),
        }
    }
    Ok(Imported {
        ecosystem: eco,
        lines,
        skipped,
    })
}

type Lines = (Vec<String>, Vec<String>);

/// A TOML basic string. JSON string escaping is valid TOML.
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn cargo(content: &str) -> Result<Lines, String> {
    let doc: toml::Table = content
        .parse()
        .map_err(|e: toml::de::Error| format!("not a valid Cargo.toml: {}", e.message()))?;
    let (mut lines, mut skipped) = (Vec::new(), Vec::new());
    let mut tables: Vec<&toml::Table> = Vec::new();
    fn collect<'a>(t: &'a toml::Table, out: &mut Vec<&'a toml::Table>) {
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
    collect(&doc, &mut tables);
    if let Some(ws) = doc.get("workspace").and_then(|v| v.as_table()) {
        collect(ws, &mut tables);
    }
    if let Some(targets) = doc.get("target").and_then(|v| v.as_table()) {
        for t in targets.values().filter_map(|v| v.as_table()) {
            collect(t, &mut tables);
        }
    }
    for table in tables {
        for (name, value) in table {
            match cargo_line(name, value) {
                Ok(l) => lines.push(l),
                Err(why) => skipped.push(format!("{name}: {why}")),
            }
        }
    }
    Ok((lines, skipped))
}

fn cargo_line(name: &str, value: &toml::Value) -> Result<String, String> {
    let t = match value {
        toml::Value::String(req) => return Ok(format!("{name} = {}", toml_str(req))),
        toml::Value::Table(t) => t,
        _ => return Err("not a dependency".into()),
    };
    if t.contains_key("workspace") {
        return Err("inherits from the workspace; import the workspace Cargo.toml instead".into());
    }
    if t.contains_key("path") || t.contains_key("git") {
        return Err("is a path or git dependency, which can't be warmed".into());
    }
    let version = t
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("has no version")?;
    let mut parts = vec![format!("version = {}", toml_str(version))];
    if let Some(features) = t.get("features").and_then(|v| v.as_array()) {
        let list: Vec<String> = features
            .iter()
            .filter_map(|f| f.as_str())
            .map(toml_str)
            .collect();
        if !list.is_empty() {
            parts.push(format!("features = [{}]", list.join(", ")));
        }
    }
    let default_features = t
        .get("default-features")
        .or_else(|| t.get("default_features"))
        .and_then(|v| v.as_bool());
    if default_features == Some(false) {
        parts.push("default-features = false".into());
    }
    if let Some(p) = t.get("package").and_then(|v| v.as_str()) {
        parts.push(format!("package = {}", toml_str(p)));
    }
    Ok(if parts.len() == 1 {
        format!("{name} = {}", toml_str(version))
    } else {
        format!("{name} = {{ {} }}", parts.join(", "))
    })
}

fn npm(content: &str) -> Result<Lines, String> {
    let doc: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("not a valid package.json: {e}"))?;
    let (mut lines, mut skipped) = (Vec::new(), Vec::new());
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        let Some(deps) = doc.get(section).and_then(|v| v.as_object()) else {
            continue;
        };
        for (name, range) in deps {
            match range.as_str() {
                Some(r) => lines.push(format!(
                    "{}: {}",
                    serde_json::to_string(name).unwrap(),
                    serde_json::to_string(r).unwrap()
                )),
                None => skipped.push(format!("{name}: the version is not a string")),
            }
        }
    }
    Ok((lines, skipped))
}

fn pypi(content: &str) -> Lines {
    let (mut lines, mut skipped) = (Vec::new(), Vec::new());
    // Join continuation lines ending in a backslash.
    let joined = content.replace("\\\r\n", " ").replace("\\\n", " ");
    for raw in joined.lines() {
        let line = match raw.find('#') {
            // A # starts a comment at the start of a line or after a space.
            Some(i) if i == 0 || raw[..i].ends_with(char::is_whitespace) => &raw[..i],
            _ => raw,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('-') {
            skipped.push(format!("{line}: pip options are not imported"));
            continue;
        }
        // Drop per-requirement options such as --hash=sha256:...
        let req = line.split(" --").next().unwrap_or(line).trim();
        lines.push(req.to_string());
    }
    (lines, skipped)
}

fn maven(content: &str) -> Result<Lines, String> {
    let doc =
        roxmltree::Document::parse(content).map_err(|e| format!("not a valid pom.xml: {e}"))?;
    let project = doc.root_element();
    if project.tag_name().name() != "project" {
        return Err("not a pom.xml: the root element is not <project>".into());
    }
    fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
        n.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
    }
    fn text(n: Option<roxmltree::Node<'_, '_>>) -> Option<String> {
        n.and_then(|n| n.text()).map(|t| t.trim().to_string())
    }

    // ${...} references: <properties>, the project's own version and the parent's.
    let mut props: Vec<(String, String)> = Vec::new();
    if let Some(p) = child(project, "properties") {
        for c in p.children().filter(|c| c.is_element()) {
            if let Some(v) = c.text() {
                props.push((c.tag_name().name().to_string(), v.trim().to_string()));
            }
        }
    }
    let parent_version = child(project, "parent").and_then(|p| text(child(p, "version")));
    if let Some(v) = text(child(project, "version")).or(parent_version.clone()) {
        props.push(("project.version".into(), v));
    }
    if let Some(v) = parent_version {
        props.push(("project.parent.version".into(), v));
    }
    let resolve = |s: String| -> Option<String> {
        let mut out = s;
        for _ in 0..5 {
            let Some(start) = out.find("${") else {
                return Some(out);
            };
            let end = out[start..].find('}')? + start;
            let key = &out[start + 2..end];
            let value = props.iter().find(|(k, _)| k == key)?.1.clone();
            out.replace_range(start..=end, &value);
        }
        (!out.contains("${")).then_some(out)
    };

    let (mut lines, mut skipped) = (Vec::new(), Vec::new());
    let mut lists = Vec::new();
    if let Some(d) = child(project, "dependencies") {
        lists.push(d);
    }
    if let Some(d) = child(project, "dependencyManagement").and_then(|m| child(m, "dependencies")) {
        lists.push(d);
    }
    for list in lists {
        for dep in list
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "dependency")
        {
            let (Some(g), Some(a)) = (
                text(child(dep, "groupId")).and_then(&resolve),
                text(child(dep, "artifactId")).and_then(&resolve),
            ) else {
                skipped.push("a dependency with no groupId or artifactId".into());
                continue;
            };
            if text(child(dep, "scope")).as_deref() == Some("system") {
                skipped.push(format!(
                    "{g}:{a}: system-scoped dependencies are local files"
                ));
                continue;
            }
            match text(child(dep, "version")) {
                // No version: managed by a parent or BOM; warm the latest.
                None => lines.push(format!("{g}:{a}")),
                Some(v) => match resolve(v.clone()) {
                    Some(v) => lines.push(format!("{g}:{a}:{v}")),
                    None => {
                        skipped.push(format!(
                            "{g}:{a}: version {v} uses a property not defined in this pom, so it was left out (the latest will be warmed)"
                        ));
                        lines.push(format!("{g}:{a}"));
                    }
                },
            }
        }
    }
    Ok((lines, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_cargo_toml() {
        let toml = r#"
[package]
name = "app"

[dependencies]
serde = { version = "1", features = ["derive"] }
anyhow = "1"
local = { path = "../local" }
shared = { workspace = true }

[dev-dependencies]
serde = "1"
tokio = { version = "1", default-features = false, features = ["rt"] }

[target.'cfg(unix)'.dependencies]
libc = "0.2"
"#;
        let r = import(Some("Cargo.toml"), toml).unwrap();
        assert_eq!(r.ecosystem, Eco::Cargo);
        assert_eq!(
            r.lines,
            vec![
                r#"anyhow = "1""#,
                r#"serde = { version = "1", features = ["derive"] }"#,
                r#"tokio = { version = "1", features = ["rt"], default-features = false }"#,
                r#"libc = "0.2""#,
            ]
        );
        assert_eq!(r.skipped.len(), 2);
        assert!(r.skipped.iter().any(|s| s.starts_with("local:")));
        assert!(r.skipped.iter().any(|s| s.starts_with("shared:")));
    }

    #[test]
    fn imports_package_json() {
        let json = r#"{"name":"web","dependencies":{"react":"^19.0.0","local":"file:../x"},
            "devDependencies":{"vite":"^6","@types/react":"^19"},"peerDependencies":{"x":"1"}}"#;
        let r = import(Some("package.json"), json).unwrap();
        assert_eq!(r.ecosystem, Eco::Npm);
        assert_eq!(
            r.lines,
            // Each section in name order.
            vec![
                r#""react": "^19.0.0""#,
                r#""@types/react": "^19""#,
                r#""vite": "^6""#,
            ]
        );
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].contains("local"));
    }

    #[test]
    fn imports_requirements_txt() {
        let txt = "# app\n-r base.txt\nfastapi>=0.115  # web\nuvicorn[standard]==0.30.0 \\\n    --hash=sha256:abc\npkg @ https://x/y.whl\n\nnumpy\n";
        let r = import(Some("requirements.txt"), txt).unwrap();
        assert_eq!(r.ecosystem, Eco::Pypi);
        assert_eq!(
            r.lines,
            vec!["fastapi>=0.115", "uvicorn[standard]==0.30.0", "numpy"]
        );
        assert_eq!(r.skipped.len(), 2);
    }

    #[test]
    fn imports_pom_xml() {
        let pom = r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <parent><groupId>org.springframework.boot</groupId><artifactId>spring-boot-starter-parent</artifactId><version>3.4.0</version></parent>
  <properties><guava.version>33.4.0-jre</guava.version></properties>
  <dependencies>
    <dependency><groupId>org.springframework.boot</groupId><artifactId>spring-boot-starter-web</artifactId></dependency>
    <dependency><groupId>com.google.guava</groupId><artifactId>guava</artifactId><version>${guava.version}</version></dependency>
    <dependency><groupId>x</groupId><artifactId>y</artifactId><version>${missing}</version></dependency>
    <dependency><groupId>sys</groupId><artifactId>tools</artifactId><version>1</version><scope>system</scope></dependency>
  </dependencies>
</project>"#;
        let r = import(Some("pom.xml"), pom).unwrap();
        assert_eq!(r.ecosystem, Eco::Maven);
        assert_eq!(
            r.lines,
            vec![
                "org.springframework.boot:spring-boot-starter-web",
                "com.google.guava:guava:33.4.0-jre",
                "x:y",
            ]
        );
        assert_eq!(r.skipped.len(), 2);
    }

    #[test]
    fn detects_pasted_manifests() {
        assert_eq!(
            import(None, "{\"dependencies\":{}}").unwrap().ecosystem,
            Eco::Npm
        );
        assert_eq!(
            import(None, "<project></project>").unwrap().ecosystem,
            Eco::Maven
        );
        assert_eq!(
            import(None, "[dependencies]\nserde = \"1\"")
                .unwrap()
                .ecosystem,
            Eco::Cargo
        );
        assert_eq!(import(None, "requests\n").unwrap().ecosystem, Eco::Pypi);
        assert!(import(Some("pom.xml"), "<html>").is_err());
        assert!(import(Some("Cargo.toml"), "[[[").is_err());
    }
}
