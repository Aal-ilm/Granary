//! Templates: named sets of packages for a kind of project, used to fill
//! the cache ahead of time.
//!
//! Built-in templates ship as TOML files in `templates/` and are loaded at
//! startup. They are read-only in the web UI; updating Granary updates them
//! and never touches custom templates. Custom templates live in SQLite.

pub mod api;
pub mod build;
pub mod lines;
pub mod manifest;
pub mod repo;
pub mod repo_api;
pub mod starter;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{policy::Resource, store::now_secs};
use lines::{Eco, LineError};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS templates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    builtin_key TEXT UNIQUE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    tags TEXT NOT NULL DEFAULT '[]',
    platforms TEXT NOT NULL DEFAULT '[]',
    allow_sdist INTEGER NOT NULL DEFAULT 0,
    cargo TEXT NOT NULL DEFAULT '',
    npm TEXT NOT NULL DEFAULT '',
    pypi TEXT NOT NULL DEFAULT '',
    maven TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_warmed_at INTEGER
)";

/// How often a template is warmed again on its own: (id, seconds between).
/// The schedule belongs to this Granary, not to the template's content, so
/// it can be set on built-in templates too and isn't exported.
pub const SCHEDULES: [(&str, u64); 3] = [("off", 0), ("daily", 86_400), ("weekly", 7 * 86_400)];

/// Platforms a template can be warmed for: (id, label).
pub const PLATFORMS: [(&str, &str); 5] = [
    ("linux-x86_64", "Linux x86-64"),
    ("linux-aarch64", "Linux ARM64"),
    ("macos-arm64", "macOS Apple silicon"),
    ("macos-x86_64", "macOS Intel"),
    ("windows-x86_64", "Windows x86-64"),
];

pub fn default_platforms() -> Vec<String> {
    vec!["linux-x86_64".into(), "macos-arm64".into()]
}

/// The built-in templates, compiled in so the container image needs no
/// extra files. The key is the file name without `.toml`.
const BUILTINS: [(&str, &str); 8] = [
    ("rust-cli", include_str!("../../templates/rust-cli.toml")),
    (
        "rust-async-service",
        include_str!("../../templates/rust-async-service.toml"),
    ),
    (
        "rust-embedded-cortex-m",
        include_str!("../../templates/rust-embedded-cortex-m.toml"),
    ),
    ("node-api", include_str!("../../templates/node-api.toml")),
    (
        "react-vite-ts",
        include_str!("../../templates/react-vite-ts.toml"),
    ),
    (
        "python-web-api",
        include_str!("../../templates/python-web-api.toml"),
    ),
    (
        "python-data",
        include_str!("../../templates/python-data.toml"),
    ),
    (
        "java-spring-web",
        include_str!("../../templates/java-spring-web.toml"),
    ),
];

/// A template's editable content: what the editor sends and what a TOML
/// file holds.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TemplateInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_platforms")]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub allow_sdist: bool,
    /// Dependency lines per ecosystem, as typed (comments kept).
    #[serde(default)]
    pub cargo: String,
    #[serde(default)]
    pub npm: String,
    #[serde(default)]
    pub pypi: String,
    #[serde(default)]
    pub maven: String,
}

impl TemplateInput {
    pub fn section(&self, eco: Eco) -> &str {
        match eco {
            Eco::Cargo => &self.cargo,
            Eco::Npm => &self.npm,
            Eco::Pypi => &self.pypi,
            Eco::Maven => &self.maven,
        }
    }

    fn section_mut(&mut self, eco: Eco) -> &mut String {
        match eco {
            Eco::Cargo => &mut self.cargo,
            Eco::Npm => &mut self.npm,
            Eco::Pypi => &mut self.pypi,
            Eco::Maven => &mut self.maven,
        }
    }

    /// Valid dependency lines per ecosystem.
    pub fn deps(&self, eco: Eco) -> Vec<lines::Dep> {
        lines::check(eco, self.section(eco)).0
    }

    pub fn counts(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        for eco in Eco::ALL {
            m.insert(eco.as_str().into(), self.deps(eco).len().into());
        }
        m.into()
    }

