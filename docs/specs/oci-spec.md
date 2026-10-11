# Spec: Docker/OCI pull-through cache

Goal: cache container images (and other OCI artifacts like Helm charts) from
Docker Hub, GHCR, Quay, and other registries. Every machine pulls through
Granary, images keep working in offline mode, and nothing is pulled from the
internet twice. Two phases, each its own local branch and commit. Never
push. Build after the C/C++ spec is merged.

Read-only: Granary never accepts pushes. Publishing images belongs in
Gitea's container registry, which phase 2 adds as an upstream.

## Phase 1: pull-through registry

### Protocol
- Implement the read side of the OCI Distribution spec under `/v2/`:
  - `GET /v2/` (version check)
  - manifests by tag or digest (GET and HEAD)
  - blobs by digest (GET and HEAD, with Range support)
  - `GET /v2/{name}/tags/list`
- Push endpoints (blob uploads, manifest PUT, DELETE) return the standard
  UNSUPPORTED error with a message pointing to Gitea.
- Accept and return the standard manifest media types: OCI image manifest
  and index, and Docker v2 manifest and manifest list. Never convert
  between formats, because that would change digests.

### Upstream from the name
- If the first path segment of the name contains a dot or a colon, it is
  the upstream registry:
  - `ghcr.io/owner/app` goes to ghcr.io
  - `quay.io/org/img` goes to quay.io
- Otherwise the upstream is Docker Hub. Single names get the official
  `library/` prefix, so `nginx` becomes `library/nginx`. This lets
  Docker's built-in `registry-mirrors` setting work, since mirrors send
  plain Docker Hub names.
- `GRANARY_OCI_UPSTREAMS`, an allowlist. Default:
  - docker.io
  - ghcr.io
  - quay.io
  - registry.k8s.io
  - gcr.io
  - mcr.microsoft.com
  - public.ecr.aws

  Same rules as the git and raw allowlists: HTTPS, no IPs, no private
  addresses.

### Upstream auth
- Handle the standard Bearer token flow: on 401, read
  `WWW-Authenticate`, request an anonymous pull token from the realm, and
  retry. Cache tokens until they expire.
- Optional Docker Hub credentials, `GRANARY_DOCKERHUB_USER` and
  `GRANARY_DOCKERHUB_TOKEN_FILE`, to get higher pull limits. Never logged,
  never sent to other registries.
- Use HEAD requests for tag revalidation where the registry allows it.
  Docker Hub doesn't count HEAD requests against its pull limit.

### Caching
- Blobs and manifests fetched by digest are immutable. Verify the SHA-256
  while streaming, and never cache a mismatch.
- Store blobs once by digest, shared across all images and registries.
- Tags are metadata. Resolve a tag to a digest using the existing TTL with
  revalidation and stale-if-error.
- Multi-arch images: cache the index, and fetch platform manifests and
  layers only when a client asks for them.
- **Stream through:** on a blob miss, send bytes to the client as they
  arrive while writing to the temp file. Other clients asking for the same
  blob wait on the same download through the existing per-key lock. Never
  load a blob into memory. Layers can be several GB.
- Large blob downloads must survive slow clients without holding the
  upstream connection open forever. Use a sensible timeout and
  backpressure.
- Blobs count toward GRANARY_CACHE_LIMIT.

### Client auth
- Docker supports Basic auth to registries. `docker login granary.host`
  with any username and an access token as the password.
- Without credentials, return 401 with
  `WWW-Authenticate: Basic realm="granary"`.
- Check how Docker sends credentials to a host configured in
  `registry-mirrors`. If it doesn't send stored credentials there, add
  `GRANARY_OCI_ANONYMOUS_PULL` (default off). When on, pulls under `/v2/`
  need no token, while still honoring the access policy for private
  content. Document the tradeoff.

### Offline mode
Serve cached tags and digests. Anything not cached returns the standard
MANIFEST_UNKNOWN or BLOB_UNKNOWN error, with a message saying Granary is
offline.

### TLS note (docs)
Docker refuses plain HTTP registries unless marked insecure. Document:
- running behind the reverse proxy with an internal certificate
- installing the CA at `/etc/docker/certs.d/{host}/ca.crt`
- the Docker Desktop equivalent on macOS

### UI
- Browse gets an Images section: registry, then repository, then tags.
  For each tag show the digest, platforms, total size, and when it was
  last pulled.
- Admins can delete a tag, or a whole repository.
- Overview gets an OCI tank.

### Client setup
`make use-granary TOOLS=docker` doesn't edit the Docker daemon config,
because that needs root and differs on macOS. Instead it:
- runs `docker login` against Granary with the token
- prints the exact `registry-mirrors` snippet for `daemon.json`, and where
  that file lives on Linux and on Docker Desktop
- prints the image-name prefix form for registries other than Docker Hub,
  for example `granary.host/ghcr.io/owner/app:tag`

Docs also cover containerd and Kubernetes (`hosts.toml` mirror config)
and Podman (`registries.conf`).

### Tests
- Against a mock registry with Bearer auth:
  - pull a single-arch and a multi-arch image
  - tag revalidation, digest immutability, and the digest mismatch case
  - concurrent pulls of one blob causing one upstream download
  - Range requests
  - push refused
  - offline mode
- Docker Hub name normalization (`library/`) and upstream detection.
- A real `docker pull` or `crane pull` through a test Granary, skipped
  when neither tool is available.

## Phase 2: search, templates, cleanup, Gitea registry

### Search
- The search page gets OCI:
  - Docker Hub through its public search API
  - other registries by exact repository name, listing tags through
    `/v2/{name}/tags/list`
- README note: search sends the query to Docker Hub.

### Templates and warming
- Templates get an images section with image references, like
  `nginx:1.27` or `ghcr.io/owner/app:1.0`, plus platforms, defaulting to
  linux/amd64 and linux/arm64.
- Granary warms images itself, with no warmer needed: resolve each tag,
  then fetch the manifests and blobs for each listed platform.
- Add a default "Homelab basics" template: nginx, postgres, redis, and
  alpine.

### Cleanup
- Cleanup policies for images:
  - delete tags not pulled in N days
  - keep the newest N tags per repository
- Garbage collection removes blobs no cached manifest references. Run it
  as a job with a dry-run option that shows what would be freed.

### Gitea container registry
- With the Gitea integration configured, images under the Gitea host
  (`{gitea-host}/{owner}/{image}`) are fetched with the Gitea token and
  marked private, following the access policy.

### Tests
- Template warming for two platforms.
- Cleanup policies and GC, including dry run and a blob shared by two
  images.
- Private images hidden from principals without access.
- Docker Hub search.

## Commits and docs
- Commit messages contain only a description of the change and the
  `Signed-off-by: Dev <dev@dynatheia.com>` line.
- README sections:
  - setup for Docker, Docker Desktop, containerd, Kubernetes, and Podman
  - TLS
  - Docker Hub credentials
  - limits

  No em dashes.
