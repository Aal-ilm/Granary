# Running Granary on TrueNAS SCALE

This guide installs Granary as a custom app on TrueNAS SCALE 24.10
(Electric Eel) or later, which run apps with Docker. It keeps the cache and
accounts on a ZFS dataset, so they are covered by your snapshots and
replication.

## 1. Create datasets

In **Datasets**, create these under your pool (`POOL` below):

| Dataset | Purpose |
|---|---|
| `POOL/apps/granary` | Parent |
| `POOL/apps/granary/data` | The cache, accounts database and settings |
| `POOL/apps/granary/src` | Granary's source, used to build the image |

For `data`, choose the **Apps** dataset preset, or edit its permissions so
the owner is user `apps` (568) and group `apps` (568). Granary runs as that
user and needs to write there.

Snapshots of `data` are a complete backup. The `state/` folder inside it
holds the accounts database, settings and audit log; everything else is
cache that is downloaded again when needed.

## 2. Build the image

TrueNAS can't build images from the Apps screen, so build once from the
shell (**System → Shell**, or SSH as an admin):

```sh
# Copy the source onto the NAS first, e.g. with git, scp or an SMB share.
cd /mnt/POOL/apps/granary/src
sudo docker build -t granary:0.2.0 .
```

The build takes a few minutes. Repeat it with a new tag when you upgrade.

If you publish the image to a registry instead (see
`.github/workflows/ci.yml`, which pushes to GitHub Container Registry on
version tags), skip this step and use the `ghcr.io/...` image line in the
compose file.

## 3. Install the app

1. Go to **Apps → Discover Apps**, open the **⋮** menu and choose
   **Install via YAML**.
2. Name it `granary` and paste the contents of
   [`deploy/truenas/compose.yaml`](../deploy/truenas/compose.yaml).
3. Replace `POOL` with your pool's name, and set `GRANARY_PUBLIC_URL` to the
   address your machines will use, such as `http://truenas.lan:8480`.
4. Save. When the app is running, open its logs (**Apps → granary → Logs**)
   to find the first admin password. It is printed once, on first start.

Open `GRANARY_PUBLIC_URL` in a browser, sign in as `admin`, and choose a new
password. Then:

- Turn on two-factor authentication under **Account**.
- Create named accounts for people under **Users** instead of sharing
  `admin`.
- Create one access token per machine under **Access tokens**. The page
  shows the client configuration for Cargo, npm and pip.

## 4. HTTPS (recommended)

Granary itself serves plain HTTP. Tokens and passwords cross the network
with every request, so put a TLS reverse proxy in front of it if anything
other than your own trusted machines can see the traffic. Nginx Proxy
Manager, Caddy and Traefik are all available as TrueNAS apps.

Point the proxy at `http://NAS_IP:8480`, then change the app's environment:

```yaml
GRANARY_PUBLIC_URL: "https://granary.example.lan"
GRANARY_TRUSTED_PROXIES: "172.16.0.0/12"   # or the proxy's own IP
```

With an `https://` public URL, Granary marks its session cookie `Secure`
and sends `Strict-Transport-Security`. `GRANARY_TRUSTED_PROXIES` makes it
believe the proxy's `X-Forwarded-For` header, so sign-in limits and the
audit log see each client's real address rather than the proxy's. Only list
addresses that belong to your proxy: anything listed can claim to be any
client.

Once the proxy works, you can remove the `ports:` entry so Granary is only
reachable through it, provided the two apps share a Docker network.

## 5. Monitoring

Granary serves Prometheus metrics at `/metrics`. Create an access token for
your Prometheus server and add:

```yaml
scrape_configs:
  - job_name: granary
    scheme: https
    authorization:
      credentials: gran_...
    static_configs:
      - targets: ['granary.example.lan']
```

Useful alerts: `granary_upstream_up == 0` (a registry is unreachable),
`increase(granary_integrity_failures_total[1h]) > 0` (an upstream served a
file that didn't match its checksum), and `granary_disk_free_bytes` falling
below a threshold.

## Upgrading

1. Update the source in `POOL/apps/granary/src` and build with a new tag,
   e.g. `sudo docker build -t granary:0.3.0 .`
2. Take a snapshot of `POOL/apps/granary/data`.
3. Edit the app, change the image tag and save.

## Locked out?

From the TrueNAS shell:

```sh
sudo docker exec -it $(sudo docker ps -qf name=granary) \
  granary reset-password admin --disable-2fa
```