    /// Tidy up what was typed: trim fields, lowercase and dedupe tags.
    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        self.description = self.description.trim().to_string();
        let mut tags: Vec<String> = Vec::new();
        for t in &self.tags {
            let t = t.trim().to_ascii_lowercase();
            if !t.is_empty() && !tags.contains(&t) {
                tags.push(t);
            }
        }
        self.tags = tags;
        let mut platforms: Vec<String> = Vec::new();
        for (id, _) in PLATFORMS {
            if self.platforms.iter().any(|p| p == id) {
                platforms.push(id.to_string());
            }
        }
        // Unknown platform ids are kept so validation can name them.
        for p in &self.platforms {
            if !platforms.contains(p) {
                platforms.push(p.clone());
            }
        }
        self.platforms = platforms;
        for eco in Eco::ALL {
            let s = self.section_mut(eco);
            *s = s.trim_end().to_string();
        }
    }

    /// Check everything. Returns `None` when valid.
    pub fn validate(&self) -> Option<Problems> {
        let mut p = Problems::default();
        if self.name.is_empty() {
            p.fields.push(("name", "give the template a name".into()));
        } else if self.name.chars().count() > 80 {
            p.fields
                .push(("name", "names can be at most 80 characters".into()));
        }
        if self.description.chars().count() > 500 {
            p.fields.push((
                "description",
                "descriptions can be at most 500 characters".into(),
            ));
        }
        if self.tags.len() > 10 {
            p.fields.push(("tags", "use at most 10 tags".into()));
        }
        for t in &self.tags {
            let ok = t.len() <= 24
                && t.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                p.fields.push((
                    "tags",
                    format!(
                        "{t:?}: tags use lowercase letters, digits and dashes, up to 24 characters"
                    ),
                ));
            }
        }
        if self.platforms.is_empty() {
            p.fields
                .push(("platforms", "choose at least one platform".into()));
        }
        for plat in &self.platforms {
            if !PLATFORMS.iter().any(|(id, _)| id == plat) {
                p.fields
                    .push(("platforms", format!("unknown platform {plat:?}")));
            }
        }
        let mut total = 0;
        for eco in Eco::ALL {
            let (deps, errors) = lines::check(eco, self.section(eco));
            total += deps.len();
            if deps.len() > 500 {
                p.fields.push((
                    "packages",
                    format!("{} has more than 500 packages", eco.label()),
                ));
            }
            if !errors.is_empty() {
                p.lines.push((eco, errors));
            }
        }
        if total == 0 && p.lines.is_empty() {
            p.fields
                .push(("packages", "add at least one package".into()));
        }
        (!p.fields.is_empty() || !p.lines.is_empty()).then_some(p)
    }

    /// Parse a template TOML file.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let file: TemplateFile = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let mut t = TemplateInput {
            name: file.name,
            description: file.description,
            tags: file.tags,
            platforms: file.platforms,
            allow_sdist: file.allow_sdist,
            ..Default::default()
        };
        for (eco, section) in [
            (Eco::Cargo, file.cargo),
            (Eco::Npm, file.npm),
            (Eco::Pypi, file.pypi),
            (Eco::Maven, file.maven),
        ] {
            *t.section_mut(eco) = section.dependencies.join("\n");
        }
        t.normalize();
        Ok(t)
    }

    /// Write the template as a TOML file (dependency lines only; comments
    /// are left out).
    pub fn to_toml(&self) -> String {
        let section = |eco: Eco| Section {
            dependencies: self.deps(eco).into_iter().map(|d| d.line).collect(),
        };
        let file = TemplateFile {
            name: self.name.clone(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            platforms: self.platforms.clone(),
            allow_sdist: self.allow_sdist,
            cargo: section(Eco::Cargo),
            npm: section(Eco::Npm),
            pypi: section(Eco::Pypi),
            maven: section(Eco::Maven),
        };
        toml::to_string_pretty(&file).unwrap_or_default()
    }
}

