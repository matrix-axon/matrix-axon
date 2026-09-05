# syntax=docker/dockerfile:1

# Multi-stage build for the `axon-server` binary (crate axon-server, [[bin]] axon-server).
#
# The whole workspace builds one self-contained binary: rustls + aws-lc-rs, sqlx's
# Postgres driver compiled in (no libpq), no OpenSSL / libsqlite3. So the runtime
# image needs only ca-certificates (to reach Matrix homeservers over HTTPS) plus
# curl for the healthcheck. See ADR 0052.

# ---- builder ---------------------------------------------------------------
# Pinned to the toolchain in rust-toolchain.toml (1.95.0). aws-lc-rs needs cmake +
# a C compiler at build time (clang/libclang for its bindgen path); nothing at run.
FROM rust:1.95-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# build.rs reads the GIT_HASH env var below first, then falls back to git/jj.
# `.git` is excluded from the build context (see .dockerignore) so those
# subprocesses can't run here — pass --build-arg GIT_HASH=$(git rev-parse --short
# HEAD) to stamp the image (it surfaces as AXON_GIT_HASH in the server's logs).
ARG GIT_HASH=unknown
ENV GIT_HASH=${GIT_HASH}

# Copy the whole workspace, then build only the server binary. BuildKit cache
# mounts keep the cargo registry and target/ warm across rebuilds (the dependency
# graph — matrix-rust-sdk, tantivy, aws-lc-rs — compiles slowly). The binary is
# copied out of the cache-mounted target/ before the mount is unmounted.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p axon-server --bin axon-server \
    && cp /build/target/release/axon-server /usr/local/bin/axon-server

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Links a GHCR package to this repo (inherits access/visibility, shows on the
# repo's Packages) when pushed to ghcr.io/matrix-axon/axon-server. See ADR 0052.
LABEL org.opencontainers.image.source=https://github.com/matrix-axon/matrix-axon

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root service account. The data volume is owned by it so a fresh named
# volume inherits the right ownership on first mount.
RUN useradd --system --uid 10001 --user-group --home-dir /var/lib/axon \
        --create-home --shell /usr/sbin/nologin axon

# All durable + disposable state lives under the mounted data dir:
#   axon.toml     — generated config (store_key + db url), CRITICAL, back this up
#   data/         — matrix-rust-sdk state + crypto store, CRITICAL
#   search/       — Tantivy index, rebuildable from Postgres
#   media/        — disposable LRU cache
ENV AXON_CONFIG=/var/lib/axon/axon.toml \
    AXON_SYNC__DATA_DIR=/var/lib/axon/data \
    AXON_SEARCH__INDEX_PATH=/var/lib/axon/search \
    AXON_MEDIA__CACHE_DIR=/var/lib/axon/media \
    AXON_SERVER__HOST=0.0.0.0 \
    AXON_SERVER__ALLOW_INSECURE_BIND=true

COPY --from=builder /usr/local/bin/axon-server /usr/local/bin/axon-server
# --chmod=0755 so the non-root `axon` user can read+execute it regardless of the
# source file's mode (a plain `chmod +x` on a 0640 source yields 0750, root-owned
# and unreadable by `axon`, breaking the entrypoint).
COPY --chmod=0755 deploy/entrypoint.sh /usr/local/bin/entrypoint.sh

VOLUME /var/lib/axon
EXPOSE 8080
USER axon
WORKDIR /var/lib/axon

# Liveness probe: /healthz is unauthenticated and does not touch the DB.
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=5 \
    CMD curl -fsS http://localhost:8080/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
