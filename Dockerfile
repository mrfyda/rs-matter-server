# Build for arm64 Linux with:
#   docker build --platform linux/arm64 -t rs-matter-server .
#
# The file deliberately avoids BuildKit-only syntax so it also builds with the
# classic builder.

FROM rust:1-bookworm AS builder

# rs-matter is a large crate and rustc's peak memory is per-parallel-job, so a
# small builder can get the compiler OOM-killed. `default` lets cargo choose;
# set it to 1 on a constrained machine. A single rustc on rs-matter needs
# roughly 2 GiB on its own, so a builder below about 4 GiB will struggle
# whatever this is set to.
ARG CARGO_JOBS=default
ENV CARGO_BUILD_JOBS=${CARGO_JOBS}

# Optimization level for the release profile. The default is what any normal
# builder should use; lowering it is an escape hatch for a builder too
# small to run LLVM at full optimization, and it produces a slower binary.
ARG CARGO_OPT_LEVEL=3
ENV CARGO_PROFILE_RELEASE_OPT_LEVEL=${CARGO_OPT_LEVEL}

WORKDIR /build/server

# Dependencies are compiled against a stub crate first, so editing this
# project's own source does not recompile rs-matter and the rest of the tree.
COPY server/Cargo.toml server/Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && : > src/lib.rs \
    && cargo build --release \
    && rm -rf src

COPY server/src ./src
# COPY preserves the context's timestamps, which can predate the stub build;
# without this cargo may consider the stub artifacts still current.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release \
    && install -D target/release/rs-matter-server /out/rs-matter-server

# The state directory is staged here because the runtime image has no shell to
# create it with, and it needs to belong to the unprivileged user.
RUN mkdir -p /out/data

# ---------------------------------------------------------------------------

# glibc and libgcc, no package manager, no shell. The Debian 12 variant matches
# the bookworm builder's glibc, which a dynamically linked binary depends on.
#
# No CA certificates are installed and none are needed: the HTTPS client used
# for firmware update checks is rustls with `webpki-roots`, so the Mozilla root
# store is compiled into the binary rather than read from disk.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /out/rs-matter-server /usr/local/bin/rs-matter-server
# uid 65532 is distroless's `nonroot`. Nothing here needs privilege: the ports
# are above 1024 and joining a multicast group does not require root.
COPY --from=builder --chown=65532:65532 /out/data /data

USER 65532:65532
WORKDIR /

# Fabric, node and configuration state. Losing this means re-commissioning
# every device, so it must be a volume.
VOLUME ["/data"]

ENV LISTEN_ADDRESS=0.0.0.0:5580 \
    STORAGE_PATH=/data \
    LOG_LEVEL=info

# The WebSocket API and the OTA upload endpoint. Matter itself needs host
# networking (see the compose file), which bypasses this mapping.
EXPOSE 5580/tcp

# Exec form on purpose: there is no shell in this image to interpret a string.
HEALTHCHECK --interval=30s --timeout=10s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/rs-matter-server", "--health-check"]

ENTRYPOINT ["/usr/local/bin/rs-matter-server"]