/// What is wrong with a template, for the editor.
#[derive(Debug, Default)]
pub struct Problems {
    /// (field, message)
    pub fields: Vec<(&'static str, String)>,
    pub lines: Vec<(Eco, Vec<LineError>)>,
}

impl Problems {
    pub fn to_json(&self) -> serde_json::Value {
        let lines: serde_json::Map<String, serde_json::Value> = self
            .lines
            .iter()
            .map(|(eco, errs)| (eco.as_str().to_string(), serde_json::json!(errs)))
            .collect();
        serde_json::json!({
            "fields": self.fields.iter().map(|(f, m)| serde_json::json!({"field": f, "message": m})).collect::<Vec<_>>(),
            "lines": lines,
        })
    }

    /// One sentence for a plain error response.
    pub fn summary(&self) -> String {
        if let Some((_, m)) = self.fields.first() {
            return m.clone();
        }
        if let Some((eco, errs)) = self.lines.first() {
            let e = &errs[0];
            return format!("{} line {}: {}", eco.label(), e.line, e.message);
        }
        "the template is not valid".into()
    }
}

/// The on-disk TOML layout of a template.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_platforms")]
    platforms: Vec<String>,
    #[serde(default)]
    allow_sdist: bool,
    #[serde(default, skip_serializing_if = "Section::is_empty")]
    cargo: Section,
    #[serde(default, skip_serializing_if = "Section::is_empty")]
    npm: Section,
    #[serde(default, skip_serializing_if = "Section::is_empty")]
    pypi: Section,
    #[serde(default, skip_serializing_if = "Section::is_empty")]
    maven: Section,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Section {
    #[serde(default)]
    dependencies: Vec<String>,
}

impl Section {
    fn is_empty(&self) -> bool {
        self.dependencies.is_empty()
    }
}

/// A stored template.
#[derive(Clone, Debug, Serialize)]
pub struct Template {
    pub id: i64,
    pub builtin: bool,
    #[serde(flatten)]
    pub input: TemplateInput,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_warmed_at: Option<i64>,
    /// "off", "daily" or "weekly".
    pub schedule: String,
    /// Set for templates made with "New from repo".
    #[serde(skip)]
    pub source: Option<RepoSource>,
    /// Warnings from the latest successful warm (a JSON list), if any.
    #[serde(serialize_with = "crate::jobs::warnings_list")]
    pub last_warm_warnings: Option<String>,
    /// Made from private content (a Gitea repo, or a clone of a template
    /// that was), so the access policy decides who sees it.
    #[serde(skip)]
    pub private: Option<Resource>,
}

impl Template {
    /// The private content this template comes from, if any.
    pub fn resource(&self, cfg: &crate::config::Config) -> Option<Resource> {
        self.private
            .clone()
            .or_else(|| self.source.as_ref().and_then(|s| s.resource(cfg)))
    }
}

/// Where a "New from repo" template came from, and the files read there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoSource {
    /// `https://{host}/{owner}/{repo}`
    pub url: String,
    /// The ref as entered; empty for the default branch.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// The commit the files were read at.
    pub commit: String,
    pub synced_at: i64,
    /// The manifests and lockfiles read: path -> contents.
    pub files: std::collections::BTreeMap<String, String>,
}

impl RepoSource {
    /// The Gitea repo this was read from, when it is one.
    pub fn resource(&self, cfg: &crate::config::Config) -> Option<Resource> {
        let rest = self.url.strip_prefix("https://")?;
        let mut parts = rest.split('/');
        let (host, owner, name) = (parts.next()?, parts.next()?, parts.next()?);
        cfg.is_gitea_host(host)
            .then(|| Resource::gitea_repo(owner, name))
    }

    pub fn lockfiles(&self) -> Vec<&str> {
        self.files
            .keys()
            .map(String::as_str)
            .filter(|p| p.ends_with("Cargo.lock") || p.ends_with("package-lock.json"))
            .collect()
    }

    /// What the UI shows: everything but the file contents.
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "url": self.url,
            "ref": self.git_ref,
            "commit": self.commit,
            "synced_at": self.synced_at,
            "files": self.files.keys().collect::<Vec<_>>(),
            "lockfiles": self.lockfiles(),
        })
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    builtin_key: Option<String>,
    name: String,
    description: String,
    tags: String,
    platforms: String,
    allow_sdist: bool,
    cargo: String,
    npm: String,
    pypi: String,
    maven: String,
    created_at: i64,
    updated_at: i64,
    last_warmed_at: Option<i64>,
    schedule: String,
    source: Option<String>,
    last_warm_warnings: Option<String>,
    private: Option<String>,
}

