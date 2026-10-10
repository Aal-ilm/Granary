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
            maven_upstream: url_var("GRANARY_MAVEN_UPSTREAM", "https://repo1.maven.org/maven2"),
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
            audit_days: var("GRANARY_AUDIT_DAYS", "365")
                .parse::<u32>()
                .context("GRANARY_AUDIT_DAYS must be a whole number of days")?
                .clamp(1, 3650),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
