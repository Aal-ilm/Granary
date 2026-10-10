# Spec: package search, cache warming, and templates

Goal: fill Granary's cache from the web UI, without running npm, cargo, pip
or mvn on a laptop. An admin can search for a package and cache it, or pick
a template (a named set of packages for a kind of project) and cache all of
it, including every transitive dependency. Granary ships default templates,
and admins can create, edit, clone and delete their own.

Do this after the Maven proxy is merged, so Maven is covered from the start.

## 1. How warming works

Do not write dependency resolvers in Granary. Resolving npm semver trees,
Cargo features or pip markers by hand is a large source of bugs. Instead,
run the real package tools in a separate worker container that points only
at Granary. The tools resolve the dependencies. Granary caches everything
they download as a side effect, through the normal proxy paths.

- New service `granary-warmer` in compose. It uses its own image with
  cargo, node/npm, python/pip, and maven installed.
- The warmer has no route to the internet. Put it on an internal Docker
  network that can reach only the Granary container. All downloads go
  through Granary.
- The warmer authenticates with a dedicated access token. Granary creates
  it automatically, marks it as a system token, and shows it in the tokens
  list as "warmer". Its only permission is downloading.
- Granary sends jobs to the warmer over the internal network. A small HTTP
  API on the warmer is fine. Protect it with a shared secret from an env
  var, and have it accept connections only from Granary.
- Each job runs in a fresh temp directory, deleted afterwards.

What runs for each ecosystem:

| Ecosystem | Steps |
|---|---|
| Cargo | write a Cargo.toml with the template's dependencies, then `cargo generate-lockfile` and `cargo fetch` |
| npm | write a package.json, then `npm install --ignore-scripts --no-audit --no-fund` |
| PyPI | `pip download --only-binary=:all: -d out -r requirements.txt`, once for each target platform |
| Maven | write a pom.xml, then `mvn -B dependency:go-offline` |

- Never run install scripts or build steps. Use npm `--ignore-scripts`, and
  binary-only pip by default. Allow sdists only through a per-template
  setting, with a warning in the UI that building an sdist can run code.
- Platform targets. A template lists the platforms to warm for, defaulting
  to linux x86_64 plus macOS arm64.
  - For pip, pass `--platform`, `--python-version`, and
    `--implementation` for each target.
  - For npm, run the install once per target with `--os` and `--cpu`, so
    platform packages get cached (esbuild, swc, sharp, and so on).
  - For Cargo, `cargo fetch` already downloads every target's dependencies.

## 2. Search

Add a "Get packages" page, admin only. Pick an ecosystem, then search:

| Ecosystem | Search source |
|---|---|
| npm | `GET {npm upstream}/-/v1/search?text=...` |
| Cargo | `https://crates.io/api/v1/crates?q=...`. This is a new upstream host: add the setting `GRANARY_CARGO_API`, and say in the README that search sends the query text to crates.io. |
| PyPI | No search API exists. Use exact name lookup only, via `GET {pypi index}/{name}/` with the JSON Accept header. Say so in the UI. |
| Maven | `https://search.maven.org/solrsearch/select?q=...`, with the new setting `GRANARY_MAVEN_SEARCH`. Same README note as Cargo. |

- Results show name, latest version, description, and whether the package
  is already cached.
- Each result has a "Cache" button. It opens a version picker (latest by
  default) and a "with dependencies" checkbox, checked by default. With
  dependencies, it runs a one-package warm job. Without, Granary fetches
  that single file directly.
- Rate limit search to 1 request per second per user, and cache search
  results for 10 minutes.
- New setting `GRANARY_SEARCH=on|off`, default on. When off, or when
  offline mode is on, the page offers exact name lookup only. Lookups that
  are already cached still work.

## 3. Templates

Store templates in SQLite. A template has:
- name
- description
- tags
- built-in flag
- target platforms
- an allow-sdist flag
- one section per ecosystem, holding dependency lines in that ecosystem's
  own syntax:
  - Cargo: `serde = { version = "1", features = ["derive"] }`
  - npm: `"react": "^19"`
  - PyPI: `fastapi>=0.115`
  - Maven: `org.springframework.boot:spring-boot-starter-web:3.4.0`