impl From<Row> for Template {
    fn from(r: Row) -> Self {
        Template {
            id: r.id,
            builtin: r.builtin_key.is_some(),
            input: TemplateInput {
                name: r.name,
                description: r.description,
                tags: serde_json::from_str(&r.tags).unwrap_or_default(),
                platforms: serde_json::from_str(&r.platforms)
                    .unwrap_or_else(|_| default_platforms()),
                allow_sdist: r.allow_sdist,
                cargo: r.cargo,
                npm: r.npm,
                pypi: r.pypi,
                maven: r.maven,
            },
            created_at: r.created_at,
            updated_at: r.updated_at,
            last_warmed_at: r.last_warmed_at,
            schedule: r.schedule,
            source: r.source.and_then(|s| serde_json::from_str(&s).ok()),
            last_warm_warnings: r.last_warm_warnings,
            private: r.private.and_then(|s| serde_json::from_str(&s).ok()),
        }
    }
}

const COLUMNS: &str = "id, builtin_key, name, description, tags, platforms, allow_sdist, cargo, npm, pypi, maven, created_at, updated_at, last_warmed_at, schedule, source, last_warm_warnings, private";

/// Columns added after templates first shipped.
pub async fn migrate(db: &SqlitePool) -> sqlx::Result<()> {
    crate::auth::add_column(db, "templates", "schedule", "TEXT NOT NULL DEFAULT 'off'").await?;
    crate::auth::add_column(db, "templates", "schedule_queued_at", "INTEGER").await?;
    crate::auth::add_column(db, "templates", "source", "TEXT").await?;
    crate::auth::add_column(db, "templates", "last_warm_warnings", "TEXT").await?;
    crate::auth::add_column(db, "templates", "private", "TEXT").await?;
    // Who set the schedule: scheduled warms act for them.
    crate::auth::add_column(
        db,
        "templates",
        "schedule_by",
        "INTEGER REFERENCES users(id) ON DELETE SET NULL",
    )
    .await
}

/// Mark a template as made from private content.
pub async fn set_private(db: &SqlitePool, id: i64, r: &Resource) -> sqlx::Result<()> {
    sqlx::query("UPDATE templates SET private = ? WHERE id = ?")
        .bind(serde_json::to_string(r).unwrap())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Who set a template's schedule, if they are still around.
pub async fn schedule_by(db: &SqlitePool, id: i64) -> sqlx::Result<Option<i64>> {
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT schedule_by FROM templates WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(by,)| by))
}

/// Record (or replace) where a repo template came from.
pub async fn set_source(db: &SqlitePool, id: i64, source: &RepoSource) -> sqlx::Result<()> {
    sqlx::query("UPDATE templates SET source = ? WHERE id = ?")
        .bind(serde_json::to_string(source).unwrap())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Replace a repo template's package lines with what the repo lists now.
/// Its name, description, tags, platforms and schedule stay as they are.
pub async fn resync(
    db: &SqlitePool,
    id: i64,
    t: &TemplateInput,
    source: &RepoSource,
) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "UPDATE templates SET cargo = ?, npm = ?, pypi = ?, maven = ?, source = ?, updated_at = ?
         WHERE id = ? AND builtin_key IS NULL AND source IS NOT NULL",
    )
    .bind(&t.cargo)
    .bind(&t.npm)
    .bind(&t.pypi)
    .bind(&t.maven)
    .bind(serde_json::to_string(source).unwrap())
    .bind(now_secs() as i64)
    .bind(id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn set_schedule(db: &SqlitePool, id: i64, schedule: &str, by: i64) -> sqlx::Result<bool> {
    let r = sqlx::query("UPDATE templates SET schedule = ?, schedule_by = ? WHERE id = ?")
        .bind(schedule)
        .bind(by)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn list(db: &SqlitePool) -> sqlx::Result<Vec<Template>> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM templates ORDER BY builtin_key IS NULL, name COLLATE NOCASE"
    )))
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Template::from).collect())
}

