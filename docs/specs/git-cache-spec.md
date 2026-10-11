# Spec: git repo caching and "template from repo"

Goal: paste a repo URL (GitHub, GitLab, Codeberg, and so on) into the
Granary UI. If Granary doesn't have the repo yet, it clones it and keeps a
cached mirror. Granary reads the repo's manifests, builds a template from
them, and warms every dependency. After that, the repo and its
dependencies all come from Granary, including in offline mode.

Do this after v0.6.0 is merged. Build it in two phases, each its own commit
on its own local branch. Never push.

## Phase 1: git pull-through cache

### Serving
- Serve cached repos read-only at
  `/git/{host}/{owner}/{repo}.git`, for example
  `/git/github.com/serde-rs/serde.git`.
- Use the smart HTTP protocol by running `git http-backend` for each
  request, so shallow clones and partial fetches work. Allow only the
  upload-pack (fetch/clone) service. Reject receive-pack (push) with 403.
- Install `git` in the Granary image.
- Access uses the existing token auth (HTTP Basic, password is the token),
  with the same 401 challenge as Maven.

### Pull-through
- On a request for a repo that isn't cached, clone it from upstream with
  `git clone --mirror` into `{data_dir}/git/{host}/{owner}/{repo}.git`, then
  serve it.
- Use the existing per-key lock so two clients asking at once cause one
  clone.
- Refresh a cached repo with `git remote update --prune` when it is
  requested and older than the metadata TTL. If upstream fails, serve the
  cached copy (stale-if-error).
- Offline mode: serve only cached repos, never contact upstream, and return
  404 for anything not cached.

### Safety
- New setting `GRANARY_GIT_HOSTS`, a comma-separated allowlist, default
  `github.com,gitlab.com,codeberg.org`. Only HTTPS upstreams. Reject any
  other host, IP addresses, and localhost.
- Validate owner and repo names: letters, digits, `.`, `-` and `_` only.
  No `..` and no leading `-`.
- `GRANARY_GIT_MAX_SIZE` (default 2GB): if a clone grows past this, abort
  it, delete the partial copy, and log why. Count cached repos toward
  GRANARY_CACHE_LIMIT.
- Run git with hooks disabled, `protocol.file.allow=never`,
  `GIT_TERMINAL_PROMPT=0`, and a timeout (default 10 minutes).
- Don't fetch submodules or LFS objects in this phase. Say so in the UI and
  README.
- Public repos only for now. No upstream credentials.

### UI
- New "Git" section in Browse: repo list with size, last refresh, default
  branch, and buttons for Refresh and Delete (admin only).
- "Add repo" box (admin only): paste a URL, Granary clones it with a job
  that shows progress in the existing job view.
- Overview gets a Git gauge like the other formats.

### Client setup (Makefile and scripts/granary-client.sh)
- Add `git` to TOOLS.
- use-granary writes `~/.config/granary-client/gitconfig` (mode 600) with
  one `url."GRANARY_URL/git/{host}/".insteadOf "https://{host}/"` line
  per allowed host, plus an `http."GRANARY_URL/".extraHeader` line for the
  token. Include it from the global git config with `include.path`, so the
  token never sits in `~/.gitconfig` itself.
- use-public and remove take out the include line. Keep compatibility with
  Bash 3.2 and GNU Make 3.81.
- This sends `git clone https://github.com/...`, Cargo git dependencies,
  and CMake FetchContent through Granary automatically.

## Phase 2: template from repo

- On the Templates page, add "New from repo": a URL and an optional ref
  (branch, tag, or commit; default branch if empty).
- Granary caches the repo through phase 1 if it isn't cached yet, then
  reads these files at that ref, from the repo root and up to two levels
  down: Cargo.toml, Cargo.lock, package.json, package-lock.json,
  requirements*.txt, pyproject.toml, pom.xml, build.gradle(.kts).
- It fills a new custom template from them, using the existing manifest
  import. The template stores the repo URL and ref so it can be re-synced
  later with a "Resync from repo" button.
- If lockfiles exist, warming uses them so the exact versions get cached:
  `cargo fetch --locked` and `npm ci --ignore-scripts`. Store the lockfiles
  with the template.
- Cargo workspaces and npm workspaces: include all members.
- Git dependencies found in manifests (Cargo `git =`, npm `github:`) are
  cached through phase 1 too, if their host is allowed.
- Show a preview before saving: which manifests were found, how many
  packages per ecosystem, and anything skipped with the reason.
- Warming from a repo template works like other templates. The warmer
  reaches the repo through Granary's `/git/` path, never upstream.

## Tests
- Smart HTTP clone and shallow clone from the cache. Push is rejected.
- Pull-through on a miss, refresh after the TTL, stale-if-error, and lock
  coalescing, against a local bare repo standing in for upstream.
- Allowlist, name validation, size limit abort, and timeout.
- Offline mode serves cached repos and refuses everything else.
- Template from repo: a fixture repo with a Cargo workspace, an npm
  workspace with a lockfile, and a requirements.txt. Check the preview
  and the saved template.
- The client gitconfig file is mode 600 and the token isn't in
  ~/.gitconfig.

## Commits and docs
- Commit messages contain only a description of the change and the
  `Signed-off-by: Dev <dev@dynatheia.com>` line.
- README sections for the git cache, the allowlist, client setup, and the
  limits (no submodules, no LFS, public repos only). No em dashes.
