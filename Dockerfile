FROM rust:1-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

# meshoptimizer pinned to v1.1 to match the LOD parity gates; do not bump the
# tag without re-running them.
ARG WITH_GLTFPACK=0
RUN mkdir -p /gltfpack-dist \
    && if [ "$WITH_GLTFPACK" = "1" ]; then \
        git clone --depth 1 --branch v1.1 \
            https://github.com/zeux/meshoptimizer /tmp/meshoptimizer \
        && cmake -S /tmp/meshoptimizer -B /tmp/meshoptimizer/build \
            -DCMAKE_BUILD_TYPE=Release -DMESHOPT_BUILD_GLTFPACK=ON \
        && cmake --build /tmp/meshoptimizer/build --target gltfpack -j "$(nproc)" \
        && cp /tmp/meshoptimizer/build/gltfpack /gltfpack-dist/ \
        && rm -rf /tmp/meshoptimizer; \
    fi

WORKDIR /src
COPY . .

RUN cargo build --release --locked --bin abgen

FROM debian:bookworm-slim

ARG COMMIT_HASH=unknown
LABEL org.opencontainers.image.revision=$COMMIT_HASH

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libturbojpeg0 tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 abgen \
    && mkdir -p /data/out /data/cache \
    && chown -R abgen /data

COPY --from=build /src/target/release/abgen /usr/local/bin/abgen

# Template payload is required: without it every JIT conversion fails and
# /health degrades. The windows+mac scene shaders self-prime from the vendored
# payloads on first request (S3 write-back included); the lit_ignore_* /
# texarray / linux shader names 404 by design (absent upstream too).
COPY --from=build /src/template /opt/abgen/template
COPY --from=build /src/crate/shader /opt/abgen/shader

# LOD JIT is double-gated: WITH_GLTFPACK=1 image AND ABGEN_LOD_JIT=1; without
# the binary the lane fails closed.
COPY --from=build /gltfpack-dist/ /usr/local/bin/

ENV ABGEN_ROOT=/opt/abgen \
    ABGEN_SHADER_BUNDLE=/opt/abgen/shader/scene_ignore_windows \
    ABGEN_OUT_ROOT=/data/out \
    ABGEN_CACHE_DIR=/data/cache \
    ABGEN_HTTP_HOST=0.0.0.0 \
    ABGEN_LOG_FORMAT=json

USER abgen
EXPOSE 5147

# tini is PID 1: it reaps zombies and forwards SIGTERM to the server so ECS
# deploy/scale-in drains in seconds instead of hanging to the 30s SIGKILL.
# Please _DO NOT_ replace this with a bare-binary ENTRYPOINT - as PID 1 the
# server would receive no signals and never shut down cleanly.
# https://aws.amazon.com/blogs/containers/graceful-shutdowns-with-ecs/
ENTRYPOINT ["/usr/bin/tini", "-g", "--", "/usr/local/bin/abgen"]
