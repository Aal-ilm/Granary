//! Runtime configuration, read from environment variables so the service is
//! easy to configure from `docker run -e` or a compose file.

use std::{net::IpAddr, path::PathBuf, time::Duration};

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
    /// Maven repository in the standard Maven 2 layout.
    pub maven_upstream: String,
    /// Allow "Get packages" to send search text to search hosts.
    pub search: bool,
    /// crates.io's web API, used only for search.
    pub cargo_api: String,
    /// Maven Central's search API, used only for search.
    pub maven_search: String,
    /// The warmer service, e.g. `http://granary-warmer:9090`. Warming is
    /// off when unset.
    pub warmer_url: Option<String>,
    /// Shared secret Granary sends to the warmer.
    pub warmer_secret: Option<String>,
    /// How the warmer reaches Granary, e.g. `http://granary:8080`.
    pub warmer_granary_url: String,
    /// Warm jobs that may run at once.
    pub warm_jobs: usize,
    /// Longest a warm job may run.
    pub warm_timeout: Duration,
    /// Python version pip resolves wheels for.
    pub warm_python: String,
    /// Git hosts Granary may clone from (HTTPS only).
    pub git_hosts: Vec<String>,
    /// Largest a cached git repo may grow.
    pub git_max_size: u64,
    /// Longest a git clone or refresh may run.
    pub git_timeout: Duration,
    /// Tests only: where to clone from instead of `https://{host}`. The
    /// environment can't set it.
    pub git_upstream_override: Option<String>,
    /// Username for the first admin account, created when no users exist.
    pub admin_user: String,
    /// Password for that account. When unset, one is generated and printed once.
    pub admin_password: Option<String>,
    /// Require an access token (or a signed-in session) for package downloads.
    pub client_auth: bool,
    /// Mark the session cookie `Secure` (HTTPS only).
    pub cookie_secure: bool,
    /// Sign users out after this much inactivity.
    pub session_idle: Duration,
    /// Reverse proxies whose `X-Forwarded-*` headers are believed. Requests
    /// from anywhere else have those headers removed before routing.
    pub trusted_proxies: Vec<IpNet>,
    /// Delete audit log entries older than this many days.
    pub audit_days: u32,
    /// Never contact an upstream: serve only what is cached.
    pub offline: bool,
    /// Point npm packuments' tarball URLs at Granary. When off, they keep
    /// the upstream's URLs, so lockfiles written through Granary stay
    /// portable.
    pub npm_rewrite_tarballs: bool,
    /// Byte budget for the cache (files plus metadata). Shown on the
    /// Overview gauges; warm jobs stop before going past it.
    pub cache_limit: Option<u64>,
    /// A self-hosted Gitea used as a private upstream.
    pub gitea: Option<Gitea>,
    /// Extra root certificates (PEM) trusted for every HTTPS upstream.
    pub extra_ca: Option<PathBuf>,
    /// Internal hosts Granary may connect to although they resolve to a
    /// private address (besides the Gitea host).
    pub private_hosts: Vec<String>,
    /// Who may read private content, when the environment decides it. When
    /// `None`, admins choose on the System page.
    pub private_access: Option<crate::policy::Mode>,
    /// Private namespaces set by the environment, per format.
    pub namespace_env: crate::namespaces::Preset,
}

/// The Gitea connection.
#[derive(Clone, Debug)]
pub struct Gitea {
    /// `https://host[:port]`, without a trailing slash.
    pub url: String,
    /// `host[:port]`, lowercase, as it appears in git paths.
    pub host: String,
    pub token: Secret,
}

impl Gitea {
    /// The host name alone, without a port.
    pub fn hostname(&self) -> &str {
        host_without_port(&self.host)
    }
}

/// A value that must never be printed: its `Debug` output is a placeholder
/// and it has no `Display` or `Serialize`.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(hidden)")
    }
}

/// `host` from `host:port` (and `[v6]` from `[v6]:port`).
pub fn host_without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .split_once(']')
            .map_or(authority, |(h, _)| &authority[..h.len() + 1]);
    }
    authority.split(':').next().unwrap_or(authority)
}

/// An IP network such as `10.0.0.0/8` or `fd00::/8` (a bare address is a /32 or /128).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (s.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // Treat IPv4-mapped IPv6 peers (::ffff:a.b.c.d) as the IPv4 address.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

fn parse_proxies(raw: &str) -> anyhow::Result<Vec<IpNet>> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            IpNet::parse(s).with_context(|| {
                format!("GRANARY_TRUSTED_PROXIES: {s:?} is not an IP address or CIDR range")
            })
        })
        .collect()
}

