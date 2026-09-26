# syntax=docker/dockerfile:1.7
#
# 2kHz's server: the API, the crawl and the whole analysis pipeline, in one
# binary. The CLAP weights (~620MB) are not in the image; the server fetches
# them into /data/models the first time analyse, build-space or text steering
# needs them, or up front with `two-khz-server models`.
#
# Build from the repo root; the context needs app/, server/ and schema.sql.
#
#   docker build -t two-khz-server .

ARG RUST_VERSION=1.96.0
ARG DEBIAN_SUITE=trixie

# --------------------------------------------------------------------- build

FROM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS builder

# SQLite is `bundled` and tokenizers brings onig, so both want a C compiler.
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY app ./app
COPY server ./server
# `db.rs` embeds it with include_str!, so it is a build input, not data.
COPY schema.sql ./schema.sql

# No GTK, no webkit: the server takes `two-khz-app` with default features
# off, so nothing here wants a window.
#
# `ort` fetches an onnxruntime build during this step, so the build needs the
# network. Whatever it leaves behind gets carried into the runtime image; the
# cache mount means the target directory is gone by the time the layer lands,
# hence the copy into /out inside the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/server/target \
    set -eu; \
    cargo build --release --locked --manifest-path server/Cargo.toml; \
    mkdir -p /out; \
    cp server/target/release/two-khz-server /out/; \
    find server/target/release -name 'libonnxruntime*.so*' -exec cp {} /out/ \;

# -------------------------------------------------------------------- server

FROM debian:${DEBIAN_SUITE}-slim AS server

# libstdc++ for onnxruntime, curl for the healthcheck.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl libstdc++6 \
 && rm -rf /var/lib/apt/lists/*

# Not --system: that expects a uid below SYS_UID_MAX, and a high fixed uid is
# what keeps a bind-mounted /data chownable to something predictable.
RUN useradd --uid 10001 --user-group --create-home --home-dir /home/twokhz \
        --shell /usr/sbin/nologin twokhz

COPY --from=builder /out/ /opt/two-khz/
RUN set -eu; \
    mv /opt/two-khz/two-khz-server /usr/local/bin/two-khz-server; \
    find /opt/two-khz -name 'libonnxruntime*.so*' -exec mv {} /usr/local/lib/ \; ; \
    ldconfig; \
    rm -rf /opt/two-khz

# The corpus, the space, the device tokens, the model weights and any .env
# `login` writes all live in /data, it is the only thing here worth a
# backup. /cache holds fetched excerpts, evicted as analyse runs.
ENV HOME=/home/twokhz \
    TWO_KHZ_DATA_DIR=/data \
    TWO_KHZ_ENV_DIR=/data \
    TWO_KHZ_MODEL_DIR=/data/models \
    TWO_KHZ_CACHE_DIR=/cache/audio

RUN set -eu; \
    mkdir -p /data/models /cache/audio; \
    chown -R twokhz:twokhz /data /cache

WORKDIR /data
USER twokhz
EXPOSE 7700

HEALTHCHECK --interval=30s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7700/api/health || exit 1

# Subcommands pass straight through: `docker run … analyse`, `… pair --name phone`.
ENTRYPOINT ["two-khz-server"]
CMD ["serve", "--bind", "0.0.0.0:7700"]
