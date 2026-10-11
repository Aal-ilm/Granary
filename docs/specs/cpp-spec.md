# Spec: C and C++ support (raw proxy, vcpkg, Conan)

Goal: cache what C and C++ builds download, so they work from Granary alone,
including in offline mode. Three phases, each its own local branch and
commit. Never push. Build after the Gitea spec is merged.

Every phase follows the existing patterns: per-key locks, atomic writes,
offline mode, the cache budget, the access policy, audit log entries, a
tank on the Overview, Browse support, and client setup in the Makefile.

## Phase 1: raw URL proxy

A pull-through cache for plain HTTPS downloads: source tarballs, GitHub
release assets, and other files fetched by URL.

### Serving
- `GET /raw/{host}/{path...}` fetches `https://{host}/{path...}` on a miss,
  caches it, and serves it. Query strings are part of the cache key.
- `GRANARY_RAW_HOSTS`, a comma-separated allowlist. Default:
  - github.com
  - codeload.github.com
  - objects.githubusercontent.com
  - release-assets.githubusercontent.com
  - gitlab.com
  - codeberg.org

  HTTPS only. Reject IP addresses, localhost, and private addresses. The
  Gitea host is the one exception, as in the Gitea spec.
- Redirects: follow at most 5, and only to allowlisted hosts. GitHub
  release downloads redirect to its asset host, so this is needed. A
  redirect anywhere else fails the request.
- Cached files are immutable. Admins can delete one from Browse to force a
  refetch.

### Integrity
- Store the SHA-256 and SHA-512 of every cached file.
- Optional `?sha256=` or `?sha512=` on a request: verify the download
  before caching, and return 502 on a mismatch.
- `GET /raw/by-hash/sha512/{hash}` and `/sha256/{hash}` serve a cached file
  by its hash.

### Safety
- `GRANARY_RAW_MAX_SIZE`, default 2GB per file.
- A timeout, the same as git's.

### Client setup and docs
- `make use-granary TOOLS=raw` writes a small env file with
  `GRANARY_RAW_URL` for scripts and CMake.
- README examples:
  - CMake FetchContent with `URL ${GRANARY_RAW_URL}/github.com/...`
  - Bazel's downloader config rewrite rules
  - curl and wget
  - a note that git-based FetchContent already goes through the git cache

### Tests
- Miss, then hit.
- The allowlist, including redirects to a disallowed host.
- Hash verification, by-hash lookup, the size limit, and offline mode.

## Phase 2: vcpkg

vcpkg downloads tool and source archives (its "assets"), keyed by SHA-512,
and can store built binaries in an HTTP cache.

### Assets
- Use vcpkg's `x-script` asset source, so Granary does the download and
  vcpkg never contacts the origin:
  ```
  X_VCPKG_ASSET_SOURCES="clear;x-script,curl -fsSL -u token:$GRANARY_TOKEN \"{granary}/vcpkg/asset?url={url}&sha512={sha512}\" -o {dst};x-block-origin"
  ```
- `GET /vcpkg/asset?url=...&sha512=...` serves the file from cache if that
  SHA-512 is cached. Otherwise it fetches the URL through the raw proxy's
  rules (allowlist, redirects, size), verifies the SHA-512, and caches it.
- Check the exact x-script placeholder syntax against the current vcpkg
  docs, and adjust the example to match.

### Binary cache
- vcpkg's `http` binary source:
  `http,{granary}/vcpkg/bin/{name}/{version}/{sha},readwrite`.
- GET serves a stored binary package.
- PUT stores one, only for tokens with a new "vcpkg binary write" scope.
  Verify the uploaded size and content type, and cap the size.
- Binaries are hosted content, not proxied. They follow the access policy
  and the cache budget, and admins can delete them.

### Ports and registries
- vcpkg reads its ports from the microsoft/vcpkg git repo, and from other
  git registries. That already goes through the git cache with the
  insteadOf setup. Document it.

### Warming and templates
- Templates get a vcpkg section: a list of ports, like `fmt` or `spdlog`,
  plus a baseline commit.
- The warmer runs `vcpkg install --only-downloads` with the asset source
  above, so every source archive gets cached without building anything.
- Add vcpkg, cmake, and ninja to the warmer image. Keep it passing the
  Trivy gate.

### Client setup
`make use-granary TOOLS=vcpkg` writes `~/.config/granary-client/vcpkg.env`
(mode 600) with X_VCPKG_ASSET_SOURCES and VCPKG_BINARY_SOURCES, and prints
how to source it.

### Tests
- Asset miss then hit, SHA-512 mismatch, and x-block-origin behavior with
  a fake vcpkg call.
- Binary GET and PUT, the write scope, and the size cap.
- Warming a small port list.

## Phase 3: Conan 2

A pull-through proxy for ConanCenter.

### Serving
- Implement the Conan v2 REST API endpoints a client needs:
  - ping
  - authenticate (accepts a Granary access token over Basic and returns it
    as the bearer token)
  - recipe and package revision listing
  - latest revision
  - file lists
  - file downloads
  - search

  Check the exact routes against Conan's own server implementation.
- Upstream: `GRANARY_CONAN_UPSTREAM`, default `https://center2.conan.io`.
- Caching: files for a given recipe or package revision are immutable.
  Revision lists, "latest", and search results are metadata, using the
  existing TTL with stale-if-error.
- Read-only: refuse uploads with a clear message. Publishing belongs in
  Gitea.

### Search, templates, and warming
- The search page gets Conan, using the upstream search endpoint.
- Templates get a Conan section with references like `fmt/11.0.2`, plus
  profiles to warm for. Default profiles:
  - Linux x86_64 gcc
  - macOS armv8 apple-clang
- The warmer runs `conan install --requires=... --build=never` once per
  profile. That caches prebuilt binaries where ConanCenter has them, and
  the recipes for everything.
- Add conan to the warmer image.

### Client setup
`make use-granary TOOLS=conan` runs:
- `conan remote add granary {url}/conan`
- `conan remote login granary token -p <token>`
- `conan remote disable conancenter`

use-public reverses these. Keep compatibility with Bash 3.2 and GNU Make
3.81.

### Default template
Add "C++ CMake essentials", with both Conan and vcpkg sections:
- fmt
- spdlog
- nlohmann_json
- catch2

### Tests
- The REST flow against a mock upstream: authenticate, latest, revisions,
  and a download.
- Revision immutability and metadata refresh.
- Upload refused.
- Offline mode.
- Warming a small reference list, with the real conan where available
  (skipped without it).

## Commits and docs
- Commit messages contain only a description of the change and the
  `Signed-off-by: Dev <dev@dynatheia.com>` line.
- README sections for each phase, with client setup and limits. No em
  dashes.