pub async fn get(db: &SqlitePool, id: i64) -> sqlx::Result<Option<Template>> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM templates WHERE id = ?"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(Template::from))
}

/// Whether another template already uses this name (case-insensitive).
pub async fn name_taken(db: &SqlitePool, name: &str, except: Option<i64>) -> sqlx::Result<bool> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM templates WHERE name = ? COLLATE NOCASE AND id != ?")
            .bind(name)
            .bind(except.unwrap_or(-1))
            .fetch_one(db)
            .await?;
    Ok(n > 0)
}

pub async fn insert(db: &SqlitePool, t: &TemplateInput) -> sqlx::Result<i64> {
    let now = now_secs() as i64;
    let r = sqlx::query(
        "INSERT INTO templates (name, description, tags, platforms, allow_sdist, cargo, npm, pypi, maven, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&t.name)
    .bind(&t.description)
    .bind(serde_json::to_string(&t.tags).unwrap())
    .bind(serde_json::to_string(&t.platforms).unwrap())
    .bind(t.allow_sdist)
    .bind(&t.cargo)
    .bind(&t.npm)
    .bind(&t.pypi)
    .bind(&t.maven)
    .bind(now)
    .bind(now)
    .execute(db)
    .await?;
    Ok(r.last_insert_rowid())
}

/// Update a custom template. Returns false if there is no such custom template.
pub async fn update(db: &SqlitePool, id: i64, t: &TemplateInput) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "UPDATE templates SET name = ?, description = ?, tags = ?, platforms = ?, allow_sdist = ?,
            cargo = ?, npm = ?, pypi = ?, maven = ?, updated_at = ?
         WHERE id = ? AND builtin_key IS NULL",
    )
    .bind(&t.name)
    .bind(&t.description)
    .bind(serde_json::to_string(&t.tags).unwrap())
    .bind(serde_json::to_string(&t.platforms).unwrap())
    .bind(t.allow_sdist)
    .bind(&t.cargo)
    .bind(&t.npm)
    .bind(&t.pypi)
    .bind(&t.maven)
    .bind(now_secs() as i64)
    .bind(id)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Delete a custom template. Returns false if there is no such custom template.
