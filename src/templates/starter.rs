//! "Download starter": a zip with a minimal project for a template, set up
//! to install through this Granary.
//!
//! It holds the manifests, the lockfiles from the template's last successful
//! warm (when there is one) and client configuration pointing at Granary.
//! It never holds a token: every tool reads it from `GRANARY_TOKEN` (or the
//! user runs `make use-granary`), and README.txt says so.

use std::{
    collections::HashMap,
    io::{Cursor, Write},
};

use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use super::{
    TemplateInput,
    build::{Manifest, project_name},
    lines::Eco,
};

pub struct Starter<'a> {
    pub template: &'a TemplateInput,
    pub manifests: Vec<Manifest>,
    /// Lockfiles from the last successful warm: ecosystem -> path -> contents.
    pub lockfiles: HashMap<String, HashMap<String, String>>,
    /// When those lockfiles were made (unix seconds).
    pub warmed_at: Option<i64>,
    /// The address clients use for this Granary.
    pub base_url: &'a str,
    /// The address the warmer used, which lockfiles may contain.
    pub warmer_granary_url: &'a str,
    pub client_auth: bool,
}

impl Starter<'_> {
    /// Every file in the starter: (path inside the project, contents).
    pub fn files(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for m in &self.manifests {
            let locks = self.lockfiles.get(m.eco.as_str());
            for (path, text) in &m.files {
                // Maven has no lockfile; the pom.xml that was warmed, with
                // every version filled in, takes the generated one's place.
                let warmed = locks
                    .and_then(|l| l.get(path))
                    .filter(|_| m.eco == Eco::Maven);
                out.push((path.clone(), warmed.unwrap_or(text).clone()));
            }
            if let Some(locks) = locks {
                for (path, text) in locks {
                    if m.files.iter().any(|(p, _)| p == path) {
                        continue;
                    }
                    let text = if path == "package-lock.json" {
                        self.portable_npm_lock(text)
                    } else {
                        text.clone()
                    };
                    out.push((path.clone(), text));
                }
            }
            out.extend(self.client_config(m.eco));
        }
        out.push(("README.txt".into(), self.readme()));
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// npm writes the registry it used into package-lock.json. Point those
    /// URLs at registry.npmjs.org: npm maps them onto whatever registry is
    /// configured, so the lockfile works through Granary or without it.
    fn portable_npm_lock(&self, text: &str) -> String {
        let mut text = text.to_string();
        for base in [self.warmer_granary_url, self.base_url] {
            text = text.replace(&format!("{base}/npm/"), "https://registry.npmjs.org/");
        }
        text
    }

    fn host(&self) -> &str {
        let rest = self
            .base_url
            .split_once("://")
            .map_or(self.base_url, |(_, r)| r);
        rest.split('/').next().unwrap_or(rest)
    }

    fn client_config(&self, eco: Eco) -> Vec<(String, String)> {
        let base = self.base_url;
        let auth = self.client_auth;
        match eco {
            Eco::Cargo => vec![(
                ".cargo/config.toml".into(),
                format!(
                    "# Fetch crates through Granary for this project.\n{}[registries.granary]\nindex = \"sparse+{base}/cargo/\"\n{}\n[source.crates-io]\nreplace-with = \"granary\"\n",
                    if auth {
                        "# Granary needs a token: run `make use-granary`, or set\n# CARGO_REGISTRIES_GRANARY_TOKEN to your Granary access token.\n"
                    } else {
                        ""
                    },
                    if auth {
                        "credential-provider = \"cargo:token\"\n"
                    } else {
                        ""
                    },
                ),
            )],
            Eco::Npm => vec![(
                ".npmrc".into(),
                if auth {
                    format!(
                        "registry={base}/npm/\n# Set GRANARY_TOKEN to your Granary access token (npm reads it from the environment).\n//{}/npm/:_authToken=${{GRANARY_TOKEN}}\n",
                        self.host()
                    )
                } else {
                    format!("registry={base}/npm/\n")
                },
            )],
            Eco::Pypi => {
                let (scheme, _) = base.split_once("://").unwrap_or(("http", base));
                vec![(
                    "pip.conf".into(),
                    if auth {
                        format!(
                            "# pip can't read a token from a config file. Either run `make use-granary`, or\n# set PIP_INDEX_URL={scheme}://__token__:$GRANARY_TOKEN@{}/pypi/simple/\n# and leave this file unused.\n[global]\nindex-url = {base}/pypi/simple/\n",
                            self.host()
                        )
                    } else {
                        format!(
                            "# Use with: PIP_CONFIG_FILE=pip.conf pip install -r requirements.txt\n[global]\nindex-url = {base}/pypi/simple/\n"
                        )
                    },
                )]
            }
            Eco::Maven => {
                let server = if auth {
                    "  <servers>\n    <server>\n      <id>granary</id>\n      <username>token</username>\n      <!-- Read from the environment: set GRANARY_TOKEN to your access token. -->\n      <password>${env.GRANARY_TOKEN}</password>\n    </server>\n  </servers>\n"
                } else {
                    ""
                };
                vec![
                    (
                        ".mvn/settings.xml".into(),
                        format!(
                            "<settings>\n  <mirrors>\n    <mirror>\n      <id>granary</id>\n      <mirrorOf>central</mirrorOf>\n      <url>{base}/maven</url>\n    </mirror>\n  </mirrors>\n{server}</settings>\n"
                        ),
                    ),
                    // Maven reads extra command-line options from here.
                    (".mvn/maven.config".into(), "-s\n.mvn/settings.xml\n".into()),
                ]
            }
        }
    }

    fn readme(&self) -> String {
        let t = self.template;
        let used: Vec<&str> = self.manifests.iter().map(|m| m.eco.label()).collect();
        let mut s = format!(
            "{name}\n{rule}\n\n{desc}This starter was made by Granary ({base}) from the template \"{name}\".\nIt uses {used}, set up to install packages through Granary.\n\n",
            name = t.name,
            rule = "=".repeat(t.name.chars().count().max(3)),
            desc = if t.description.is_empty() {
                String::new()
            } else {
                format!("{}\n\n", t.description)
            },
            base = self.base_url,
            used = used.join(", "),
        );
        s.push_str(match self.warmed_at {
            Some(_) => "The lockfiles come from the template's last successful warm, so they\nlist versions Granary already has cached.\n\n",
            None => "The template has not been warmed yet, so there are no lockfiles; the\npackage tools will pick versions when you first install.\n\n",
        });
        if self.client_auth {
            s.push_str(&format!(
                "ACCESS TOKEN\n\nThis zip contains no access token. Create one in Granary under Access\ntokens, then either:\n\n  * run `make use-granary GRANARY_URL={}` from Granary's source folder,\n    which sets up Cargo, npm, pip, Maven and Gradle for your user, or\n  * set GRANARY_TOKEN to the token in your shell. The project's .npmrc and\n    .mvn/settings.xml read it from there. For Cargo also set\n    CARGO_REGISTRIES_GRANARY_TOKEN, and for pip set PIP_INDEX_URL as\n    pip.conf explains.\n\n",
                self.base_url
            ));
        } else {
            s.push_str("This Granary doesn't need an access token for downloads.\n\n");
        }
        s.push_str("FILES\n\n");
        for m in &self.manifests {
            let line = match m.eco {
                Eco::Cargo => {
                    "Cargo.toml, Cargo.lock, src/main.rs, .cargo/config.toml (crates.io replaced by Granary)"
                }
                Eco::Npm => "package.json, package-lock.json, .npmrc (Granary as the registry)",
                Eco::Pypi => {
                    "requirements.txt, requirements.lock.txt (versions that were cached), pip.conf"
                }
                Eco::Maven => {
                    "pom.xml, .mvn/settings.xml and .mvn/maven.config (Granary as Central's mirror)"
                }
            };
            s.push_str(&format!("  {}: {line}\n", m.eco.label()));
        }
        s
    }

    /// The starter as a zip, with everything inside one project folder.
    pub fn zip(&self) -> zip::result::ZipResult<Vec<u8>> {
        let folder = project_name(&self.template.name);
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        // Without a time, zip entries are dated 1980; stamp them with now (UTC).
        let now = time::OffsetDateTime::now_utc();
        let stamp = zip::DateTime::from_date_and_time(
            now.year().clamp(1980, 2107) as u16,
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
        )
        .unwrap_or_default();
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o644)
            .last_modified_time(stamp);
        for (path, text) in self.files() {
            zip.start_file(format!("{folder}/{path}"), opts)?;
            zip.write_all(text.as_bytes())?;
        }
        Ok(zip.finish()?.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::templates::{build::manifests, default_platforms};

    fn template() -> TemplateInput {
        TemplateInput {
            name: "Team Service".into(),
            description: "Our stack".into(),
            platforms: default_platforms(),
            cargo: "serde = \"1\"".into(),
            npm: "\"left-pad\": \"^1\"".into(),
            pypi: "httpx".into(),
            maven: "com.google.guava:guava".into(),
            ..Default::default()
        }
    }

    fn starter<'a>(t: &'a TemplateInput, auth: bool, locks: bool) -> Starter<'a> {
        let versions = HashMap::from([(
            "com.google.guava:guava".to_string(),
            "33.4.0-jre".to_string(),
        )]);
        let mut lockfiles = HashMap::new();
        if locks {
            lockfiles.insert(
                "npm".to_string(),
                HashMap::from([(
                    "package-lock.json".to_string(),
                    r#"{"packages":{"node_modules/left-pad":{"resolved":"http://granary:8080/npm/left-pad/-/left-pad-1.3.0.tgz"}}}"#.to_string(),
                )]),
            );
            lockfiles.insert(
                "cargo".to_string(),
                HashMap::from([("Cargo.lock".to_string(), "version = 4\n".to_string())]),
            );
            lockfiles.insert(
                "maven".to_string(),
                HashMap::from([(
                    "pom.xml".to_string(),
                    "<project>warmed</project>".to_string(),
                )]),
            );
        }
        Starter {
            template: t,
            manifests: manifests(t, &versions).unwrap(),
            lockfiles,
            warmed_at: locks.then_some(1),
            base_url: "https://granary.lan",
            warmer_granary_url: "http://granary:8080",
            client_auth: auth,
        }
    }

    fn unzip(bytes: &[u8]) -> HashMap<String, String> {
        let mut z = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut out = HashMap::new();
        for i in 0..z.len() {
            let mut f = z.by_index(i).unwrap();
            let name = f.name().unwrap().to_string();
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            out.insert(name, s);
        }
        out
    }

    #[test]
    fn zip_has_manifests_locks_and_config() {
        let t = template();
        let files = unzip(&starter(&t, true, true).zip().unwrap());
        let mut names: Vec<&str> = files.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "team-service/.cargo/config.toml",
                "team-service/.mvn/maven.config",
                "team-service/.mvn/settings.xml",
                "team-service/.npmrc",
                "team-service/Cargo.lock",
                "team-service/Cargo.toml",
                "team-service/README.txt",
                "team-service/package-lock.json",
                "team-service/package.json",
                "team-service/pip.conf",
                "team-service/pom.xml",
                "team-service/requirements.txt",
                "team-service/src/main.rs",
            ]
        );
        let f = |p: &str| files[&format!("team-service/{p}")].clone();
        assert!(
            f("package-lock.json")
                .contains("https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz")
        );
        assert!(!f("package-lock.json").contains("granary:8080"));
        assert_eq!(
            f("pom.xml"),
            "<project>warmed</project>",
            "the warmed pom wins"
        );
        assert!(f(".cargo/config.toml").contains("sparse+https://granary.lan/cargo/"));
        assert!(f(".npmrc").contains("//granary.lan/npm/:_authToken=${GRANARY_TOKEN}"));
        assert!(f(".mvn/settings.xml").contains("${env.GRANARY_TOKEN}"));
        assert!(f("pip.conf").contains("https://granary.lan/pypi/simple/"));
        assert!(f("README.txt").contains("contains no access token"));
    }

    #[test]
    fn open_granary_and_no_warm_yet() {
        let t = template();
        let files = unzip(&starter(&t, false, false).zip().unwrap());
        assert!(
            !files
                .keys()
                .any(|k| k.ends_with(".lock") || k.ends_with("package-lock.json"))
        );
        assert!(!files["team-service/.npmrc"].contains("authToken"));
        assert!(!files["team-service/.mvn/settings.xml"].contains("<servers>"));
        assert!(files["team-service/pom.xml"].contains("<version>33.4.0-jre</version>"));
        assert!(files["team-service/README.txt"].contains("has not been warmed yet"));
    }
}