Built-in templates:
- Ship as TOML files in `templates/` in the repo, loaded at startup.
- They are read-only in the UI. "Clone" makes an editable copy.
- Updating Granary updates the built-ins. It never touches custom
  templates.

Template editor:
- One text area per ecosystem with syntax checking. Validate a line with
  the real tool when you can, or a simple parser otherwise.
- "Import from manifest": upload or paste a Cargo.toml, package.json,
  requirements.txt, or pom.xml, and fill the template from its dependency
  sections.

Templates page:
- A card grid with search and tag filter.
- Each card shows the package count per ecosystem, and when it was last
  warmed.
- Buttons: Warm, Edit (custom only), Clone, Delete (custom only), Export
  (TOML), and Download starter.

**Download starter.** Returns a zip with a minimal project for that
template. It contains:
- the manifest files
- a lockfile from the last successful warm, if there is one
- client config pointing at this Granary: `.cargo/config.toml` with the
  source replacement, a project `.npmrc` with the registry line, and a
  `pip.conf` snippet

Never put a token in the zip. Add a README.txt that says to run `make
use-granary` or set the token.

Ship these default templates. Use only well known packages and leave
versions unpinned, at latest compatible:

- **Rust CLI**: clap (derive), anyhow, thiserror, serde (derive),
  serde_json, tracing, tracing-subscriber
- **Rust async service**: tokio (full), axum, tower-http, reqwest
  (rustls-tls), sqlx (sqlite, runtime-tokio), serde, tracing
- **Rust embedded (Cortex-M)**: cortex-m, cortex-m-rt, embedded-hal,
  defmt, defmt-rtt, panic-probe, heapless, embassy-executor, embassy-time
- **Node API**: fastify, zod, pino, typescript, tsx, vitest
- **React + Vite + TS**: react, react-dom, vite, @vitejs/plugin-react,
  typescript, eslint, prettier, vitest
- **Python web API**: fastapi, uvicorn, pydantic, httpx, pytest, ruff
- **Python data**: numpy, pandas, matplotlib, jupyterlab, scipy
- **Java Spring web**: spring-boot-starter-web, spring-boot-starter-test

## 4. Jobs

- Warm jobs go in a queue table (SQLite). Run at most 2 at once; make that
  a setting.
- A job page shows:
  - status (queued, running, done, failed, cancelled)
  - live log lines, streamed with server-sent events or polled every 2
    seconds
  - how many packages and bytes were newly cached
  - a cancel button
- Keep the last 200 jobs, and prune older ones.
- Limits:
  - Per-job timeout of 30 minutes.
  - If a job would push the cache past the cache budget
    (GRANARY_CACHE_LIMIT, when set), stop it with a clear message.
- Optional per-template schedule (off, daily, weekly) to re-warm, so new
  versions get cached before the network is cut off.

## 5. Permissions and security

- Search, warm, and editing templates are admin only. Viewers can see
  templates and job history.
- All new POST/PUT/DELETE routes use the existing CSRF header check.
- Keep the strict CSP: no inline scripts or styles on the new pages.
- Every job, template change, and search goes to the audit log, if one
  exists by then.
- The warmer image builds from pinned base images. Running the warmer at
  all is optional: when it isn't running, the UI hides "with
  dependencies" and template warming, but single-file caching still
  works.

## 6. Tests

- Template TOML loading and validation, including bad lines in each
  ecosystem's syntax.
- Manifest import for all four manifest types.
- Starter zip contents, and a check that no token is ever in it.
- Job queue: concurrency limit, cancel, timeout, and the budget stop.
- End to end, with a mock upstream: warm a small template and assert the
  files are in the cache, with Granary's network as the only path.
- Offline mode and `GRANARY_SEARCH=off` both block outbound search.

## 7. Docs

- README sections for search, templates, the warmer service, and the
  privacy note on search hosts.
- No em dashes in copy, comments, or commit messages.
