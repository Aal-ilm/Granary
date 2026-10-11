# Spec: Go modules

Goal: a Go module proxy. `go` fetches every module through Granary, public
ones from proxy.golang.org and private ones from your own sources only.
Granary also serves the checksum database and the vulnerability database,
so builds work offline and never reach the internet on their own.

Build after the Gitea spec is merged. Three phases, each its own local
branch and commit. Never push. The last phase's commit bumps the version in
Cargo.toml.

References: the GOPROXY protocol, checksum database, private modules and
zip file sections of https://go.dev/ref/mod, `golang.org/x/mod/zip`,
`golang.org/x/mod/sumdb/dirhash`, and https://go.dev/security/vuln/database.

## Phase 1: public modules, checksum database, vulnerability database

### The GOPROXY protocol at `/go/`
Serve these paths under `/go/`, with `$module` and `$version`
case-encoded:

| Path | Returns | Cached as |
|---|---|---|
| `$module/@v/list` | known versions, one per line, no pseudo-versions | metadata (TTL) |
| `$module/@v/$version.info` | `{"Version": ..., "Time": ...}` (RFC 3339) | file for a canonical version; metadata for a query (branch, commit) |
| `$module/@v/$version.mod` | the go.mod file | file, verified |
| `$module/@v/$version.zip` | the module zip | file, verified |
| `$module/@latest` | like `.info`, for the latest version | metadata (TTL) |

- Case encoding: each uppercase letter is written as `!` and the lowercase
  letter (`example.com/!m` for `example.com/M`). Decode it on the way in.
  Refuse a path that contains an uppercase letter, or a `!` that isn't
  followed by a lowercase letter. Cache under the encoded form, so the cache
  works on case-insensitive file systems.
- Validate module paths and versions as `golang.org/x/mod/module` does
  (CheckPath, canonical semver, major version suffix). `sumdb` and `vuln`
  can't be the first path element of a module (they have no dot), so they
  are free for the next two sections.
- Upstream: `GRANARY_GO_UPSTREAM`, default `https://proxy.golang.org`.
- A miss is a 404 with a `text/plain; charset=utf-8` body, whether upstream
  said 404 or 410. Never a 5xx for a module that doesn't exist: with
  `GOPROXY` lists, `go` only falls back after a 404 or 410.
- Limits as in `golang.org/x/mod/zip`: a zip may be at most 500 MiB, and so
  may its files uncompressed. `go.mod` and `LICENSE` may be at most 16 MiB
  each. Refuse anything larger, and don't cache it.

### Checksum database through the proxy
`go` asks `$GOPROXY/sumdb/<name>/supported` before contacting a checksum
database. On a 200 it sends every checksum database request through the
proxy, and never connects to the database itself.

- Serve `/go/sumdb/sum.golang.org/` with `supported` (200, empty body),
  `latest`, `lookup/$module@$version` (case-encoded) and
  `tile/$H/$L/$K[.p/$W]` (including `tile/$H/data/$K`). Fetch them from
  `GRANARY_GO_SUMDB`, default `https://sum.golang.org`.
- Answer 404 for any other database name, so `go` falls back to its usual
  behavior for it.
- Lookups and tiles never change once they exist, partial tiles included,
  so cache them as files. `latest` changes: cache it as metadata, and serve
  the cached copy offline.
- Granary passes these through unchanged; `go` itself checks the
  signatures and the inclusion and consistency proofs. Granary doesn't
  replace that check.

### Verified cache
- Before a `.zip` or `.mod` enters the cache, Granary checks it against the
  `h1:` hash in the checksum database's lookup record for that version
  (`$module $version h1:...` and `$module $version/go.mod h1:...`).
- A lookup record is trusted only after its signed tree note verifies
  against the database's public key, and the record is proved to be in that
  tree from the log's tiles, as `go` does. A valid signature alone says
  nothing about the record next to it. The key is `GRANARY_GO_SUMDB_KEY`
  (`name+hash+key`, as in `GOSUMDB`), sum.golang.org's by default; its name
  is the database's name at `/go/sumdb/<name>/`.
  - The zip's hash is `dirhash.Hash1` over its entry names (with the
    `$module@$version/` prefix) and contents.
  - The go.mod hash is `Hash1` over the single file `go.mod`.
