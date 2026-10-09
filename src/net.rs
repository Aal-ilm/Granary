//! Request-edge middleware: works out the real client address (believing
//! `X-Forwarded-*` only from trusted reverse proxies) and adds security
//! headers to every response.

use std::net::{IpAddr, SocketAddr};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

use crate::AppState;

/// The client's IP address, as seen through any trusted proxies.
#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

const FORWARDED: [&str; 4] = [
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "forwarded",
];

/// Strip forwarding headers from untrusted peers and record the client IP.
pub async fn client_ip(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let trusted = |ip: IpAddr| st.cfg.trusted_proxies.iter().any(|n| n.contains(ip));

    let ip = if trusted(peer) {
        // Walk X-Forwarded-For from the right, skipping our own proxies; the
        // first address that isn't one of them is the client.
        let chain: Vec<IpAddr> = req
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        chain
            .iter()
            .rev()
            .copied()
            .find(|ip| !trusted(*ip))
            .or(chain.first().copied())
            .unwrap_or(peer)
    } else {
        for h in FORWARDED {
            req.headers_mut().remove(h);
        }
        peer
    };
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

/// Defensive headers on every response. Package files and upstream metadata
/// (which can include HTML from PyPI) are served under a sandboxing CSP so
/// nothing from an upstream registry can run script on Granary's origin.
pub async fn security_headers(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let mut set = |name: header::HeaderName, value: &'static str| {
        h.entry(name).or_insert(HeaderValue::from_static(value));
    };
    set(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(header::X_FRAME_OPTIONS, "DENY");
    set(header::REFERRER_POLICY, "no-referrer");
    set(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; sandbox; frame-ancestors 'none'",
    );
    set(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        "same-origin",
    );
    set(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        "same-origin",
    );
    if path.starts_with("/api/") || path == "/metrics" {
        set(header::CACHE_CONTROL, "no-store");
    }
    if st.cfg.cookie_secure {
        set(header::STRICT_TRANSPORT_SECURITY, "max-age=31536000");
    }
    resp
}
