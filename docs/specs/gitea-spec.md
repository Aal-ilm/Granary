# Spec: Gitea integration

Goal: use a self-hosted Gitea as a private upstream. Packages you publish to
Gitea, and your private Gitea repos, come through Granary alongside the
public registries, under one URL per format. Gitea does the publishing;
Granary does the caching.

Build after v0.9.0 is merged. Two phases, each its own local branch and
commit. Never push.

Out of scope for now, but the design must make them easy to add later:
- per-user Gitea permission checks for private content
- signing in to Granary with a Gitea account

## Phase 1: connection, private access policy, private git repos

### Connection settings
- `GRANARY_GITEA_URL`, for example `https://gitea.skynet.core`.
- `GRANARY_GITEA_TOKEN_FILE`: path to a file holding a Gitea access token
  with read-only repository and package scopes. Read at startup. Never shown
  in the UI, API responses, logs, or job output.
- `GRANARY_EXTRA_CA`: optional path to a PEM file of extra root
  certificates, for internal CAs like skynet.core. It applies to everything
  Granary fetches over HTTPS, the Gitea connection and the public
  upstreams alike:
  - add it to reqwest's root store
  - pass it to git with `http.sslCAInfo`, combined with the system roots so
    public hosts keep working

  Document mounting the file read-only in compose.
- The System page gets a Gitea section showing the URL, connection status,
  Gitea version, and the token's owner (not the token). A "Test connection"
  button runs the check on demand.

### Private access policy
Everything that comes from Gitea counts as private: repos, packages, and
templates and starters built from them. Who may read private content is
decided in one place.

- Add a policy module with one function, roughly
  `can_read_private(principal, resource) -> bool`. Every private read goes
  through it: proxy downloads, Browse, search results, template and starter
  views, job logs, and Overview detail lists. The `resource` describes the
  source, owner, and repo or package name, so a later per-resource mode can
  use it without changing callers.
- Setting `GRANARY_PRIVATE_ACCESS`, also changeable on the System page by
  admins:
  - `signed-in`: any signed-in user or valid access token.
  - `scoped` (default): admins, plus users and tokens with a new
    "private access" flag. Show the flag as a checkbox on the user page and
    the token creation form.
- Write the mode enum so a third mode, such as `gitea` (check the user's
  own Gitea permissions), can be added later. No mode needs a data
  migration, so changing modes only changes the setting.
- Content a principal can't read behaves as if it doesn't exist: 404 for
  downloads, and left out of lists and counts. It never leaks a name.
- The warmer's system account can read private content only while running
  a job started by someone who can.

### Auth extensibility
- Put the login check behind a small provider interface, with the current
  local password + TOTP login as its only implementation. Sessions, roles,
  and tokens stay as they are. The goal is that adding an OAuth2/OIDC
  provider later means adding an implementation, not reworking sessions.
  Keep it small: one trait, one implementation, no OAuth code now.

### Private git repos
- When `GRANARY_GITEA_URL` is set, its host joins the git allowlist
  automatically and is marked private.
- Upstream requests to the Gitea host send the token. The token is never
  sent to any other host, including on redirects. Redirects stay disabled,
  as they are now.
- Allow the Gitea host even though it resolves to a private address. This
  is the one exception to the private-address rule, and it is tied to the
  configured URL only.
- Repos from the Gitea host follow the access policy everywhere: clone,
  Browse, and template from repo.
- Owner/group paths: support Gitea's `owner/repo` layout.

### Tests
- Policy: both modes. Content hidden from those who can't read it,
  including counts and search. Switching modes needs no migration. The
  warmer's access is limited to a job's starter.
- Token: never in responses, logs, or job output. Never sent to a
  non-Gitea host.
- Extra CA: an upstream signed by a test CA works with GRANARY_EXTRA_CA
  and fails without it. Public hosts still work with it set.
- Private repo clone and template from repo against a mock Gitea.

## Phase 2: Gitea packages as an upstream

### Private namespaces
Admins set which names belong to Gitea, per format, on the System page.
Env vars can preset them. Each entry maps a pattern to a Gitea owner (user
or org):

| Format | Pattern | Gitea endpoint |
|---|---|---|
| npm | scope, e.g. `@dynatheia` | `{gitea}/api/packages/{owner}/npm/` |
| PyPI | exact names or prefixes, e.g. `dynatheia-*` | `{gitea}/api/packages/{owner}/pypi/simple/` |
| Maven | groupId prefixes, e.g. `com.dynatheia` | `{gitea}/api/packages/{owner}/maven/` |
| Cargo | the whole registry (see below) | `{gitea}/api/packages/{owner}/cargo/` |

### Dependency confusion rule
- A name matching a private namespace is only ever fetched from Gitea. If
  Gitea doesn't have it, return 404. Never fall back to the public
  registry for it.
- A name not matching any namespace is never fetched from Gitea.
- Test both directions explicitly.

### Per format
- npm: route matching scopes to Gitea. Rewrite tarball URLs to Granary
  paths, following GRANARY_NPM_REWRITE_TARBALLS. Cache as usual.
- PyPI: route matching names to Gitea's simple index and files.
- Maven: route matching groupIds to Gitea's Maven endpoint. Checksum
  verification applies as for Central.
- Cargo: Cargo has no per-name routing. A private Cargo registry is a
  separate named registry. Serve it at `/cargo-private/` as its own sparse
  index. Check which index formats the connected Gitea version supports,
  and use sparse if it can; otherwise read Gitea's git index through the
  git cache. Client config adds `[registries.granary-private]`.
- Private packages follow the access policy and show a "private" badge in
  Browse and search.
- Search: when signed in with private access, Granary searches Gitea's
  packages too. Results are labelled by source.

### Client setup
- `make use-granary` adds the Cargo private registry to Cargo's config and
  the token to credentials, alongside the existing setup. npm, pip, and
  Maven need no change, since routing happens in Granary.
- Docs: how to point Gitea Actions runners at Granary for npm, pip, Cargo,
  Maven, and git, so CI builds don't reach the internet.

### Tests
- Routing for each format, including the dependency confusion rule in
  both directions.
- Cargo private registry: index, download, and auth.
- The access policy applied to private packages in each format.
- Offline mode serves cached private packages.

## Commits and docs
- Commit messages contain only a description of the change and the
  `Signed-off-by: Dev <dev@dynatheia.com>` line.
- README sections: Gitea setup, the extra CA, private access modes and how
  to change them, private namespaces, and the dependency confusion rule.
  No em dashes.
