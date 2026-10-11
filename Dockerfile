# ---- build ----
FROM rust:1-slim-trixie AS build
WORKDIR /src

# Build dependencies first so they are cached between source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY web ./web
# Built-in templates are compiled into the binary.
COPY templates ./templates
RUN touch src/main.rs && cargo build --release --locked \
    && mkdir /data-dir

# ---- runtime ----
# Debian slim with only what Granary runs: git (for the git cache) and CA
# certificates. Packages are upgraded at build time so security fixes land
# with every build, and CI's Trivy scan still fails on fixable high or
# critical issues. SQLite is compiled into Granary itself.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    # UID 10001 matches earlier images, so existing data volumes stay writable.
    && useradd --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin granary

COPY --from=build /src/target/release/granary /usr/local/bin/granary
COPY --from=build --chown=10001:10001 /data-dir /data

ENV GRANARY_DATA_DIR=/data \
    GRANARY_LISTEN=0.0.0.0:8080 \
    RUST_LOG=granary=info

VOLUME /data
EXPOSE 8080
USER 10001:10001

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["granary", "healthcheck"]

ENTRYPOINT ["granary"]
