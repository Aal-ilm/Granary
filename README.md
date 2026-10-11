# Granary

A small, self-hostable caching proxy for package registries. Point Cargo, npm,
pip, Maven and Gradle at Granary, and every package is downloaded from the
public registry once, then served from your own disk after that.

- **Free and open source** (MIT or Apache-2.0), with no usage caps, no license
  keys and no telemetry. The only outbound connections Granary makes are to
  the upstream registries you configure.
- **One container**, one data volume, configured entirely with environment
  variables.
- **Keeps working when upstream is down.** Cached metadata is served stale
  if the public registry can't be reached, so builds of known dependencies
  don't break.
- **Safe by design.** Granary checks every download against the
  registry's own checksum (Cargo's `cksum`, npm's `integrity`, PyPI's
  `sha256`, Maven Central's `.sha1`) before caching it, so a corrupted or tampered response is never
  stored or served. Clients check again on their side.
- **Web UI built in.** Browse and search what's cached, delete packages, set
  cleanup rules, watch tasks, check upstream health and read logs. It's served
  by the same binary and never loads anything from the internet.
- **Fill the cache ahead of time.** Search a registry and cache a version,
  or keep templates of the packages each kind of project needs and warm
  them, transitive dependencies and all, before the network goes away.
- **Accounts built in.** Sign-in with admin and viewer roles, two-factor
  codes, and per-machine access tokens for Cargo, npm, pip, Maven and Gradle. Built on the
  open-source axum-login, tower-sessions, password-auth (Argon2id) and
  totp-rs libraries, stored in a local SQLite file. No external identity
  service is needed.

![Granary overview page](docs/overview.png)

| Ecosystem | Status | Client path |
|---|---|---|
| Cargo (crates.io, sparse protocol) | ✅ proxy | `/cargo/` |
| npm (registry.npmjs.org) | ✅ proxy | `/npm/` |
| PyPI (Simple API, PEP 503/691) | ✅ proxy | `/pypi/simple/` |
| Maven Central (Maven 2 layout, for Maven and Gradle) | ✅ proxy | `/maven` |
| Git repos from GitHub, GitLab and Codeberg (smart HTTP, read-only) | ✅ cache | `/git/{host}/{owner}/{repo}.git` |
| Docker / OCI | planned | |
| Debian (apt) | planned | |
| Publishing your own packages | planned | |

## Quick start

```sh
docker compose up -d --build
docker compose logs granary            # shows the first admin password once
docker compose port granary 8080       # shows the port, e.g. 0.0.0.0:32768
```

Open that address in a browser and sign in as `admin` with the password from
the log. Granary asks you to choose your own password straight away.

The automatic port can change when the container is recreated or the host
reboots, and every client config contains the URL. Once Granary is set up the
way you like, pin the port it was given by changing the `ports:` entry in
`docker-compose.yml` from `"8080"` to `"32768:8080"` (using your port).

Without compose:

```sh
docker build -t granary .
docker run -d --name granary -p 8080:8080 -v granary-data:/data granary
```

Or run it directly with a recent stable Rust toolchain:

```sh
cargo run --release      # caches into ./data, listens on 0.0.0.0:8080
```

## Client setup

Package downloads need an access token. In the web UI, open **Access
tokens**, create one per machine, and Granary shows the token once along with
ready-to-paste configuration that includes it. By hand, with `HOST` as your
Granary address and `TOKEN` as the token:

**Cargo** (`~/.cargo/config.toml`), then run `cargo login --registry granary`
and paste the token:

```toml
[registries.granary]
index = "sparse+http://HOST/cargo/"
credential-provider = "cargo:token"

[source.crates-io]
replace-with = "granary"
```

**npm / pnpm / yarn** (`~/.npmrc` or a project `.npmrc`):

```ini
registry=http://HOST/npm/
//HOST/npm/:_authToken=TOKEN
```

**pip** (`pip.conf`) or **uv** (`UV_INDEX_URL`):

```ini
[global]
index-url = http://__token__:TOKEN@HOST/pypi/simple/
```

**Maven** (`~/.m2/settings.xml`, inside `<settings>`). Maven only sends the
`<server>` credentials after Granary answers 401, which it does when a token
is required. The username can be anything; the password is the token.

