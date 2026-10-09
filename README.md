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
- **Safe by design.** Clients still verify every download against the
  registry's own checksums (Cargo's `cksum`, npm's `integrity`, pip's
  `sha256`), so the cache can't silently serve tampered files.

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
```

Then open `http://localhost:8080/`. The status page shows cache activity and
ready-to-paste client configuration using the address you reached it on.

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

Replace `HOST` with the address of your Granary server.

**Cargo** (`~/.cargo/config.toml`):

```toml
[source.crates-io]
replace-with = "granary"

[source.granary]
registry = "sparse+http://HOST:8080/cargo/"
```

**npm / pnpm / yarn** (`~/.npmrc` or a project `.npmrc`):

```ini
registry=http://HOST:8080/npm/
```

**pip** (`pip.conf`) or **uv** (`UV_INDEX_URL=http://HOST:8080/pypi/simple/`):

```ini
[global]
index-url = http://HOST:8080/pypi/simple/
```

## Configuration

All settings are environment variables.

| Variable | Default | Purpose |
|---|---|---|
| `GRANARY_LISTEN` | `0.0.0.0:8080` | Address and port to listen on |
| `GRANARY_DATA_DIR` | `/data` in Docker, `./data` otherwise | Where the cache lives |
| `GRANARY_PUBLIC_URL` | *(from request headers)* | Fixed external URL, e.g. `https://granary.lan`. Set it when clients reach Granary through a reverse proxy that doesn't send `X-Forwarded-Host` / `X-Forwarded-Proto` |
| `GRANARY_META_TTL_SECS` | `300` | How long index/metadata responses are fresh before revalidation |
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
└── tmp/                       in-progress downloads (cleared at startup)
```

Deleting anything under `/data` is safe; it will be re-fetched on demand.

## Current limitations

- **Read-only.** There is no `cargo publish` / `npm publish` / `twine upload`
  yet; npm write requests are refused.
- **No authentication.** Run it on a trusted network, or put it behind a
  reverse proxy that handles auth and TLS.
- **No automatic cleanup.** The cache grows until you prune it. Removing
  files you no longer need is safe.

## Roadmap

1. Cleanup policies (evict files not requested in N days)
2. Docker / OCI registry proxy, including Docker Hub's token auth
3. Hosted repositories: publish your own crates, npm packages and wheels
4. Debian / apt proxy
5. Access tokens and per-repository permissions
6. Prometheus metrics

## Development

```sh
cargo test           # unit tests
cargo clippy         # lints
RUST_LOG=granary=debug cargo run
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Granary is an independent project and is not affiliated with Sonatype, JFrog,
the Rust Foundation, npm, Inc. or the Python Software Foundation.
