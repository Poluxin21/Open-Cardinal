# syntax=docker/dockerfile:1.7
#
# Open Cardinal — container image.
#
#   docker build -t open-cardinal .
#   docker build -t open-cardinal:ai --build-arg FEATURES=prompt .   # + embedded ONNX Runtime
#
# The image is distroless (no shell, no package manager), runs as a non-root user and keeps all
# state under /var/lib/cardinal (CARDINAL_HOME): mount a volume there.

ARG RUST_VERSION=1
FROM rust:${RUST_VERSION}-bookworm AS build
ARG FEATURES=""
WORKDIR /src

# Only what the build needs, so editing docs or manifests does not invalidate the cache.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto proto
COPY src src

# protoc comes from the `protoc-bin-vendored` build dependency: nothing to install.
RUN cargo build --release --locked --bin open-cardinal --bin client ${FEATURES:+--features "$FEATURES"}

RUN mkdir -p /out /rootfs/var/lib/cardinal \
 && cp target/release/open-cardinal target/release/client /out/ \
 # the ONNX Runtime shared library (only present for --features onnx/prompt) must sit next to the binary
 && (cp target/release/*.so* /out/ 2>/dev/null || true)

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/ /usr/local/bin/
COPY --from=build --chown=65532:65532 /rootfs/var/lib/cardinal /var/lib/cardinal

ENV CARDINAL_HOME=/var/lib/cardinal \
    LD_LIBRARY_PATH=/usr/local/bin

VOLUME ["/var/lib/cardinal"]
# agents (gRPC) · HTTP (health, metrics, audit) · cluster (Raft)
EXPOSE 50051 8080 50052
USER 65532:65532

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
  CMD ["/usr/local/bin/open-cardinal", "health"]

ENTRYPOINT ["/usr/local/bin/open-cardinal"]