```xml
<mirrors>
  <mirror>
    <id>granary</id>
    <mirrorOf>central</mirrorOf>
    <url>http://HOST/maven</url>
  </mirror>
</mirrors>
<servers>
  <server>
    <id>granary</id>
    <username>token</username>
    <password>TOKEN</password>
  </server>
</servers>
```

**Gradle** (`~/.gradle/init.d/granary.gradle`). This init script sends every
`mavenCentral()` repository to Granary, including plugin management and
`dependencyResolutionManagement` in settings files. Leave out
`allowInsecureProtocol` when Granary is served over HTTPS.

```groovy
def granary = { RepositoryHandler repos ->
    repos.configureEach { repo ->
        if (repo instanceof MavenArtifactRepository && repo.url.toString().startsWith('https://repo.maven.apache.org/maven2')) {
            repo.url = 'http://HOST/maven'
            repo.allowInsecureProtocol = true
            repo.credentials {
                username = 'token'
                password = 'TOKEN'
            }
        }
    }
}
beforeSettings { settings ->
    granary(settings.pluginManagement.repositories)
    granary(settings.dependencyResolutionManagement.repositories)
}
allprojects {
    granary(buildscript.repositories)
    granary(repositories)
}
```

Maven Central has no snapshots, so any `-SNAPSHOT` path gets a 404 straight
away. Keep snapshot repositories out of the `central` mirror.

To let anyone on your network download without a token, set
`GRANARY_CLIENT_AUTH=off`; the configuration then needs only the URLs.

### Setting up a machine with make

`make use-granary GRANARY_URL=http://HOST` asks for a token once and points
npm, pip, Cargo, Maven, Gradle and git at Granary. `make use-public` switches them
back to the public registries (for example while Granary is down), `make
status` shows where each one points and `make remove` undoes everything.
Every file is backed up before its first change. Choose tools with `TOOLS`,
for example `make use-granary TOOLS=maven`; `maven` covers both Maven and
Gradle.

