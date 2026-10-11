//! Outbound connections: which addresses Granary may connect to, the extra
//! root certificates it trusts, and how git is held to the same rules.
//!
//! **The private-address rule.** Upstreams are on the internet, so Granary
//! refuses to connect to loopback, private, link-local and other non-public
//! addresses. That keeps a crafted upstream name, redirect or DNS answer from
//! turning Granary into a way into the local network. Two kinds of host are
//! let through: the Gitea host, and the hosts listed in
//! `GRANARY_PRIVATE_HOSTS`. Proxies named in the `*_PROXY` variables are let
//! through too, since every connection goes to them.
//!
//! **Pinning.** A name is checked when it is resolved, and the connection
//! goes to exactly the addresses that passed: Granary's HTTP clients use a
//! resolver that applies the rule, and git gets the checked addresses through
//! `http.curloptResolve`, so DNS can't change between the check and the
//! connection.
//!
//! **Extra CA.** `GRANARY_EXTRA_CA` adds root certificates for internal CAs.
//! The HTTP clients trust them next to the system roots; git gets a bundle
//! of both through `http.sslCAInfo`.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;

use crate::config::Config;

pub struct Egress {
    /// Hosts allowed past the rule: lowercase, without ports or brackets.
    allowed: HashSet<String>,
    /// Extra root certificates for the HTTP clients.
    roots: Vec<reqwest::Certificate>,
    /// System roots plus the extra CA, for git. `None` without an extra CA.
    git_ca: Option<PathBuf>,
}

impl Egress {
    /// Load the extra CA (if any) and write git's bundle under `{data}/state`.
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let mut allowed: HashSet<String> = cfg.private_hosts.iter().cloned().collect();
        if let Some(g) = &cfg.gitea {
            allowed.insert(bare(g.hostname()));
        }
        for key in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            if let Some(host) = std::env::var(key)
                .ok()
                .and_then(|v| reqwest::Url::parse(&v).ok())
                .and_then(|u| u.host_str().map(bare))
            {
                allowed.insert(host);
            }
        }
        let (roots, git_ca) = match &cfg.extra_ca {
            None => (Vec::new(), None),
            Some(path) => {
                let pem = std::fs::read(path).with_context(|| {
                    format!("GRANARY_EXTRA_CA: could not read {}", path.display())
                })?;
                let roots = reqwest::Certificate::from_pem_bundle(&pem)
                    .ok()
                    .filter(|r| !r.is_empty())
                    .with_context(|| {
                        format!(
                            "GRANARY_EXTRA_CA: {} holds no PEM certificates",
                            path.display()
                        )
                    })?;
                let bundle = cfg.data_dir.join("state").join("ca-bundle.pem");
                write_git_bundle(&bundle, &pem)?;
                tracing::info!(
                    "trusting {} extra root certificate(s) from {}",
                    roots.len(),
                    path.display()
                );
                (roots, Some(bundle))
            }
        };
        Ok(Self {
            allowed,
            roots,
            git_ca,
        })
    }

    /// For tests: allow these hosts, no extra CA.
    #[cfg(test)]
    pub fn allowing(hosts: &[&str]) -> Self {
        Self {
            allowed: hosts.iter().map(|h| bare(h)).collect(),
            roots: Vec::new(),
            git_ca: None,
        }
    }

    fn allows(&self, host: &str) -> bool {
        self.allowed.contains(&bare(host))
    }

    /// The addresses Granary may connect to for `host`: every address for an
    /// allowed host, otherwise only the public ones (an error when none is).
    pub async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let name = bare(host);
        if let Ok(ip) = name.parse::<IpAddr>() {
            return if self.allows(&name) || is_public(ip) {
                Ok(vec![SocketAddr::new(ip, port)])
            } else {
                Err(refused(host, ip))
            };
        }
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), port))
            .await
            .map_err(|e| format!("could not resolve {host}: {e}"))?
            .collect();
        if self.allows(&name) {
            return Ok(addrs);
        }
        let public: Vec<SocketAddr> = addrs
            .iter()
            .copied()
            .filter(|a| is_public(a.ip()))
            .collect();
        match (public.is_empty(), addrs.first()) {
            (false, _) => Ok(public),
            (true, Some(a)) => Err(refused(host, a.ip())),
            (true, None) => Err(format!("could not resolve {host}")),
        }
    }

    /// Check a URL before a request: HTTP(S) only, and an IP address in it
    /// must pass the rule (host names are checked when they are resolved).
    pub fn check_url(&self, url: &str) -> Result<(), String> {
        let u = reqwest::Url::parse(url).map_err(|e| format!("invalid URL {url}: {e}"))?;
        self.check(&u)
    }

    fn check(&self, u: &reqwest::Url) -> Result<(), String> {
        if !matches!(u.scheme(), "http" | "https") {
            return Err(format!(
                "only http and https upstreams are allowed, not {u}"
            ));
        }
        let ip = match u.host() {
            Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
            Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
            Some(url::Host::Domain(_)) => return Ok(()),
            None => return Err(format!("{u} has no host")),
        };
        if self.allows(&ip.to_string()) || is_public(ip) {
            Ok(())
        } else {
            Err(refused(u.host_str().unwrap_or(""), ip))
        }
    }

    /// An HTTP client builder that follows the rule, pins each connection to
    /// the addresses that passed it, checks every redirect, and trusts the
    /// extra CA.
    pub fn client(self: &Arc<Self>) -> reqwest::ClientBuilder {
        let me = self.clone();
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            match me.check(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        });
        let mut b = reqwest::Client::builder()
            .dns_resolver(Arc::new(Resolver(self.clone())))
            .redirect(redirects);
        for root in &self.roots {
            b = b.add_root_certificate(root.clone());
        }
        b
    }

    /// `-c` settings for a git command fetching `url`: the addresses its host
    /// may be reached at, and the CA bundle when there is an extra CA.
    pub async fn git_settings(&self, url: &str) -> Result<Vec<String>, String> {
        let u = reqwest::Url::parse(url).map_err(|e| format!("invalid URL {url}: {e}"))?;
        self.check(&u)?;
        let mut out = Vec::new();
        if let (Some(url::Host::Domain(host)), Some(port)) = (u.host(), u.port_or_known_default()) {
            let addrs = self.resolve(host, port).await?;
            let list: Vec<String> = addrs
                .iter()
                .map(|a| match a.ip() {
                    IpAddr::V4(ip) => ip.to_string(),
                    IpAddr::V6(ip) => format!("[{ip}]"),
                })
                .collect();
            out.push(format!(
                "http.curloptResolve={host}:{port}:{}",
                list.join(",")
            ));
        }
        if let Some(ca) = &self.git_ca {
            out.push(format!("http.sslCAInfo={}", ca.display()));
        }
        Ok(out)
    }
}

