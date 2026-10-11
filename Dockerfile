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
# Wolfi: a minimal glibc base whose packages track upstream security fixes
# closely. Granary needs only git (git-daemon carries git-http-backend for
# the git cache) and CA certificates. The tag is left unpinned so every
# build, including CI's weekly rebuild, picks up fixes; CI's Trivy scan
# still fails on fixable high or critical issues. SQLite is compiled into
# Granary itself.
FROM cgr.dev/chainguard/wolfi-base

RUN apk add --no-cache git git-daemon ca-certificates-bundle \
    # UID 10001 matches earlier images, so existing data volumes stay writable.
    && addgroup -g 10001 granary \
    && adduser -D -H -u 10001 -G granary -s /sbin/nologin granary

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