- A mismatch is logged and counted as an integrity failure. Nothing is
  cached, and the client gets a 502, as for the other formats.
- When the database has no record (a 404 or 410 from it), the download
  fails closed: a 502, counted as an integrity failure, nothing cached.
  `GRANARY_GO_ALLOW_UNVERIFIED=on` (default off) serves such modules
  anyway, still checked to be well-formed zips of the right module.

### Vulnerability database mirror
- Serve `/go/vuln/` as a mirror of `GRANARY_GO_VULNDB`, default
  `https://vuln.go.dev`. The paths are `index/db.json`,
  `index/modules.json`, `index/vulns.json` and `ID/$id.json`, each also as
  `.json.gz`.
- The indexes are metadata (TTL). Reports are metadata too, since they can
  be updated.
- A **Tasks** entry downloads `vulndb.zip` daily and unpacks it into the
  cache, so the whole database is available offline, not just what was
  asked for. It is off by default; turning it on is a setting on the Tasks
  page.

### Offline mode
- Cached `list`, `.info`, `@latest`, `.mod` and `.zip` are served however
  old they are.
- From the checksum database, cached lookups, tiles and `latest` are
  served.
- The vulnerability database serves whatever is cached; with the daily task
  on, that's all of it.
- Everything else is a 404 that says Granary is offline, as for the other
  formats.

### Web UI and the rest
- Go joins Browse, the Overview gauges, cleanup (with its `.mod` and
  `.info` files as companions of the zip), metrics and upstream health.
  **Get packages** comes in phase 3.
- Repositories page: the client setup below.

### Client setup
```sh
go env -w GOPROXY=https://HOST/go
go env -w GONOSUMDB=gitea.example.lan,go.example.com   # your private prefixes (phase 2)
go env -u GOPRIVATE
go env -w GONOPROXY=
export GOVULNDB=https://HOST/go/vuln                   # gopls
govulncheck -db https://HOST/go/vuln ./...             # govulncheck has only the flag
```
- `GOPROXY` lists Granary alone, without `,direct`, so a miss never makes
  `go` fetch from somewhere else.
- No `GOSUMDB` override is needed: `go` finds the checksum database through
  the proxy.
- `GONOPROXY` must be empty, so private modules come through Granary too.
  An empty `GONOPROXY` falls back to `GOPRIVATE` (`cmd/go` reads it with a
  `GOPRIVATE` default), so `GOPRIVATE` must stay unset. Anyone who needs
  `GOPRIVATE` for another reason sets `GONOPROXY=none`, a pattern that
  matches nothing.
- `GONOSUMDB` lists the private prefixes. Their sums then come from the
  first download into `go.sum`, not from the public database.
- **Tokens need HTTPS.** `go` refuses to send credentials to an `http://`
  URL ("refusing to pass credentials to insecure URL"), and `GOAUTH` and
  netrc only apply to HTTPS. With `GRANARY_CLIENT_AUTH=required`, Go
  clients need Granary behind TLS. Give the token in netrc
  (`machine HOST login token password TOKEN`) or in the URL
  (`https://token:TOKEN@HOST/go`).
- Granary answers an anonymous request on `/go/` with a 401 and a Basic
  challenge, so netrc credentials are sent.
- Over plain HTTP, Go works only with `GRANARY_CLIENT_AUTH=off`, and then
  without private modules. Say so in the README and on the Repositories
  page.
- `make use-granary TOOLS=go` sets the variables with `go env -w` and
  writes the netrc entry. It doesn't set `GOVULNDB` for gopls; the README
  shows that line instead.

### Tests
- Protocol: every endpoint, case encoding both ways (`example.com/!m`),
  invalid encodings and paths refused, 404 for both 404 and 410 upstream.
- Verification: a `.zip` or `.mod` whose hash doesn't match the lookup
  record is refused and not cached; a correct one is cached and later
  served without asking upstream. A forged record with a genuine signed
  tree, a wrong key and a tampered tile are refused; a module without a
  record fails closed unless `GRANARY_GO_ALLOW_UNVERIFIED` is on. One test
  verifies a real sum.golang.org lookup with its real key.
