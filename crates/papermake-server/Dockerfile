ARG RUST_VERSION=1.97.0
ARG PACKAGE_NAME=papermake-server
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
    --mount=type=bind,source=vendor,target=vendor \
    --mount=type=cache,target=/app/target/,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git/db,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry/,sharing=locked \
    cargo build --locked --release -p papermake-server && \
    cp ./target/release/papermake-server /bin/server

# Provides the compiled binary. CI overrides this stage with a prebuilt binary:
#   docker build --build-context dist=./dist ...
# where dist/papermake-server is the release binary the test stage already
# compiled, so the image build skips the (slow, sometimes flaky) in-Docker
# compile above. Without that flag — local `docker compose build`, GitHub
# Actions — it falls back to the binary the `build` stage just produced.
FROM scratch AS dist
COPY --from=build /bin/server /papermake-server

# Distroless runtime: ships glibc, libssl3, ca-certificates, and a nonroot
# user (uid 65532) — no shell, no package manager, nothing else. The `cc`
# variant adds libgcc/libstdc++ needed by the dynamically-linked Rust binary.
FROM gcr.io/distroless/cc-debian13:nonroot AS final

# No fonts are baked into the image beyond Typst's embedded set. Scan a mounted
# /fonts volume so operators add fonts by dropping files in the mount — no
# rebuild. FONTS_DIR is a `:`-separated list of directories; missing dirs are
# skipped.
ENV FONTS_DIR=/fonts

# Copy the executable last; this is the layer that changes with app code.
COPY --from=dist /papermake-server /app/server
CMD ["/app/server"]
