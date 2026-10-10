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
# Distroless holds only glibc, libgcc and CA certificates: everything the
# binary links against, with no shell, package manager or other tools to
# carry vulnerabilities. SQLite is compiled into Granary itself.
FROM gcr.io/distroless/cc-debian13:nonroot

COPY --from=build /src/target/release/granary /usr/local/bin/granary
# UID 10001 matches earlier images, so existing data volumes stay writable.
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
