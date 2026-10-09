//! Serves the web UI. The files under `web/` are compiled into the binary, so
//! the container needs nothing else and the UI never loads anything from the
//! internet (no CDNs, fonts or analytics).

use axum::{
    Router,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};

use crate::AppState;

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const LOGO: &str = include_str!("../web/granary-logo.svg");
const ICON: &str = include_str!("../web/granary-icon.svg");
// Saira Condensed (SIL Open Font License, see web/fonts/OFL.txt), for headings.
const HEADING_FONT: &[u8] = include_bytes!("../web/fonts/saira-condensed-600.woff2");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                   img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; \
                   form-action 'self'; frame-ancestors 'none'";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route(
            "/ui/app.js",
            get(|| asset(APP_JS, "text/javascript; charset=utf-8")),
        )
        .route(
            "/ui/app.css",
            get(|| asset(APP_CSS, "text/css; charset=utf-8")),
        )
        .route("/ui/granary-logo.svg", get(|| asset(LOGO, "image/svg+xml")))
        .route("/ui/granary-icon.svg", get(|| asset(ICON, "image/svg+xml")))
        .route(
            "/ui/fonts/saira-condensed-600.woff2",
            get(|| asset(HEADING_FONT, "font/woff2")),
        )
}

async fn index() -> Response {
    let html = INDEX.replace("{{VERSION}}", env!("CARGO_PKG_VERSION"));
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

async fn asset(body: impl IntoResponse, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // index.html references these with ?v=<version>, so they can be cached.
            (header::CACHE_CONTROL, "public, max-age=86400"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}
