# syntax=docker/dockerfile:1
#
# Launchpad headless deployment image(s).
#
# One Dockerfile, two build targets (see deploy/docker-compose.yml):
#
#   backend   — the `ao-server` axum daemon plus the `ao-engine-tools-cli`
#               REPL binary. No Tauri shell: the server is a plain headless
#               HTTP daemon (crates/ao-server/src/main.rs).
#   frontend  — the Vite/React static build served by nginx. ao-server does
#               NOT serve static files itself (no tower-http ServeDir in its
#               router), so a separate static file server is required.
#
# Build context is the REPO ROOT — both the Rust workspace and the frontend
# are built from it, trimmed by the repo-root .dockerignore.
#
# Arch-neutral: builds whatever platform `docker build` targets (OrbStack on
# Apple Silicon produces aarch64 images automatically).

# ---------------------------------------------------------------------------
# Rust builder
# ---------------------------------------------------------------------------
# rust-toolchain.toml pins channel = "1.99" (floor set by matrix-sdk 0.19's
# MSRV). The official rust:1.99 image matches it exactly; if that tag is ever
# unavailable, switch to `rust:1-bookworm` and add
# `RUN rustup toolchain install 1.99 && rustup default 1.99` before the build.
FROM rust:1.99-bookworm AS rust-builder

# Native build requirements, by dependent:
#   - build-essential/pkg-config: cc-driven builds (rusqlite `bundled`,
#     libdbus-sys, aws-lc-sys).
#   - libdbus-1-dev: nosleep-nix (via `nosleep` in ao-engine) links libdbus
#     on Linux; libdbus-sys's build script pkg-configs it.
#   - cmake + perl: aws-lc-sys (matrix-sdk's `rustls-aws-lc-rs` feature)
#     compiles aws-lc from source.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libdbus-1-dev \
        cmake \
        perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the whole (dockerignore-trimmed) workspace rather than a manifest-only
# pre-build: every member is a path dependency of every other, so the usual
# "dummy main.rs" cache-warming trick needs a stub per crate and rots fast.
COPY . .

# --locked: build exactly what Cargo.lock pins (same exact-pin discipline as
# the workspace manifests). Only the two deployable binaries are built; the
# Tauri shell (frontend/src-tauri) is deliberately not a build target here.
RUN cargo build --release --locked \
        -p ao-server \
        -p ao-engine-tools-cli

# ---------------------------------------------------------------------------
# Frontend builder
# ---------------------------------------------------------------------------
# engines in frontend/package.json: ^20.19 || ^22.13 || >=24.
FROM node:22-bookworm-slim AS frontend-builder

WORKDIR /build/frontend

# Baked into the bundle at build time (import.meta.env.VITE_API_BASE_URL,
# read in frontend/src/lib/api.ts) — this is the address a LAN browser will
# use to reach the backend, e.g. http://192.168.1.150:3101. Passed by
# docker-compose.yml from the host environment; NOT a secret.
ARG VITE_API_BASE_URL=http://localhost:3101
ENV VITE_API_BASE_URL=${VITE_API_BASE_URL}

# Dependency layer first so source-only edits reuse the npm ci cache.
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci

COPY frontend/ ./
RUN npm run build

# ---------------------------------------------------------------------------
# Backend runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS backend

# ca-certificates: outbound TLS to model providers / Matrix homeservers /
# Telegram. curl: HEALTHCHECK only.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root daemon user. The data root lives on the compose-managed named
# volume mounted at /data; Docker seeds an empty named volume with this
# directory's ownership, so chown here is sufficient for first boot.
RUN useradd --system --create-home --uid 10001 launchpad \
    && mkdir -p /data \
    && chown launchpad:launchpad /data

COPY --from=rust-builder /build/target/release/ao-server /usr/local/bin/ao-server
COPY --from=rust-builder /build/target/release/ao-engine-tools-cli /usr/local/bin/ao-engine-tools-cli

USER launchpad

# All of these are readable in crates/ao-server/src/main.rs and
# crates/ao-protocol/src/data_root.rs:
#   AO_PORT                          — listen port (server default: 3001)
#   AO_BIND_HOST                     — bind address (server default: 127.0.0.1;
#                                      0.0.0.0 required for LAN reachability.
#                                      NOTE: binding non-loopback disables the
#                                      webhook INSECURE_NO_AUTH loopback bypass
#                                      — every webhook route then requires a
#                                      real HMAC secret. See
#                                      crates/ao-server/src/webhook_gateway.rs)
#   LAUNCHPAD_STUDIO_DATA_DIR        — data root override; pinned at /data so
#                                      ALL state (agents, transcripts,
#                                      providers.toml, the file-backed secret
#                                      vault, workspace lock) lives on the
#                                      volume regardless of $HOME.
#   LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK — force the file-backed secret vault
#                                      (<data-root>/secret_vault.json, mode
#                                      0600). A headless container has no OS
#                                      keychain; without this the vault's
#                                      keychain probe is the only thing
#                                      deciding the backend.
ENV AO_PORT=3101 \
    AO_BIND_HOST=0.0.0.0 \
    LAUNCHPAD_STUDIO_DATA_DIR=/data \
    LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK=1

EXPOSE 3101
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${AO_PORT}/health" || exit 1

# ao-server installs its own SIGTERM handler (graceful channel-lease release),
# so `docker stop` shuts it down cleanly with the default stop signal.
ENTRYPOINT ["ao-server"]

# ---------------------------------------------------------------------------
# Frontend runtime
# ---------------------------------------------------------------------------
FROM nginx:1.27-alpine AS frontend

COPY deploy/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=frontend-builder /build/frontend/dist /usr/share/nginx/html

EXPOSE 80