For Maven the script adds the `<mirror>` and `<server>` above inside marked
comment blocks in `~/.m2/settings.xml` (creating the file if needed), using
`scripts/m2-settings.py`, which edits the XML rather than raw lines and needs
`python3`. It refuses to run if you already have your own mirror for Central.
For Gradle it writes `~/.gradle/init.d/granary.gradle`; `use-public` and
`remove` delete it again. For git, see [Git repos](#git-repos).

## Web UI

| Page | What it's for |
|---|---|
| Overview | Disk use per repository, cache hit rate, upstream status, recently cached packages |
| Repositories | Each proxy's URL, upstream, size and status, client setup snippets, and **Invalidate cache** to force fresh metadata |
| Browse | Every cached package, filterable and sortable by size, versions, cache date and last download; the sidebar search covers all repositories. The Git tab lists cached repos with Add repo, Refresh and Delete |
| Package detail | Each cached version and file, with download and delete |
| Get packages | Search a registry and cache a version before anyone needs it (admins only; see [Getting packages ahead of time](#getting-packages-ahead-of-time)) |
| Templates | Named sets of packages for a kind of project: browse, create (also from a git repo), edit, clone, delete, export, warm, schedule re-warms and download a starter project (see [Templates](#templates)) |
| Jobs | Warm jobs with their status, live log, what they cached, and cancel (see [Warming](#warming-templates-with-the-warmer)) |
| Cleanup | Delete packages nobody has downloaded in *N* days, per repository, with a dry-run preview and an optional daily schedule |
| Tasks | Scheduled jobs (cleanup, upstream checks, saving download history) with last run, result and **Run now** |
| System | Upstream reachability and latency, disk space, and the active configuration |
| Logs | Recent server events, filterable by level and text (admins only) |
| Audit log | Sign-ins, account, token and user changes, deletions and settings changes, with who, when and from where; filterable and exportable as CSV (admins only) |
| Access tokens | Create and revoke tokens for package managers, with ready-made client configuration |
| Users | Add people as admins or viewers, reset passwords, turn off two-factor, disable or delete accounts (admins only) |
| Account | Change your password and set up two-factor authentication |

## Git repos

Granary can keep a cached mirror of public git repos, so clones keep working
when GitHub is slow, down or out of reach. Clone through Granary with:

```sh
git clone http://HOST/git/github.com/serde-rs/serde.git
```

The first request for a repo clones it from `https://github.com/serde-rs/serde.git`
(`git clone --mirror`) and serves it; later requests come from the cache.
Repos are served read-only over git's smart HTTP protocol, so shallow clones
(`--depth`) and partial clones (`--filter`) work; pushes are refused. A cached
repo is refreshed (`git remote update --prune`) when a client fetches it and
it is older than `GRANARY_META_TTL_SECS`; if upstream can't be reached, the
cached copy is served. Concurrent first requests for a repo cause one clone.
In offline mode, only repos already cached are served.

Admins can also add a repo ahead of time on **Browse → Git**: paste its URL
and Granary clones it as a job, with git's progress on the job page. The same
tab shows each repo's size, default branch and last refresh, with Refresh and
Delete. Cached repos have their own gauge on the Overview and count toward
`GRANARY_CACHE_LIMIT`.

**Allowed hosts.** Granary only clones from the hosts in `GRANARY_GIT_HOSTS`
(default `github.com,gitlab.com,codeberg.org`), always over HTTPS. IP
addresses and `localhost` can't be listed, owner and repo names may use only
letters, digits, `.`, `-` and `_`, and git never follows redirects, so a
repo can't send Granary somewhere else. Hooks are off and git never prompts
for credentials. A clone or refresh that grows past `GRANARY_GIT_MAX_SIZE`
(2GB) or runs longer than `GRANARY_GIT_TIMEOUT_MINS` (10) is stopped, the
partial copy is deleted, and the log says why.

**Client setup.** `make use-granary TOOLS=git` writes
`~/.config/granary-client/gitconfig` (mode 600) with an `insteadOf` rule per
allowed host and an `http.extraHeader` line carrying your token, and adds a
single `include.path` line for it to your global git config, so the token
never sits in `~/.gitconfig`. After that, plain URLs such as
`git clone https://github.com/serde-rs/serde` go through Granary, and so do
Cargo git dependencies and CMake `FetchContent`. Cargo's own git client
can't send that header, so the script also sets `net.git-fetch-with-cli =
true` in Cargo's config (in a marked block; it only warns if you already
have a `[net]` section). `make use-public` removes the include line and
`make remove` deletes the file as well. By hand, the equivalent is:

```sh
git config --global url."http://HOST/git/github.com/".insteadOf "https://github.com/"
git config --global http."http://HOST/".extraHeader "Authorization: Basic $(printf 'token:%s' TOKEN | base64)"
```

**Limits.** Submodules and Git LFS objects are not fetched, only public
repos can be cached (Granary sends no credentials upstream), and repos must
live at `host/owner/repo` (GitLab subgroups aren't supported yet).

## Getting packages ahead of time

Admins can fill the cache from the web UI, without running npm, cargo, pip
or mvn anywhere. Open **Get packages**, pick an ecosystem and search:

| Ecosystem | Where the search goes |
|---|---|
| npm | The npm registry's search (`GRANARY_NPM_UPSTREAM/-/v1/search`) |
| Cargo | crates.io's web API (`GRANARY_CARGO_API`) |
| PyPI | Nowhere: PyPI has no search API, so type the exact project name |
| Maven | search.maven.org (`GRANARY_MAVEN_SEARCH`) |

Each result shows the latest version, a description where the registry has
one, and how many versions Granary already holds. **Cache** asks for a
version (latest by default) and fetches that one version straight away: the
`.crate`, the npm tarball, the PyPI file you pick (a pure-Python wheel is
suggested) or a Maven POM with its jar, aar or war. Every file is checked
against the registry's checksum, exactly as for client downloads. Its
dependencies are fetched when a client first asks for them, unless you tick
**With dependencies** (shown when the [warmer](#warming-templates-with-the-warmer)
is running), which starts a warm job for that one version.

**Privacy note.** Search sends the text you type to crates.io and
search.maven.org, which are not otherwise contacted, and to the npm
registry. Exact-name lookups use the same registry metadata clients already
fetch. To keep search text inside your network, set `GRANARY_SEARCH=off`:
the page then looks packages up by exact name only. In offline mode nothing
is sent anywhere, and only packages already in the cache can be looked up.

Search is limited to one request per second per admin when it reaches a
search host; results are reused for ten minutes. Every search, lookup and
cached version is recorded in the audit log.

## Templates

A template is a named set of packages for a kind of project, listed one
per line in each ecosystem's own syntax:

| Ecosystem | Example line |
|---|---|
| Cargo | `serde = { version = "1", features = ["derive"] }` |
| npm | `"react": "^19"` |
| PyPI | `fastapi>=0.115` |
| Maven | `org.springframework.boot:spring-boot-starter-web:3.4.0` (version optional) |

Only registry packages can be cached, so git, path, file and URL
dependencies are refused, as are pip options such as `-r` and Maven
snapshots. Lines starting with `#` are comments. A template also lists the
platforms to cache for (Linux x86-64 and macOS Apple silicon by default) and
whether Python source distributions are allowed; building an sdist runs the
package's own code, so templates use wheels only unless you allow them.

Granary ships these built-in templates, with versions left open so the
newest compatible release is used:

| Template | Packages |
|---|---|
| Rust CLI | clap, anyhow, thiserror, serde, serde_json, tracing, tracing-subscriber |
| Rust async service | tokio, axum, tower-http, reqwest, sqlx, serde, tracing |
| Rust embedded (Cortex-M) | cortex-m, cortex-m-rt, embedded-hal, defmt, defmt-rtt, panic-probe, heapless, embassy-executor, embassy-time |
| Node API | fastify, zod, pino, typescript, tsx, vitest |
| React + Vite + TS | react, react-dom, vite, @vitejs/plugin-react, typescript, eslint, prettier, vitest |
| Python web API | fastapi, uvicorn, pydantic, httpx, pytest, ruff |
| Python data | numpy, pandas, matplotlib, jupyterlab, scipy |
| Java Spring web | spring-boot-starter-web, spring-boot-starter-test |

Built-in templates live as TOML files in `templates/` and are compiled into
Granary. They are read-only in the web UI: **Clone** makes an editable copy.
Upgrading Granary updates the built-ins and never touches your own
templates, which are stored in `state/granary.db`.

The template editor checks every line as you type and names the line and
the problem. **Import from manifest** fills a section from a `Cargo.toml`,
`package.json`, `requirements.txt` or `pom.xml` (uploaded or pasted), and
lists anything it had to leave out, such as path dependencies. **Export**
downloads a template as a TOML file in the same format as the built-ins.

Viewers can see templates and export them; creating, editing, cloning,
deleting and warming them needs an admin. Every change is recorded in the
audit log.

### Templates from repos

**New from repo** on the Templates page builds a template from a git repo:
paste its URL (on an [allowed host](#git-repos)) and, optionally, a branch,
tag or commit. If Granary doesn't have the repo yet it caches it first, with
the clone's progress shown on the page. It then pins the ref to a commit and
reads these files there, from the root and up to two levels down:
`Cargo.toml`, `Cargo.lock`, `package.json`, `package-lock.json`,
`requirements*.txt`, `pyproject.toml`, `pom.xml` and `build.gradle(.kts)`.

- Cargo and npm workspace members are included wherever they are.
  Dependencies that are part of the repo (other workspace members, path
  dependencies, Maven modules) are left out; `workspace = true`
  dependencies take their version and features from the workspace.
- `pyproject.toml`: PEP 621 dependencies and optional dependencies, and
  Poetry's dependency tables (`^` and `~` ranges are translated for pip).
- Gradle: string coordinates such as `implementation("g:a:v")`. Version
  catalogs, variables and map notation are listed as skipped.
- Git dependencies (Cargo `git = ...`, npm `github:` or git URLs, pip
  `name @ git+https://...`) on an allowed host are cached through the git
  cache when the template is saved.

A preview shows the manifests and lockfiles found, how many packages each
ecosystem gets, the git dependencies and everything left out, with the
reason, before anything is saved. The template is an ordinary custom
template that also records the repo, the ref and the commit.

**Warming a repo template** with lockfiles installs exactly the locked
versions: the warmer checks the repo out at the recorded commit through
Granary's git cache (never from upstream) and runs `cargo fetch --locked`
or `npm ci --ignore-scripts` (once per platform) in each lockfile's folder.
Tool settings in the repo (`.cargo/config.toml`, `.npmrc`,
`rust-toolchain.toml`) are removed from the checkout first, so they can't
send downloads elsewhere. A lockfile that no longer matches its manifest is
reported in the log and installed without it. Ecosystems without a lockfile
are warmed from the template's lines as usual.

**Resync from repo** on a repo template reads the repo again (a branch
moves to its newest commit) and, after the same preview, replaces the
template's package lines with what the repo lists now. Lines edited by hand
are replaced too; the name, tags, platforms and schedule stay.

### Download starter

**Download starter** gives a zip with a minimal project for a template, set
up to install through your Granary:

- the manifests (`Cargo.toml`, `package.json`, `requirements.txt`,
  `pom.xml`), for the ecosystems the template uses
- the lockfiles from the template's last successful warm, if it has one:
  `Cargo.lock`, `package-lock.json` (with URLs pointing at
  registry.npmjs.org, which npm maps onto whatever registry is configured),
  `requirements.lock.txt` (the versions that were cached) and, for Maven,
  the `pom.xml` that was warmed, with every version filled in
- client configuration for this Granary: `.cargo/config.toml` with the
  crates.io source replacement, a project `.npmrc` with the registry line,
  a `pip.conf` snippet, and `.mvn/settings.xml` with `.mvn/maven.config`
- `README.txt`

The zip never contains a token. `.npmrc` and `.mvn/settings.xml` read it
from the `GRANARY_TOKEN` environment variable, and README.txt explains how
to set it, or to run `make use-granary`, for Cargo and pip. Anyone signed in
can download a starter.

## Warming templates with the warmer

**Warm** fills the cache with everything a template needs, including every
transitive dependency, for each of its platforms. Granary doesn't resolve
dependencies itself; it runs the real tools in a separate container,
`granary-warmer`, which can reach nothing but Granary. Everything the tools
download goes through Granary's normal proxy paths, so it is checked and
cached on the way.

| Ecosystem | What the warmer runs |
|---|---|
| Cargo | `cargo generate-lockfile` and `cargo fetch` on a generated Cargo.toml (this covers every target) |
| npm | `npm install --ignore-scripts --no-audit --no-fund` on a generated package.json, once per platform with `--os` and `--cpu`, so platform packages (esbuild, swc, sharp...) are cached too |
| PyPI | `pip download --only-binary=:all:` once per platform, with that platform's wheel tags, `--python-version` (`GRANARY_WARM_PYTHON`) and `--implementation cp` |
| Maven | `mvn dependency:go-offline` on a generated pom.xml; Maven lines without a version use the latest stable release |

For [templates made from a repo](#templates-from-repos) with lockfiles,
the warmer checks the repo out through Granary's git cache and installs the
locked versions instead (`cargo fetch --locked`, `npm ci`). Git
dependencies go through Granary's git cache too.

Install scripts and build steps never run: npm gets `--ignore-scripts` and
pip downloads wheels only, unless the template allows source distributions.
Each job runs in a fresh directory with empty tool caches, deleted
afterwards.

**Turning it on.** The warmer is optional. Put a shared secret in `.env`
next to `docker-compose.yml`, then start it with its profile:

```sh
echo "GRANARY_WARMER_SECRET=$(openssl rand -hex 24)" >> .env
docker compose --profile warmer up -d --build
```

`docker-compose.yml` already points Granary at it (`GRANARY_WARMER_URL`)
and puts both on an internal Docker network with no route to the internet,
so the warmer can only download through Granary. When the warmer isn't
running, the web UI hides **Warm** and **With dependencies**, and
single-file caching still works.

**Security.** The warmer accepts jobs only with the shared secret and only
from the Granary container (`WARMER_ALLOW_FROM`). It downloads with an access
token Granary creates for it, listed under **Access tokens** as `warmer` with
a System badge. That token can only download packages (it is refused for
`/metrics`), is never stored in plain text, and is made again automatically
if you revoke it. Tokens are masked in job logs.

**Scheduled re-warms.** On a template's page, **Re-warm automatically** can
be set to daily or weekly, so new versions are cached before the network is
cut off. Granary checks every minute and queues a template when that long
has passed since it was last warmed (or last queued, so a template that
keeps failing isn't retried every minute). A template that was never warmed
is queued straight away. Schedules wait while the warmer is unavailable or
offline mode is on, and pick up when it's back. The schedule belongs to your
Granary, not to the template's content: built-in templates can be scheduled,
and exports don't include it. Scheduled jobs show `schedule` as who queued
them.

**Jobs.** Warm jobs wait in a queue and at most `GRANARY_WARM_JOBS` (2) run
at once. A job's page streams the tools' output and, when it ends, shows how
many packages, files and bytes were newly cached. A job can be cancelled; it
stops after `GRANARY_WARM_TIMEOUT_MINS` (30); and when `GRANARY_CACHE_LIMIT`
is set it stops as soon as the cache reaches the budget, saying so. The last
200 jobs are kept. Starting and cancelling jobs needs an admin and is
recorded in the audit log; viewers can see the job history.

## Sign-in and security

- **Roles.** Admins can delete packages, change cleanup rules, run tasks,
  invalidate caches and manage users. Viewers can browse and create their own
  access tokens. The server enforces this; hidden buttons are a convenience.
- **First admin.** On first start Granary creates `admin` with a random
  password, printed once to the container log (never to the in-app log view).
  Set `GRANARY_ADMIN_PASSWORD` to choose it instead. New users and resets get a
  temporary password that must be changed at first sign-in.
- **Passwords** are hashed with Argon2id and must be at least 12 characters.
  Changing a password signs out the account's other sessions.
- **Two-factor authentication** uses standard 6-digit codes (TOTP) from any
  authenticator app. A code can't be used twice.
- **No username probing.** Wrong usernames and wrong passwords get the
  same response and take the same time.
- **Sessions** are HttpOnly, `SameSite=Strict` cookies that expire after
  `GRANARY_SESSION_HOURS` of inactivity, stored in the local database so a
  restart doesn't sign anyone out. They are marked `Secure` when
  `GRANARY_PUBLIC_URL` is `https://`. Every change through the API also needs
  an `X-Granary-Request` header, so other websites can't trigger actions from
  your browser.
- **Access tokens** are only shown when created. Granary stores a SHA-256
  hash, so a copy of the database can't be used to download packages.
  Session IDs are stored the same way. Disabling an account stops its
  tokens too.
- **Sign-in limits.** Five wrong passwords or codes from one address lock
  that address out of that account for 15 minutes, without locking out the
  account's owner elsewhere. Fifty failures against one account from
  anywhere, or from one address against any accounts, also lock for 15
  minutes. Re-entering your password to change it or turn off two-factor
  counts the same way.
- **Audit log.** Security-relevant actions are recorded in the database
  with the user, time, target and client address, and kept for
  `GRANARY_AUDIT_DAYS`. Admins can search and export it.
- **Upstream content is sandboxed.** Every response carries `nosniff`,
  `X-Frame-Options: DENY` and a restrictive Content Security Policy, and
  package files and registry pages (PyPI's are HTML) are served with
  `sandbox`, so nothing fetched from a registry can run script on
  Granary's origin.
- **No credential relaying.** Of npm's `/-/` API, Granary forwards only
  audit, search and ping. Login, token, publish and account requests are
  refused, and clients' credentials are never forwarded upstream.
- **Locked out?** Reset a password from the server, optionally turning off
  two-factor:

  ```sh
  docker compose exec granary granary reset-password admin --disable-2fa
  ```

- **Monitoring.** `/metrics` serves Prometheus metrics: cache hits and
  misses, rejected downloads, storage per repository, disk space, upstream
  health and task results. Scrapers authenticate with an access token
  (`authorization: Bearer gran_…`); the System page has a ready-made scrape
  config.

Granary never contacts anything except the upstream registries it caches.
There are no update checks, analytics, fonts or avatars from outside services.

![System page in dark mode](docs/system-dark.png)

## Running on TrueNAS SCALE

See [docs/truenas.md](docs/truenas.md) for a step-by-step guide: datasets
and permissions, building the image, installing it as a custom app from
[`deploy/truenas/compose.yaml`](deploy/truenas/compose.yaml), HTTPS through a
reverse proxy, monitoring, upgrades and recovery.

Both compose files run Granary as an unprivileged user with a read-only
root filesystem, no Linux capabilities and `no-new-privileges`.

The image is built on [Wolfi](https://github.com/wolfi-dev), a minimal
glibc base whose packages follow upstream security fixes closely, with only
git (for the git cache) and CA certificates added. SQLite is compiled into
the binary. CI scans the image with Trivy and the dependencies with
`cargo audit`. The base image isn't pinned, and CI rebuilds and republishes
the latest release's image every week, so it picks up security fixes even
when Granary's code hasn't changed.

## Configuration

All settings are environment variables.

| Variable | Default | Purpose |
|---|---|---|
| `GRANARY_LISTEN` | `0.0.0.0:8080` | Address and port to listen on |
| `GRANARY_DATA_DIR` | `/data` in Docker, `./data` otherwise | Where the cache lives |
| `GRANARY_PUBLIC_URL` | *(from request headers)* | Fixed external URL, e.g. `https://granary.lan`. Set it when clients reach Granary through a reverse proxy that doesn't send `X-Forwarded-Host` / `X-Forwarded-Proto` |
| `GRANARY_META_TTL_SECS` | `300` | How long index/metadata responses are fresh before revalidation |
| `GRANARY_CACHE_LIMIT` | *(none)* | Cache budget such as `500MB`, `20GB` or `1.5TB` (KB, MB, GB and TB are powers of 1024). The Overview gauges fill against it, or against the disk size when unset. It is a budget, not a hard cap: client downloads are never refused because of it, but warm jobs stop when the cache reaches it |
| `GRANARY_OFFLINE` | `off` | `on`: never contact an upstream registry; serve only what is cached (see [Offline mode](#offline-mode)) |
| `GRANARY_CLIENT_AUTH` | `required` | `required`: package downloads need an access token or a signed-in session. `off`: anyone who can reach Granary can download |
| `GRANARY_ADMIN_USER` | `admin` | Username of the first admin, created when there are no users |
| `GRANARY_ADMIN_PASSWORD` | *(generated)* | That admin's password (12+ characters). Only used on first start |
| `GRANARY_SESSION_HOURS` | `12` | Sign people out after this many hours of inactivity |
| `GRANARY_COOKIE_SECURE` | `true` if the public URL is HTTPS | Send the session cookie over HTTPS only (also turns on `Strict-Transport-Security`) |
| `GRANARY_TRUSTED_PROXIES` | *(none)* | Comma-separated IPs or CIDR ranges of your reverse proxies, e.g. `172.16.0.0/12`. Granary believes `X-Forwarded-*` headers only from these, and uses `X-Forwarded-For` for sign-in limits and the audit log. From anywhere else, those headers are removed |
| `GRANARY_AUDIT_DAYS` | `365` | Keep audit log entries for this many days |
| `GRANARY_CARGO_INDEX` | `https://index.crates.io` | Upstream sparse index |
| `GRANARY_CARGO_DL` | `https://static.crates.io/crates/{crate}/{version}/download` | Upstream crate download template |
| `GRANARY_NPM_UPSTREAM` | `https://registry.npmjs.org` | Upstream npm registry |
| `GRANARY_PYPI_INDEX` | `https://pypi.org/simple` | Upstream Simple API |
| `GRANARY_PYPI_FILES` | `https://files.pythonhosted.org` | Host that PyPI file links point to |
| `GRANARY_MAVEN_UPSTREAM` | `https://repo1.maven.org/maven2` | Upstream Maven repository (Maven 2 layout) |
| `GRANARY_WARMER_URL` | *(none)* | The warmer service, e.g. `http://granary-warmer:9090`. Warming is off when unset |
| `GRANARY_WARMER_SECRET` | *(none)* | Shared secret for the warmer (16+ characters); give the warmer the same value as `WARMER_SECRET` |
| `GRANARY_WARMER_GRANARY_URL` | `http://granary:8080` | How the warmer reaches Granary |
| `GRANARY_WARM_JOBS` | `2` | Warm jobs that may run at once (1 to 8) |
| `GRANARY_WARM_TIMEOUT_MINS` | `30` | Longest a warm job may run |
| `GRANARY_WARM_PYTHON` | `3.12` | Python version pip picks wheels for when warming |
| `GRANARY_GIT_HOSTS` | `github.com,gitlab.com,codeberg.org` | Hosts git repos may be cached from (HTTPS only; no IP addresses or localhost) |
| `GRANARY_GIT_MAX_SIZE` | `2GB` | Largest a cached git repo may grow; bigger clones are stopped and deleted |
| `GRANARY_GIT_TIMEOUT_MINS` | `10` | Longest a git clone or refresh may run |
| `GRANARY_SEARCH` | `on` | `off`: **Get packages** never sends search text to a search host and looks packages up by exact name only |
| `GRANARY_CARGO_API` | `https://crates.io/api/v1` | crates.io web API, used only for search |
| `GRANARY_MAVEN_SEARCH` | `https://search.maven.org/solrsearch/select` | Maven Central search, used only for search |
| `RUST_LOG` | `granary=info` | Log level (`granary=debug` logs every cache hit) |

Granary honors the standard `HTTPS_PROXY` / `NO_PROXY` variables for its own
outbound requests, and reads extra trusted CAs from `SSL_CERT_FILE`.

### Offline mode

Set `GRANARY_OFFLINE=on` when Granary has no route to the internet, or must
not use one. It then never contacts an upstream registry, for Cargo, npm,
PyPI and Maven alike:

- Cached files are served as usual.
- Cached metadata is served however old it is, without revalidation.
- Anything not in the cache gets a 404 whose message says Granary is offline.
  A 404 rather than a 5xx lets Maven, Gradle, pip and npm carry on as they do
  for any missing file (for example, pip tries another version).
- npm's audit, search and ping calls get a 503.
- Upstream health checks are skipped, and the web UI shows offline mode.

Fill the cache while online first, then switch offline mode on.

## How it works

Every format comes down to two kinds of data:

- **Files** (`.crate` archives, npm tarballs, wheels and sdists, Maven jars,
  POMs, Gradle `.module` files, signatures and checksums) never change once
  published. Granary downloads each one once, streams it to disk, and
  serves it forever with `Cache-Control: immutable`.
- **Metadata** (Cargo index entries, npm packuments, PyPI project pages,
  Maven `maven-metadata.xml` and its checksums) lists
  the available versions and changes as new ones are published. It is cached
  for `GRANARY_META_TTL_SECS`, then revalidated with `If-None-Match` /
  `If-Modified-Since`. Download links inside it are rewritten to point back at
  Granary.

For Maven, Granary fetches the `.sha1` that Central publishes next to each
`.jar` and `.pom` and checks the download against it before caching; a
mismatch is logged, nothing is cached and the client gets a 502. Maven and
Gradle probe for many files that don't exist, so a 404 from upstream is
remembered for five minutes.

Concurrent requests for the same uncached item are coalesced into a single
upstream download. Downloads are written to a temporary file and renamed into
place, so a crash never leaves a half-written package in the cache.

### Storage layout

```text
/data
├── files/{cargo,npm,pypi,maven}/…   immutable package files
├── meta/{cargo,npm,pypi,maven}/…    cached metadata (*.body + *.json info)
├── git/{host}/{owner}/{repo}.git     cached git mirrors (+ .granary.json info)
├── state/                     users, tokens, sessions and audit log (granary.db),
│                              cleanup settings, last-download times
└── tmp/                       in-progress downloads (cleared at startup)
```

Back up `state/`: it holds your accounts. Deleting anything under `files/` or
`meta/` is safe; it will be re-fetched on
demand. Last-download times start being recorded when you first run a version
with the web UI; cleanup treats older files as last used on the day they were
cached.

## Current limitations

- **Read-only.** There is no `cargo publish` / `npm publish` / `twine upload`
  / `mvn deploy` yet; npm and Maven write requests are refused, and so are
  git pushes.
- **Git: public repos only, no submodules or LFS.** See [Git repos](#git-repos).
- **No single sign-on yet.** Accounts live in Granary itself; LDAP and OpenID
  Connect aren't supported. Granary serves plain HTTP, so put it behind a
  reverse proxy with TLS if it's reachable from untrusted networks.
- **Single instance.** Granary stores everything on one local volume and isn't
  designed for running several replicas against shared storage.

## Roadmap

1. Docker / OCI registry proxy, including Docker Hub's token auth
2. Hosted repositories: publish your own crates, npm packages and wheels
3. Debian / apt proxy
4. Optional OpenID Connect sign-in and per-repository permissions

## Development

```sh
cargo test           # unit tests, plus end-to-end tests against a mock upstream
cargo clippy         # lints
RUST_LOG=granary=debug cargo run
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

The web UI bundles the Saira Condensed typeface (SemiBold, Latin subset),
licensed under the [SIL Open Font License 1.1](web/fonts/OFL.txt). It is
served by Granary itself; the UI never loads fonts from the internet.

Granary is an independent project and is not affiliated with Sonatype, JFrog,
the Rust Foundation, npm, Inc. or the Python Software Foundation.
