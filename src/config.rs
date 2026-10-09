//! Runtime configuration, read from environment variables so the service is
//! easy to configure from `docker run -e` or a compose file.

use std::{path::PathBuf, time::Duration};

use anyhow::Context;

#[derive(Clone, Debug)]
pub struct Config {
    /// Address to listen on, e.g. `0.0.0.0:8080`.
    pub listen: String,
    /// Root directory for cached data.
    pub data_dir: PathBuf,
    /// Externally visible base URL (e.g. `https://granary.lan`). When unset,
    /// it is derived from each request's `Host` / `X-Forwarded-*` headers.
    pub public_url: Option<String>,
    /// How long cached index/metadata responses are considered fresh.
    pub meta_ttl: Duration,

    pub cargo_index: String,
    /// Download URL template with `{crate}` and `{version}` placeholders.
    pub cargo_dl: String,
    pub npm_upstream: String,
    pub pypi_index: String,
    pub pypi_files: String,
    /// When set, deleting, cleanup and other changes made through the web UI
    /// or API require this token. Browsing is always allowed.
    pub admin_token: Option<String>,
}

fn var(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn url_var(key: &str, default: &str) -> String {
    var(key, default).trim_end_matches('/').to_string()
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let ttl: u64 = var("GRANARY_META_TTL_SECS", "300")
            .parse()
            .context("GRANARY_META_TTL_SECS must be a whole number of seconds")?;

        let public_url = std::env::var("GRANARY_PUBLIC_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());

        Ok(Self {
            listen: var("GRANARY_LISTEN", "0.0.0.0:8080"),
            data_dir: var("GRANARY_DATA_DIR", "./data").into(),
            public_url,
            meta_ttl: Duration::from_secs(ttl),
            cargo_index: url_var("GRANARY_CARGO_INDEX", "https://index.crates.io"),
            cargo_dl: var(
                "GRANARY_CARGO_DL",
                "https://static.crates.io/crates/{crate}/{version}/download",
            ),
            npm_upstream: url_var("GRANARY_NPM_UPSTREAM", "https://registry.npmjs.org"),
            pypi_index: url_var("GRANARY_PYPI_INDEX", "https://pypi.org/simple"),
            pypi_files: url_var("GRANARY_PYPI_FILES", "https://files.pythonhosted.org"),
            admin_token: std::env::var("GRANARY_ADMIN_TOKEN")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        })
    }
}
