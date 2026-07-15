ARG RUST_VERSION=1.97.0
ARG PACKAGE_NAME=papermake-worker
FROM rust:${RUST_VERSION}-slim AS build

# Install required build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy workspace files and build
RUN --mount=type=bind,source=Cargo.toml,target=Cargo.toml \
    --mount=type=bind,source=Cargo.lock,target=Cargo.lock \
    --mount=type=bind,source=crates,target=crates \
    --mount=type=cache,target=/app/target/,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry/,sharing=locked \
    cargo build --locked --release -p papermake-worker && \
    cp ./target/release/papermake-worker /bin/worker

# Distroless runtime: ships glibc, libssl3, ca-certificates, and a nonroot
# user (uid 65532) — no shell, no package manager, nothing else. The `cc`
# variant adds libgcc/libstdc++ needed by the dynamically-linked Rust binary.
FROM gcr.io/distroless/cc-debian13:nonroot AS final

# No fonts baked in beyond Typst's embedded set; the worker scans the same
# mounted /fonts volume as the server so batch renders match single renders.
# FONTS_DIR is a `:`-separated list of directories.
ENV FONTS_DIR=/fonts

# Copy the executable from the "build" stage.
COPY --from=build /bin/worker /app/worker

# What the container should run when it is started.
CMD ["/app/worker"]
