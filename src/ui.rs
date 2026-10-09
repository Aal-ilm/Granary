//! A small status page at `/` with client setup snippets and cache counters.

use std::sync::atomic::Ordering;

use axum::{extract::State, http::HeaderMap, response::Html};

use crate::{AppState, util::base_url};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn index(State(st): State<AppState>, headers: HeaderMap) -> Html<String> {
    let base = esc(&base_url(&st.cfg, &headers));
    let s = &st.store.stats;
    let n = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);

    Html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Granary</title>
<style>
:root {{ --bg:#f7f6f2; --fg:#1d1d1b; --muted:#6b6a65; --card:#fff; --line:#e3e1da; --accent:#8a5a14; }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg:#151513; --fg:#ecebe6; --muted:#9a998f; --card:#1f1f1c; --line:#33322d; --accent:#e0a84a; }}
}}
* {{ box-sizing:border-box; }}
body {{ margin:0; background:var(--bg); color:var(--fg); font:15px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif; }}
main {{ max-width:860px; margin:0 auto; padding:40px 16px 64px; }}
h1 {{ font-size:28px; margin:0 0 4px; letter-spacing:-.01em; }}
h1 span {{ color:var(--accent); }}
p.lead {{ color:var(--muted); margin:0 0 28px; }}
h2 {{ font-size:15px; text-transform:uppercase; letter-spacing:.06em; color:var(--muted); margin:32px 0 12px; }}
.stats {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr)); gap:10px; }}
.stat {{ background:var(--card); border:1px solid var(--line); border-radius:10px; padding:12px 14px; }}
.stat b {{ display:block; font-size:22px; font-variant-numeric:tabular-nums; }}
.stat small {{ color:var(--muted); }}
.card {{ background:var(--card); border:1px solid var(--line); border-radius:10px; padding:14px 16px; margin-bottom:12px; }}
.card h3 {{ margin:0 0 6px; font-size:16px; }}
.card p {{ margin:0 0 8px; color:var(--muted); }}
pre {{ margin:0; padding:10px 12px; overflow-x:auto; background:var(--bg); border-radius:8px; font:13px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace; }}
footer {{ margin-top:36px; color:var(--muted); font-size:13px; }}
</style></head>
<body><main>
<h1>Granary <span>●</span></h1>
<p class="lead">Caching proxy for package registries. Base URL: <code>{base}</code></p>

<h2>Cache activity since start</h2>
<div class="stats">
  <div class="stat"><b>{fh}</b><small>file hits</small></div>
  <div class="stat"><b>{fm}</b><small>file downloads</small></div>
  <div class="stat"><b>{mh}</b><small>metadata hits</small></div>
  <div class="stat"><b>{mr}</b><small>metadata refreshes</small></div>
  <div class="stat"><b>{ms}</b><small>stale served</small></div>
</div>

<h2>Client setup</h2>
<div class="card"><h3>Cargo</h3><p><code>~/.cargo/config.toml</code></p>
<pre>[source.crates-io]
replace-with = "granary"

[source.granary]
registry = "sparse+{base}/cargo/"</pre></div>

<div class="card"><h3>npm / pnpm / yarn</h3><p><code>~/.npmrc</code> or a project <code>.npmrc</code></p>
<pre>registry={base}/npm/</pre></div>

<div class="card"><h3>pip / uv</h3><p><code>pip.conf</code>, or <code>UV_INDEX_URL</code> for uv</p>
<pre>[global]
index-url = {base}/pypi/simple/</pre></div>

<footer>Granary {ver} · read-only proxy · <a href="/healthz">health</a></footer>
</main></body></html>"#,
        fh = n(&s.file_hits),
        fm = n(&s.file_misses),
        mh = n(&s.meta_hits),
        mr = n(&s.meta_refreshes),
        ms = n(&s.meta_stale_served),
        ver = env!("CARGO_PKG_VERSION"),
    ))
}
