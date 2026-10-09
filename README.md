# Granary

A small, self-hostable caching proxy for package registries. Point Cargo, npm
and pip at Granary, and every package is downloaded from the public registry
once, then served from your own disk after that.

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
  `sha256`) before caching it, so a corrupted or tampered response is never
  stored or served. Clients check again on their side.
- **Web UI built in.** Browse and search what's cached, delete packages, set
  cleanup rules, watch tasks, check upstream health and read logs. It's served
  by the same binary and never loads anything from the internet.
- **Accounts built in.** Sign-in with admin and viewer roles, two-factor
  codes, and per-machine access tokens for Cargo, npm and pip. Built on the
  open-source axum-login, tower-sessions, password-auth (Argon2id) and
  totp-rs libraries, stored in a local SQLite file. No external identity
  service is needed.

![Granary overview page](docs/overview.png)

| Ecosystem | Status | Client path |
|---|---|---|
| Cargo (crates.io, sparse protocol) | ✅ proxy | `/cargo/` |
| npm (registry.npmjs.org) | ✅ proxy | `/npm/` |
| PyPI (Simple API, PEP 503/691) | ✅ proxy | `/pypi/simple/` |
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

To let anyone on your network download without a token, set
`GRANARY_CLIENT_AUTH=off`; the configuration then needs only the URLs.

## Web UI

| Page | What it's for |
|---|---|
| Overview | Disk use per repository, cache hit rate, upstream status, recently cached packages |
| Repositories | Each proxy's URL, upstream, size and status, client setup snippets, and **Invalidate cache** to force fresh metadata |
| Browse | Every cached package, filterable and sortable by size, versions, cache date and last download; the sidebar search covers all repositories |
| Package detail | Each cached version and file, with download and delete |
| Cleanup | Delete packages nobody has downloaded in *N* days, per repository, with a dry-run preview and an optional daily schedule |
| Tasks | Scheduled jobs (cleanup, upstream checks, saving download history) with last run, result and **Run now** |
| System | Upstream reachability and latency, disk space, and the active configuration |
| Logs | Recent server events, filterable by level and text (admins only) |
| Audit log | Sign-ins, account, token and user changes, deletions and settings changes, with who, when and from where; filterable and exportable as CSV (admins only) |
| Access tokens | Create and revoke tokens for package managers, with ready-made client configuration |
| Users | Add people as admins or viewers, reset passwords, turn off two-factor, disable or delete accounts (admins only) |
| Account | Change your password and set up two-factor authentication |

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

The image is built on Google's distroless base, which contains only the C
runtime libraries and CA certificates Granary needs: no shell, package
manager or other tools. SQLite is compiled into the binary. CI scans the
image with Trivy and the dependencies with `cargo audit` (also weekly).

## Configuration

All settings are environment variables.

| Variable | Default | Purpose |
|---|---|---|
| `GRANARY_LISTEN` | `0.0.0.0:8080` | Address and port to listen on |
| `GRANARY_DATA_DIR` | `/data` in Docker, `./data` otherwise | Where the cache lives |
| `GRANARY_PUBLIC_URL` | *(from request headers)* | Fixed external URL, e.g. `https://granary.lan`. Set it when clients reach Granary through a reverse proxy that doesn't send `X-Forwarded-Host` / `X-Forwarded-Proto` |
| `GRANARY_META_TTL_SECS` | `300` | How long index/metadata responses are fresh before revalidation |
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
| `RUST_LOG` | `granary=info` | Log level (`granary=debug` logs every cache hit) |

Granary honors the standard `HTTPS_PROXY` / `NO_PROXY` variables for its own
outbound requests, and reads extra trusted CAs from `SSL_CERT_FILE`.

## How it works

Every format comes down to two kinds of data:

- **Files** (`.crate` archives, npm tarballs, wheels and sdists) never change
  once published. Granary downloads each one once, streams it to disk, and
  serves it forever with `Cache-Control: immutable`.
- **Metadata** (Cargo index entries, npm packuments, PyPI project pages) lists
  the available versions and changes as new ones are published. It is cached
  for `GRANARY_META_TTL_SECS`, then revalidated with `If-None-Match` /
  `If-Modified-Since`. Download links inside it are rewritten to point back at
  Granary.

Concurrent requests for the same uncached item are coalesced into a single
upstream download. Downloads are written to a temporary file and renamed into
place, so a crash never leaves a half-written package in the cache.

### Storage layout

```text
/data
├── files/{cargo,npm,pypi}/…   immutable package files
├── meta/{cargo,npm,pypi}/…    cached metadata (*.body + *.json info)
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
  yet; npm write requests are refused.
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
cargo test           # unit tests
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
