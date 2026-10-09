# ---- build ----
FROM rust:1-slim-trixie AS build
WORKDIR /src

# Build dependencies first so they are cached between source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime ----
FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin granary \
    && mkdir -p /data && chown granary:granary /data

COPY --from=build /src/target/release/granary /usr/local/bin/granary

ENV GRANARY_DATA_DIR=/data \
    GRANARY_LISTEN=0.0.0.0:8080 \
    RUST_LOG=granary=info

VOLUME /data
EXPOSE 8080
USER granary

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["granary", "healthcheck"]

ENTRYPOINT ["granary"]