- Checksum database: `supported` is 200 for sum.golang.org and 404 for
  other names; lookups and tiles are cached, `latest` is metadata.
- Vulnerability database: the paths, `.gz` variants, and the daily task.
- Offline: everything cached is served, misses are 404.
- A real client, skipped without `go` or network: `go mod download` and
  `go build` of a small module through Granary with only `GOPROXY`, and
  `GONOSUMDB` set as above. Check that `go` used the checksum database
  through Granary, from Granary's request log.

## Phase 2: private modules

### Private prefixes
- A list of module path patterns, in the same syntax as `GONOSUMDB` (Go's
  `path.Match` globs, matched against path prefixes), so admins can paste
  the same list in both places.
- Edited under **System → Go private modules**. `GRANARY_GO_PRIVATE` sets
  it instead and locks it, as for the Gitea namespaces.
- Each entry names its source:
  - `git` (the default): zip-from-git through the git cache. The module path
    must be on an allowed git host. Its first three elements are the
    repository (`host/owner/repo`), and the rest, without a major version
    suffix, is the module's subdirectory.
  - `git:<host>/<owner>`: the same, for a vanity prefix. For example
    `go.example.com=git:gitea.example.lan/acme` reads
    `go.example.com/tool` from `gitea.example.lan/acme/tool`.
  - `gitea:<owner>`: Gitea's Go registry
    (`{gitea}/api/packages/{owner}/go`, which speaks the GOPROXY protocol),
    when the connected Gitea has one. The System page shows whether it
    does.

### The rule
- A module path matching a private prefix is only ever served from its
  private source: never from proxy.golang.org, and never looked up in the
  checksum database.
  - A checksum database `lookup` for it is a 404 without contacting
    anyone, with a body that says to add the prefix to `GONOSUMDB`. A
    misconfigured client fails closed and leaks nothing.
  - Not found in the private source is a 404.
- A path matching no prefix is never served from a private source.
- Private modules are private content: they follow the access policy on
  every request, cached or not and offline too. They are cached in their own
  namespace, apart from public modules.

### Zip from git
Build module zips the way `go` builds them when it downloads from the
repository directly, so the `h1:` hash Granary's zip gives is the one `go`
would compute itself. Clients' `go.sum` lines then match whichever way a
module was first fetched.
- File list from `git archive` at the revision, so `.gitattributes`
  `export-ignore` applies as it does for `go`.
- Keep only the module's subdirectory. Without a `LICENSE` there, add the
  repository root's `LICENSE`, as `go` does.
- Apply `golang.org/x/mod/zip`'s rules exactly:
  - leave out vendor directories, following its rules for the module's
    `go` directive (they changed in go 1.24, for `vendor/modules.txt` and
    for `vendor` directories inside package paths);
  - leave out subdirectories that hold their own `go.mod`;
  - leave out symbolic links and other irregular files;
  - check the file name rules, Windows reserved names, and case-fold
    collisions;
  - apply the size limits.
- `.mod`: the go.mod file at the revision. Without one, a file with only
  `module <path>`.
- Versions map to tags as in "Mapping versions to commits":
  - `vX.Y.Z` tags, prefixed with `subdir/` for a module in a subdirectory;
  - major versions matching the path's `/vN` suffix;
  - `+incompatible` for v2 and later at the repository root without a
    go.mod file.
- `list` gives the tagged versions.
- `.info` for a branch, tag or commit gives its version: a tag on that
  commit, else a pseudo-version built from the nearest ancestor tag, in the
  three documented forms.
- `@latest` gives the highest release, else the highest pre-release, else
  a pseudo-version of the default branch's head.
- Pseudo-versions asked for directly are checked as `go` checks them: the
  timestamp matches the commit time, and the base version is an ancestor
  tag.
- A zip, once built, is kept like any cached file and never rebuilt, so
  its hash never changes.

