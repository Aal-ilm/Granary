# Granary warmer: cargo, node/npm, pip, Maven and git, run against Granary only.
# git checks out repo templates (through Granary's git cache) and fetches
# Cargo and npm git dependencies.
# Base images are pinned by digest; update the tag and digest together.
# Packages are upgraded at build time (Debian's, and npm itself), so the
# weekly rebuild picks up security fixes between digest bumps.

FROM node:24-trixie-slim@sha256:173f125896c3b47ddf056734c7ea789d04595a6a08769a8f78e0df642781fb66 AS node

FROM rust:1.98-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7

RUN apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends \
        ca-certificates git python3 python3-venv maven openjdk-21-jdk-headless \
    && rm -rf /var/lib/apt/lists/* \
    # Removed in this same layer so no layer keeps the stock settings.xml;
    # a minimal one is copied in below.
    && rm -f /etc/maven/settings.xml /usr/share/maven/conf/settings.xml

# Node and npm from the official image.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && npm install -g npm@latest --no-audit --no-fund \
    && npm cache clean --force \
    && node --version && npm --version

# npm bundles its own copies of some libraries. Where npm hasn't shipped a
# patched copy yet, swap in the fixed patch release (same major version, so
# npm's own dependency ranges still hold). Drop a line once npm catches up.
RUN set -e; cd /tmp; \
    for pkg in brace-expansion@5.0.11 undici@6.28.1; do \
        name="${pkg%@*}"; dir="/usr/local/lib/node_modules/npm/node_modules/$name"; \
        [ -d "$dir" ] || continue; \
        tgz="$(npm pack --silent "$pkg")"; \
        rm -rf "$dir" && mkdir -p "$dir" && tar -xzf "$tgz" -C "$dir" --strip-components=1 && rm "$tgz"; \
    done; \
    npm cache clean --force; npm --version

COPY maven-settings.xml /etc/maven/settings.xml
COPY maven-settings.xml /usr/share/maven/conf/settings.xml

# pip in its own environment, so Debian's system Python is left alone.
RUN python3 -m venv /opt/pip && /opt/pip/bin/pip --version

COPY warmer.py /usr/local/bin/granary-warmer
RUN chmod 0755 /usr/local/bin/granary-warmer \
    && useradd --uid 10002 --create-home --shell /usr/sbin/nologin warmer \
    && mkdir /work && chown 10002:10002 /work

ENV WARMER_PIP=/opt/pip/bin/pip \
    WARMER_LISTEN=0.0.0.0:9090 \
    WARMER_WORKDIR=/work
USER 10002
EXPOSE 9090
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s \
    CMD ["python3", "/usr/local/bin/granary-warmer", "healthcheck"]
CMD ["python3", "/usr/local/bin/granary-warmer"]
