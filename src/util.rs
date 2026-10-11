use std::path::PathBuf;

use axum::http::HeaderMap;

use crate::{config::Config, error::AppError};

/// The base URL clients should use to reach this server. Used when rewriting
/// download links inside upstream metadata so clients come back to us.
pub fn base_url(cfg: &Config, headers: &HeaderMap) -> String {
    if let Some(url) = &cfg.public_url {
        return url.clone();
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let proto = header("x-forwarded-proto").unwrap_or_else(|| "http".into());
    let host = header("x-forwarded-host")
        .or_else(|| header("host"))
        .unwrap_or_else(|| "localhost:8080".into());
    format!("{proto}://{host}")
}

/// Validate a client-supplied relative path and turn it into a filesystem path
/// that cannot escape the cache directory.
pub fn safe_rel_path(rel: &str) -> Result<PathBuf, AppError> {
    if rel.is_empty() || rel.len() > 1024 {
        return Err(AppError::bad_request("invalid path"));
    }
    let mut out = PathBuf::new();
    for seg in rel.split('/') {
        let bad = seg.is_empty()
            || seg == "."
            || seg == ".."
            || seg.contains('\\')
            || seg.contains('\0')
            || seg.len() > 255;
        if bad {
            return Err(AppError::bad_request("invalid path"));
        }
        out.push(seg);
    }
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn rejects_traversal() {
        for bad in [
            "",
            "../etc/passwd",
            "a/../b",
            "a//b",
            "/abs",
            "a/./b",
            "a\\b",
            "a/",
        ] {
            assert!(safe_rel_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn accepts_normal_paths() {
        for good in [
            "3/s/syn",
            "@types/node/-/node-20.0.0.tgz",
            "packages/ab/cd/x-1.0.whl",
        ] {
            assert!(safe_rel_path(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn base_url_prefers_forwarded_headers() {
        let cfg = test_config();
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("internal:8080"));
        assert_eq!(base_url(&cfg, &h), "http://internal:8080");
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        h.insert("x-forwarded-host", HeaderValue::from_static("granary.lan"));
        assert_eq!(base_url(&cfg, &h), "https://granary.lan");
    }

    pub fn test_config() -> Config {
        Config {
            listen: String::new(),
            data_dir: "/tmp".into(),
            public_url: None,
            meta_ttl: std::time::Duration::from_secs(1),
            cargo_index: "https://index.crates.io".into(),
            cargo_dl: "https://static.crates.io/crates/{crate}/{version}/download".into(),
            npm_upstream: "https://registry.npmjs.org".into(),
            pypi_index: "https://pypi.org/simple".into(),
            pypi_files: "https://files.pythonhosted.org".into(),
            maven_upstream: "https://repo1.maven.org/maven2".into(),
            search: true,
            cargo_api: "https://crates.io/api/v1".into(),
            maven_search: "https://search.maven.org/solrsearch/select".into(),
            warmer_url: None,
            warmer_secret: None,
            warmer_granary_url: "http://granary:8080".into(),
            warm_jobs: 2,
            warm_timeout: std::time::Duration::from_secs(1800),
            warm_python: "3.12".into(),
            git_hosts: vec!["github.com".into()],
            git_max_size: 2 << 30,
            git_timeout: std::time::Duration::from_secs(600),
            git_upstream_override: None,
            admin_user: "admin".into(),
            admin_password: None,
            client_auth: true,
            cookie_secure: false,
            session_idle: std::time::Duration::from_secs(3600),
            trusted_proxies: Vec::new(),
            audit_days: 365,
            offline: false,
            npm_rewrite_tarballs: true,
            cache_limit: None,
        }
    }
}
