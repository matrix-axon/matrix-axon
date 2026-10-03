# syntax=docker/dockerfile:1

# Web client image for the full-stack deployment (ADR 0052). Two stages:
# build the Preact/Vite SPA (clients/web) with Node + pnpm, then serve the
# static bundle with Caddy — which also reverse-proxies the API (/v1 + /healthz,
# including the /v1/ws WebSocket) to axon-server on the compose bridge. Serving
# the SPA and the API from one origin means the browser needs no CORS and the
# client's default same-origin base URL "just works". See deploy/web/Caddyfile.
#
# Build context is the repo root (matching the axon-server image), so the paths
# below are repo-relative: clients/web/… and deploy/web/Caddyfile.

# ---- build -----------------------------------------------------------------
# Node 24 (LTS) matches the web client's CI floor (README: Node >= 22.13).
FROM node:24-slim AS build
RUN corepack enable
WORKDIR /web

# Manifests first so the dependency layer is cached and only re-resolves when
# they change. pnpm is pinned via package.json's packageManager field, which
# corepack honors. (pnpm-workspace.yaml carries only an allowBuilds note — no
# workspace globs — so a manifest-only copy resolves cleanly.)
COPY clients/web/package.json clients/web/pnpm-lock.yaml clients/web/pnpm-workspace.yaml ./
RUN --mount=type=cache,target=/pnpm-store \
    pnpm install --frozen-lockfile --store-dir=/pnpm-store

# Sources, then the production build. The .git dir is excluded from the build
# context, so vite's git-based version stamp can't run; pass the commit in as
# GIT_HASH (mirrors the server image) to show a real build id in Settings.
COPY clients/web/ ./
# The build bundles the privacy policy from docs/ (the `axon-privacy-policy`
# plugin in vite.config.ts reads ../../docs/PRIVACY_POLICY.md, which from
# /web resolves to /docs) and fails without it.
COPY docs/PRIVACY_POLICY.md /docs/PRIVACY_POLICY.md
ARG GIT_HASH=unknown
ENV VITE_AXON_WEB_VERSION=${GIT_HASH}
RUN pnpm build

# ---- runtime ---------------------------------------------------------------
FROM caddy:2 AS runtime
# Links a GHCR package to this repo (inherits access/visibility) when pushed to
# ghcr.io/matrix-axon/axon-web. See ADR 0052.
LABEL org.opencontainers.image.source=https://github.com/matrix-axon/matrix-axon
COPY deploy/web/Caddyfile /etc/caddy/Caddyfile
COPY --from=build /web/dist /srv
EXPOSE 8080
