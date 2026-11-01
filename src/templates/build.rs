//! Turn a template into project manifests: the files the warmer runs the
//! package tools on, and the files in a starter project.

use std::collections::HashMap;

use super::{
    TemplateInput,
    api::slug,
    lines::{self, Eco},
};

/// The manifest files for one ecosystem: (path, contents).
#[derive(Clone, Debug)]
pub struct Manifest {
    pub eco: Eco,
    pub files: Vec<(String, String)>,
}

/// A project name every tool accepts: lowercase letters, digits and dashes,
/// starting with a letter.
pub fn project_name(template: &str) -> String {
    let s = slug(template);
    if s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        s
    } else {
        format!("granary-{s}")
    }
}

/// Manifests for every ecosystem the template uses. `versions` gives a
/// version for each Maven `group:artifact` listed without one, and for
/// each Go module listed as `@latest` (keyed by module path; Go paths
/// never contain a colon, so the keys can't clash).
pub fn manifests(
    t: &TemplateInput,
    versions: &HashMap<String, String>,
) -> Result<Vec<Manifest>, String> {
    let name = project_name(&t.name);
    let mut out = Vec::new();
    for eco in Eco::LINES {
        let deps = t.deps(eco);
        if deps.is_empty() {
            continue;
        }
        let lines: Vec<&str> = deps.iter().map(|d| d.line.as_str()).collect();
        let files = match eco {
            Eco::Cargo => cargo(&name, &lines),
            Eco::Npm => npm(&name, &lines)?,
            Eco::Pypi => vec![("requirements.txt".into(), format!("{}\n", lines.join("\n")))],
            Eco::Maven => maven(&name, &lines, versions)?,
            Eco::Go => vec![("go.mod".into(), go_mod(&name, &lines, versions)?)],
        };
        out.push(Manifest { eco, files });
    }
    Ok(out)
}

fn cargo(name: &str, lines: &[&str]) -> Vec<(String, String)> {
    let manifest = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\n{}\n",
        lines.join("\n")
    );
    vec![
        ("Cargo.toml".into(), manifest),
        ("src/main.rs".into(), "fn main() {}\n".into()),
    ]
}

fn npm(name: &str, lines: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut deps = serde_json::Map::new();
    for line in lines {
        let one: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&format!("{{{}}}", line.trim_end_matches(',')))
                .map_err(|e| format!("npm line {line}: {e}"))?;
        deps.extend(one);
    }
    let doc = serde_json::json!({
        "name": name,
        "version": "0.0.0",
        "private": true,
        "dependencies": deps,
    });
    Ok(vec![(
        "package.json".into(),
        format!("{}\n", serde_json::to_string_pretty(&doc).unwrap()),
    )])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn maven(
    name: &str,
    lines: &[&str],
    versions: &HashMap<String, String>,
) -> Result<Vec<(String, String)>, String> {
    let mut deps = String::new();
    for line in lines {
        let (g, a, v) = lines::maven(line)?;
        let v = match v {
            Some(v) => v,
            None => versions
                .get(&format!("{g}:{a}"))
                .cloned()
                .ok_or_else(|| format!("no version found for {g}:{a}"))?,
        };
        deps.push_str(&format!(
            "    <dependency>\n      <groupId>{}</groupId>\n      <artifactId>{}</artifactId>\n      <version>{}</version>\n    </dependency>\n",
            xml_escape(&g),
            xml_escape(&a),
            xml_escape(&v)
        ));
    }
    let pom = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0"
         xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
         xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 https://maven.apache.org/xsd/maven-4.0.0.xsd">
  <modelVersion>4.0.0</modelVersion>
  <groupId>local.granary</groupId>
  <artifactId>{name}</artifactId>
  <version>0.1.0</version>
  <properties>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
    <maven.compiler.release>17</maven.compiler.release>
  </properties>
  <dependencies>
{deps}  </dependencies>
</project>
"#
    );
    Ok(vec![("pom.xml".into(), pom)])
}

/// The go.mod for Go lines: one `require` each, from checked values only
/// (an `@latest` line takes its version from `versions`). No `replace`,
/// `toolchain` or anything else.
pub fn go_mod(
    name: &str,
    lines: &[&str],
    versions: &HashMap<String, String>,
) -> Result<String, String> {
    let mut requires = String::new();
    for line in lines {
        crate::gomodfile::check_line(line)?;
        let (path, version) = line.split_once('@').ok_or("not a Go line")?;
        let version = if version == "latest" {
            versions
                .get(path)
                .ok_or_else(|| format!("no version found for {path}"))?
        } else {
            version
        };
        crate::gomod::check(path, version)?;
        requires.push_str(&format!("\t{path} {version}\n"));
    }
    Ok(format!(
        "module {name}\n\ngo 1.22\n\nrequire (\n{requires})\n"
    ))
}

