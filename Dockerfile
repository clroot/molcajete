# syntax=docker/dockerfile:1.7
#
# molcajete images: the web gateway (with the wasm client) and molcajeted.
#
#   docker build --target gateway -t molcajete-gateway .
#   docker build --target daemon  -t molcajeted .
#
# CARGO_PROFILE=profiling (default) is the release build plus line tables, so
# CPU profiles resolve inlined code; use `release` for slightly smaller images.

ARG RUST_VERSION=1.94
ARG DEBIAN_SUITE=trixie

# ---------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS toolchain
ARG WASM_BINDGEN_VERSION=0.2.130
RUN apt-get update \
 && apt-get install -y --no-install-recommends binaryen \
 && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install wasm-bindgen-cli --version "${WASM_BINDGEN_VERSION}" --locked

# ---------------------------------------------------------------------------
FROM toolchain AS build
ARG CARGO_PROFILE=profiling
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    cargo build --locked --profile "${CARGO_PROFILE}" \
        -p molcajete-gateway -p molcajete-daemon; \
    scripts/build-web.sh; \
    mkdir -p /out/bin; \
    cp "target/${CARGO_PROFILE}/molcajete-gateway" "target/${CARGO_PROFILE}/molcajeted" /out/bin/; \
    cp -r web /out/web; \
    chmod -R a+rX /out

# ---------------------------------------------------------------------------
FROM debian:${DEBIAN_SUITE}-slim AS runtime-base
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin molcajete
ENV MOLCAJETE_LOG_FORMAT=json
ENTRYPOINT ["/usr/bin/tini", "--"]

# ---------------------------------------------------------------------------
FROM runtime-base AS daemon
# ffmpeg provides the video encoders (libsvtav1, libx265, libx264); it runs as
# a child process and is not linked.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/bin/molcajeted /usr/local/bin/molcajeted
ENV MOLCAJETED_BIND_HOST=0.0.0.0 \
    MOLCAJETED_BIND_PORT=4822
USER molcajete
EXPOSE 4822 9100
CMD ["molcajeted"]

# ---------------------------------------------------------------------------
FROM runtime-base AS gateway
COPY --from=build /out/bin/molcajete-gateway /usr/local/bin/molcajete-gateway
COPY --from=build /out/web /usr/share/molcajete/web
ENV MOLCAJETE_CONFIG=/etc/molcajete/gateway.toml \
    MOLCAJETE_LISTEN=0.0.0.0:8080 \
    MOLCAJETE_STATIC_DIR=/usr/share/molcajete/web
USER molcajete
EXPOSE 8080 9100
CMD ["molcajete-gateway"]