/// reqwest's resolver: names resolve to the addresses the rule allows.
struct Resolver(Arc<Egress>);

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let egress = self.0.clone();
        Box::pin(async move {
            let addrs = egress
                .resolve(name.as_str(), 0)
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn bare(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn refused(host: &str, ip: IpAddr) -> String {
    format!(
        "{host} is at a private address ({ip}), so Granary won't connect to it. If it is an internal host Granary should use, add it to GRANARY_PRIVATE_HOSTS"
    )
}

/// Whether an address is on the public internet: not loopback, private,
/// link-local, shared (CGNAT), multicast, documentation, benchmarking or
/// reserved. IPv6 forms that embed an IPv4 address are judged by it.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| Ipv4Addr::from(((hi as u32) << 16) | lo as u32);
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            // IPv4-compatible (::a.b.c.d), NAT64 (64:ff9b::/96) and 6to4 (2002::/16).
            if s[..6] == [0; 6] && !v6.is_loopback() && !v6.is_unspecified() {
                return is_public_v4(embedded(s[6], s[7]));
            }
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return is_public_v4(embedded(s[6], s[7]));
            }
            if s[0] == 0x2002 {
                return is_public_v4(embedded(s[1], s[2]));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xffc0) == 0xfec0
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x100 && s[1..4] == [0, 0, 0]))
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(o[0] == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || o[0] >= 240)
}

/// git's CA bundle: the system's roots followed by the extra ones, so public
/// hosts keep working when the extra CA is set.
fn write_git_bundle(path: &Path, extra: &[u8]) -> anyhow::Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let native = rustls_native_certs::load_native_certs();
    for e in &native.errors {
        tracing::warn!("reading the system's root certificates: {e}");
    }
    let mut pem = String::new();
    for cert in &native.certs {
        pem.push_str("-----BEGIN CERTIFICATE-----\n");
        let b64 = STANDARD.encode(cert.as_ref());
        for line in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).unwrap_or_default());
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
    }
    if native.certs.is_empty() {
        tracing::warn!("found no system root certificates; git will trust only GRANARY_EXTRA_CA");
    }
    let mut data = pem.into_bytes();
    data.extend_from_slice(extra);
    if !data.ends_with(b"\n") {
        data.push(b'\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("pem.part");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_addresses() {
        for p in [
            "8.8.8.8",
            "1.1.1.1",
            "140.82.112.3",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip(p)), "{p} is public");
        }
        for p in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2002:a00:1::",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip(p)), "{p} is not public");
        }
    }

    #[tokio::test]
    async fn the_rule() {
        let e = Egress::allowing(&["gitea.lan", "10.9.9.9"]);
        assert!(e.resolve("127.0.0.1", 80).await.is_err());
        assert!(e.resolve("[::1]", 80).await.is_err());
        assert!(e.resolve("10.9.9.9", 80).await.is_ok());
        assert!(e.resolve("8.8.8.8", 80).await.is_ok());
        let err = e.resolve("localhost", 80).await.unwrap_err();
        assert!(err.contains("GRANARY_PRIVATE_HOSTS"), "{err}");
        assert!(e.check_url("http://127.0.0.1:1/x").is_err());
        assert!(e.check_url("http://[::1]/x").is_err());
        assert!(e.check_url("http://10.9.9.9/x").is_ok());
        assert!(e.check_url("https://registry.npmjs.org/x").is_ok());
        assert!(e.check_url("file:///etc/passwd").is_err());
        let open = Egress::allowing(&["localhost"]);
        assert!(!open.resolve("localhost", 80).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn git_is_pinned() {
        let e = Egress::allowing(&["localhost"]);
        let s = e
            .git_settings("https://localhost:3000/o/r.git")
            .await
            .unwrap();
        assert_eq!(s.len(), 1);
        assert!(
            s[0].starts_with("http.curloptResolve=localhost:3000:"),
            "{s:?}"
        );
        assert!(
            Egress::allowing(&[])
                .git_settings("https://localhost/o/r.git")
                .await
                .is_err()
        );
    }
}