pub async fn delete(db: &SqlitePool, id: i64) -> sqlx::Result<bool> {
    let r = sqlx::query("DELETE FROM templates WHERE id = ? AND builtin_key IS NULL")
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// A name for a copy that no other template uses: "X (copy)", "X (copy 2)"...
pub async fn copy_name(db: &SqlitePool, name: &str) -> sqlx::Result<String> {
    let base: String = name.chars().take(70).collect();
    for n in 1..1000 {
        let candidate = if n == 1 {
            format!("{base} (copy)")
        } else {
            format!("{base} (copy {n})")
        };
        if !name_taken(db, &candidate, None).await? {
            return Ok(candidate);
        }
    }
    Ok(format!("{base} (copy {})", now_secs()))
}

/// Parse the built-in templates. A broken one is a bug in Granary itself,
/// so it is reported rather than silently skipped.
pub fn builtins() -> Vec<(&'static str, Result<TemplateInput, String>)> {
    BUILTINS
        .iter()
        .map(|(key, text)| {
            let parsed = TemplateInput::from_toml(text).and_then(|t| match t.validate() {
                None => Ok(t),
                Some(p) => Err(p.summary()),
            });
            (*key, parsed)
        })
        .collect()
}

/// Load the built-in templates into the database: add new ones, update
/// changed ones and remove ones Granary no longer ships. Custom templates
/// are never touched.
pub async fn sync_builtins(db: &SqlitePool) -> anyhow::Result<()> {
    let now = now_secs() as i64;
    let mut keys = Vec::new();
    for (key, parsed) in builtins() {
        let t = match parsed {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("built-in template {key} is invalid and was skipped: {e}");
                continue;
            }
        };
        keys.push(key);
        sqlx::query(
            "INSERT INTO templates (builtin_key, name, description, tags, platforms, allow_sdist, cargo, npm, pypi, maven, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(builtin_key) DO UPDATE SET
                name = excluded.name, description = excluded.description, tags = excluded.tags,
                platforms = excluded.platforms, allow_sdist = excluded.allow_sdist,
                cargo = excluded.cargo, npm = excluded.npm, pypi = excluded.pypi, maven = excluded.maven,
                updated_at = CASE WHEN templates.cargo = excluded.cargo AND templates.npm = excluded.npm
                    AND templates.pypi = excluded.pypi AND templates.maven = excluded.maven
                    AND templates.name = excluded.name AND templates.description = excluded.description
                    AND templates.tags = excluded.tags AND templates.platforms = excluded.platforms
                    AND templates.allow_sdist = excluded.allow_sdist
                    THEN templates.updated_at ELSE excluded.updated_at END",
        )
        .bind(key)
        .bind(&t.name)
        .bind(&t.description)
        .bind(serde_json::to_string(&t.tags).unwrap())
        .bind(serde_json::to_string(&t.platforms).unwrap())
        .bind(t.allow_sdist)
        .bind(&t.cargo)
        .bind(&t.npm)
        .bind(&t.pypi)
        .bind(&t.maven)
        .bind(now)
        .bind(now)
        .execute(db)
        .await?;
    }
    let stored: Vec<(String,)> =
        sqlx::query_as("SELECT builtin_key FROM templates WHERE builtin_key IS NOT NULL")
            .fetch_all(db)
            .await?;
    for (key,) in stored {
        if !keys.contains(&key.as_str()) {
            sqlx::query("DELETE FROM templates WHERE builtin_key = ?")
                .bind(&key)
                .execute(db)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_valid() {
        let all = builtins();
        assert_eq!(all.len(), 8);
        for (key, parsed) in all {
            let t = parsed.unwrap_or_else(|e| panic!("built-in {key}: {e}"));
            assert!(!t.tags.is_empty(), "{key} has tags");
            assert_eq!(t.platforms, default_platforms(), "{key} platforms");
        }
    }

    #[test]
    fn toml_round_trip() {
        let text = r#"
name = "Demo"
description = "A demo"
tags = ["Rust", "cli", "rust"]
allow_sdist = true

[cargo]
dependencies = ['serde = { version = "1", features = ["derive"] }', 'anyhow = "*"']

[pypi]
dependencies = ["fastapi>=0.115"]
"#;
        let t = TemplateInput::from_toml(text).unwrap();
        assert_eq!(t.tags, vec!["rust", "cli"]);
        assert_eq!(t.platforms, default_platforms());
        assert!(t.validate().is_none());
        assert_eq!(t.counts()["cargo"], 2);
        let again = TemplateInput::from_toml(&t.to_toml()).unwrap();
        assert_eq!(again.cargo, t.cargo);
        assert_eq!(again.pypi, t.pypi);
        assert!(again.npm.is_empty());
        assert!(again.allow_sdist);
    }

    #[test]
    fn toml_problems_are_reported() {
        assert!(TemplateInput::from_toml("description = \"no name\"").is_err());
        assert!(TemplateInput::from_toml("name = \"x\"\nsurprise = 1").is_err());
        assert!(TemplateInput::from_toml("name = \"x\"\n[cargo]\ndeps = []").is_err());

        let t = TemplateInput::from_toml(
            r#"
name = "Bad lines"
platforms = ["linux-x86_64", "amiga-m68k"]
tags = ["Not OK!"]
[cargo]
dependencies = ['serde = { git = "https://x" }']
[npm]
dependencies = ['"x": "file:../x"']
[pypi]
dependencies = ["-r base.txt"]
[maven]
dependencies = ["org.x:y:1.0-SNAPSHOT"]
"#,
        )
        .unwrap();
        let p = t.validate().expect("invalid");
        let fields: Vec<&str> = p.fields.iter().map(|(f, _)| *f).collect();
        assert!(fields.contains(&"platforms"));
        assert!(fields.contains(&"tags"));
        assert_eq!(p.lines.len(), 4, "every ecosystem has a bad line");
    }

    #[test]
    fn empty_templates_are_refused() {
        let t = TemplateInput {
            name: "Empty".into(),
            platforms: default_platforms(),
            cargo: "# nothing yet".into(),
            ..Default::default()
        };
        assert!(
            t.validate()
                .unwrap()
                .summary()
                .contains("at least one package")
        );
    }
}
