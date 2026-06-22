# syntax=docker/dockerfile:1.6
# Two-stage build for `holofs-web` (axum + Leptos SSR + WASM hydrate).
#
# - Stage 1 (builder): full Rust toolchain + `cargo-leptos`. Produces:
#   * `/out/holofs-web`  — release SSR binary
#   * `/out/site/`       — `target/site/` with the hydrate WASM bundle
#                          (wasm-opt'd; ~800 KB at v0.2.0)
#   * `/out/holofs*`     — every other CLI binary (`holofs`, `holofs-node`,
#                          `holofs-admin`, ...)
#
# - Stage 2 (runtime): debian-slim, non-root uid 10001, tini PID 1.
#
# `docker buildx` is used in CI to produce a multi-arch image
# (linux/amd64 + linux/arm64). The Dockerfile itself is arch-agnostic.

# ============== Builder stage =============================================
FROM rust:1.81-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        binaryen \
    && rm -rf /var/lib/apt/lists/*

# cargo-leptos drives the SSR + WASM build. `--locked` keeps it pinned.
RUN cargo install --locked cargo-leptos

# Rustup target for the hydrate WASM lib.
RUN rustup target add wasm32-unknown-unknown

WORKDIR /holofs

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY assets ./assets

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/holofs/target \
    cargo leptos build --project holofs-web --release && \
    cargo build --release --workspace --bins --exclude holofs-web && \
    mkdir -p /out/bin /out/site && \
    cp target/release/holofs-web /out/bin/ && \
    for bin in holofs holofs-node holofs-admin holofs-bench \
               holofs-inspect holofs-cluster holofs-fs; do \
        cp target/release/$bin /out/bin/ 2>/dev/null || true; \
    done && \
    cp -R target/site/. /out/site/

# ============== Runtime stage =============================================
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --create-home --shell /bin/false holofs

WORKDIR /home/holofs

COPY --from=builder /out/bin/ /usr/local/bin/
# `holofs-web` reads its hydrate bundle path from LEPTOS_SITE_ROOT; copy
# the cargo-leptos output to a stable location and point the binary at it.
COPY --from=builder /out/site/ /var/lib/holofs/site/

VOLUME ["/data"]
ENV HOLOFS_STORAGE_DIR=/data \
    LEPTOS_SITE_ROOT=/var/lib/holofs/site

USER holofs
EXPOSE 8787

ENTRYPOINT ["/usr/bin/tini", "--", "holofs-web"]
CMD ["--addr", "0.0.0.0:8787"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD wget --no-verbose --tries=1 --spider http://localhost:8787/ || exit 1

LABEL org.opencontainers.image.title="holofs" \
      org.opencontainers.image.description="Holographic distributed filesystem" \
      org.opencontainers.image.source="https://github.com/holofs/holofs" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