fn var(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The git host allowlist: lowercase DNS names with at least one dot. IP
/// addresses, localhost and anything with a port or path are refused.
pub fn parse_git_hosts(raw: &str) -> anyhow::Result<Vec<String>> {
    let mut hosts = Vec::new();
    for h in raw
        .split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let h = h.to_ascii_lowercase();
        let is_ip =
            h.parse::<IpAddr>().is_ok() || h.trim_matches(['[', ']']).parse::<IpAddr>().is_ok();
        let labels_ok = h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if is_ip || h == "localhost" || h.ends_with(".localhost") || !h.contains('.') || !labels_ok
        {
            anyhow::bail!(
                "GRANARY_GIT_HOSTS: {h:?} is not allowed; give public host names such as github.com (no IP addresses, localhost, ports or paths)"
            );
        }
        if !hosts.contains(&h) {
            hosts.push(h);
        }
    }
    Ok(hosts)
}

/// `GRANARY_GITEA_URL`: `https://host` or `https://host:port`, nothing else.
pub fn parse_gitea_url(raw: &str) -> anyhow::Result<(String, String)> {
    let url = raw.trim().trim_end_matches('/');
    let bad = || {
        anyhow::anyhow!(
            "GRANARY_GITEA_URL must look like https://gitea.example.com (https, a host name and an optional port, no path), not {raw:?}"
        )
    };
    let authority = url.strip_prefix("https://").ok_or_else(bad)?;
    if authority.is_empty() || authority.contains(['/', '?', '#', '@', '[', ']']) {
        return Err(bad());
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    let host_ok = !host.is_empty()
        && host.split('.').all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if !host_ok || port.is_some_and(|p| p.parse::<u16>().map_or(true, |p| p == 0)) {
        return Err(bad());
    }
    let authority = authority.to_ascii_lowercase();
    Ok((format!("https://{authority}"), authority))
}

/// `GRANARY_PRIVATE_HOSTS`: host names or IP addresses, without ports.
pub fn parse_private_hosts(raw: &str) -> anyhow::Result<Vec<String>> {
    let mut hosts = Vec::new();
    for h in raw
        .split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let h = h.to_ascii_lowercase();
        let bare = h.trim_start_matches('[').trim_end_matches(']');
        let ok = bare.parse::<IpAddr>().is_ok()
            || bare.split('.').all(|l| {
                !l.is_empty()
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if !ok {
            anyhow::bail!(
                "GRANARY_PRIVATE_HOSTS: {h:?} is not a host name or IP address (no ports, paths or schemes)"
            );
        }
        if !hosts.iter().any(|x| x == bare) {
            hosts.push(bare.to_string());
        }
    }
    Ok(hosts)
}

/// A size such as `20GB`, `1.5 TB`, `500MiB` or a plain number of bytes.
/// KB, MB, GB and TB are powers of 1024, as the web UI shows sizes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().ok()?;
    let shift = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 0,
        "K" | "KB" | "KIB" => 10,
        "M" | "MB" | "MIB" => 20,
        "G" | "GB" | "GIB" => 30,
        "T" | "TB" | "TIB" => 40,
        _ => return None,
    };
    let bytes = num * (1u64 << shift) as f64;
    (bytes.is_finite() && bytes >= 1.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

fn bool_var(key: &str, default: bool) -> anyhow::Result<bool> {
    match var(key, "").to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => anyhow::bail!("{key} must be \"on\" or \"off\", not {other:?}"),
    }
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

        let public_url_https = public_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"));

        Self {
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
            maven_upstream: url_var("GRANARY_MAVEN_UPSTREAM", "https://repo1.maven.org/maven2"),
            search: bool_var("GRANARY_SEARCH", true)?,
            cargo_api: url_var("GRANARY_CARGO_API", "https://crates.io/api/v1"),
            maven_search: url_var(
                "GRANARY_MAVEN_SEARCH",
                "https://search.maven.org/solrsearch/select",
            ),
            warmer_url: Some(url_var("GRANARY_WARMER_URL", "")).filter(|s| !s.is_empty()),
            warmer_secret: {
                let s = var("GRANARY_WARMER_SECRET", "");
                if s.is_empty() {
                    None
                } else if s.len() < 16 {
                    anyhow::bail!("GRANARY_WARMER_SECRET needs at least 16 characters");
                } else {
                    Some(s)
                }
            },
            warmer_granary_url: url_var("GRANARY_WARMER_GRANARY_URL", "http://granary:8080"),
            warm_jobs: var("GRANARY_WARM_JOBS", "2")
                .parse::<usize>()
                .context("GRANARY_WARM_JOBS must be a whole number")?
                .clamp(1, 8),
            warm_timeout: Duration::from_secs(
                var("GRANARY_WARM_TIMEOUT_MINS", "30")
                    .parse::<u64>()
                    .context("GRANARY_WARM_TIMEOUT_MINS must be a whole number of minutes")?
                    .clamp(1, 24 * 60)
                    * 60,
            ),
            git_hosts: parse_git_hosts(&var(
                "GRANARY_GIT_HOSTS",
                "github.com,gitlab.com,codeberg.org",
            ))?,
            git_max_size: parse_size(&var("GRANARY_GIT_MAX_SIZE", "2GB"))
                .context("GRANARY_GIT_MAX_SIZE must be a size like 500MB or 2GB")?,
            git_timeout: Duration::from_secs(
                var("GRANARY_GIT_TIMEOUT_MINS", "10")
                    .parse::<u64>()
                    .context("GRANARY_GIT_TIMEOUT_MINS must be a whole number of minutes")?
                    .clamp(1, 24 * 60)
                    * 60,
            ),
            git_upstream_override: None,
            warm_python: {
                let v = var("GRANARY_WARM_PYTHON", "3.12");
                let ok = v.split('.').count() == 2
                    && v.split('.')
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
                if !ok {
                    anyhow::bail!("GRANARY_WARM_PYTHON must look like 3.12, not {v:?}");
                }
                v
            },
            admin_user: var("GRANARY_ADMIN_USER", "admin"),
            admin_password: std::env::var("GRANARY_ADMIN_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty()),
            client_auth: match var("GRANARY_CLIENT_AUTH", "required")
                .to_ascii_lowercase()
                .as_str()
            {
                "required" | "on" | "true" => true,
                "off" | "false" | "none" => false,
                other => anyhow::bail!(
                    "GRANARY_CLIENT_AUTH must be \"required\" or \"off\", not {other:?}"
                ),
            },
            cookie_secure: match std::env::var("GRANARY_COOKIE_SECURE").ok().as_deref() {
                Some("true") | Some("1") => true,
                Some("false") | Some("0") => false,
                _ => public_url_https,
            },
            session_idle: Duration::from_secs(
                var("GRANARY_SESSION_HOURS", "12")
                    .parse::<u64>()
                    .context("GRANARY_SESSION_HOURS must be a whole number of hours")?
                    .clamp(1, 24 * 30)
                    * 3600,
            ),
            trusted_proxies: parse_proxies(&var("GRANARY_TRUSTED_PROXIES", ""))?,
            offline: bool_var("GRANARY_OFFLINE", false)?,
            npm_rewrite_tarballs: bool_var("GRANARY_NPM_REWRITE_TARBALLS", true)?,
            cache_limit: match var("GRANARY_CACHE_LIMIT", "").as_str() {
                "" => None,
                s => Some(parse_size(s).with_context(|| {
                    format!("GRANARY_CACHE_LIMIT: {s:?} is not a size like 500MB, 20GB or 1.5TB")
                })?),
            },
            audit_days: var("GRANARY_AUDIT_DAYS", "365")
                .parse::<u32>()
                .context("GRANARY_AUDIT_DAYS must be a whole number of days")?
                .clamp(1, 3650),
            gitea: None,
            extra_ca: Some(var("GRANARY_EXTRA_CA", ""))
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            private_hosts: parse_private_hosts(&var("GRANARY_PRIVATE_HOSTS", ""))?,
            private_access: match var("GRANARY_PRIVATE_ACCESS", "").as_str() {
                "" => None,
                s => Some(crate::policy::Mode::parse(s).with_context(|| {
                    format!("GRANARY_PRIVATE_ACCESS must be \"signed-in\" or \"scoped\", not {s:?}")
                })?),
            },
            namespace_env: {
                use crate::{namespaces, templates::lines::Eco};
                let mut preset = namespaces::Preset::new();
                for eco in Eco::ALL {
                    let key = namespaces::env_var(eco);
                    if let Ok(raw) = std::env::var(key) {
                        let rules = namespaces::parse_rules(eco, &raw)
                            .map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
                        preset.insert(eco.as_str().to_string(), rules);
                    }
                }
                preset
            },
        }
        .with_gitea_from_env()
    }

    /// Read the Gitea settings; its host joins the git allowlist.
    fn with_gitea_from_env(mut self) -> anyhow::Result<Self> {
        let url = var("GRANARY_GITEA_URL", "");
        let token_file = var("GRANARY_GITEA_TOKEN_FILE", "");
        if url.is_empty() {
            if !token_file.is_empty() {
                anyhow::bail!("GRANARY_GITEA_TOKEN_FILE is set but GRANARY_GITEA_URL is not");
            }
            return Ok(self);
        }
        let (url, host) = parse_gitea_url(&url)?;
        if token_file.is_empty() {
            anyhow::bail!(
                "GRANARY_GITEA_URL is set, so GRANARY_GITEA_TOKEN_FILE must name a file holding a Gitea access token"
            );
        }
        let token = std::fs::read_to_string(&token_file)
            .with_context(|| format!("GRANARY_GITEA_TOKEN_FILE: could not read {token_file}"))?
            .trim()
            .to_string();
        if token.is_empty() || token.contains(char::is_whitespace) {
            anyhow::bail!("GRANARY_GITEA_TOKEN_FILE: {token_file} must hold a single access token");
        }
        self.set_gitea(Gitea {
            url,
            host,
            token: Secret::new(token),
        });
        Ok(self)
    }

    /// Connect to Gitea: its host joins the git allowlist.
    pub fn set_gitea(&mut self, gitea: Gitea) {
        if !self.git_hosts.contains(&gitea.host) {
            self.git_hosts.push(gitea.host.clone());
        }
        self.gitea = Some(gitea);
    }

    /// Whether a git host is the Gitea host, whose content is private.
    pub fn is_gitea_host(&self, host: &str) -> bool {
        self.gitea
            .as_ref()
            .is_some_and(|g| g.host.eq_ignore_ascii_case(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_hosts() {
        assert_eq!(
            parse_git_hosts("GitHub.com, gitlab.com,github.com").unwrap(),
            vec!["github.com", "gitlab.com"]
        );
        for bad in [
            "127.0.0.1",
            "localhost",
            "git.localhost",
            "[::1]",
            "::1",
            "intranet",
            "github.com:8443",
            "github.com/x",
            "-x.com",
        ] {
            assert!(parse_git_hosts(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn gitea_urls() {
        assert_eq!(
            parse_gitea_url("https://Gitea.Skynet.Core/").unwrap(),
            (
                "https://gitea.skynet.core".to_string(),
                "gitea.skynet.core".to_string()
            )
        );
        assert_eq!(
            parse_gitea_url("https://git.lan:3000").unwrap().1,
            "git.lan:3000"
        );
        for bad in [
            "http://gitea.lan",
            "gitea.lan",
            "https://gitea.lan/sub",
            "https://user@gitea.lan",
            "https://gitea.lan:0",
            "https://gitea.lan:x",
            "https://[::1]",
            "https://",
        ] {
            assert!(parse_gitea_url(bad).is_err(), "{bad:?} should be refused");
        }
        assert_eq!(host_without_port("git.lan:3000"), "git.lan");
        assert_eq!(host_without_port("[::1]:3000"), "[::1]");
        assert_eq!(
            parse_private_hosts("Nexus.lan, 10.0.0.5,[fd00::1]").unwrap(),
            vec!["nexus.lan", "10.0.0.5", "fd00::1"]
        );
        assert!(parse_private_hosts("nexus.lan:8081").is_err());
        assert!(parse_private_hosts("https://nexus.lan").is_err());
        assert_eq!(format!("{:?}", Secret::new("s3cret")), "Secret(hidden)");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20GB"), Some(20 << 30));
        assert_eq!(parse_size("20 gib"), Some(20 << 30));
        assert_eq!(parse_size("1.5TB"), Some(3 << 39));
        assert_eq!(parse_size("500M"), Some(500 << 20));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("20 bananas"), None);
        assert_eq!(parse_size("GB"), None);
        assert_eq!(parse_size("-5GB"), None);
    }

    #[test]
    fn ip_networks() {
        let lan = IpNet::parse("192.168.1.0/24").unwrap();
        assert!(lan.contains("192.168.1.77".parse().unwrap()));
        assert!(!lan.contains("192.168.2.1".parse().unwrap()));
        assert!(lan.contains("::ffff:192.168.1.5".parse().unwrap()));
        let one = IpNet::parse("10.0.0.1").unwrap();
        assert!(one.contains("10.0.0.1".parse().unwrap()));
        assert!(!one.contains("10.0.0.2".parse().unwrap()));
        assert!(
            IpNet::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        assert!(
            IpNet::parse("fd00::/8")
                .unwrap()
                .contains("fd12::1".parse().unwrap())
        );
        assert!(IpNet::parse("10.0.0.0/33").is_none());
        assert!(parse_proxies("10.0.0.0/8, 172.16.0.0/12").unwrap().len() == 2);
        assert!(parse_proxies("nonsense").is_err());
    }
}
