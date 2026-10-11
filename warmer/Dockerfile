# Granary warmer: cargo, node/npm, pip and Maven, run against Granary only.
# Base images are pinned by digest; update the tag and digest together.

FROM node:24-trixie-slim@sha256:173f125896c3b47ddf056734c7ea789d04595a6a08769a8f78e0df642781fb66 AS node

FROM rust:1.98-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates python3 python3-venv maven openjdk-21-jdk-headless \
    && rm -rf /var/lib/apt/lists/*

# Node and npm from the official image.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && node --version && npm --version

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