/// The module path of every Go line listed as `@latest`.
pub fn latest_go(t: &TemplateInput) -> Vec<String> {
    t.deps(Eco::Go)
        .into_iter()
        .filter_map(|d| d.line.strip_suffix("@latest").map(String::from))
        .collect()
}

/// The `group:artifact` of every Maven line without a version.
pub fn unversioned_maven(t: &TemplateInput) -> Vec<String> {
    t.deps(Eco::Maven)
        .into_iter()
        .filter_map(|d| match lines::maven(&d.line) {
            Ok((g, a, None)) => Some(format!("{g}:{a}")),
            _ => None,
        })
        .collect()
}

/// A one-package template: the given version, exactly.
pub fn single(
    eco: Eco,
    name: &str,
    version: &str,
    platforms: Vec<String>,
) -> Result<TemplateInput, String> {
    let line = match eco {
        Eco::Cargo => format!("{name} = \"={version}\""),
        Eco::Npm => format!(
            "{}: {}",
            serde_json::to_string(name).unwrap(),
            serde_json::to_string(version).unwrap()
        ),
        Eco::Pypi => format!("{name}=={version}"),
        Eco::Maven => format!("{name}:{version}"),
        Eco::Go => {
            return Err(
                "warming a Go module with its dependencies is not supported yet; use Cache instead"
                    .into(),
            );
        }
    };
    lines::check_line(eco, &line)?;
    let mut t = TemplateInput {
        name: format!("{name} {version}"),
        platforms,
        ..Default::default()
    };
    *t.section_mut(eco) = line;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::default_platforms;

    fn template() -> TemplateInput {
        TemplateInput {
            name: "1 Demo & <Tests>".into(),
            platforms: default_platforms(),
            cargo: "# c\nserde = { version = \"1\", features = [\"derive\"] }\nanyhow = \"*\""
                .into(),
            npm: "\"react\": \"^19\",\n\"@types/node\": \"latest\"".into(),
            pypi: "fastapi>=0.115\nuvicorn[standard]".into(),
            maven: "com.google.guava:guava\njunit:junit:4.13.2".into(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_every_manifest() {
        let t = template();
        assert_eq!(unversioned_maven(&t), vec!["com.google.guava:guava"]);
        assert!(
            manifests(&t, &HashMap::new())
                .unwrap_err()
                .contains("guava")
        );

        let versions = HashMap::from([(
            "com.google.guava:guava".to_string(),
            "33.4.0-jre".to_string(),
        )]);
        let all = manifests(&t, &versions).unwrap();
        assert_eq!(all.len(), 4);
        let file = |eco: Eco, path: &str| {
            all.iter()
                .find(|m| m.eco == eco)
                .unwrap()
                .files
                .iter()
                .find(|(p, _)| p == path)
                .unwrap()
                .1
                .clone()
        };

        let cargo: toml::Table = file(Eco::Cargo, "Cargo.toml").parse().unwrap();
        assert_eq!(
            cargo["package"]["name"].as_str(),
            Some("granary-1-demo-tests")
        );
        assert!(cargo["dependencies"]["serde"]["features"].is_array());

        let npm: serde_json::Value = serde_json::from_str(&file(Eco::Npm, "package.json")).unwrap();
        assert_eq!(npm["dependencies"]["@types/node"], "latest");
        assert_eq!(npm["private"], true);

        assert_eq!(
            file(Eco::Pypi, "requirements.txt"),
            "fastapi>=0.115\nuvicorn[standard]\n"
        );

        let pom = file(Eco::Maven, "pom.xml");
        roxmltree::Document::parse(&pom).unwrap();
        assert!(pom.contains("<version>33.4.0-jre</version>"));
        assert!(pom.contains("<version>4.13.2</version>"));
    }

    #[test]
    fn single_package_templates() {
        let t = single(Eco::Cargo, "serde", "1.0.229", default_platforms()).unwrap();
        assert_eq!(t.cargo, "serde = \"=1.0.229\"");
        let t = single(Eco::Npm, "@types/node", "22.0.0", default_platforms()).unwrap();
        assert_eq!(t.npm, "\"@types/node\": \"22.0.0\"");
        assert_eq!(
            single(Eco::Pypi, "requests", "2.32.3", vec![])
                .unwrap()
                .pypi,
            "requests==2.32.3"
        );
        assert_eq!(
            single(Eco::Maven, "a.b:c", "1.0", vec![]).unwrap().maven,
            "a.b:c:1.0"
        );
        assert!(single(Eco::Npm, "x", "file:../x", vec![]).is_err());
    }
}