### Tests
- The rule both ways: private paths never reach proxy.golang.org or the
  checksum database (checked from the mock upstreams' request logs);
  public paths never reach a private source; a checksum database lookup
  for a private path is a 404 that names `GONOSUMDB`.
- Zip from git, against fixture repos, comparing Granary's `h1:` with what
  `go mod download` computes cloning the same repo directly (skipped
  without `go`). The fixtures cover:
  - a module at the repository root, and one in a subdirectory without its
    own `LICENSE`;
  - `/v2` both ways (subdirectory and major-version branch);
  - `+incompatible`;
  - vendor directories under go 1.23 and go 1.24;
  - a nested module, a symbolic link, and an `export-ignore` file.
- Versions: `list`, `@latest` with and without tags, branch and commit
  queries, pseudo-version checks.
- Gitea's Go registry as a source, against the mock Gitea.
- The access policy on private modules, offline included.
- A real client with private modules: `go build` with `GONOSUMDB` set and
  `GOPRIVATE` unset, over HTTPS with a netrc token.

## Phase 3: templates, warming and Get packages

### Templates
- A Go section, one `module@version` line each, such as
  `golang.org/x/text@v0.21.0`. `@latest` is allowed and is resolved
  through the proxy when the template is warmed, like Maven lines without a
  version. Lines are checked as module paths and versions are checked as in
  phase 1. Private modules are allowed and follow the phase 2 rule.
- Built-in templates may gain Go entries; existing ones don't change.
- Starter zips get a `go.mod` with the lines as `require`s, plus the
  `go.sum` from the last warm when there is one, and a README section with
  the client setup.
- Import: a `go.mod` file's `require` block becomes Go lines.

### Repo templates
- A repo's `go.mod` and `go.sum` files are its manifests and lockfiles,
  workspace members (`go.work`) included. The preview lists them and the
  number of requirements.
- Warming a repo template uses the repo's own `go.mod` and `go.sum` at the
  pinned commit. Like the other ecosystems, a `go.sum` that doesn't match
  its `go.mod` makes the job finish as done with warnings, with the newest
  matching versions cached instead.

### The warmer
- Add Go to the warmer image: the latest stable release from go.dev's
  downloads, checked against its published SHA-256. The image must keep
  passing the Trivy gate (CRITICAL and HIGH, fixable).
- A Go step runs `go mod download all` through Granary, once (module
  downloads don't depend on the platform):
  - `GOPROXY` set to Granary only. Go won't send credentials over plain
    HTTP, and the warmer reaches Granary over plain HTTP on the internal
    network, so the job's token needs another way in. Preferred: TLS on
    the internal network between the warmer and Granary, which also
    protects the other formats' warmer token. If the plan ends up with the
    token in the `GOPROXY` path instead, it is redacted from every log and
    error, and accepted only from the warmer's address together with the
    warmer secret. Settle this in the plan for phase 3.
  - `GONOSUMDB` set to the private prefixes the job's starter may read,
    `GOFLAGS=-mod=mod`, and `GOTOOLCHAIN=local`, so a `toolchain` line never
    downloads another Go.
  - A clean `GOMODCACHE` per job, deleted afterwards.
- For a template's lines, the warmer writes a `go.mod` that requires them
  and runs `go mod download all`. The `go.sum` it writes is kept as the
  template's lockfile, for its starter.

### Get packages
- Go joins **Get packages** as exact module-path lookup through the proxy:
  `@v/list` and `@latest`, and the version's `.info`. proxy.golang.org has
  no search API, so there is no search text to send anywhere.
- **Cache** fetches the version's `.mod` and `.zip` through the same path
  clients use, so they are verified as in phase 1. **Cache with
  dependencies** starts a warm job.

### Tests
- Template lines, import from `go.mod`, starter contents.
- Repo template preview and warm with a fake warmer, and the
  done-with-warnings path.
- A real warmer run, skipped without `go`: a template with a public module
  and a private one, through Granary, with the job's token.
- Get packages lookups and caching.

## Not in this spec
- Vanity import paths found through `?go-get=1` pages: private prefixes
  name their source explicitly instead.
- Publishing modules through Granary.

## Commits and docs
- Commit messages contain only a description of the change and the
  `Signed-off-by: Dev <dev@dynatheia.com>` line.
- README sections: Go client setup (including the HTTPS requirement for
  tokens, and `GOPRIVATE`), the checksum database through the proxy, the
  vulnerability database mirror, private module prefixes and their sources,
  and zip from git. No em dashes.
